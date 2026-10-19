// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bursty cache policy for OVCC.
//!
//! "Bursty" caching gives short-TTL, tenant-isolated caching to list/HEAD
//! operations that are otherwise uncacheable (they carry no validator, so the
//! normal path bypasses the cache). It exists to absorb the request burst at
//! simulation startup, where many clients list the same prefix at once, so the
//! goal is consistency *within* a run, not reuse across runs.
//!
//! This crate owns the **policy**, not the mechanism.
//!
//! - [`matches()`] — is this request bursty-eligible (method + URI prefix rule)?
//! - [`adaptive_id`] — the per-tenant cache-key discriminator, so two tenants
//!   listing the same prefix get isolated entries.
//!
//! It is pure (no async, no I/O) and deliberately does **not** render the final
//! cache key — all key rendering stays in `ovcc-cachekey` (the single home that
//! guarantees byte-identical keys). The proxy combines [`adaptive_id`] with
//! `ovcc_cachekey::BurstyKeyParts` to form the lookup key.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Per-backend bursty cache configuration (a slice of the chart's
/// `nginx.backends.<name>.burstyCache`). Present only when bursty is enabled
/// for the backend; absent means bursty is off and the normal path runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BurstyCache {
    /// Cache lifetime for a bursty entry, in seconds. Stored for when TTL
    /// expiry is enforced; not yet acted on (see crate `AGENTS.md`).
    #[serde(default, alias = "ttlSec")]
    pub ttl_secs: u64,
    /// Routing rules; a request is bursty-eligible if it matches any rule.
    #[serde(default)]
    pub rules: Vec<BurstyRule>,
}

impl BurstyCache {
    /// Whether `method` + `request_uri` match any configured rule.
    pub fn matches(&self, method: &str, request_uri: &str) -> bool {
        matches(&self.rules, method, request_uri)
    }
}

/// One bursty routing rule, a method plus a `request_uri` prefix (e.g. `GET`
/// `/?list-type=` for S3 list operations).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BurstyRule {
    pub method: String,
    #[serde(default, alias = "uriStartsWith")]
    pub uri_starts_with: String,
}

/// Whether `method` + `request_uri` match any `rules` entry, with the method matched
/// case-insensitively and `request_uri` starting with the rule's prefix.
pub fn matches(rules: &[BurstyRule], method: &str, request_uri: &str) -> bool {
    rules.iter().any(|rule| {
        rule.method.eq_ignore_ascii_case(method) && request_uri.starts_with(&rule.uri_starts_with)
    })
}

/// Builds the tenant-adaptive cache-key discriminator from request headers in
/// priority order:
///
/// 1. `nvcf-function-id` (with `nvcf-function-version`, defaulting to `-`),
/// 2. else `nvcf-nca-id`,
/// 3. else a hex `sha256` of the `Authorization` header (so distinct tokens get
///    distinct entries without storing the raw token in the key),
/// 4. else `-` (no identifying information).
///
/// Empty header values are treated as absent.
pub fn adaptive_id(
    fn_id: Option<&str>,
    fn_version: Option<&str>,
    nca_id: Option<&str>,
    authorization: Option<&str>,
) -> String {
    if let Some(fn_id) = non_empty(fn_id) {
        let version = non_empty(fn_version).unwrap_or("-");
        return format!("{fn_id}|{version}");
    }
    if let Some(nca_id) = non_empty(nca_id) {
        return nca_id.to_owned();
    }
    if let Some(auth) = non_empty(authorization) {
        return hex(&Sha256::digest(auth.as_bytes()));
    }
    "-".to_owned()
}

/// The absolute expiry instant (unix seconds) for a bursty entry admitted at
/// `now`, given the backend's `ttl_secs`. Returns `None` when `ttl_secs == 0`
/// (TTL disabled — the entry never expires), so the proxy can store `None` and
/// the lookup never treats it as stale. A fresh fill is valid for `ttl_secs`
/// seconds.
///
/// The add saturates, so a pathological `ttl_secs` can never wrap the instant.
pub fn expires_at(ttl_secs: u64, now: i64) -> Option<i64> {
    if ttl_secs == 0 {
        return None;
    }
    let ttl = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
    Some(now.saturating_add(ttl))
}

/// Whether a bursty entry with the stored `expires_at` is stale at `now`: stale
/// once `now` reaches the expiry instant. A `None` expiry (TTL disabled) never
/// goes stale. The proxy treats a stale bursty hit as a MISS (purge + refill),
/// matching nginx serving a fresh response past `proxy_cache_valid`.
pub fn is_expired(expires_at: Option<i64>, now: i64) -> bool {
    matches!(expires_at, Some(deadline) if now >= deadline)
}

