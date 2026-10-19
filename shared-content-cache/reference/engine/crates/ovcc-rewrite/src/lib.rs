// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Content rewrite policy for OVCC.
//!
//! Some upstreams serve an index document that links its artifacts at a
//! different absolute host than the index itself. A read-through cache must
//! rewrite those links to point back at the cache so the artifact bytes flow
//! through it too. The canonical case is a pip simple index on `pypi.org`
//! linking wheels at `files.pythonhosted.org`.
//!
//! This crate owns the **policy**, not the mechanism. [`apply`] decides whether
//! a response body is eligible for substitution (by request URI prefix and
//! response content type) and, if so, produces the rewritten bytes. It is pure
//! (no async, no I/O) and does a literal byte-substring replace, so it never
//! parses or interprets the body and is safe on arbitrary input.
//!
//! The proxy applies the rewrite on the cache-fill path before admitting the
//! object, so a hit serves the already-rewritten bytes straight from disk and
//! the stored length stays correct. Eligibility is gated to small index
//! documents, so large artifact bodies are never matched and never buffered for
//! rewriting.

use std::borrow::Cow;

use serde::Deserialize;

/// One rewrite rule: a request URI prefix and a response content-type allowlist
/// that gate a literal substring [`Substitute`]. A response is rewritten by the
/// first rule whose `uri_starts_with` prefixes the request path and whose
/// `types` admit the response content type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RewriteRule {
    /// Request path prefix this rule applies to (e.g. `/simple/`). An empty
    /// prefix matches every path.
    #[serde(default, alias = "uriStartsWith")]
    pub uri_starts_with: String,
    /// Response content types this rule applies to, matched on the media type
    /// (the part before any `;` parameters). An empty list matches every type.
    #[serde(default)]
    pub types: Vec<String>,
    /// The literal substitution applied to a matched body.
    #[serde(default)]
    pub substitute: Substitute,
}

/// A literal substring substitution: every non-overlapping occurrence of
/// `match_` in the body becomes `replace`. Both are literal bytes, never
/// patterns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Substitute {
    /// The literal byte sequence to find. The chart spells this `match`; it is
    /// renamed here because `match` is a Rust keyword.
    #[serde(rename = "match")]
    pub match_: String,
    /// The literal byte sequence each occurrence is replaced with.
    #[serde(default)]
    pub replace: String,
}

/// Rewrites `body` by the first rule that matches `uri` and `content_type`,
/// returning the rewritten bytes, or `None` when nothing changed.
///
/// `None` covers every no-op so the caller serves and stores the original body
/// without an extra copy: no rule matched, the matched rule's `match_` is empty
/// or equal to `replace`, or the body contained no occurrence. A returned
/// `Some` always differs from `body`.
pub fn apply<'a>(
    rules: &[RewriteRule],
    uri: &str,
    content_type: &str,
    body: &'a [u8],
) -> Option<Cow<'a, [u8]>> {
    let rule = rules
        .iter()
        .find(|rule| rule_matches(rule, uri, content_type))?;
    let needle = rule.substitute.match_.as_bytes();
    let replacement = rule.substitute.replace.as_bytes();
    if needle.is_empty() || needle == replacement {
        return None;
    }
    replace_all(body, needle, replacement).map(Cow::Owned)
}

/// Whether a rule applies to a request `uri` and response `content_type`. The
/// URI must start with the rule's prefix and the content type's media type must
/// be in the rule's `types` (an empty `types` admits any type).
fn rule_matches(rule: &RewriteRule, uri: &str, content_type: &str) -> bool {
    uri.starts_with(&rule.uri_starts_with) && content_type_admitted(&rule.types, content_type)
}

/// Whether `content_type` is admitted by `types`, comparing only the media type
/// (before any `;` parameters) case-insensitively. An empty `types` admits any.
fn content_type_admitted(types: &[String], content_type: &str) -> bool {
    if types.is_empty() {
        return true;
    }
    let media = media_type(content_type);
    types
        .iter()
        .any(|allowed| media_type(allowed).eq_ignore_ascii_case(media))
}

