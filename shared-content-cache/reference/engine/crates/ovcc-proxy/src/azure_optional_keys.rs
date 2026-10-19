// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Azure API-version aliases retain every other representation selector.

use ovcc_cachekey::{KeyPart, LogicalKey, PhysicalKeyParts};
use ovcc_storage::KeyPolicy;

use super::*;

pub(super) const POLICY: &str = "ovcc.azure.optional-x-ms-version.v1";
const NAMES: [&str; 13] = [
    "generation",
    "host",
    "method",
    "uri",
    "args",
    "if-match",
    "x-ms-version",
    "range",
    "structured",
    "md5",
    "crc64",
    "upn",
    "origin",
];

pub(super) struct AzurePolicy {
    pub(super) backends: Vec<Backend>,
}

impl AzurePolicy {
    /// Ambiguous legacy delimiters stay exact-only.
    pub(super) fn from_key(key: &str) -> Option<LogicalKey> {
        let parts: Vec<_> = key.split('|').collect();
        if parts.len() != 7 || parts[2] != "GET" {
            return None;
        }
        let mut fields = vec![parts[6], "", "", "", "", "", ""];
        if parts[6].starts_with("range=") {
            fields.clear();
            let mut tail = parts[6];
            for name in [
                "range",
                "structured",
                "md5",
                "crc64",
                "version",
                "upn",
                "origin",
            ] {
                tail = tail.strip_prefix(name)?.strip_prefix('=')?;
                let (length, rest) = tail.split_once(':')?;
                let length: usize = length.parse().ok()?;
                fields.push(rest.get(..length)?);
                tail = rest.get(length..)?;
                if name != "origin" {
                    tail = tail.strip_prefix(';')?;
                }
            }
            if !tail.is_empty() {
                return None;
            }
        }
        let values = [
            Some(parts[0]),
            Some(parts[1]),
            Some(parts[2]),
            Some(parts[3]),
            Some(parts[4]),
            Some(parts[5]),
            (!fields[4].is_empty()).then_some(fields[4]),
            Some(fields[0]),
            Some(fields[1]),
            Some(fields[2]),
            Some(fields[3]),
            Some(fields[5]),
            Some(fields[6]),
        ];
        let logical = LogicalKey::new(
            POLICY.into(),
            NAMES
                .into_iter()
                .zip(values)
                .map(|(name, value)| KeyPart {
                    name: name.into(),
                    value: value.map(str::to_owned),
                    optional: name == "x-ms-version",
                })
                .collect(),
        )
        .ok()?;
        let rendered = logical.physical_keys(Self::render_parts).ok()?;
        (rendered[0].key.as_str() == key).then_some(logical)
    }

    fn allows(&self, logical: &LogicalKey) -> bool {
        let host = logical
            .parts()
            .iter()
            .find(|p| p.name == "host")
            .and_then(|p| p.value.as_deref())
            .unwrap_or("");
        self.backends
            .iter()
            .find(|backend| backend.resolve_backend(host).is_some())
            .is_some_and(|backend| backend.name == "azure")
    }

    fn render_parts(parts: PhysicalKeyParts<'_>) -> CacheKey {
        let get = |name| parts.get(name).flatten().unwrap_or("");
        let headers = [
            ("x-ms-version", "x-ms-version"),
            ("structured", "x-ms-structured-body"),
            ("md5", "x-ms-range-get-content-md5"),
            ("crc64", "x-ms-range-get-content-crc64"),
            ("upn", "x-ms-upn"),
            ("origin", "origin"),
        ]
        .into_iter()
        .map(|(part, header)| (header.to_owned(), get(part).to_owned()))
        .collect::<Vec<_>>();
        KeyParts {
            generation: get("generation"),
            host: get("host"),
            method: get("method"),
            uri: get("uri"),
            filtered_args: get("args"),
            if_match: get("if-match"),
            range: &azure_get_variant(&headers, get("range")),
        }
        .render()
    }

    /// Sends the identity without the requester-owned cache generation.
    pub(super) fn wire(key: &str) -> Option<LogicalKey> {
        let logical = Self::from_key(key)?;
        LogicalKey::new(
            POLICY.into(),
            logical
                .parts()
                .iter()
                .filter(|part| part.name != "generation")
                .cloned()
                .collect(),
        )
        .ok()
    }