/// Returns the value only when it is present and non-empty.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Lowercase hex encoding of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Vec<BurstyRule> {
        vec![
            BurstyRule {
                method: "GET".to_owned(),
                uri_starts_with: "/?list-type=".to_owned(),
            },
            BurstyRule {
                method: "HEAD".to_owned(),
                uri_starts_with: "/".to_owned(),
            },
        ]
    }

    #[test]
    fn matches_list_get_and_any_head() {
        let rules = rules();
        assert!(matches(&rules, "GET", "/?list-type=2&prefix=a/"));
        assert!(matches(&rules, "HEAD", "/some/object"));
        // A non-list GET does not match the list rule.
        assert!(!matches(&rules, "GET", "/object.usd"));
        // Method is matched case-insensitively.
        assert!(matches(&rules, "get", "/?list-type=2"));
    }

    #[test]
    fn general_get_and_head_bursty_cache_routes_configuration_matrix() {
        // Arrange.
        let disabled: Option<BurstyCache> = None;
        let no_rules = Some(BurstyCache {
            ttl_secs: 300,
            rules: Vec::new(),
        });
        let get_only = Some(BurstyCache {
            ttl_secs: 300,
            rules: vec![BurstyRule {
                method: "GET".to_owned(),
                uri_starts_with: "/?list-type=".to_owned(),
            }],
        });
        let head_only = Some(BurstyCache {
            ttl_secs: 300,
            rules: vec![BurstyRule {
                method: "HEAD".to_owned(),
                uri_starts_with: "/".to_owned(),
            }],
        });
        let both = Some(BurstyCache {
            ttl_secs: 300,
            rules: rules(),
        });
        let cases = [
            (&disabled, "GET", "/?list-type=2", false),
            (&disabled, "HEAD", "/asset.usd", false),
            (&no_rules, "GET", "/?list-type=2", false),
            (&no_rules, "HEAD", "/asset.usd", false),
            (&get_only, "GET", "/?list-type=2", true),
            (&get_only, "GET", "/asset.usd", false),
            (&get_only, "HEAD", "/asset.usd", false),
            (&head_only, "GET", "/?list-type=2", false),
            (&head_only, "HEAD", "/asset.usd", true),
            (&both, "GET", "/?list-type=2", true),
            (&both, "GET", "/asset.usd", false),
            (&both, "HEAD", "/asset.usd", true),
        ];

        // Act.
        let actual = cases.map(|(configuration, method, uri, _expected)| {
            configuration
                .as_ref()
                .is_some_and(|bursty| bursty.matches(method, uri))
        });

        // Assert.
        assert_eq!(actual, cases.map(|(_, _, _, expected)| expected));
    }

    #[test]
    fn adaptive_id_prefers_function_id_then_version() {
        assert_eq!(
            adaptive_id(
                Some("fn-alpha"),
                Some("v2.0"),
                Some("nca"),
                Some("Bearer x")
            ),
            "fn-alpha|v2.0"
        );
        // Function id without a version defaults the version segment to "-".
        assert_eq!(
            adaptive_id(Some("fn-gamma"), None, None, None),
            "fn-gamma|-"
        );
        // An empty version is treated as absent.
        assert_eq!(
            adaptive_id(Some("fn-gamma"), Some(""), None, None),
            "fn-gamma|-"
        );
    }

    #[test]
    fn adaptive_id_falls_back_to_nca_then_auth_then_dash() {
        // No fn id -> nca id wins over auth.
        assert_eq!(
            adaptive_id(None, None, Some("nca-tenant-1"), Some("Bearer x")),
            "nca-tenant-1"
        );
        // No fn id, no nca id -> hash of the auth header (distinct per token).
        let a = adaptive_id(None, None, None, Some("Bearer token-abc-123"));
        let b = adaptive_id(None, None, None, Some("Bearer token-xyz-789"));
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "sha256 hex is 64 chars");
        // Same auth -> same id (stable, so an immediate repeat HITs).
        assert_eq!(
            a,
            adaptive_id(None, None, None, Some("Bearer token-abc-123"))
        );
        // Nothing identifying -> "-".
        assert_eq!(adaptive_id(None, None, None, None), "-");
        // Empty strings count as absent at every level.
        assert_eq!(adaptive_id(Some(""), Some(""), Some(""), Some("")), "-");
    }

    #[test]
    fn expires_at_adds_ttl_and_disables_on_zero() {
        // A positive TTL yields an absolute deadline `now + ttl`.
        assert_eq!(expires_at(3, 1_000), Some(1_003));
        // TTL of 0 disables expiry (the entry never goes stale).
        assert_eq!(expires_at(0, 1_000), None);
        // A pathological TTL saturates rather than wrapping.
        assert_eq!(expires_at(u64::MAX, i64::MAX), Some(i64::MAX));
    }

    #[test]
    fn is_expired_only_when_now_reaches_deadline() {
        // Before the deadline, the entry is fresh.
        assert!(!is_expired(Some(1_003), 1_002));
        // At and after the deadline, the entry is stale (the TTL window is exclusive of the
        // expiry second, matching nginx revalidating once validity elapses).
        assert!(is_expired(Some(1_003), 1_003));
        assert!(is_expired(Some(1_003), 1_004));
        // No deadline (TTL disabled) is never stale.
        assert!(!is_expired(None, i64::MAX));
    }
}
