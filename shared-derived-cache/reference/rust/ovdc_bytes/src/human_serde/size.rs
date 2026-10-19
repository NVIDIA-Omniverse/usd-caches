// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Converts a string with byte units to a number.

use std::ops::Deref;

use serde::de::Visitor;

fn mul_for(units: &str) -> Option<u64> {
    Some(match units {
        "Ti" | "ti" => 1_099_511_627_776,
        "T" | "t" | "TB" | "tb" => 1_000_000_000_000,
        "Gi" | "gi" => 1_073_741_824,
        "G" | "g" | "GB" | "gb" => 1_000_000_000,
        "Mi" | "mi" => 1_048_576,
        // Bare lowercase `m` is deliberately absent: it is the time unit for
        // minutes (see `is_time`). Case is significant for the single-letter
        // form -- `M` is megabytes, `m` is minutes -- while the unambiguous
        // `mb`/`MB` spellings stay sizes.
        "M" | "MB" | "mb" => 1_000_000,
        "Ki" | "ki" => 1024,
        "K" | "k" | "KB" | "kb" => 1000,
        "B" | "b" => 1,
        _ => return None,
    })
}

pub(super) fn is_size(units: &str) -> bool {
    mul_for(units).is_some()
}

pub(super) fn parse_split(input: (&str, &str)) -> Option<u64> {
    let (left, right) = input;

    if right.is_empty() {
        return left.parse::<u64>().ok();
    }

    let multiplier: u64 = mul_for(right)?;

    let i = left.parse::<u64>().ok()?;
    Some(multiplier.saturating_mul(i))
}

/// A size descriptor such as 100, 1M, 1mb, 1Mi, 1GB, etc.
///
/// Deserializes from either a unit string or a bare number of bytes. Serializes
/// as a bare number of bytes: nothing in the workspace currently serializes
/// configuration, and emitting the resolved byte count is both exact and the
/// more useful thing to show if something ever does.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct HumanByteSize(pub u64);

impl From<u64> for HumanByteSize {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl Deref for HumanByteSize {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct SizeStrVisitor;
impl Visitor<'_> for SizeStrVisitor {
    type Value = u64;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("byte size that is a number followed by unit (valid units: B, K, M, G, T or Ki, Mi, Gi, Ti)")
    }

    fn visit_i32<E>(self, v: i32) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if v < 0 {
            return Err(E::custom(format!("byte size cannot be negative: {}", v)));
        }
        Ok(v as u64)
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if v < 0 {
            return Err(E::custom(format!("byte size cannot be negative: {}", v)));
        }
        Ok(v as u64)
    }

    fn visit_u32<E>(self, v: u32) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(v as u64)
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(v)
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        // Route through `parse_number` rather than parsing as a size directly,
        // so time units resolve the same way here as everywhere else: "64m" is
        // 64 minutes, not 64 megabytes. Size-suffixed values are unaffected --
        // `parse_number` falls through to the same size parser for them.
        match super::parse_number(v) {
            Some(value) if value >= 0 => Ok(value as u64),
            Some(value) => Err(E::custom(format!("byte size cannot be negative: {value}"))),
            None => Err(E::custom("invalid size string")),
        }
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_str(&v)
    }
}

impl<'de> serde::Deserialize<'de> for HumanByteSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = deserializer.deserialize_any(SizeStrVisitor)?;
        Ok(HumanByteSize(value))
    }
}

#[cfg(test)]
mod tests {
    use crate::human_serde::parse_number;

    #[test]
    fn test_size_of_size_fn() {
        assert_eq!(parse_number("200Ti").unwrap(), 219902325555200);
        assert_eq!(parse_number("200T").unwrap(), 200000000000000);

        assert_eq!(parse_number("200Gi").unwrap(), 214748364800);
        assert_eq!(parse_number("200G").unwrap(), 200000000000);

        assert_eq!(parse_number("200Mi").unwrap(), 209715200);
        assert_eq!(parse_number("200M").unwrap(), 200000000);
    }

    #[test]
    fn single_letter_m_is_case_sensitive() {
        // `M` is megabytes, `m` is minutes -- everywhere, including on a field
        // typed as a size. The unambiguous two-letter spellings stay sizes.
        assert_eq!(parse_number("64M").unwrap(), 64_000_000);
        assert_eq!(parse_number("64m").unwrap(), 3_840_000); // 64 minutes in ms
        assert_eq!(parse_number("64mb").unwrap(), 64_000_000);
        assert_eq!(parse_number("64MB").unwrap(), 64_000_000);
        assert_eq!(parse_number("64Mi").unwrap(), 67_108_864);
    }
}
