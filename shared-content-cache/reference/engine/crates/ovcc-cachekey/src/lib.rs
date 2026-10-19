// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cache-key construction for OVCC.
//!
//! The cache key must be **byte-identical** across the read path and the
//! write-through seed path — a one-byte difference is a permanent MISS — so all
//! key rendering lives here, behind explicit and tested rules rather than
//! scattered string formatting.
//!
//! Canonical cache-key form:
//!
//! ```text
//! <generation>|<host>|<method>|<uri>|<filtered_args>|<if_match>|<range>
//! ```

mod logical;
pub use logical::{
    KeyPart, LogicalKey, LogicalKeyError, MAX_PHYSICAL_KEYS, PhysicalKey, PhysicalKeyParts,
};

/// Query parameters dropped before keying for S3/GCS backends, the presigned-auth
/// noise plus the AWS SDK operation hint (`x-id`). None affect object identity,
/// so stripping them lets a write-through `PUT` and a consumer `GET` share one
/// key.
pub const STRIPPED_PARAMS: &[&str] = &[
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-Signature",
    "X-Amz-SignedHeaders",
    "x-id",
];

/// Query parameters dropped before keying for Azure backends. SAS credentials
/// and response overrides vary per request without changing object bytes, so
/// signed GETs for one blob share a key. Cache hits replay response overrides
/// from the current request.
pub const STRIPPED_PARAMS_AZURE: &[&str] = &[
    "sv", "ss", "srt", "sp", "sr", "se", "sig", "st", "spr", "sip", "si", "skoid", "sktid", "skt",
    "ske", "sks", "skv", "saoid", "suoid", "scid", "rscc", "rscd", "rsce", "rscl", "rsct",
];

/// A rendered cache key. Opaque on purpose; compare by value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CacheKey(String);

impl CacheKey {
    /// Borrows the rendered key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes into the owned `String`.
    pub fn into_string(self) -> String {
        self.0
    }

    /// Wraps an already-composed key string. Used to prepend a backend
    /// namespace to a rendered key (e.g. `nucleus|<rendered>`) so entries from
    /// different backends cannot collide in the shared store. The caller owns
    /// the composition; the render helpers remain the only source of the
    /// canonical, nginx-equivalent portion.
    pub fn from_raw(raw: String) -> Self {
        CacheKey(raw)
    }
}

impl std::fmt::Display for CacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The identity components of a request, in canonical key order.
///
/// Borrowed for zero-copy assembly on the hot path; `render` is the only thing
/// that allocates.
#[derive(Clone, Copy, Debug)]
pub struct KeyParts<'a> {
    pub generation: &'a str,
    pub host: &'a str,
    pub method: &'a str,
    pub uri: &'a str,
    pub filtered_args: &'a str,
    pub if_match: &'a str,
    pub range: &'a str,
}

impl KeyParts<'_> {
    /// Renders the canonical pipe-joined cache key.
    pub fn render(&self) -> CacheKey {
        CacheKey(format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.generation,
            self.host,
            self.method,
            self.uri,
            self.filtered_args,
            self.if_match,
            self.range,
        ))
    }
}

/// The components of a **bursty** cache key, in canonical order. Bursty caching
/// (short-TTL list/HEAD caching) keys on a tenant-adaptive id and the full,
/// unfiltered `request_uri` rather than the normal filtered-args/`If-Match`
/// shape — so a bursty entry is per-tenant and never collides with a normal
/// cache entry because the segment count differs.
///
/// The `adaptive_id` is supplied by `ovcc-bursty` (the policy crate); rendering
/// stays here so all key construction lives in one place.
#[derive(Clone, Copy, Debug)]
pub struct BurstyKeyParts<'a> {
    pub generation: &'a str,
    pub adaptive_id: &'a str,
    pub host: &'a str,
    pub method: &'a str,
    pub request_uri: &'a str,
}

impl BurstyKeyParts<'_> {
    /// Renders the canonical pipe-joined bursty cache key
    /// `<generation>|<adaptive_id>|<host>|<method>|<request_uri>`.
    pub fn render(&self) -> CacheKey {
        CacheKey(format!(
            "{}|{}|{}|{}|{}",
            self.generation, self.adaptive_id, self.host, self.method, self.request_uri,
        ))
    }
}

