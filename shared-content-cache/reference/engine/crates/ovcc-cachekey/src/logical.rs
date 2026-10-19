// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use crate::CacheKey;

/// Maximum expansion before deduplication. Bounding work as well as output
/// prevents a policy with many optional parts from causing exponential work.
pub const MAX_PHYSICAL_KEYS: usize = 16;
const MAX_KEY_PARTS: usize = 64;

/// One named component. `None` and `Some("")` remain distinct here even when a
/// legacy backend renderer maps them to the same wire key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyPart {
    pub name: String,
    pub value: Option<String>,
    pub optional: bool,
}

/// Backend-independent identity. Policy supplies the renderer and validates a
/// match; expanding a key alone never authorizes reuse of a cached response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogicalKey {
    policy: String,
    parts: Vec<KeyPart>,
}

/// Invalid policies fail before registration, without partially publishing keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalKeyError {
    InvalidPolicy,
    InvalidPartName,
    DuplicatePart,
    UnsortedParts,
    TooManyParts,
    ExpansionLimit,
}

impl std::fmt::Display for LogicalKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidPolicy => "invalid logical-key policy identifier",
            Self::InvalidPartName => "invalid logical-key part name",
            Self::DuplicatePart => "duplicate logical-key part name",
            Self::UnsortedParts => "logical-key parts are not in canonical order",
            Self::TooManyParts => "too many logical-key parts",
            Self::ExpansionLimit => "logical key exceeds the physical-key expansion limit",
        })
    }
}

impl std::error::Error for LogicalKeyError {}

/// Carries the validation failure into the shared error type without losing its
/// rendered detail, so a caller working in [`ovcc_core::Result`] can use `?`.
impl From<LogicalKeyError> for ovcc_core::Error {
    fn from(error: LogicalKeyError) -> Self {
        Self::cache(error.to_string())
    }
}

/// A physical key plus the omissions that produced it. The first result is
/// always the full physical key and therefore has no omissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalKey {
    pub key: CacheKey,
    pub omitted_parts: Vec<String>,
}

/// Read-only view given to a backend renderer. Required parts cannot be omitted.
/// Iteration and lookup preserve absence versus explicit empty values.
#[derive(Clone, Copy)]
pub struct PhysicalKeyParts<'a> {
    parts: &'a [KeyPart],
    omitted: &'a [usize],
}

impl<'a> PhysicalKeyParts<'a> {
    /// Walks the parts a renderer may use, skipping the ones omitted from this key.
    pub fn iter(self) -> impl Iterator<Item = (&'a str, Option<&'a str>)> {
        self.parts.iter().enumerate().filter_map(move |(i, part)| {
            (!self.omitted.contains(&i)).then_some((part.name.as_str(), part.value.as_deref()))
        })
    }

    /// Outer `None` means omitted or unknown; inner `None` means a named part
    /// whose value was absent in the original request.
    pub fn get(self, name: &str) -> Option<Option<&'a str>> {
        self.iter()
            .find_map(|(part, value)| (part == name).then_some(value))
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

impl LogicalKey {
    /// Sorts parts into the canonical order before validating, so callers need not.
    pub fn new(policy: String, mut parts: Vec<KeyPart>) -> Result<Self, LogicalKeyError> {
        parts.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let key = Self { policy, parts };
        key.validate()?;
        Ok(key)
    }

    /// Names the policy whose renderer and schema this key must match.
    pub fn policy(&self) -> &str {
        &self.policy
    }

    /// Parts in canonical name order, required and optional alike.
    pub fn parts(&self) -> &[KeyPart] {
        &self.parts
    }

    /// Also validates deserialized records. Persisted/wire input must not bypass
    /// the constructor's bound or duplicate-name checks.
    pub fn validate(&self) -> Result<(), LogicalKeyError> {
        if !valid_name(&self.policy) {
            return Err(LogicalKeyError::InvalidPolicy);
        }
        if self.parts.len() > MAX_KEY_PARTS {
            return Err(LogicalKeyError::TooManyParts);
        }
        let mut expansion = 1_usize;
        for (i, part) in self.parts.iter().enumerate() {
            if !valid_name(&part.name) {
                return Err(LogicalKeyError::InvalidPartName);
            }
            if self.parts[..i].iter().any(|p| p.name == part.name) {
                return Err(LogicalKeyError::DuplicatePart);
            }
            if i > 0 && self.parts[i - 1].name > part.name {
                return Err(LogicalKeyError::UnsortedParts);
            }
            if part.optional {
                expansion *= 2;
                if expansion > MAX_PHYSICAL_KEYS {
                    return Err(LogicalKeyError::ExpansionLimit);
                }
            }
        }
        Ok(())
    }

