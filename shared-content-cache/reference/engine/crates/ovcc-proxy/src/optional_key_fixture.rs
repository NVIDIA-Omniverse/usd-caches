// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local upgrade-test policy. Compiled out of ordinary runtime images.
//! The fixture's origin promises one representation for every API version.

use ovcc_cachekey::{KeyPart, LogicalKey, PhysicalKeyParts};
use ovcc_storage::KeyPolicy;

use super::*;

pub(super) const POLICY: &str = "ovcc.test.optional-x-ms-version.v1";
const HOST: &str = "az.mock.local";
const PREFIX: &str = "/optional-key-upgrade/";
const NAMES: [&str; 7] = [
    "generation",
    "host",
    "method",
    "uri",
    "args",
    "if-match",
    "x-ms-version",
];

pub(super) struct FixturePolicy;

impl FixturePolicy {
    /// Reconstructs only unambiguous ordinary fixture GET keys, never other traffic.
    pub(super) fn from_key(key: &str) -> Option<LogicalKey> {
        let parts: Vec<_> = key.split('|').collect();
        if parts.len() != 7
            || parts[1] != HOST
            || parts[2] != "GET"
            || !parts[3].starts_with(PREFIX)
            || parts[5].is_empty()
        {
            return None;
        }
        let version = if parts[6].is_empty() {
            None
        } else {
            let encoded =
                parts[6].strip_prefix("range=0:;structured=0:;md5=0:;crc64=0:;version=")?;
            let (length, tail) = encoded.split_once(':')?;
            let length: usize = length.parse().ok()?;
            let version = tail.get(..length)?;
            if tail.get(length..)? != ";upn=0:;origin=0:" || version.is_empty() {
                return None;
            }
            Some(version)
        };
        let values = [
            Some(parts[0]),
            Some(parts[1]),
            Some(parts[2]),
            Some(parts[3]),
            Some(parts[4]),
            Some(parts[5]),
            version,
        ];
        LogicalKey::new(
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
        .ok()
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
    pub(super) fn from_wire(
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
        Self.validate(&local).ok()?;
        let get = |name| {
            local
                .parts()
                .iter()
                .find(|part| part.name == name)
                .and_then(|part| part.value.as_deref())
        };
        (identity.method == http::Method::GET
            && identity.range.is_empty()
            && get("host") == Some(identity.host.as_str())
            && get("uri") == Some(identity.uri.as_str())
            && get("args") == Some(identity.args.as_str())
            && get("if-match") == Some(identity.if_match.as_str()))
        .then_some(local)
    }
}

impl KeyPolicy for FixturePolicy {
    fn validate(&self, logical: &LogicalKey) -> Result<()> {
        logical
            .validate()
            .map_err(|error| Error::storage(error.to_string()))?;
        if logical.policy() != POLICY
            || logical.parts().len() != NAMES.len()
            || logical.parts().iter().any(|part| {
                !NAMES.contains(&part.name.as_str())
                    || part.optional != (part.name == "x-ms-version")
                    || (part.name != "x-ms-version" && part.value.is_none())
                    || part.value.as_ref().is_some_and(|value| value.contains('|'))
            })
        {
            return Err(Error::storage("invalid optional-key fixture descriptors"));
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
                "logical key is outside the optional-key fixture",
            ));
        }
        Ok(())
    }

    fn render(&self, parts: PhysicalKeyParts<'_>) -> CacheKey {
        let get = |name| parts.get(name).flatten().unwrap_or("");
        let version = get("x-ms-version");
        let headers = vec![("x-ms-version".into(), version.into())];
        let variant = azure_get_variant(&headers, "");
        KeyParts {
            generation: get("generation"),
            host: get("host"),
            method: get("method"),
            uri: get("uri"),
            filtered_args: get("args"),
            if_match: get("if-match"),
            range: &variant,
        }
        .render()
    }