/// The components of a **Nucleus LFT** cache key (`location /omni/lft`), in
/// canonical order. LFT content lives on the Nucleus bridge, so the key is
/// keyed by tenant (`nvcf-nca-id`) and content identity (`omniverse-content-uid`)
/// plus the byte range — it deliberately ignores the host and the request URI,
/// so two requests for the same content UID with different tickets share an
/// entry. The LFT wire key is:
/// `$cache_generation|$http_nvcf_nca_id|$request_method|$http_omniverse_content_uid|$http_range`.
///
/// Empty segments are the nginx contract: a missing `nvcf-nca-id`,
/// `omniverse-content-uid`, or `Range` renders as the empty string (nginx
/// `$http_*` for an absent header is empty).
#[derive(Clone, Copy, Debug)]
pub struct NucleusLftKeyParts<'a> {
    pub generation: &'a str,
    pub nca_id: &'a str,
    pub method: &'a str,
    pub content_uid: &'a str,
    pub range: &'a str,
}

impl NucleusLftKeyParts<'_> {
    /// Renders the canonical pipe-joined Nucleus LFT cache key
    /// `<generation>|<nca_id>|<method>|<content_uid>|<range>`.
    pub fn render(&self) -> CacheKey {
        CacheKey(format!(
            "{}|{}|{}|{}|{}",
            self.generation, self.nca_id, self.method, self.content_uid, self.range,
        ))
    }
}

/// The components of a **Nucleus generic** cache key (`location /`), in
/// canonical order. The generic forward proxy serves content that is not on
/// Nucleus, so the key additionally includes the host and the full request URI
/// (unlike the LFT key). The generic Nucleus wire key is:
/// `$cache_generation|$http_nvcf_nca_id|$request_method|$host|$request_uri|$http_omniverse_content_uid|$http_range`.
///
/// Empty segments follow the same nginx `$http_*`-is-empty contract as
/// [`NucleusLftKeyParts`].
#[derive(Clone, Copy, Debug)]
pub struct NucleusGenericKeyParts<'a> {
    pub generation: &'a str,
    pub nca_id: &'a str,
    pub method: &'a str,
    pub host: &'a str,
    pub request_uri: &'a str,
    pub content_uid: &'a str,
    pub range: &'a str,
}

impl NucleusGenericKeyParts<'_> {
    /// Renders the canonical pipe-joined Nucleus generic cache key
    /// `<generation>|<nca_id>|<method>|<host>|<request_uri>|<content_uid>|<range>`.
    pub fn render(&self) -> CacheKey {
        CacheKey(format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.generation,
            self.nca_id,
            self.method,
            self.host,
            self.request_uri,
            self.content_uid,
            self.range,
        ))
    }
}

/// Filters and canonicalizes a raw URL query string for keying, dropping the
/// S3/GCS [`STRIPPED_PARAMS`]. See [`filter_query_args_with`].
pub fn filter_query_args(raw_query: &str) -> String {
    filter_query_args_with(raw_query, STRIPPED_PARAMS)
}

/// Normalizes a PUT response `etag` into the `if_match` cache-key segment used
/// when seeding a write-through entry.
///
/// The seed key must be byte-identical to the consumer GET key. When the
/// backend requires versioning, that GET carries `If-Match: <etag>`, so the
/// seed keys on the object's `etag`; otherwise the consumer sends no `If-Match`
/// and the segment is empty. A missing/empty `etag` also yields an empty
/// segment.
pub fn if_match_for_seed(etag: &str, requires_versioning: bool) -> &str {
    if requires_versioning && !etag.is_empty() {
        etag
    } else {
        ""
    }
}