    /// Rebuilds the key with the receiving pod's generation and checks base identity.
    pub(super) fn local_from_wire(
        &self,
        identity: &ObjectIdentity,
        logical: &LogicalKey,
        generation: &str,
    ) -> Option<LogicalKey> {
        if logical.policy() != POLICY
            || logical.parts().iter().any(|part| part.name == "generation")
        {
            return None;
        }
        let mut parts = logical.parts().to_vec();
        parts.push(KeyPart {
            name: "generation".into(),
            value: Some(generation.into()),
            optional: false,
        });
        let local = LogicalKey::new(POLICY.into(), parts).ok()?;
        self.validate(&local).ok()?;
        let get = |name| {
            local
                .parts()
                .iter()
                .find(|part| part.name == name)
                .and_then(|part| part.value.as_deref())
        };
        (identity.method == http::Method::GET
            && get("range") == Some(identity.range.as_str())
            && get("host") == Some(identity.host.as_str())
            && get("uri") == Some(identity.uri.as_str())
            && get("args") == Some(identity.args.as_str())
            && get("if-match") == Some(identity.if_match.as_str()))
        .then_some(local)
    }
}

impl KeyPolicy for AzurePolicy {
    fn validate(&self, logical: &LogicalKey) -> Result<()> {
        logical
            .validate()
            .map_err(|error| Error::storage(error.to_string()))?;
        if logical.policy() != POLICY
            || !self.allows(logical)
            || logical.parts().len() != NAMES.len()
            || logical.parts().iter().any(|part| {
                !NAMES.contains(&part.name.as_str())
                    || part.optional != (part.name == "x-ms-version")
                    || (part.name != "x-ms-version" && part.value.is_none())
                    || part.value.as_ref().is_some_and(|value| value.contains('|'))
            })
        {
            return Err(Error::storage(
                "invalid Azure optional-key policy descriptors",
            ));
        }
        let keys = logical
            .physical_keys(|parts| self.render(parts))
            .map_err(|error| Error::storage(error.to_string()))?;
        if keys
            .first()
            .and_then(|key| Self::from_key(key.key.as_str()))
            .as_ref()
            != Some(logical)
        {
            return Err(Error::storage(
                "logical key is outside the Azure optional-key policy",
            ));
        }
        Ok(())
    }

    fn render(&self, parts: PhysicalKeyParts<'_>) -> CacheKey {
        Self::render_parts(parts)
    }

    fn recover(&self, key: &str, meta: &StoredMeta) -> Option<LogicalKey> {
        if !matches!(meta.status, 200 | 206) {
            return None;
        }
        let logical = Self::from_key(key)?;
        self.validate(&logical).ok()?;
        Some(logical)
    }
}

/// Returns the owning physical key so revalidation and eviction retain their normal authority.
pub(super) async fn lookup(
    cache: &Arc<Cache<DiskStorage>>,
    key: &CacheKey,
) -> Result<(CacheKey, Lookup)> {
    let Some(logical) = AzurePolicy::from_key(key.as_str()) else {
        return Ok((
            key.clone(),
            cache_lookup_blocking(cache, key.as_str()).await?,
        ));
    };
    let cache = Arc::clone(cache);
    let matched = tokio::task::spawn_blocking(move || cache.lookup_logical(&logical))
        .await
        .map_err(|error| Error::internal(format!("Azure lookup task failed: {error}")))??;
    Ok(match matched {
        Some(matched) => (
            CacheKey::from_raw(matched.owner_key),
            Lookup::Hit(matched.value.into()),
        ),
        None => (key.clone(), Lookup::Miss),
    })
}

