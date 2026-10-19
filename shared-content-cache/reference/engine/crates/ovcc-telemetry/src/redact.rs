// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Redaction helpers for span attributes and error messages.
//!
//! Presigned query parameters (`X-Amz-Signature`, Azure SAS `sig`, ...) and
//! userinfo must never reach a span attribute or an error string. The access
//! log's `request_uri` is the deliberate exception, since it mirrors nginx's raw
//! `$request_uri` for A/B parity and is handled separately in [`crate::access`].

/// Strips the query, fragment, and userinfo from a URL or request target,
/// leaving `scheme://host[:port]/path` (or just `/path` for an origin-form
/// target). Use for the `url.path`/`server.address` span attributes so signed
/// URLs never land in traces.
pub fn redact_url(raw: &str) -> String {
    // Split off fragment then query first; both can carry signatures.
    let without_fragment = raw.split('#').next().unwrap_or(raw);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);

    // Drop userinfo in an absolute URL of the form scheme://user:pass@host/path.
    if let Some(scheme_end) = without_query.find("://") {
        let (scheme, rest) = without_query.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            // Keep everything after the last '@' in the authority+path.
            let host_and_path = &rest[at + 1..];
            return format!("{scheme}{host_and_path}");
        }
    }
    without_query.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_query_and_fragment() {
        assert_eq!(
            redact_url(
                "https://bucket.s3.amazonaws.com/obj?X-Amz-Signature=deadbeef&X-Amz-Date=1#frag"
            ),
            "https://bucket.s3.amazonaws.com/obj"
        );
    }

    #[test]
    fn strips_userinfo() {
        assert_eq!(
            redact_url("https://user:secret@host.example.com/path?q=1"),
            "https://host.example.com/path"
        );
    }

    #[test]
    fn origin_form_target_kept_minus_query() {
        assert_eq!(redact_url("/objects/a.usd?sig=abc"), "/objects/a.usd");
    }

    #[test]
    fn plain_path_unchanged() {
        assert_eq!(redact_url("/healthz"), "/healthz");
    }
}
