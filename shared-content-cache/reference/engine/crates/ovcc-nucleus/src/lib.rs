// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Nucleus LFT request/response **policy** for OVCC.
//!
//! The Nucleus backend is a reverse-proxy cache for the Omniverse Nucleus
//! Large-File-Transfer (LFT) bridge plus a generic forward proxy. It has two
//! request classes, split by URL path:
//!
//! - **LFT** (`/omni/lft` endpoint and descendants): cacheable bridge traffic. The tenant JWT
//!   (`Omni-Tenant-Token`) is forwarded to the bridge, permissive CORS is added
//!   to the response, and the cache key is keyed by tenant + content UID + range
//!   (ignoring host/URI).
//! - **Generic** (everything else): a forward proxy for content not on Nucleus.
//!   The tenant JWT is **stripped** before the origin, an `Authorization` header
//!   bypasses the cache (multi-tenant safety), no CORS is added, and the cache
//!   key additionally includes host + request URI.
//!
//! This crate owns the **policy** — classification, the request-header
//! transform, the response CORS policy, the auth-bypass decision, and cache-key
//! assembly. It is pure (no async, no I/O). Cache-key *rendering* lives in
//! [`ovcc_cachekey`]; this crate assembles the part structs and delegates the
//! pipe-join to that crate. The proxy owns fetching, caching, and the
//! backend-namespace prefix on the storage key.

use ovcc_cachekey::{CacheKey, NucleusGenericKeyParts, NucleusLftKeyParts};

/// Request header carrying the Nucleus bridge tenant JWT. Forwarded on LFT,
/// stripped on the generic forward proxy.
const OMNI_TENANT_TOKEN: &str = "omni-tenant-token";
/// Request header naming the tenant (NVCF NCA id); a cache-key segment.
const NVCF_NCA_ID: &str = "nvcf-nca-id";
/// Request header naming the Omniverse content identity; a cache-key segment.
const OMNIVERSE_CONTENT_UID: &str = "omniverse-content-uid";
/// Standard range header; a cache-key segment (LFT clients slice files).
const RANGE: &str = "range";
/// Standard authorization header; its presence bypasses the generic cache.
const AUTHORIZATION: &str = "authorization";

/// The permissive CORS triplet required by successful LFT responses.
const LFT_CORS_HEADERS: &[(&str, &str)] = &[
    ("Access-Control-Allow-Origin", "*"),
    ("Access-Control-Allow-Headers", "*"),
    ("Access-Control-Allow-Methods", "*"),
];

/// Which Nucleus request class a path falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// The `/omni/lft` bridge path — cacheable, JWT forwarded, CORS added.
    Lft,
    /// Everything else — generic forward proxy, JWT stripped, auth bypasses.
    Generic,
}

/// The LFT endpoint path. Matched at a path-segment boundary (`/omni/lft`
/// itself or anything under `/omni/lft/`).
const LFT_PATH: &str = "/omni/lft";

/// Classifies a request path into its Nucleus [`Location`].
///
/// LFT is the `/omni/lft` endpoint, matched at a path-segment boundary:
/// `/omni/lft` itself or anything under `/omni/lft/` (e.g. `/omni/lft/foo`).
/// Everything else — including a path that merely shares the textual prefix
/// without crossing a segment boundary, like `/omni/lftextra` — is
/// [`Location::Generic`]. The comparison is on the raw path (query already split
/// off by the caller).
///
/// Keeping the endpoint on a segment boundary avoids forwarding tenant
/// credentials to lookalike paths such as `/omni/lftextra`.
pub fn classify(path: &str) -> Location {
    let under_lft = path
        .strip_prefix(LFT_PATH)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    if under_lft {
        Location::Lft
    } else {
        Location::Generic
    }
}