    /// Optional matching only weakens values declared optional on both sides
    /// of the same policy/schema. Missing required descriptors are not wildcards.
    pub fn same_required_parts(&self, other: &Self) -> bool {
        self.policy == other.policy
            && self.parts.len() == other.parts.len()
            && self.parts.iter().zip(&other.parts).all(|(a, b)| {
                a.name == b.name && a.optional == b.optional && (a.optional || a.value == b.value)
            })
    }

    /// Full first, then fewer omissions first, with bytewise part-name order
    /// for ties. Identical rendered keys keep their earliest (most exact) match.
    /// The renderer preserves each backend's existing physical-key format.
    pub fn physical_keys(
        &self,
        mut render: impl FnMut(PhysicalKeyParts<'_>) -> CacheKey,
    ) -> Result<Vec<PhysicalKey>, LogicalKeyError> {
        self.validate()?;
        let mut optional: Vec<usize> = self
            .parts
            .iter()
            .enumerate()
            .filter_map(|(i, part)| part.optional.then_some(i))
            .collect();
        optional.sort_unstable_by(|&a, &b| self.parts[a].name.cmp(&self.parts[b].name));
        let mut omissions: Vec<Vec<usize>> = (0..(1_usize << optional.len()))
            .map(|mask| {
                optional
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &i)| (mask & (1 << bit) != 0).then_some(i))
                    .collect()
            })
            .collect();
        omissions.sort_unstable_by(|a, b| {
            a.len().cmp(&b.len()).then_with(|| {
                a.iter()
                    .map(|&i| &self.parts[i].name)
                    .cmp(b.iter().map(|&i| &self.parts[i].name))
            })
        });
        let mut keys: Vec<PhysicalKey> = Vec::with_capacity(omissions.len());
        for omitted in omissions {
            let key = render(PhysicalKeyParts {
                parts: &self.parts,
                omitted: &omitted,
            });
            if !keys.iter().any(|existing| existing.key == key) {
                keys.push(PhysicalKey {
                    key,
                    omitted_parts: omitted
                        .iter()
                        .map(|&i| self.parts[i].name.clone())
                        .collect(),
                });
            }
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KeyParts;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn part(name: &str, value: Option<&str>, optional: bool) -> KeyPart {
        KeyPart {
            name: name.into(),
            value: value.map(str::to_owned),
            optional,
        }
    }

    fn render(parts: PhysicalKeyParts<'_>) -> CacheKey {
        CacheKey::from_raw(format!("{:?}", parts.iter().collect::<Vec<_>>()))
    }

    #[test]
    fn general_render_optional_parts_exact_first_and_name_order() -> TestResult {
        // Arrange.
        let logical = LogicalKey::new(
            "test.v1".into(),
            vec![part("z", Some("B"), true), part("a", Some("A"), true)],
        )?;
        // Act.
        let keys = logical.physical_keys(render)?;
        // Assert.
        let omissions: Vec<_> = keys.iter().map(|k| k.omitted_parts.as_slice()).collect();
        assert_eq!(omissions, [vec![], vec!["a"], vec!["z"], vec!["a", "z"]]);
        Ok(())
    }

    #[test]
    fn general_render_optional_parts_preserves_legacy_exact_key() -> TestResult {
        // Arrange.
        let legacy = KeyParts {
            generation: "0",
            host: "host",
            method: "GET",
            uri: "/blob",
            filtered_args: "",
            if_match: "\"v1\"",
            range: "",
        };
        let logical = LogicalKey::new("test.v1".into(), vec![part("uri", Some("/blob"), false)])?;
        // Act.
        let keys = logical.physical_keys(|_| legacy.render())?;
        // Assert.
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key, legacy.render());
        assert!(keys[0].omitted_parts.is_empty());
        Ok(())
    }

