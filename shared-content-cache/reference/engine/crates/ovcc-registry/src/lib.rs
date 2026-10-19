// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Registry vhost routing policy for OVCC.
//!
//! A registry backend caches package managers (npm, pip, conan, apt) read
//! through. A client addresses an upstream by a virtual host and OVCC maps it
//! to the real upstream, reaching it over TLS. The virtual host is the real
//! host, optionally followed by a cache suffix (e.g. `pypi.org` is reached as
//! `pypi.org.cache.svc.cluster.local` when OVCC runs in the `cache` namespace).
//!
//! This crate owns the **policy**: given a client `Host`, which configured
//! upstream does it name and what is the origin URL. It is pure (no async, no
//! I/O) and matches hosts by exact string comparison, never a regex. URI cache
//! eligibility uses Rust regex predicates compiled at configuration load. The content
//! rewrite rules a matched upstream carries ride along so the proxy can apply
//! them on the fill path; their type lives in [`ovcc_rewrite`].

use ovcc_rewrite::RewriteRule;
use regex::Regex;
use serde::Deserialize;

/// A cache eligibility predicate over the request path plus its raw query.
/// A bare regex matches positively; `!~` inverts the match, and other `~`
/// operators are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
pub struct CacheableUri {
    regex: Regex,
    negated: bool,
}

impl TryFrom<String> for CacheableUri {
    type Error = regex::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let value = value.trim_start();
        let (pattern, negated) = if let Some(pattern) = value.strip_prefix("!~") {
            if pattern.starts_with('*') {
                return Err(regex::Error::Syntax(
                    "use an inline Rust regex flag such as (?i) after !~".into(),
                ));
            }
            (pattern.trim_start(), true)
        } else if value.starts_with('~') {
            return Err(regex::Error::Syntax(
                "positive patterns are bare Rust regexes, without ~".into(),
            ));
        } else {
            (value, false)
        };
        Ok(Self {
            regex: Regex::new(pattern)?,
            negated,
        })
    }
}

impl PartialEq for CacheableUri {
    fn eq(&self, other: &Self) -> bool {
        self.negated == other.negated && self.regex.as_str() == other.regex.as_str()
    }
}

impl Eq for CacheableUri {}

impl CacheableUri {
    /// Whether this predicate allows caching `uri`, using an unanchored search.
    pub fn is_match(&self, uri: &str) -> bool {
        self.regex.is_match(uri) != self.negated
    }
}

/// How a client `Host` is matched against an upstream's allowlisted host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Matching {
    /// Accept only the bare host, e.g. `pypi.org`.
    Direct,
    /// Accept only the suffixed host, e.g. `pypi.org.<suffix>`.
    Suffix,
    /// Accept either the bare or the suffixed host.
    #[default]
    Any,
}

/// Registry backend configuration: a match mode, the cache vhost suffix, an
/// optional URI cache eligibility predicate, and the upstreams served. A slice
/// of the chart's `nginx.backends.registry`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RegistryConfig {
    /// Host match mode. Defaults to [`Matching::Any`], matching the chart.
    #[serde(default)]
    pub matching: Matching,
    /// The cache vhost suffix without a leading dot (e.g.
    /// `cache.svc.cluster.local`). Empty disables suffix matching.
    #[serde(default)]
    pub suffix: String,
    /// Backend-wide URI eligibility; absent means unrestricted.
    #[serde(default, alias = "cacheableUri")]
    pub cacheable_uri: Option<CacheableUri>,
    /// The upstreams this backend serves; the first whose allowlist accepts the
    /// client `Host` wins.
    #[serde(default)]
    pub upstreams: Vec<RegistryUpstream>,
}