/// Uses local committed metadata; capability absence cannot masquerade as an optional HIT.
pub(super) async fn who_has(
    ctx: &ConnCtx,
    request: &peer_proto::WhoHasRequest,
) -> Result<Option<peer_proto::WhoHasResponse>> {
    if request.optional_keys.is_none() {
        return Ok(None);
    }
    let generation = ctx.generation.load(Ordering::SeqCst).to_string();
    let request = request.clone();
    let backends = ctx.backends.clone();
    tokio::task::spawn_blocking(move || {
        let policy = AzurePolicy {
            backends: backends
                .iter()
                .map(|runtime| runtime.config.clone())
                .collect(),
        };
        let cache_for = |identity: &ObjectIdentity| {
            backends
                .iter()
                .find(|runtime| runtime.config.resolve_backend(&identity.host).is_some())
                .filter(|runtime| runtime.config.name == "azure")
                .map(|runtime| &runtime.store.cache)
        };
        ovcc_peer::optional_who_has_response(
            &request,
            |identity, logical| {
                cache_for(identity).is_some()
                    && policy
                        .local_from_wire(identity, logical, &generation)
                        .is_some()
            },
            |identity, logical, validator| {
                let local = policy
                    .local_from_wire(identity, logical, &generation)
                    .ok_or_else(|| ovcc_peer::PeerError::Http("invalid Azure identity".into()))?;
                let matched = cache_for(identity)
                    .ok_or_else(|| ovcc_peer::PeerError::Http("missing Azure cache".into()))?
                    .peek_logical(&local)
                    .map_err(|error| ovcc_peer::PeerError::Http(error.to_string()))?;
                Ok(
                    match matched.filter(|hit| {
                        matches!(hit.value.status, 200 | 206)
                            && stored_validator_matches(&hit.value, validator)
                    }) {
                        Some(hit) => (committed_who_has(&hit.value, validator), hit.omitted_parts),
                        None => (absent_who_has(), Vec::new()),
                    },
                )
            },
        )
        .map_err(|error| Error::storage(error.to_string()))
    })
    .await
    .map_err(|error| Error::internal(format!("Azure peer lookup task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> AzurePolicy {
        AzurePolicy {
            backends: vec![Backend {
                name: "azure".into(),
                host: "azure.test".into(),
                ..Backend::default()
            }],
        }
    }
    fn key(headers: &[(String, String)], range: &str) -> CacheKey {
        KeyParts {
            generation: "7",
            host: "azure.test",
            method: "GET",
            uri: "/blob",
            filtered_args: "snapshot=s",
            if_match: "\"etag\"",
            range: &azure_get_variant(headers, range),
        }
        .render()
    }
    #[test]
    fn azure_get_optional_version_preserves_required_selectors() -> Result<()> {
        let policy = policy();
        let headers = vec![
            ("x-ms-version".into(), "2025-01-05".into()),
            ("origin".into(), "https://example.test".into()),
            ("x-ms-range-get-content-md5".into(), "true".into()),
            ("x-ms-structured-body".into(), "true".into()),
            ("x-ms-upn".into(), "true".into()),
            ("x-ms-range-get-content-crc64".into(), "true".into()),
        ];
        let full = key(&headers, "bytes=0-9");
        let logical = AzurePolicy::from_key(full.as_str()).expect("Azure key");
        policy.validate(&logical)?;
        let keys = logical
            .physical_keys(|parts| policy.render(parts))
            .expect("expand");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key, full);
        assert_eq!(keys[1].key, key(&headers[1..], "bytes=0-9"));
        for name in NAMES.into_iter().filter(|name| *name != "x-ms-version") {
            let mut parts = logical.parts().to_vec();
            let part = parts
                .iter_mut()
                .find(|part| part.name == name)
                .expect("part");
            part.value = Some(format!("{}other", part.value.as_deref().unwrap_or("")));
            let changed = LogicalKey::new(POLICY.into(), parts).expect("logical");
            assert!(!logical.same_required_parts(&changed));
        }
        Ok(())
    }
    #[test]
    fn azure_get_optional_version_rejects_ambiguous_and_foreign_keys() {
        let policy = policy();
        let full = key(&[("x-ms-version".into(), "2025-01-05".into())], "");
        let foreign =
            AzurePolicy::from_key(&full.as_str().replace("azure.test", "s3.test")).expect("parsed");
        assert!(policy.validate(&foreign).is_err());
        for bad in [
            full.as_str().replace("/blob", "/blob|suffix"),
            full.as_str().replace("version=10:", "version=999:"),
        ] {
            assert!(AzurePolicy::from_key(&bad).is_none());
        }
        let wire = AzurePolicy::wire(full.as_str()).expect("wire");
        let mut identity = ObjectIdentity {
            host: "azure.test".into(),
            method: http::Method::GET,
            uri: "/blob".into(),
            args: "snapshot=s".into(),
            if_match: "\"etag\"".into(),
            range: "".into(),
        };
        let local = policy
            .local_from_wire(&identity, &wire, "99")
            .expect("local generation");
        assert!(
            local
                .physical_keys(|parts| policy.render(parts))
                .expect("keys")[0]
                .key
                .as_str()
                .starts_with("99|")
        );
        identity.if_match = "wrong".into();
        assert!(policy.local_from_wire(&identity, &wire, "99").is_none());
    }
}