/// Case-insensitive header lookup returning the first value for `name`
/// (`name` must be lowercase), or `""` when absent — matching nginx's
/// `$http_*`-is-empty semantics for an unset header.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// Produces the header set to forward to the origin for this [`Location`].
///
/// - [`Location::Lft`] forwards every client header unchanged (the bridge needs
///   the tenant JWT and the OVC headers).
/// - [`Location::Generic`] drops `Omni-Tenant-Token`: generic traffic does not
///   go through the bridge, so the JWT must not leak to an arbitrary origin.
///
/// The input is never mutated; a new owned vector is returned so the caller can
/// keep the original request headers for logging.
pub fn transform_request_headers(
    location: Location,
    headers: &[(String, String)],
) -> Vec<(String, String)> {
    match location {
        Location::Lft => headers.to_vec(),
        Location::Generic => headers
            .iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case(OMNI_TENANT_TOKEN))
            .cloned()
            .collect(),
    }
}

/// The response headers to add for this [`Location`] at the given status.
///
/// LFT adds the permissive CORS triplet on the explicitly supported success and
/// redirect statuses. Generic adds nothing. `X-Cache-Status` is handled
/// separately because it applies to every response status.
pub fn cors_headers(location: Location, status: u16) -> &'static [(&'static str, &'static str)] {
    match location {
        Location::Lft
            if matches!(
                status,
                200 | 201 | 204 | 206 | 301 | 302 | 303 | 304 | 307 | 308
            ) =>
        {
            LFT_CORS_HEADERS
        }
        _ => &[],
    }
}

/// Whether this request must bypass the cache.
///
/// The generic forward proxy bypasses the cache whenever an `Authorization`
/// header is present because the forward proxy is shared across tenants. LFT
/// **never** bypasses on auth: its cache is tenant-isolated by the `nvcf-nca-id`
/// key segment, so an authenticated LFT request still caches.
pub fn auth_bypasses_cache(location: Location, headers: &[(String, String)]) -> bool {
    matches!(location, Location::Generic) && !header_value(headers, AUTHORIZATION).is_empty()
}

/// Assembles and renders the LFT cache key from the request.
///
/// Delegates the pipe-join to [`ovcc_cachekey::NucleusLftKeyParts`]; this crate
/// only maps request headers to the key segments. The returned key is
/// **un-namespaced**; the proxy prefixes it with the backend name before hitting
/// the shared store.
pub fn lft_cache_key(generation: &str, method: &str, headers: &[(String, String)]) -> CacheKey {
    NucleusLftKeyParts {
        generation,
        nca_id: header_value(headers, NVCF_NCA_ID),
        method,
        content_uid: header_value(headers, OMNIVERSE_CONTENT_UID),
        range: header_value(headers, RANGE),
    }
    .render()
}