/// One registry upstream: a name, the hosts it answers for, an optional URI
/// cache eligibility predicate, and any content rewrite rules for its index
/// responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RegistryUpstream {
    #[serde(default)]
    pub name: String,
    /// Real hosts this upstream serves (e.g. `[pypi.org, files.pythonhosted.org]`
    /// for pip's two-host layout).
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Additional URI restriction, ANDed with the backend predicate.
    #[serde(default, alias = "cacheableUri")]
    pub cacheable_uri: Option<CacheableUri>,
    /// Rewrite rules applied to this upstream's index responses, rewriting
    /// cross-host artifact links back through the cache. Empty for upstreams
    /// that serve index and artifacts from one host.
    #[serde(default, alias = "contentRewrite")]
    pub content_rewrite: Vec<RewriteRule>,
}

/// A resolved registry route: the origin URL to fetch from, the content
/// rewrite rules of the matched upstream, and any backend and upstream URI
/// cache eligibility predicates.
#[derive(Debug, PartialEq, Eq)]
pub struct RegistryMatch<'a> {
    /// The origin base URL, always `https://<real-host>`.
    pub upstream_url: String,
    /// The matched upstream's content rewrite rules (possibly empty).
    pub content_rewrite: &'a [RewriteRule],
    /// Backend-wide cache eligibility predicate.
    pub cacheable_uri: Option<&'a CacheableUri>,
    /// Additional predicate from the matched upstream.
    pub upstream_cacheable_uri: Option<&'a CacheableUri>,
}

impl RegistryMatch<'_> {
    /// The matched allowlisted host, without scheme, suffix, or port. Origin
    /// requests carry it as `Host` whichever virtual host the client used.
    pub fn upstream_host(&self) -> &str {
        self.upstream_url
            .strip_prefix("https://")
            .unwrap_or(&self.upstream_url)
    }

    /// Whether either scope restricts URIs, so callers can skip assembling one.
    pub fn has_uri_rules(&self) -> bool {
        self.cacheable_uri.is_some() || self.upstream_cacheable_uri.is_some()
    }

    /// Both scopes must allow caching; an absent predicate adds no restriction.
    pub fn is_cacheable_uri(&self, uri: &str) -> bool {
        self.cacheable_uri.is_none_or(|rule| rule.is_match(uri))
            && self
                .upstream_cacheable_uri
                .is_none_or(|rule| rule.is_match(uri))
    }
}

impl RegistryConfig {
    /// Resolves a client `Host` to its upstream route, or `None` when no
    /// configured upstream answers for it. Any `:port` on the host is ignored.
    pub fn resolve(&self, client_host: &str) -> Option<RegistryMatch<'_>> {
        let bare = client_host.split(':').next().unwrap_or(client_host);
        for upstream in &self.upstreams {
            for host in &upstream.hosts {
                if self.accepts(bare, host) {
                    return Some(RegistryMatch {
                        upstream_url: format!("https://{host}"),
                        content_rewrite: &upstream.content_rewrite,
                        cacheable_uri: self.cacheable_uri.as_ref(),
                        upstream_cacheable_uri: upstream.cacheable_uri.as_ref(),
                    });
                }
            }
        }
        None
    }

    /// Whether a client `Host` is accepted as naming `host` under the configured
    /// match mode.
    fn accepts(&self, candidate: &str, host: &str) -> bool {
        match self.matching {
            Matching::Direct => candidate.eq_ignore_ascii_case(host),
            Matching::Suffix => self.is_suffixed(candidate, host),
            Matching::Any => {
                candidate.eq_ignore_ascii_case(host) || self.is_suffixed(candidate, host)
            }
        }
    }

    /// Whether `candidate` equals `host.<suffix>` (case-insensitive). Always
    /// false when the suffix is empty.
    fn is_suffixed(&self, candidate: &str, host: &str) -> bool {
        eq_host_dot_suffix(candidate, host, &self.suffix)
    }
}