    #[test]
    fn general_render_optional_parts_absent_empty_and_duplicate_rendering() -> TestResult {
        // Arrange.
        let absent = LogicalKey::new("test.v1".into(), vec![part("version", None, true)])?;
        let empty = LogicalKey::new("test.v1".into(), vec![part("version", Some(""), true)])?;
        // Act.
        let absent_keys = absent.physical_keys(render)?;
        let empty_keys = empty.physical_keys(render)?;
        let legacy_keys = empty.physical_keys(|p| {
            CacheKey::from_raw(p.get("version").flatten().unwrap_or("").to_owned())
        })?;
        // Assert.
        assert_ne!(absent, empty);
        assert_ne!(absent_keys[0], empty_keys[0]);
        assert_eq!(absent_keys[1].key, empty_keys[1].key);
        assert_eq!(legacy_keys.len(), 1);
        assert!(legacy_keys[0].omitted_parts.is_empty());
        Ok(())
    }

    #[test]
    fn general_render_optional_parts_required_identity_never_disappears() -> TestResult {
        // Arrange.
        for required in [None, Some(""), Some("tenant-a"), Some("tenant-b")] {
            for value in [None, Some(""), Some("A"), Some("B")] {
                let logical = LogicalKey::new(
                    "test.v1".into(),
                    vec![
                        part("tenant", required, false),
                        part("version", value, true),
                    ],
                )?;
                // Act.
                let keys = logical.physical_keys(|p| {
                    assert_eq!(p.get("tenant"), Some(required));
                    render(p)
                })?;
                // Assert.
                assert!(
                    keys.iter()
                        .all(|k| !k.omitted_parts.iter().any(|n| n == "tenant"))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn general_render_optional_parts_bounds_and_duplicate_names() -> TestResult {
        // Arrange.
        let parts: Vec<_> = (0..5)
            .map(|i| part(&format!("p{i}"), Some("v"), true))
            .collect();
        // Act.
        let at_limit =
            LogicalKey::new("test.v1".into(), parts[..4].to_vec())?.physical_keys(render)?;
        let over_limit = LogicalKey::new("test.v1".into(), parts);
        let duplicate = LogicalKey::new(
            "test.v1".into(),
            vec![part("p", None, true), part("p", None, false)],
        );
        // Assert.
        assert_eq!(at_limit.len(), MAX_PHYSICAL_KEYS);
        assert_eq!(over_limit, Err(LogicalKeyError::ExpansionLimit));
        assert_eq!(duplicate, Err(LogicalKeyError::DuplicatePart));
        Ok(())
    }

    #[test]
    fn general_render_optional_parts_order_independent_and_shared_fallback() -> TestResult {
        // Arrange.
        let parts = vec![
            part("tenant", Some("one"), false),
            part("version", Some("A"), true),
        ];
        let a = LogicalKey::new("test.v1".into(), parts.clone())?;
        let reversed = LogicalKey::new("test.v1".into(), parts.into_iter().rev().collect())?;
        let b = LogicalKey::new(
            "test.v1".into(),
            vec![
                part("tenant", Some("one"), false),
                part("version", Some("B"), true),
            ],
        )?;
        let other = LogicalKey::new(
            "test.v1".into(),
            vec![
                part("tenant", Some("two"), false),
                part("version", Some("A"), true),
            ],
        )?;
        // Act.
        let a_keys = a.physical_keys(render)?;
        let b_keys = b.physical_keys(render)?;
        let other_keys = other.physical_keys(render)?;
        // Assert.
        assert_eq!(a, reversed);
        assert_eq!(a_keys, reversed.physical_keys(render)?);
        assert_ne!(a_keys[0].key, b_keys[0].key);
        assert_eq!(a_keys[1].key, b_keys[1].key);
        assert!(
            a_keys
                .iter()
                .all(|a| other_keys.iter().all(|b| a.key != b.key))
        );
        Ok(())
    }

    #[test]
    fn logical_key_failure_bridges_into_the_cache_error_kind() {
        // Arrange.
        let error = LogicalKeyError::DuplicatePart;

        // Act.
        let bridged = ovcc_core::Error::from(error);

        // Assert.
        assert_eq!(bridged.kind(), ovcc_core::ErrorKind::Cache);
        assert_eq!(bridged.message(), "duplicate logical-key part name");
    }
}