/// Assembles and renders the generic cache key from the request.
///
/// Like [`lft_cache_key`] but additionally keyed by host + request URI, since
/// generic content is not on Nucleus. Delegates rendering to
/// [`ovcc_cachekey::NucleusGenericKeyParts`].
pub fn generic_cache_key(
    generation: &str,
    method: &str,
    host: &str,
    request_uri: &str,
    headers: &[(String, String)],
) -> CacheKey {
    NucleusGenericKeyParts {
        generation,
        nca_id: header_value(headers, NVCF_NCA_ID),
        method,
        host,
        request_uri,
        content_uid: header_value(headers, OMNIVERSE_CONTENT_UID),
        range: header_value(headers, RANGE),
    }
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn classify_lft_matches_segment_boundary() {
        assert_eq!(classify("/omni/lft"), Location::Lft);
        assert_eq!(classify("/omni/lft/"), Location::Lft);
        assert_eq!(classify("/omni/lft/foo/bar"), Location::Lft);
    }

    #[test]
    fn classify_non_lft_is_generic() {
        assert_eq!(classify("/"), Location::Generic);
        assert_eq!(classify("/usd/thing"), Location::Generic);
        assert_eq!(classify("/omni/other"), Location::Generic);
        // Shares the textual prefix but crosses no segment boundary, so it is
        // generic and cannot receive bridge credentials.
        assert_eq!(classify("/omni/lftextra"), Location::Generic);
    }

    #[test]
    fn generic_strips_tenant_token() {
        let headers = h(&[
            ("Omni-Tenant-Token", "secret-jwt"),
            ("X-OV-URI", "keep-me"),
            ("nvcf-nca-id", "t1"),
        ]);
        let out = transform_request_headers(Location::Generic, &headers);
        assert!(
            !out.iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("omni-tenant-token")),
            "generic must strip Omni-Tenant-Token"
        );
        // Other headers survive.
        assert!(out.iter().any(|(k, v)| k == "X-OV-URI" && v == "keep-me"));
        assert!(out.iter().any(|(k, v)| k == "nvcf-nca-id" && v == "t1"));
    }

    #[test]
    fn lft_forwards_tenant_token() {
        let headers = h(&[
            ("Omni-Tenant-Token", "secret-jwt"),
            ("X-OV-URI", "u"),
            ("Omniverse-Proxy-Scheme", "https"),
        ]);
        let out = transform_request_headers(Location::Lft, &headers);
        assert!(
            out.iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("omni-tenant-token") && v == "secret-jwt"),
            "LFT must forward Omni-Tenant-Token to the bridge"
        );
        assert_eq!(
            out.len(),
            headers.len(),
            "LFT forwards all headers unchanged"
        );
    }

    #[test]
    fn cors_only_on_lft_success() {
        assert_eq!(cors_headers(Location::Lft, 200).len(), 3);
        assert_eq!(cors_headers(Location::Lft, 206).len(), 3);
        assert_eq!(cors_headers(Location::Lft, 304).len(), 3);
        // Other successful statuses are outside the LFT CORS contract.
        assert!(cors_headers(Location::Lft, 202).is_empty());
        assert!(cors_headers(Location::Lft, 203).is_empty());
        assert!(cors_headers(Location::Lft, 205).is_empty());
        // Error responses are outside the LFT CORS contract.
        assert!(cors_headers(Location::Lft, 403).is_empty());
        assert!(cors_headers(Location::Lft, 502).is_empty());
        // Generic never adds CORS.
        assert!(cors_headers(Location::Generic, 200).is_empty());
    }

    #[test]
    fn generic_auth_bypasses_but_lft_never_does() {
        let authed = h(&[("Authorization", "Bearer x")]);
        let anon = h(&[("nvcf-nca-id", "t")]);
        assert!(auth_bypasses_cache(Location::Generic, &authed));
        assert!(!auth_bypasses_cache(Location::Generic, &anon));
        // LFT never bypasses on auth — tenant isolation comes from the key.
        assert!(!auth_bypasses_cache(Location::Lft, &authed));
    }

    #[test]
    fn empty_authorization_does_not_bypass() {
        let empty = h(&[("Authorization", "")]);
        assert!(!auth_bypasses_cache(Location::Generic, &empty));
    }

    #[test]
    fn lft_key_maps_headers_to_segments() {
        let headers = h(&[
            ("nvcf-nca-id", "tenant1"),
            ("omniverse-content-uid", "uid-123"),
            ("range", "bytes=0-1023"),
            ("omni-tenant-token", "ignored-in-key"),
        ]);
        let key = lft_cache_key("0", "GET", &headers);
        assert_eq!(key.as_str(), "0|tenant1|GET|uid-123|bytes=0-1023");
    }

    #[test]
    fn lft_key_absent_headers_render_empty() {
        let key = lft_cache_key("0", "HEAD", &[]);
        assert_eq!(key.as_str(), "0||HEAD||");
    }

    #[test]
    fn generic_key_includes_host_and_uri() {
        let headers = h(&[
            ("nvcf-nca-id", "t"),
            ("omniverse-content-uid", "u"),
            ("range", ""),
        ]);
        let key = generic_cache_key("0", "GET", "nucleus.mock.local", "/usd/x", &headers);
        assert_eq!(key.as_str(), "0|t|GET|nucleus.mock.local|/usd/x|u|");
    }

    #[test]
    fn lft_key_is_case_insensitive_on_header_names() {
        let lower = h(&[("nvcf-nca-id", "t"), ("omniverse-content-uid", "u")]);
        let upper = h(&[("NVCF-NCA-ID", "t"), ("Omniverse-Content-Uid", "u")]);
        assert_eq!(
            lft_cache_key("0", "GET", &lower),
            lft_cache_key("0", "GET", &upper)
        );
    }
}