/// Whether `candidate` equals `host` + `.` + `suffix`, compared case-insensitively
/// over raw bytes so it never panics on non-ASCII or short input. An empty
/// `suffix` never matches.
fn eq_host_dot_suffix(candidate: &str, host: &str, suffix: &str) -> bool {
    if suffix.is_empty() {
        return false;
    }
    let candidate = candidate.as_bytes();
    let host = host.as_bytes();
    let suffix = suffix.as_bytes();
    if candidate.len() != host.len() + 1 + suffix.len() {
        return false;
    }
    candidate[..host.len()].eq_ignore_ascii_case(host)
        && candidate[host.len()] == b'.'
        && candidate[host.len() + 1..].eq_ignore_ascii_case(suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pip_rewrite() -> RewriteRule {
        RewriteRule {
            uri_starts_with: "/simple/".to_owned(),
            types: vec!["text/html".to_owned()],
            substitute: ovcc_rewrite::Substitute {
                match_: "https://files.pythonhosted.org".to_owned(),
                replace: "http://files.pythonhosted.org.cache.svc.cluster.local:14128".to_owned(),
            },
        }
    }

    fn config(matching: Matching) -> RegistryConfig {
        RegistryConfig {
            matching,
            suffix: "cache.svc.cluster.local".to_owned(),
            cacheable_uri: None,
            upstreams: vec![
                RegistryUpstream {
                    name: "npm".to_owned(),
                    hosts: vec!["registry.npmjs.org".to_owned()],
                    cacheable_uri: None,
                    content_rewrite: Vec::new(),
                },
                RegistryUpstream {
                    name: "pip".to_owned(),
                    hosts: vec!["pypi.org".to_owned(), "files.pythonhosted.org".to_owned()],
                    cacheable_uri: None,
                    content_rewrite: vec![pip_rewrite()],
                },
            ],
        }
    }

    #[test]
    fn suffix_mode_accepts_only_the_suffixed_host() {
        let cfg = config(Matching::Suffix);
        let m = cfg
            .resolve("registry.npmjs.org.cache.svc.cluster.local")
            .expect("suffixed npm host resolves");
        assert_eq!(m.upstream_url, "https://registry.npmjs.org");
        assert!(m.content_rewrite.is_empty());
        // The bare host is not accepted in suffix mode.
        assert!(cfg.resolve("registry.npmjs.org").is_none());
    }

    #[test]
    fn direct_mode_accepts_only_the_bare_host() {
        let cfg = config(Matching::Direct);
        let m = cfg.resolve("pypi.org").expect("bare pip host resolves");
        assert_eq!(m.upstream_url, "https://pypi.org");
        // The suffixed host is not accepted in direct mode.
        assert!(cfg.resolve("pypi.org.cache.svc.cluster.local").is_none());
    }

    #[test]
    fn any_mode_accepts_bare_or_suffixed() {
        let cfg = config(Matching::Any);
        assert_eq!(
            cfg.resolve("pypi.org").map(|m| m.upstream_url),
            Some("https://pypi.org".to_owned())
        );
        assert_eq!(
            cfg.resolve("pypi.org.cache.svc.cluster.local")
                .map(|m| m.upstream_url),
            Some("https://pypi.org".to_owned())
        );
    }

    #[test]
    fn resolves_the_second_host_of_a_multi_host_upstream_with_its_rewrite_rules() {
        let cfg = config(Matching::Any);
        let m = cfg
            .resolve("files.pythonhosted.org")
            .expect("artifact host resolves to the pip upstream");
        assert_eq!(m.upstream_url, "https://files.pythonhosted.org");
        // It carries the pip upstream's rewrite rules.
        assert_eq!(m.content_rewrite.len(), 1);
        assert_eq!(m.content_rewrite[0].uri_starts_with, "/simple/");
    }

    #[test]
    fn ignores_a_port_on_the_host() {
        let cfg = config(Matching::Any);
        assert_eq!(
            cfg.resolve("pypi.org:14128").map(|m| m.upstream_url),
            Some("https://pypi.org".to_owned())
        );
    }

    #[test]
    fn matches_case_insensitively() {
        let cfg = config(Matching::Any);
        assert_eq!(
            cfg.resolve("PyPI.ORG.Cache.SVC.cluster.local")
                .map(|m| m.upstream_url),
            Some("https://pypi.org".to_owned())
        );
    }

    #[test]
    fn registry_get_upstream_host_drops_suffix_port_and_case() {
        // Arrange
        let cases = [
            (Matching::Direct, "PyPI.org:14128", "pypi.org"),
            (
                Matching::Suffix,
                "files.pythonhosted.org.Cache.svc.cluster.local:14128",
                "files.pythonhosted.org",
            ),
            (
                Matching::Any,
                "registry.npmjs.org.cache.svc.cluster.local",
                "registry.npmjs.org",
            ),
            (Matching::Any, "registry.npmjs.org", "registry.npmjs.org"),
        ];

        for (matching, client_host, expected) in cases {
            // Act
            let upstream_host = config(matching)
                .resolve(client_host)
                .map(|m| m.upstream_host().to_owned());

            // Assert
            assert_eq!(upstream_host.as_deref(), Some(expected), "{client_host}");
        }
    }

    #[test]
    fn returns_none_for_an_unlisted_host() {
        let cfg = config(Matching::Any);
        assert!(cfg.resolve("example.com").is_none());
        assert!(cfg.resolve("evil.pypi.org").is_none());
    }

    #[test]
    fn empty_suffix_disables_suffix_matching() {
        let cfg = RegistryConfig {
            matching: Matching::Any,
            suffix: String::new(),
            cacheable_uri: None,
            upstreams: vec![RegistryUpstream {
                name: "npm".to_owned(),
                hosts: vec!["registry.npmjs.org".to_owned()],
                cacheable_uri: None,
                content_rewrite: Vec::new(),
            }],
        };
        // Bare still works in Any mode; a suffixed host cannot match an empty suffix.
        assert!(cfg.resolve("registry.npmjs.org").is_some());
        assert!(
            cfg.resolve("registry.npmjs.org.cache.svc.cluster.local")
                .is_none()
        );
    }

    #[test]
    fn empty_upstreams_never_resolve() {
        let cfg = RegistryConfig::default();
        assert!(cfg.resolve("pypi.org").is_none());
    }

    #[test]
    fn registry_get_cacheable_uri_token_miss() {
        // Arrange.
        let rule = CacheableUri::try_from(r"!~ ^/v2/token(?:\?|$)".to_owned()).unwrap();
        let excluded = [
            "/v2/token",
            "/v2/token?service=registry&scope=repository:test:pull",
        ];
        let eligible = [
            "/v2/",
            "/v2/tokens",
            "/v2/token/child",
            "/v2/TOKEN",
            "/v2/repo/blobs/sha256:abc",
            "/elsewhere?path=/v2/token",
        ];

        // Act.
        let excluded_results = excluded.map(|uri| rule.is_match(uri));
        let eligible_results = eligible.map(|uri| rule.is_match(uri));

        // Assert.
        for (uri, actual) in excluded.into_iter().zip(excluded_results) {
            assert!(!actual, "{uri}");
        }
        for (uri, actual) in eligible.into_iter().zip(eligible_results) {
            assert!(actual, "{uri}");
        }
    }

    #[test]
    fn registry_get_cacheable_uri_multiple_exclusions_in_one_expression() {
        // Arrange.
        let rule =
            CacheableUri::try_from(r"!~ ^/(?:v2/token|auth/session)(?:\?|$)".to_owned()).unwrap();
        let excluded = [
            "/v2/token",
            "/v2/token?service=registry",
            "/auth/session",
            "/auth/session?client=cli",
        ];
        let eligible = [
            "/v2/tokens",
            "/auth/sessions",
            "/auth/session/history",
            "/v2/test/blobs/sha256:abc",
            "/other?next=/auth/session",
        ];

        // Act.
        let excluded_results = excluded.map(|uri| rule.is_match(uri));
        let eligible_results = eligible.map(|uri| rule.is_match(uri));

        // Assert.
        for (uri, actual) in excluded.into_iter().zip(excluded_results) {
            assert!(!actual, "{uri} must be excluded");
        }
        for (uri, actual) in eligible.into_iter().zip(eligible_results) {
            assert!(actual, "{uri} must remain eligible");
        }
    }

    #[test]
    fn registry_get_cacheable_uri_raw_query_and_search_boundaries() {
        // Arrange.
        let exact = CacheableUri::try_from("^/v2/token$".to_owned()).unwrap();
        let search = CacheableUri::try_from("token".to_owned()).unwrap();
        let raw_query = CacheableUri::try_from(r"^/download\?key=a%2Fb$".to_owned()).unwrap();
        let exclusion = CacheableUri::try_from(r"!~ ^/v2/token(?:\?|$)".to_owned()).unwrap();

        // Act.
        let cases = [
            (exact.is_match("/v2/token"), true),
            (exact.is_match("/v2/token?service=registry"), false),
            (search.is_match("/other?token=value"), true),
            (raw_query.is_match("/download?key=a%2Fb"), true),
            (raw_query.is_match("/download?key=a/b"), false),
            (exclusion.is_match("/v2/%74oken"), true),
            (exclusion.is_match("/v2/token?"), false),
        ];

        // Assert.
        for (actual, expected) in cases {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn registry_get_cacheable_uri_operators_and_inline_flags() {
        // Arrange.
        let cases = [
            ("^/v2/token$", true, false),
            ("!~ ^/v2/token$", false, true),
            ("(?i)^/v2/token$", true, true),
            ("!~ (?i)^/v2/token$", false, false),
            ("(?x)^/v2/token$ # endpoint", true, false),
            ("", true, true),
            ("!~", false, false),
        ];
        let invalid = [
            "[",
            "~ ^/v2/token$",
            "~* ^/v2/token$",
            "!~* ^/v2/token$",
            "!~ (?!/v2/token)",
            r"(a)\1",
        ];

        // Act.
        let results = cases.map(|(pattern, lower, upper)| {
            let rule = CacheableUri::try_from(pattern.to_owned()).unwrap();
            (
                pattern,
                lower,
                upper,
                rule.is_match("/v2/token"),
                rule.is_match("/v2/TOKEN"),
            )
        });
        let invalid_results = invalid.map(|pattern| CacheableUri::try_from(pattern.to_owned()));

        // Assert.
        for (pattern, lower, upper, actual_lower, actual_upper) in results {
            assert_eq!(actual_lower, lower, "{pattern}");
            assert_eq!(actual_upper, upper, "{pattern}");
        }
        for (pattern, result) in invalid.into_iter().zip(invalid_results) {
            assert!(result.is_err(), "{pattern}");
        }
    }

    #[test]
    fn registry_get_cacheable_uri_backend_and_upstream_restrictions() {
        // Arrange.
        let mut cfg = config(Matching::Any);

        // Act.
        let unrestricted = cfg.resolve("pypi.org").unwrap();

        // Assert.
        assert!(!unrestricted.has_uri_rules());
        assert!(unrestricted.is_cacheable_uri("/v2/token"));

        // Arrange.
        cfg.cacheable_uri = Some(CacheableUri::try_from("!~ ^/v2/token$".to_owned()).unwrap());
        cfg.upstreams[0].cacheable_uri = Some(CacheableUri::try_from("^/v2/".to_owned()).unwrap());

        // Act.
        let npm = cfg.resolve("registry.npmjs.org").unwrap();
        let pip = cfg.resolve("pypi.org.cache.svc.cluster.local").unwrap();

        // Assert.
        assert!(npm.has_uri_rules() && pip.has_uri_rules());
        assert!(!npm.is_cacheable_uri("/v2/token"));
        assert!(!npm.is_cacheable_uri("/package"));
        assert!(npm.is_cacheable_uri("/v2/package"));
        assert!(!pip.is_cacheable_uri("/v2/token"));
        assert!(pip.is_cacheable_uri("/package"));

        // Arrange.
        cfg.cacheable_uri = None;

        // Act.
        let npm = cfg.resolve("registry.npmjs.org").unwrap();

        // Assert.
        assert!(npm.has_uri_rules());
        assert!(!npm.is_cacheable_uri("/package"));
    }
}