/// Filters and canonicalizes a raw URL query string for keying, dropping the
/// given `stripped` parameter set.
///
/// Drops the named params, then sorts the survivors so the key is stable
/// regardless of the order the client serialized parameters in. Operates on the
/// raw `k=v&k=v` wire form; percent-decoding is intentionally not performed
/// (the key keys on the bytes as received). Pairs without `=` are preserved
/// verbatim. Use [`STRIPPED_PARAMS`] for S3/GCS and [`STRIPPED_PARAMS_AZURE`]
/// for Azure backends.
pub fn filter_query_args_with(raw_query: &str, stripped: &[&str]) -> String {
    let mut kept: Vec<&str> = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let key = pair.split_once('=').map_or(*pair, |(k, _)| k);
            !stripped.contains(&key)
        })
        .collect();
    kept.sort_unstable();
    kept.join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_canonical_pipe_join() {
        let key = KeyParts {
            generation: "7",
            host: "bucket.s3.us-west-2.amazonaws.com",
            method: "GET",
            uri: "/a/b.usd",
            filtered_args: "list-type=2",
            if_match: "\"etag\"",
            range: "",
        }
        .render();
        assert_eq!(
            key.as_str(),
            "7|bucket.s3.us-west-2.amazonaws.com|GET|/a/b.usd|list-type=2|\"etag\"|"
        );
    }

    #[test]
    fn filter_strips_auth_and_operation_params() {
        let raw =
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&prefix=foo&X-Amz-Signature=deadbeef&x-id=GetObject";
        assert_eq!(filter_query_args(raw), "prefix=foo");
    }

    #[test]
    fn filter_is_order_independent() {
        let a = filter_query_args("b=2&a=1&c=3");
        let b = filter_query_args("c=3&a=1&b=2");
        assert_eq!(a, b);
        assert_eq!(a, "a=1&b=2&c=3");
    }

    #[test]
    fn filter_empty_query_is_empty() {
        assert_eq!(filter_query_args(""), "");
    }

    #[test]
    fn filter_preserves_valueless_params() {
        assert_eq!(filter_query_args("versions&prefix=x"), "prefix=x&versions");
    }

    #[test]
    fn azure_strip_drops_sas_noise_so_two_sas_gets_share_a_key() {
        // Two Azure Account SAS GETs for the same blob differ only in credential
        // scope and signature noise; both must retain the same object identity.
        let a = "sp=r&ss=b&srt=o&st=2026-01-01T00:00:00Z&se=2026-01-01T00:10:00Z\
            &spr=https&sv=2024-11-04&sig=AAAA";
        let b = "sp=r&ss=bf&srt=co&st=2026-06-23T11:11:11Z&se=2026-06-23T11:21:11Z\
            &spr=https&sv=2024-11-04&sig=ZZZZ";
        assert_eq!(
            filter_query_args_with(a, STRIPPED_PARAMS_AZURE),
            filter_query_args_with(b, STRIPPED_PARAMS_AZURE)
        );
        assert_eq!(filter_query_args_with(a, STRIPPED_PARAMS_AZURE), "");
        // A non-SAS param survives.
        assert_eq!(
            filter_query_args_with("comp=block&sig=AAAA&se=x", STRIPPED_PARAMS_AZURE),
            "comp=block"
        );
    }

    #[test]
    fn azure_strip_drops_delegation_and_response_override_noise() {
        let first = "rscc=public&rsce=identity&rscl=en-US&rscd=attachment&rsct=application%2Fzip\
            &se=2026-08-16T18%3A08%3A23Z\
            &sig=AAAA&ske=2026-08-16T21%3A28%3A54Z&skoid=first&sks=b\
            &skt=2026-08-16T17%3A28%3A54Z&sktid=tenant&skv=2025-11-05\
            &sp=r&spr=https&sr=b&st=x&sv=2025-11-05";
        let second = "rscc=no-cache&rsce=gzip&rscl=fr&rscd=inline&rsct=text%2Fplain\
            &se=2026-08-16T18%3A08%3A24Z\
            &sig=BBBB&ske=2026-08-16T21%3A28%3A26Z&skoid=second&sks=b\
            &skt=2026-08-16T17%3A28%3A26Z&sktid=tenant&skv=2025-11-05\
            &sp=r&spr=https&sr=b&st=y&sv=2025-11-05";
        let expected = "";
        assert_eq!(
            filter_query_args_with(first, STRIPPED_PARAMS_AZURE),
            expected
        );
        assert_eq!(
            filter_query_args_with(second, STRIPPED_PARAMS_AZURE),
            expected
        );
    }

    #[test]
    fn bursty_render_is_canonical_pipe_join() {
        let key = BurstyKeyParts {
            generation: "0",
            adaptive_id: "fn-alpha|v2.0",
            host: "bucket.s3.us-west-2.amazonaws.com",
            method: "GET",
            request_uri: "/?list-type=2&prefix=a%2F",
        }
        .render();
        assert_eq!(
            key.as_str(),
            "0|fn-alpha|v2.0|bucket.s3.us-west-2.amazonaws.com|GET|/?list-type=2&prefix=a%2F"
        );
    }

    #[test]
    fn if_match_for_seed_uses_etag_only_when_versioned() {
        // Versioned backend with a real ETag, so the seed keys on it (consumer GET
        // sends the matching If-Match).
        assert_eq!(if_match_for_seed("\"v1\"", true), "\"v1\"");
        // Versioning disabled, so no If-Match segment regardless of the ETag.
        assert_eq!(if_match_for_seed("\"v1\"", false), "");
        // Missing/empty ETag yields an empty segment even when versioned.
        assert_eq!(if_match_for_seed("", true), "");
        assert_eq!(if_match_for_seed("", false), "");
    }

    #[test]
    fn registry_seed_and_get_share_a_key() {
        // A registry backend runs `requires_versioning=false`, so a package GET
        // carries no If-Match and the write-through seed keys on empty If-Match
        // too. GET and PUT differ only in the stripped `x-id` op hint, so the
        // seed key and the consumer GET key are byte-identical (a HIT).
        let generation = "0";
        let host = "registry.npmjs.org.cache.svc.cluster.local";
        let uri = "/lodash/-/lodash-4.17.21.tgz";
        let get_key = KeyParts {
            generation,
            host,
            method: "GET",
            uri,
            filtered_args: &filter_query_args("x-id=GetObject"),
            if_match: "",
            range: "",
        }
        .render();
        let seed_key = KeyParts {
            generation,
            host,
            method: "GET",
            uri,
            filtered_args: &filter_query_args("x-id=PutObject"),
            if_match: if_match_for_seed("\"etag\"", false),
            range: "",
        }
        .render();
        assert_eq!(get_key, seed_key);
    }

    #[test]
    fn nucleus_lft_render_matches_wire_key() {
        // Canonical LFT key:
        // $cache_generation|$http_nvcf_nca_id|$request_method|$http_omniverse_content_uid|$http_range
        let key = NucleusLftKeyParts {
            generation: "0",
            nca_id: "tenant1",
            method: "GET",
            content_uid: "uid-123",
            range: "bytes=0-1023",
        }
        .render();
        assert_eq!(key.as_str(), "0|tenant1|GET|uid-123|bytes=0-1023");
    }

    #[test]
    fn nucleus_lft_empty_segments_render_empty() {
        // A missing nca-id / content-uid / range renders as empty (nginx $http_*
        // for an absent header is the empty string).
        let key = NucleusLftKeyParts {
            generation: "0",
            nca_id: "",
            method: "HEAD",
            content_uid: "",
            range: "",
        }
        .render();
        assert_eq!(key.as_str(), "0||HEAD||");
    }

    #[test]
    fn nucleus_lft_method_distinguishes_get_and_head() {
        // The Nucleus key includes $request_method, so GET and HEAD are distinct
        // entries for the same content.
        let get = NucleusLftKeyParts {
            generation: "0",
            nca_id: "t",
            method: "GET",
            content_uid: "u",
            range: "",
        }
        .render();
        let head = NucleusLftKeyParts {
            generation: "0",
            nca_id: "t",
            method: "HEAD",
            content_uid: "u",
            range: "",
        }
        .render();
        assert_ne!(get, head);
    }

    #[test]
    fn nucleus_lft_ignores_host_and_uri_by_construction() {
        // The LFT key has no host/uri fields, so two requests differing only in
        // ticket (request URI) but sharing the content UID key identically.
        let a = NucleusLftKeyParts {
            generation: "0",
            nca_id: "t",
            method: "GET",
            content_uid: "same-uid",
            range: "",
        }
        .render();
        let b = NucleusLftKeyParts {
            generation: "0",
            nca_id: "t",
            method: "GET",
            content_uid: "same-uid",
            range: "",
        }
        .render();
        assert_eq!(a, b);
    }

    #[test]
    fn nucleus_generic_render_matches_wire_key() {
        // Canonical generic key:
        // $cache_generation|$http_nvcf_nca_id|$request_method|$host|$request_uri|$http_omniverse_content_uid|$http_range
        let key = NucleusGenericKeyParts {
            generation: "0",
            nca_id: "tenant1",
            method: "GET",
            host: "nucleus.mock.local",
            request_uri: "/usd/thing",
            content_uid: "uid-9",
            range: "bytes=0-1",
        }
        .render();
        assert_eq!(
            key.as_str(),
            "0|tenant1|GET|nucleus.mock.local|/usd/thing|uid-9|bytes=0-1"
        );
    }

    #[test]
    fn nucleus_generic_differs_from_lft_shape() {
        // The generic key carries host + request_uri (7 segments) while LFT does
        // not (5 segments), so the two never collide even before namespacing.
        let generic = NucleusGenericKeyParts {
            generation: "0",
            nca_id: "t",
            method: "GET",
            host: "h",
            request_uri: "/p",
            content_uid: "u",
            range: "",
        }
        .render();
        let lft = NucleusLftKeyParts {
            generation: "0",
            nca_id: "t",
            method: "GET",
            content_uid: "u",
            range: "",
        }
        .render();
        assert_ne!(generic, lft);
    }

    #[test]
    fn presigned_get_and_put_share_a_key() {
        // A consumer GET and a write-through PUT differ only in stripped params
        // (signature, date, x-id) — they must key identically.
        let get = filter_query_args("x-id=GetObject&X-Amz-Date=20250101T0Z&partNumber=1");
        let put = filter_query_args("x-id=PutObject&X-Amz-Date=20250202T0Z&partNumber=1");
        assert_eq!(get, put);
        assert_eq!(get, "partNumber=1");
    }
}