    fn recover(&self, key: &str, meta: &StoredMeta) -> Option<LogicalKey> {
        if meta.status != 200 || meta.content_range.is_some() {
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
    let Some(logical) = FixturePolicy::from_key(key.as_str()) else {
        return Ok((
            key.clone(),
            cache_lookup_blocking(cache, key.as_str()).await?,
        ));
    };
    let cache = Arc::clone(cache);
    let matched = tokio::task::spawn_blocking(move || cache.lookup_logical(&logical))
        .await
        .map_err(|error| Error::internal(format!("fixture lookup task failed: {error}")))??;
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
    let generation = ctx.generation.load(Ordering::SeqCst).to_string();
    let request = request.clone();
    let cache = ctx
        .backends
        .iter()
        .find(|runtime| {
            runtime.config.name == "azure" && runtime.config.resolve_backend(HOST).is_some()
        })
        .map(|runtime| Arc::clone(&runtime.store.cache));
    tokio::task::spawn_blocking(move || {
        ovcc_peer::optional_who_has_response(
            &request,
            |identity, logical| {
                cache.is_some()
                    && FixturePolicy::from_wire(identity, logical, &generation).is_some()
            },
            |identity, logical, validator| {
                let local = FixturePolicy::from_wire(identity, logical, &generation)
                    .ok_or_else(|| ovcc_peer::PeerError::Http("invalid fixture identity".into()))?;
                let matched = cache
                    .as_ref()
                    .ok_or_else(|| ovcc_peer::PeerError::Http("missing fixture cache".into()))?
                    .peek_logical(&local)
                    .map_err(|error| ovcc_peer::PeerError::Http(error.to_string()))?;
                Ok(
                    match matched.filter(|hit| {
                        hit.value.status == 200 && stored_validator_matches(&hit.value, validator)
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
    .map_err(|error| Error::internal(format!("fixture peer lookup task failed: {error}")))?
}

/// Exposes only this fixture through the test image's health listener.
pub(super) async fn diagnostics(ctx: ConnCtx) -> Vec<u8> {
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let cache = ctx
            .backends
            .iter()
            .find(|runtime| runtime.config.name == "azure")
            .ok_or_else(|| Error::storage("missing Azure fixture backend"))?;
        let snapshot = cache.store.cache.storage().inspect_optional_fixture()?;
        serde_json::to_vec(&serde_json::json!({"policy": POLICY,
            "generation": ctx.generation.load(Ordering::SeqCst), "snapshot": snapshot}))
        .map_err(|error| Error::storage(error.to_string()))
    })
    .await;
    let (status, body) = match result {
        Ok(Ok(body)) => ("200 OK", body),
        _ => (
            "500 Internal Server Error",
            b"fixture inspection failed".to_vec(),
        ),
    };
    let mut response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
    response.extend(body);
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(version: Option<&str>) -> CacheKey {
        let headers = version
            .map(|value| vec![("x-ms-version".into(), value.into())])
            .unwrap_or_default();
        KeyParts {
            generation: "7",
            host: HOST,
            method: "GET",
            uri: "/optional-key-upgrade/witness",
            filtered_args: "",
            if_match: "\"fixture\"",
            range: &azure_get_variant(&headers, ""),
        }
        .render()
    }

    #[test]
    fn mock_get_optional_fixture_round_trips_both_legacy_formats() -> Result<()> {
        for version in [None, Some("2025-01-05")] {
            let key = key(version);
            let logical = FixturePolicy::from_key(key.as_str()).expect("fixture key");
            FixturePolicy.validate(&logical)?;
            let expanded = logical
                .physical_keys(|parts| FixturePolicy.render(parts))
                .expect("expansion");
            assert_eq!(expanded[0].key, key);
            assert_eq!(expanded.len(), if version.is_some() { 2 } else { 1 });
        }
        assert!(
            FixturePolicy::from_key(
                key(None)
                    .as_str()
                    .replace(HOST, "real.blob.core.windows.net")
                    .as_str()
            )
            .is_none()
        );
        assert!(
            FixturePolicy::from_key(key(None).as_str().replace("witness", "bad|key").as_str())
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn mock_get_optional_fixture_peer_owns_generation_and_required_identity() {
        let key = key(Some("2025-01-05"));
        let wire = FixturePolicy::wire(key.as_str()).expect("wire identity");
        let mut identity = ObjectIdentity {
            host: HOST.into(),
            method: http::Method::GET,
            uri: "/optional-key-upgrade/witness".into(),
            args: String::new(),
            if_match: "\"fixture\"".into(),
            range: String::new(),
        };
        let local = FixturePolicy::from_wire(&identity, &wire, "99").expect("server generation");
        assert!(
            local
                .physical_keys(|parts| FixturePolicy.render(parts))
                .expect("expanded")[0]
                .key
                .as_str()
                .starts_with("99|")
        );
        identity.if_match = "\"different\"".into();
        assert!(FixturePolicy::from_wire(&identity, &wire, "99").is_none());
    }
}