/// The media type of a content-type value: the part before any `;` parameters,
/// trimmed.
fn media_type(content_type: &str) -> &str {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
}

/// Replaces every non-overlapping occurrence of `needle` in `haystack` with
/// `replacement`, scanning left to right. Returns `None` when `needle` never
/// occurs, so an unchanged body is not copied.
fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Option<Vec<u8>> {
    let occurrences = find_all(haystack, needle);
    if occurrences.is_empty() {
        return None;
    }
    let grown = replacement.len().saturating_sub(needle.len()) * occurrences.len();
    let mut out = Vec::with_capacity(haystack.len() + grown);
    let mut cursor = 0;
    for start in occurrences {
        out.extend_from_slice(&haystack[cursor..start]);
        out.extend_from_slice(replacement);
        cursor = start + needle.len();
    }
    out.extend_from_slice(&haystack[cursor..]);
    Some(out)
}

/// The start offsets of every non-overlapping occurrence of `needle` in
/// `haystack`, left to right. An empty `needle` yields no matches.
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    if needle.is_empty() || needle.len() > haystack.len() {
        return offsets;
    }
    let last = haystack.len() - needle.len();
    let mut i = 0;
    while i <= last {
        if &haystack[i..i + needle.len()] == needle {
            offsets.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pip_rule() -> RewriteRule {
        RewriteRule {
            uri_starts_with: "/simple/".to_owned(),
            types: vec![
                "application/vnd.pypi.simple.v1+json".to_owned(),
                "text/html".to_owned(),
            ],
            substitute: Substitute {
                match_: "https://files.pythonhosted.org".to_owned(),
                replace: "http://files.pythonhosted.org.cache.svc.cluster.local:14128".to_owned(),
            },
        }
    }

    #[test]
    fn rewrites_a_matching_index_body() {
        let rules = [pip_rule()];
        let body = b"<a href=\"https://files.pythonhosted.org/p/idna.whl\">idna</a>";
        let out = apply(&rules, "/simple/idna/", "text/html", body).expect("rewrite applies");
        let text = String::from_utf8(out.into_owned()).unwrap();
        assert!(
            text.contains("http://files.pythonhosted.org.cache.svc.cluster.local:14128/p/idna.whl")
        );
        assert!(!text.contains("https://files.pythonhosted.org/"));
    }

    #[test]
    fn replaces_every_occurrence() {
        let rules = [pip_rule()];
        let body = b"https://files.pythonhosted.org/a https://files.pythonhosted.org/b";
        let out = apply(&rules, "/simple/x/", "text/html", body).expect("rewrite applies");
        let text = String::from_utf8(out.into_owned()).unwrap();
        assert_eq!(text.matches("cache.svc.cluster.local:14128").count(), 2);
    }

    #[test]
    fn matches_content_type_with_charset_parameter() {
        let rules = [pip_rule()];
        let body = b"https://files.pythonhosted.org/x";
        // The response carries parameters after the media type; matching keys
        // off the media type only.
        assert!(apply(&rules, "/simple/x/", "text/html; charset=utf-8", body).is_some());
    }

    #[test]
    fn skips_when_uri_prefix_does_not_match() {
        let rules = [pip_rule()];
        let body = b"https://files.pythonhosted.org/x";
        assert!(apply(&rules, "/packages/x.whl", "text/html", body).is_none());
    }

    #[test]
    fn skips_when_content_type_not_allowed() {
        let rules = [pip_rule()];
        let body = b"https://files.pythonhosted.org/x";
        assert!(apply(&rules, "/simple/x/", "application/octet-stream", body).is_none());
    }

    #[test]
    fn skips_when_match_absent_from_body() {
        let rules = [pip_rule()];
        let body = b"nothing to rewrite here";
        assert!(apply(&rules, "/simple/x/", "text/html", body).is_none());
    }

    #[test]
    fn empty_types_admits_any_content_type() {
        let rules = [RewriteRule {
            uri_starts_with: "/".to_owned(),
            types: Vec::new(),
            substitute: Substitute {
                match_: "a".to_owned(),
                replace: "bb".to_owned(),
            },
        }];
        let out = apply(&rules, "/x", "anything/at-all", b"a-a").expect("rewrite applies");
        assert_eq!(out.into_owned(), b"bb-bb");
    }

    #[test]
    fn no_op_when_match_equals_replace() {
        let rules = [RewriteRule {
            uri_starts_with: String::new(),
            types: Vec::new(),
            substitute: Substitute {
                match_: "x".to_owned(),
                replace: "x".to_owned(),
            },
        }];
        // Direct-mode registries configure a net-zero substitution; it must not
        // copy the body.
        assert!(apply(&rules, "/anything", "text/html", b"x x x").is_none());
    }

    #[test]
    fn empty_match_is_a_no_op() {
        let rules = [RewriteRule {
            uri_starts_with: String::new(),
            types: Vec::new(),
            substitute: Substitute {
                match_: String::new(),
                replace: "z".to_owned(),
            },
        }];
        assert!(apply(&rules, "/x", "text/html", b"abc").is_none());
    }

    #[test]
    fn empty_body_is_a_no_op() {
        let rules = [pip_rule()];
        assert!(apply(&rules, "/simple/x/", "text/html", b"").is_none());
    }

    #[test]
    fn shrinking_replacement_accounts_for_length() {
        let rules = [RewriteRule {
            uri_starts_with: String::new(),
            types: Vec::new(),
            substitute: Substitute {
                match_: "longneedle".to_owned(),
                replace: "x".to_owned(),
            },
        }];
        let out = apply(&rules, "/", "text/html", b"longneedle longneedle").expect("applies");
        assert_eq!(out.into_owned(), b"x x");
    }

    #[test]
    fn non_utf8_bytes_pass_through_untouched() {
        let rules = [RewriteRule {
            uri_starts_with: String::new(),
            types: Vec::new(),
            substitute: Substitute {
                match_: "ab".to_owned(),
                replace: "Z".to_owned(),
            },
        }];
        // Invalid UTF-8 framing the match must survive byte-for-byte.
        let body = [0xff, 0xfe, b'a', b'b', 0x00, 0x80];
        let out = apply(&rules, "/", "text/html", &body).expect("applies");
        assert_eq!(out.into_owned(), vec![0xff, 0xfe, b'Z', 0x00, 0x80]);
    }

    #[test]
    fn find_all_is_non_overlapping() {
        // Overlapping candidates are consumed left to right, so "aa" in "aaa"
        // is found once, not twice.
        assert_eq!(find_all(b"aaa", b"aa"), vec![0]);
        assert_eq!(find_all(b"aaaa", b"aa"), vec![0, 2]);
    }

    use proptest::prelude::*;

    proptest! {
        // apply never panics and, when it rewrites, the output length matches the
        // exact accounting for the occurrences it replaced.
        #[test]
        fn apply_never_panics_and_accounts_for_length(
            body in proptest::collection::vec(any::<u8>(), 0..512),
            needle in "[a-c]{1,4}",
            replace in "[x-z]{0,6}",
        ) {
            let rules = [RewriteRule {
                uri_starts_with: String::new(),
                types: Vec::new(),
                substitute: Substitute { match_: needle.clone(), replace: replace.clone() },
            }];
            let result = apply(&rules, "/any", "text/html", &body);
            if needle == replace {
                prop_assert!(result.is_none());
                return Ok(());
            }
            let count = find_all(&body, needle.as_bytes()).len();
            match result {
                None => prop_assert_eq!(count, 0),
                Some(out) => {
                    let expected = body.len() as isize
                        + count as isize * (replace.len() as isize - needle.len() as isize);
                    prop_assert_eq!(out.len() as isize, expected);
                    prop_assert!(count > 0);
                }
            }
        }
    }
}
