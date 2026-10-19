// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OVCC configuration covering the `values.yaml` schema plus a layered
//! load (file then env overrides) via figment.
//!
//! # Prefixed vs top-level backends
//!
//! OVCC accepts backend values at either the required deployment wire path or
//! the native top-level path. A single rule selects the source.
//!
//! - When an `nginx:` node is present, entries are read from
//!   `nginx.backends.<name>` and a top-level `backends` map is ignored.
//! - Otherwise, entries are read from `backends.<name>`.
//!
//! The deployment wire path supports exact host names, `~`/`~*` regular
//! expressions, PCRE-style `(?<name>...)` captures, and proxy expressions built
//! from `$scheme`, `$host`, `$http_host`, or named captures. Unsupported values
//! fail during startup. Native `host`/`upstream` remain aliases for direct
//! engine configurations, but deployment values win when present.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use figment::{
    Figment,
    providers::{Env, Format, Yaml},
};
use ovcc_core::{Error, Result};
use regex::Regex;
use serde::{Deserialize, Deserializer};

/// Built-in host patterns preserve direct engine configurations that omit
/// `serverName`. Deployment configurations use their validated expressions.
static S3_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[^.]+\.s3\.[^.]+\.amazonaws\.com$").unwrap());
static GCS_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^([^.]+\.)?storage\.googleapis\.com$").unwrap());
static AZURE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^[^.]+\.blob\.core\.windows\.net$").unwrap());

/// The built-in cloud host pattern for a backend `name`, if it is one of the
/// known cloud providers. `None` for non-cloud backends (mock, nucleus, …),
/// which then route purely on the exact configured `host`.
fn builtin_cloud_pattern(name: &str) -> Option<&'static Regex> {
    match name {
        "s3" => Some(&S3_HOST),
        "gcs" => Some(&GCS_HOST),
        "azure" => Some(&AZURE_HOST),
        _ => None,
    }
}

/// Top-level OVCC configuration (slice of `deploy/values.yaml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Config {
    pub backends: Vec<Backend>,
    /// Unmatched requests are relayed according to the canonical
    /// `nginx.backends.passthrough.proxyPass` expression.
    #[serde(skip)]
    pub passthrough: Passthrough,
    pub tls: Tls,
    pub cache: CacheConfig,
    /// Shared write-through staging policy from the canonical
    /// `nginx.writeThrough` wire path.
    #[serde(default)]
    pub write_through: WriteThroughConfig,
    #[serde(default)]
    pub service: ServiceConfig,
    /// In-cluster peer discovery, transport timing, and resource limits.
    #[serde(default)]
    pub peer: PeerConfig,
    /// Cache-reset paths, instance headers, and legacy alias emission.
    #[serde(default)]
    pub compat: CompatConfig,
    /// Logging + tracing settings. Read at the top level in both modes (like
    /// `tls`/`cache`); a missing `telemetry:` block yields working defaults.
    /// The schema lives in [`ovcc_telemetry`].
    #[serde(default)]
    pub telemetry: ovcc_telemetry::TelemetryConfig,
    /// DNS lookup and origin connection-pool policy shared by backends.
    #[serde(skip)]
    pub upstream_balancer: UpstreamBalancer,
    /// Trust roots and certificate-chain policy for HTTPS origins.
    #[serde(skip)]
    pub upstream_tls: UpstreamTls,
    /// Per-stage inactivity ceilings for an origin request.
    #[serde(skip)]
    pub origin_timeouts: OriginTimeouts,
}

/// Certificate verification policy for HTTPS origins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamTls {
    pub trusted_certificate: String,
    pub verify_depth: usize,
}

impl Default for UpstreamTls {
    fn default() -> Self {
        Self {
            trusted_certificate: "/etc/ssl/certs/ca-certificates.crt".to_owned(),
            verify_depth: 2,
        }
    }
}

/// Typed origin-stage inactivity policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginTimeouts {
    pub connect: Duration,
    pub response_header: Duration,
    pub body_stall: Duration,
}

impl Default for OriginTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(60),
            response_header: Duration::from_secs(60),
            body_stall: Duration::from_secs(60),
        }
    }
}

/// DNS lookup and origin connection-pool policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamBalancer {
    pub keepalive: UpstreamKeepalive,
    pub resolver: ResolverConfig,
}

/// Bounds for pooled origin connections.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UpstreamKeepalive {
    #[serde(rename = "idleTimeoutSecs")]
    pub idle_timeout_secs: u64,
    #[serde(rename = "maxRequestsPerConnection")]
    pub max_requests_per_connection: u64,
    #[serde(rename = "maxCachedConnectionsPerWorker")]
    pub max_cached_connections: usize,
}

impl Default for UpstreamKeepalive {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 60,
            max_requests_per_connection: 0,
            max_cached_connections: 200,
        }
    }
}

/// DNS cache, query, and fallback nameserver policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResolverConfig {
    pub cache: ResolverCache,
    pub query: ResolverQuery,
    pub fallback: Vec<String>,
}

/// How long resolved answers stay usable, split by query outcome.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResolverCache {
    #[serde(rename = "successTtlSecs")]
    pub success_ttl_secs: u64,
    #[serde(rename = "failureTtlSecs")]
    pub failure_ttl_secs: u64,
}

impl Default for ResolverCache {
    fn default() -> Self {
        Self {
            success_ttl_secs: 30,
            failure_ttl_secs: 10,
        }
    }
}

/// Per-query timeout, retry budget, and nameserver ordering.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ResolverQuery {
    #[serde(rename = "timeoutMs")]
    pub timeout_ms: u64,
    pub retries: usize,
    pub random: bool,
}

impl Default for ResolverQuery {
    fn default() -> Self {
        Self {
            timeout_ms: 20_000,
            retries: 3,
            random: true,
        }
    }
}

/// One cache backend, a `host` routed to an `upstream` origin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Backend {
    pub name: String,
    pub host: String,
    pub upstream: String,
    /// Optional on-disk cache root for this backend. Missing or empty values use
    /// the process-wide fallback root supplied to `ovcc-proxy`.
    #[serde(default, alias = "cacheRoot", alias = "cacheroot")]
    pub cache_root: Option<String>,
    /// Cache limits resolved from this backend's canonical
    /// `proxyCacheName` and `nginx.proxyCache` path.
    #[serde(skip)]
    pub cache_policy: BackendCachePolicy,
    /// When true, a GET without an `If-Match` validator bypasses the cache
    /// (cloud-storage default). When false, such GETs are cacheable.
    pub requires_versioning: bool,
    /// Bursty (short-TTL, tenant-adaptive) cache policy for this backend, when
    /// enabled (emitted by the chart only under `featureFlags.enableBurstyCache`).
    /// `None` means bursty is off and matching list/HEAD requests take the
    /// normal path. The policy types live in [`ovcc_bursty`].
    #[serde(default)]
    pub bursty: Option<ovcc_bursty::BurstyCache>,
    /// Registry vhost routing for a package-manager read-through backend, when
    /// configured. `None` means this backend routes on the cloud/exact host
    /// rules above. The routing policy lives in [`ovcc_registry`].
    #[serde(default)]
    pub registry: Option<ovcc_registry::RegistryConfig>,
    /// Write-through policy for this backend. Disabled by default, so a `PUT` is
    /// proxied transparently and never seeds the cache.
    #[serde(default)]
    pub write_through: WriteThrough,
    /// Whether this backend exposes the cache-generation reset endpoint
    /// (`POST /__/cache/reset`). Off by default, matching the production Nucleus
    /// deployment; a staging/test deployment may enable it. Only consulted for
    /// the Nucleus listener today (the main cache listener keeps its existing
    /// behavior). Chart alias `allowCacheReset`.
    #[serde(default, alias = "allowCacheReset")]
    pub allow_cache_reset: bool,
    /// Deployment routing and transport policy, kept separate from the native
    /// `host`/`upstream` aliases.
    #[serde(skip)]
    pub route: BackendRoute,
}

/// Per-backend routing and transport policy.
#[derive(Debug, Clone)]
pub struct BackendRoute {
    server_name: Option<ServerName>,
    proxy_pass: Option<String>,
    proxy_auth_pass: Option<String>,
    pub proxy_cache_name: Option<String>,
    pub enable_upstream_keepalive: bool,
    pub allow_http2_clients: bool,
    /// Optional nameserver used only by this backend.
    pub resolver_override: Option<String>,
    scheme: String,
}

impl Default for BackendRoute {
    fn default() -> Self {
        Self {
            server_name: None,
            proxy_pass: None,
            proxy_auth_pass: None,
            proxy_cache_name: None,
            // Direct engine configurations use the pooled origin client.
            enable_upstream_keepalive: true,
            allow_http2_clients: false,
            resolver_override: None,
            scheme: "https".to_owned(),
        }
    }
}

impl PartialEq for BackendRoute {
    fn eq(&self, other: &Self) -> bool {
        self.server_name.as_ref().map(|v| &v.source)
            == other.server_name.as_ref().map(|v| &v.source)
            && self.proxy_pass == other.proxy_pass
            && self.proxy_auth_pass == other.proxy_auth_pass
            && self.proxy_cache_name == other.proxy_cache_name
            && self.enable_upstream_keepalive == other.enable_upstream_keepalive
            && self.allow_http2_clients == other.allow_http2_clients
            && self.resolver_override == other.resolver_override
            && self.scheme == other.scheme
    }
}

impl Eq for BackendRoute {}

#[derive(Debug, Clone)]
struct ServerName {
    source: String,
    matcher: ServerNameMatcher,
}

#[derive(Debug, Clone)]
enum ServerNameMatcher {
    Exact(String),
    Regex(Regex),
    CatchAll,
}

/// A resolved content origin and the independently configurable authorization
/// probe origin. Registry HIT authorization must continue to target the real
/// allowlisted upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBackend {
    pub upstream: String,
    pub auth_upstream: Option<String>,
}

/// Unmatched-host relay policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passthrough {
    proxy_pass: String,
    scheme: String,
}

impl Default for Passthrough {
    fn default() -> Self {
        // Direct engine test configurations relay to plaintext mock origins.
        Self {
            proxy_pass: "http://$http_host".to_owned(),
            scheme: "http".to_owned(),
        }
    }
}

/// Runtime projection of canonical `nginx.proxyCache` settings for one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCachePolicy {
    pub validity_200_secs: u64,
    pub validity_206_secs: u64,
    pub max_idle_secs: Option<u64>,
    pub peer_copy_max_idle_secs: Option<u64>,
    /// Most-nested backend override persisted with each admitted object.
    pub backend_max_idle_secs: Option<u64>,
    pub max_bytes: Option<u64>,
    pub min_free_bytes: Option<u64>,
    pub lock_age_secs: u64,
    pub lock_timeout_secs: u64,
}

impl Default for BackendCachePolicy {
    fn default() -> Self {
        Self {
            validity_200_secs: 86_400,
            validity_206_secs: 86_400,
            max_idle_secs: Some(86_400),
            peer_copy_max_idle_secs: Some(14_400),
            backend_max_idle_secs: None,
            max_bytes: None,
            min_free_bytes: None,
            lock_age_secs: 60,
            lock_timeout_secs: 120,
        }
    }
}

impl BackendCachePolicy {
    /// Freshness lifetime for a cacheable status, or `None` for a status without one.
    pub fn validity_secs(&self, status: u16) -> Option<u64> {
        match status {
            200 => Some(self.validity_200_secs),
            206 => Some(self.validity_206_secs),
            _ => None,
        }
    }
}

/// How the Nucleus backend reaches its origin.
///
/// nginx `proxy_pass $scheme://$host` proxies to the request's own host. The
/// engine mirrors that: with no configured `upstream` it derives the origin from
/// the request host ([`NucleusUpstream::DynamicHost`]); the integration harness
/// pins a concrete mock upstream ([`NucleusUpstream::Configured`]) for
/// determinism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NucleusUpstream {
    /// An explicit origin base URL (e.g. the integration mock).
    Configured(String),
    /// Derive `https://<host>` from the request host at serve time.
    DynamicHost,
}

impl Backend {
    /// Resolves how to reach the origin for a request `Host`, or `None` when
    /// this backend does not serve that host.
    ///
    /// Three routing modes, checked in order.
    /// 1. **Registry** — a registry backend maps a package-manager virtual host
    ///    to its real upstream over TLS ([`ovcc_registry`]); the configured
    ///    cloud/exact rules below do not apply.
    /// 2. **Configured expression** — `serverName` matches and the proxy
    ///    expressions are evaluated.
    /// 3. **Cloud fallback** — when expression routing is absent, `Host` matches
    ///    this backend's built-in cloud pattern and proxies to itself over TLS.
    /// 4. **Configured/mock** — `Host` equals the configured exact `host` (the
    ///    integration mock, e.g. `s3.mock.local`); use the configured
    ///    `upstream`.
    ///
    /// Any `:port` on `Host` is ignored for matching.
    pub fn resolve_upstream(&self, host: &str) -> Option<String> {
        self.resolve_backend(host).map(|resolved| resolved.upstream)
    }

    /// Resolves both the content origin and the authorization-probe origin.
    pub fn resolve_backend(&self, host: &str) -> Option<ResolvedBackend> {
        if let Some(registry) = &self.registry {
            return registry.resolve(host).and_then(|m| {
                let upstream_host = m
                    .upstream_url
                    .strip_prefix("https://")
                    .unwrap_or(&m.upstream_url);
                let captures =
                    BTreeMap::from([("upstream_host".to_owned(), upstream_host.to_owned())]);
                let upstream = match self.route.proxy_pass.as_deref() {
                    Some(template) => render_proxy_pass(
                        template,
                        &self.route.scheme,
                        bare_host(host),
                        host,
                        &captures,
                    )?,
                    None => m.upstream_url,
                };
                let auth_upstream = match self.route.proxy_auth_pass.as_deref() {
                    Some(template) => render_proxy_pass(
                        template,
                        &self.route.scheme,
                        bare_host(host),
                        host,
                        &captures,
                    ),
                    None => Some(upstream.clone()),
                };
                Some(ResolvedBackend {
                    upstream,
                    auth_upstream,
                })
            });
        }
        let bare = host.split(':').next().unwrap_or(host);
        if let Some(server_name) = &self.route.server_name {
            let captures = server_name.captures(bare)?;
            let upstream = render_proxy_pass(
                self.route.proxy_pass.as_deref()?,
                &self.route.scheme,
                bare,
                host,
                &captures,
            )?;
            let auth_upstream = self.route.proxy_auth_pass.as_deref().and_then(|template| {
                render_proxy_pass(template, &self.route.scheme, bare, host, &captures)
            });
            return Some(ResolvedBackend {
                upstream,
                auth_upstream,
            });
        }
        if builtin_cloud_pattern(&self.name).is_some_and(|re| re.is_match(bare)) {
            let fallback = format!("https://{bare}");
            let upstream = self
                .route
                .proxy_pass
                .as_deref()
                .and_then(|template| {
                    render_proxy_pass(template, &self.route.scheme, bare, host, &BTreeMap::new())
                })
                .unwrap_or(fallback);
            let auth_upstream = self
                .route
                .proxy_auth_pass
                .as_deref()
                .and_then(|template| {
                    render_proxy_pass(template, &self.route.scheme, bare, host, &BTreeMap::new())
                })
                .or_else(|| Some(upstream.clone()));
            return Some(ResolvedBackend {
                auth_upstream,
                upstream,
            });
        }
        if self.host == bare {
            let upstream = self
                .route
                .proxy_pass
                .as_deref()
                .and_then(|template| {
                    render_proxy_pass(template, &self.route.scheme, bare, host, &BTreeMap::new())
                })
                .unwrap_or_else(|| self.upstream.clone());
            let auth_upstream = self
                .route
                .proxy_auth_pass
                .as_deref()
                .and_then(|template| {
                    render_proxy_pass(template, &self.route.scheme, bare, host, &BTreeMap::new())
                })
                .or_else(|| Some(upstream.clone()));
            return Some(ResolvedBackend {
                auth_upstream,
                upstream,
            });
        }
        None
    }

    /// Resolves a registry route for `host` — the origin URL plus the matched
    /// upstream's content rewrite rules — or `None` when this is not a registry
    /// backend or no upstream answers for the host. The proxy uses this on the
    /// fill path to apply index rewriting; plain routing uses
    /// [`resolve_upstream`](Self::resolve_upstream).
    pub fn registry_match(&self, host: &str) -> Option<ovcc_registry::RegistryMatch<'_>> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.resolve(host))
    }

    /// How the Nucleus backend reaches its origin: an explicitly configured
    /// `upstream` ([`NucleusUpstream::Configured`]), or — when `upstream` is
    /// empty — the request-host-derived origin ([`NucleusUpstream::DynamicHost`],
    /// nginx `proxy_pass $scheme://$host`).
    pub fn nucleus_upstream(&self) -> NucleusUpstream {
        if self.upstream.trim().is_empty() {
            NucleusUpstream::DynamicHost
        } else {
            NucleusUpstream::Configured(self.upstream.clone())
        }
    }

    /// Resolves the configured Nucleus `proxyPass`, or preserves the native
    /// configured/dynamic behavior when canonical routing is absent.
    pub fn resolve_nucleus_upstream(&self, host: &str, scheme: &str) -> String {
        if self.route.server_name.is_some() || self.route.proxy_pass.is_some() {
            return self
                .resolve_backend(host)
                .map(|resolved| resolved.upstream)
                .unwrap_or_else(|| format!("{scheme}://{}", bare_host(host)));
        }
        match self.nucleus_upstream() {
            NucleusUpstream::Configured(url) => url,
            NucleusUpstream::DynamicHost => format!("{scheme}://{}", bare_host(host)),
        }
    }
}

impl ServerName {
    fn parse(source: String) -> std::result::Result<Self, String> {
        let matcher = if source == "_" {
            ServerNameMatcher::CatchAll
        } else if let Some(pattern) = source.strip_prefix("~*") {
            let pattern = translate_pcre_named_captures(pattern.trim());
            ServerNameMatcher::Regex(
                Regex::new(&format!("(?i:{pattern})"))
                    .map_err(|err| format!("unsupported serverName {source:?}: {err}"))?,
            )
        } else if let Some(pattern) = source.strip_prefix('~') {
            let pattern = translate_pcre_named_captures(pattern.trim());
            ServerNameMatcher::Regex(
                Regex::new(&pattern)
                    .map_err(|err| format!("unsupported serverName {source:?}: {err}"))?,
            )
        } else {
            if source.split_ascii_whitespace().count() != 1 {
                return Err(format!(
                    "unsupported serverName {source:?}: multiple names are not supported"
                ));
            }
            ServerNameMatcher::Exact(source.to_ascii_lowercase())
        };
        Ok(Self { source, matcher })
    }

    fn captures(&self, host: &str) -> Option<BTreeMap<String, String>> {
        match &self.matcher {
            ServerNameMatcher::Exact(expected) => {
                expected.eq_ignore_ascii_case(host).then(BTreeMap::new)
            }
            ServerNameMatcher::CatchAll => Some(BTreeMap::new()),
            ServerNameMatcher::Regex(regex) => {
                let captures = regex.captures(host)?;
                let mut values = BTreeMap::new();
                for name in regex.capture_names().flatten() {
                    if let Some(value) = captures.name(name) {
                        values.insert(name.to_owned(), value.as_str().to_owned());
                    }
                }
                Some(values)
            }
        }
    }
}

impl Passthrough {
    /// Expands the configured `proxyPass` template against the request host.
    pub fn resolve_upstream(&self, host: &str) -> String {
        render_proxy_pass(
            &self.proxy_pass,
            &self.scheme,
            bare_host(host),
            host,
            &BTreeMap::new(),
        )
        .expect("passthrough proxyPass is validated during config load")
    }
}

fn bare_host(host: &str) -> &str {
    host.split(':').next().unwrap_or(host)
}

fn translate_pcre_named_captures(pattern: &str) -> String {
    pattern.replace("(?<", "(?P<")
}

fn render_proxy_pass(
    template: &str,
    scheme: &str,
    host: &str,
    http_host: &str,
    captures: &BTreeMap<String, String>,
) -> Option<String> {
    let mut rendered = template
        .replace("$scheme", scheme)
        .replace("$http_host", http_host)
        .replace("$host", host);
    for (name, value) in captures {
        rendered = rendered.replace(&format!("${name}"), value);
    }
    (!rendered.contains('$')
        && (rendered.starts_with("http://") || rendered.starts_with("https://")))
    .then_some(rendered)
}

/// Client-facing TLS settings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Tls {
    pub enabled: bool,
    /// PEM certificate chain mounted from the deployment TLS Secret. When both
    /// paths are absent, isolated in-process tests use a self-signed certificate.
    #[serde(default, alias = "certificatePath")]
    pub certificate_path: Option<String>,
    /// PEM private key mounted from the deployment TLS Secret.
    #[serde(default, alias = "privateKeyPath")]
    pub private_key_path: Option<String>,
    /// Optional private CA bundle used to verify TLS peer certificates.
    #[serde(default, alias = "caCertificatePath")]
    pub ca_certificate_path: Option<String>,
    /// Reloads a valid rotated Secret for newly accepted connections while
    /// retaining the last valid configuration across transient updates.
    #[serde(default, alias = "autoReloadSecret")]
    pub auto_reload_secret: bool,
    #[serde(
        default = "default_tls_protocols",
        alias = "sslProtocols",
        deserialize_with = "deserialize_tls_protocols"
    )]
    pub ssl_protocols: Vec<TlsProtocol>,
}

impl Default for Tls {
    fn default() -> Self {
        Self {
            enabled: false,
            certificate_path: None,
            private_key_path: None,
            ca_certificate_path: None,
            auto_reload_secret: false,
            ssl_protocols: default_tls_protocols(),
        }
    }
}

/// Downstream TLS protocol versions accepted by the client-facing listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsProtocol {
    Tls12,
    Tls13,
}

fn default_tls_protocols() -> Vec<TlsProtocol> {
    vec![TlsProtocol::Tls13]
}

fn deserialize_tls_protocols<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<TlsProtocol>, D::Error>
where
    D: Deserializer<'de>,
{
    let configured = String::deserialize(deserializer)?;
    let mut protocols = Vec::new();
    for value in configured.split_ascii_whitespace() {
        let protocol = match value {
            "TLSv1.2" => TlsProtocol::Tls12,
            "TLSv1.3" => TlsProtocol::Tls13,
            unsupported => {
                return Err(serde::de::Error::custom(format!(
                    "unsupported downstream TLS protocol {unsupported:?}; expected TLSv1.2 or TLSv1.3"
                )));
            }
        };
        if protocols.contains(&protocol) {
            return Err(serde::de::Error::custom(format!(
                "duplicate downstream TLS protocol {value:?}"
            )));
        }
        protocols.push(protocol);
    }
    if protocols.is_empty() {
        return Err(serde::de::Error::custom(
            "at least one downstream TLS protocol is required",
        ));
    }
    Ok(protocols)
}

/// Per-backend write-through publishing settings. A `PUT` to the backend is
/// uploaded to the origin and (on success) seeds the cache so the immediate
/// consumer `GET` is a HIT. Disabled by default — a `PUT` is then a plain
/// transparent proxy with no caching.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WriteThrough {
    /// Gates write-through for this backend. When false, `PUT`s are proxied
    /// transparently (never cached).
    #[serde(default)]
    pub enabled: bool,
    /// Allows `Transfer-Encoding: chunked` uploads to be staged. Off by default,
    /// so a streamed (unknown-length) upload is rejected with `411`. `aws-chunked`
    /// / SigV4 streaming payloads are always rejected (`400`) regardless.
    #[serde(default, alias = "enableHttpChunkedUploads")]
    pub enable_http_chunked_uploads: bool,
    /// Reassembles Azure block-blob uploads (`comp=block` blocks committed by
    /// `comp=blocklist`) into the full object before seeding, so a multi-part
    /// upload still yields a consumer-`GET` HIT. Azure-only; ignored elsewhere.
    #[serde(default, alias = "assembleBlockUploads")]
    pub assemble_block_uploads: bool,
}

/// Shared write-through staging settings. The engine stages directly on each
/// backend cache volume. `internalPort` and `stagingMetadataSize` remain
/// deserializable for hand-rolled configs but have no runtime effect; the chart
/// omits them from the engine ConfigMap.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WriteThroughConfig {
    /// Accepted without binding because staging is coordinated in-process.
    #[serde(
        default = "default_write_through_internal_port",
        alias = "internalPort"
    )]
    pub internal_port: u16,
    /// Bounds each client-body read and cache-seed drain operation.
    #[serde(
        default = "default_write_through_chunk_size",
        alias = "requestBodyChunkSize"
    )]
    pub request_body_chunk_size: usize,
    /// Relative staging tree created independently below every backend cache
    /// root.
    #[serde(
        default = "default_write_through_staging_subdir",
        alias = "stagingSubdir"
    )]
    pub staging_subdir: String,
    /// Maximum age of accepted Azure block fragments before assembly rejects
    /// them.
    #[serde(
        default = "default_write_through_token_ttl",
        alias = "stagingTokenTtlSeconds"
    )]
    pub staging_token_ttl_seconds: u64,
    /// Accepted without allocating a metadata dictionary because filesystem
    /// names carry staging identity.
    #[serde(
        default = "default_write_through_metadata_size",
        alias = "stagingMetadataSize"
    )]
    pub staging_metadata_size: String,
}

impl Default for WriteThroughConfig {
    fn default() -> Self {
        Self {
            internal_port: default_write_through_internal_port(),
            request_body_chunk_size: default_write_through_chunk_size(),
            staging_subdir: default_write_through_staging_subdir(),
            staging_token_ttl_seconds: default_write_through_token_ttl(),
            staging_metadata_size: default_write_through_metadata_size(),
        }
    }
}

fn default_write_through_internal_port() -> u16 {
    13_130
}

fn default_write_through_chunk_size() -> usize {
    65_536
}

fn default_write_through_staging_subdir() -> String {
    ".ovcc-staging".to_owned()
}

fn default_write_through_token_ttl() -> u64 {
    300
}

fn default_write_through_metadata_size() -> String {
    "1024m".to_owned()
}

/// On-disk cache settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CacheConfig {
    pub root: String,
    pub max_bytes: Option<u64>,
}

/// Where the proxy binds its client-facing listener. Defaults to
/// `0.0.0.0:14128` so a containerized deployment is reachable as a Service;
/// tests bind `127.0.0.1:0` (ephemeral) by setting `port = 0`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceConfig {
    /// Trusts workstation cache reuse, binds content to loopback, and enables same-user leases.
    #[serde(default, alias = "localMode")]
    pub local_mode: bool,
    /// Internal endpoint override for isolated in-process test servers.
    #[doc(hidden)]
    #[serde(skip)]
    pub local_endpoint: Option<String>,
    #[serde(default = "default_bind_address")]
    pub address: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Dedicated Nucleus listener port. `None`/absent disables the Nucleus
    /// listener entirely. `Some(0)` requests an ephemeral port, while any other
    /// value binds that exact port and routes to the Nucleus serve arms.
    #[serde(default)]
    pub nucleus_port: Option<u16>,
    /// Dedicated peer listener. `None`/absent (or `0`) disables peer serving.
    #[serde(default, alias = "peerPort")]
    pub peer_port: Option<u16>,
    /// Dedicated plaintext health listener. Absent by default so isolated
    /// in-process tests do not claim the production health port.
    #[serde(default, alias = "healthPort")]
    pub health_port: Option<u16>,
    /// Dedicated plaintext Prometheus listener. Absent means pull export is off.
    #[serde(default, alias = "metricsPort")]
    pub metrics_port: Option<u16>,
    /// Hostnames that name this proxy itself (its own Service FQDNs). A request
    /// whose `Host` matches one is rejected with `421 Misdirected Request`,
    /// since it is not meant for any backend.
    #[serde(default)]
    pub self_names: Vec<String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            local_mode: false,
            local_endpoint: None,
            address: default_bind_address(),
            port: default_port(),
            nucleus_port: None,
            peer_port: None,
            health_port: None,
            metrics_port: None,
            self_names: Vec::new(),
        }
    }
}

/// Peer endpoint discovery. Exactly one of `dns` or `peers` may be configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PeerDiscovery {
    /// Headless Service DNS name. Resolved to pod addresses at `refresh_secs`.
    pub dns: Option<String>,
    /// Explicit peer socket addresses, primarily for tests and local runs.
    pub peers: Vec<SocketAddr>,
}

/// Peer-fill policy. The feature is inert unless explicitly enabled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerConfig {
    pub enabled: bool,
    /// Whether peer-fetched bodies populate the requester's local cache.
    #[serde(default = "default_true")]
    pub replicate: bool,
    pub discovery: PeerDiscovery,
    /// DNS name verified in the peer's TLS certificate while dialing its IP.
    #[serde(alias = "expectedServerName")]
    pub expected_server_name: Option<String>,
    /// Total peer-attempt budget before the ordinary MISS goes upstream.
    #[serde(alias = "budgetMs")]
    pub budget_ms: u64,
    #[serde(alias = "refreshSecs")]
    pub refresh_secs: u64,
    #[serde(alias = "minObjectBytes")]
    pub min_object_bytes: u64,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            replicate: true,
            discovery: PeerDiscovery::default(),
            expected_server_name: None,
            budget_ms: 100,
            refresh_secs: 30,
            // Until deployment measurements refine the cutoff, avoid probing
            // tiny objects whose origin fetch is likely cheaper than fanout.
            min_object_bytes: 1024 * 1024,
        }
    }
}

/// Cache-reset paths and response headers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CompatConfig {
    #[serde(default, alias = "useUccDefaults")]
    pub use_ucc_defaults: bool,
    #[serde(default = "default_true", alias = "emitLegacyCacheReset")]
    pub emit_legacy_cache_reset: bool,
    #[serde(default = "default_cache_reset_path", alias = "cacheResetPath")]
    pub cache_reset_path: String,
    #[serde(default = "default_instance_header", alias = "instanceHeader")]
    pub instance_header: String,
    #[serde(
        default = "default_legacy_cache_reset_path",
        alias = "legacyCacheResetPath"
    )]
    pub legacy_cache_reset_path: String,
    #[serde(
        default = "default_legacy_instance_header",
        alias = "legacyInstanceHeader"
    )]
    pub legacy_instance_header: String,
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            use_ucc_defaults: false,
            emit_legacy_cache_reset: true,
            cache_reset_path: default_cache_reset_path(),
            instance_header: default_instance_header(),
            legacy_cache_reset_path: default_legacy_cache_reset_path(),
            legacy_instance_header: default_legacy_instance_header(),
        }
    }
}

fn default_cache_reset_path() -> String {
    "/__/cache/reset".to_owned()
}

fn default_instance_header() -> String {
    "CC-Instance".to_owned()
}

fn default_legacy_cache_reset_path() -> String {
    "/__ucc/cache/reset".to_owned()
}

fn default_legacy_instance_header() -> String {
    "UCC-Instance".to_owned()
}

fn default_bind_address() -> String {
    "0.0.0.0".to_owned()
}

fn default_port() -> u16 {
    14128
}

/// Raw, on-the-wire config before the compat/normal mode is resolved. The
/// presence of `nginx` selects compat mode; otherwise the top-level nodes win.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    backends: BTreeMap<String, BackendEntry>,
    #[serde(default)]
    nginx: Option<NginxSection>,
    // `tls` and `cache` stay required (frozen contract): a missing/empty file
    // is a config error, never a silent all-defaults load.
    tls: Tls,
    cache: CacheConfig,
    // `service` is read at the top level in both modes (the bind address is an
    // OVCC-native concept, not a chart node). Defaults to 0.0.0.0:14128.
    #[serde(default)]
    service: RawServiceConfig,
    // Peer policy is top-level in both native and nginx compatibility modes.
    #[serde(default)]
    peer: PeerConfig,
    // Missing reset settings retain both primary and legacy client contracts.
    #[serde(default)]
    compat: CompatConfig,
    // `telemetry` is read at the top level in both modes (an OVCC-native concept,
    // not a chart node). Defaults apply when the block is absent.
    #[serde(default)]
    telemetry: ovcc_telemetry::TelemetryConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawServiceConfig {
    #[serde(flatten)]
    config: ServiceConfig,
    // Figment lowercases compact environment leaves independently of YAML aliases.
    #[serde(default, rename = "localmode")]
    local_mode_env: Option<bool>,
}

impl RawServiceConfig {
    fn into_runtime(mut self) -> ServiceConfig {
        if let Some(enabled) = self.local_mode_env {
            self.config.local_mode = enabled;
        }
        self.config
    }
}

/// The `nginx:` compat prefix. Only the canonical backend slice OVCC consumes
/// is read; unrelated nginx settings are ignored by serde.
#[derive(Debug, Default, Deserialize)]
struct NginxSection {
    #[serde(
        default = "default_ssl_trusted_certificate",
        rename = "sslTrustedCertificate"
    )]
    ssl_trusted_certificate: String,
    // Figment lowercases environment-variable leaves. Keep the compact env
    // spelling separate so it can override a camelCase YAML base value.
    #[serde(default, rename = "ssltrustedcertificate")]
    ssl_trusted_certificate_env: Option<String>,
    #[serde(default = "default_ssl_verify_depth", rename = "sslVerifyDepth")]
    ssl_verify_depth: usize,
    #[serde(default, rename = "sslverifydepth")]
    ssl_verify_depth_env: Option<usize>,
    #[serde(
        default = "default_proxy_connect_timeout",
        rename = "proxyConnectTimeout"
    )]
    proxy_connect_timeout: String,
    #[serde(default, rename = "proxyconnecttimeout")]
    proxy_connect_timeout_env: Option<String>,
    #[serde(default = "default_proxy_read_timeout", rename = "proxyReadTimeout")]
    proxy_read_timeout: String,
    #[serde(default, rename = "proxyreadtimeout")]
    proxy_read_timeout_env: Option<String>,
    #[serde(default, alias = "writeThrough")]
    write_through: WriteThroughConfig,
    #[serde(default)]
    backends: BTreeMap<String, BackendEntry>,
    #[serde(default, rename = "upstreamBalancer")]
    upstream_balancer: UpstreamBalancerEntry,
    #[serde(default, alias = "proxyCache")]
    proxy_cache: RawProxyCache,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UpstreamBalancerEntry {
    keepalive: UpstreamKeepalive,
    resolver: RawResolverConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawResolverConfig {
    cache: ResolverCache,
    query: ResolverQuery,
    fallback: Vec<String>,
}

impl From<RawResolverConfig> for ResolverConfig {
    fn from(raw: RawResolverConfig) -> Self {
        Self {
            cache: raw.cache,
            query: raw.query,
            fallback: raw.fallback,
        }
    }
}

fn default_ssl_trusted_certificate() -> String {
    UpstreamTls::default().trusted_certificate
}

fn default_ssl_verify_depth() -> usize {
    UpstreamTls::default().verify_depth
}

fn default_proxy_connect_timeout() -> String {
    "60s".to_owned()
}

fn default_proxy_read_timeout() -> String {
    "60s".to_owned()
}

#[derive(Debug, Deserialize)]
struct RawProxyCache {
    #[serde(default = "default_lock_age", alias = "lockAge")]
    lock_age: String,
    #[serde(default = "default_lock_timeout", alias = "lockTimeout")]
    lock_timeout: String,
    #[serde(default)]
    validity: BTreeMap<String, String>,
    #[serde(default)]
    paths: Vec<RawCachePath>,
}

impl Default for RawProxyCache {
    fn default() -> Self {
        Self {
            lock_age: default_lock_age(),
            lock_timeout: default_lock_timeout(),
            validity: BTreeMap::new(),
            paths: Vec::new(),
        }
    }
}

fn default_lock_age() -> String {
    "60s".to_owned()
}

fn default_lock_timeout() -> String {
    "120s".to_owned()
}

#[derive(Debug, Deserialize)]
struct RawCachePath {
    name: String,
    #[serde(default, alias = "maxIdleTime")]
    max_idle_time: Option<String>,
    #[serde(default, alias = "peerCopyMaxIdleTime")]
    peer_copy_max_idle_time: Option<String>,
    #[serde(default, alias = "maxSizeGi")]
    max_size_gi: Option<u64>,
    #[serde(default, alias = "minFreeBytes")]
    min_free_bytes: Option<u64>,
}

/// One backend entry in the name-keyed `backends` map. The map key is the
/// backend `name`. `enabled` (chart spelling `include`) gates inclusion;
/// `host`/`upstream` remain optional native aliases.
#[derive(Debug, Deserialize)]
struct BackendEntry {
    #[serde(default = "default_true", alias = "include")]
    enabled: bool,
    #[serde(default)]
    host: String,
    #[serde(default)]
    upstream: String,
    #[serde(default, rename = "serverName")]
    server_name: Option<String>,
    #[serde(default, rename = "proxyPass")]
    proxy_pass: Option<String>,
    #[serde(default, rename = "proxyAuthPass")]
    proxy_auth_pass: Nullable<String>,
    #[serde(default, rename = "proxyCacheName")]
    proxy_cache_name: Option<String>,
    #[serde(default, rename = "maxIdleTime")]
    max_idle_time: Option<String>,
    #[serde(default, rename = "maxidletime")]
    max_idle_time_env: Option<String>,
    #[serde(default, rename = "enableUpstreamKeepalive")]
    enable_upstream_keepalive: Option<bool>,
    #[serde(default, rename = "allowHttp2Clients")]
    allow_http2_clients: Option<bool>,
    #[serde(default, rename = "bridgeResolver")]
    bridge_resolver: Option<String>,
    #[serde(default)]
    cache_root: Option<String>,
    // Keep the YAML and compact-env spellings separate so an env override can
    // replace a camelCase base value without serde seeing duplicate aliases.
    #[serde(default, rename = "cacheRoot")]
    cache_root_compat: Option<String>,
    #[serde(default, rename = "cacheroot")]
    cache_root_env: Option<String>,
    #[serde(default = "default_true", alias = "requiresVersioning")]
    requires_versioning: bool,
    #[serde(default, alias = "burstyCache")]
    bursty: Option<ovcc_bursty::BurstyCache>,
    // Per-backend write-through policy. The chart emits it under
    // `backends.<name>.writeThrough`; absent means disabled.
    #[serde(default, alias = "writeThrough")]
    write_through: WriteThrough,
    // Whether the cache-generation reset endpoint is exposed for this backend.
    // Chart spelling `allowCacheReset`; absent means disabled (production
    // Nucleus default).
    #[serde(default, alias = "allowCacheReset")]
    allow_cache_reset: bool,
    // Registry vhost routing fields, present only for a registry backend. The
    // chart emits these flattened onto the backend entry (not under a nested
    // key), so they are read here and assembled into a `RegistryConfig`.
    #[serde(default)]
    matching: ovcc_registry::Matching,
    #[serde(default)]
    suffix: String,
    #[serde(default, alias = "cacheableUri")]
    cacheable_uri: Option<ovcc_registry::CacheableUri>,
    #[serde(default, rename = "cacheableuri")]
    cacheable_uri_env: Option<ovcc_registry::CacheableUri>,
    #[serde(default)]
    upstreams: Vec<ovcc_registry::RegistryUpstream>,
}

#[derive(Debug, Default)]
enum Nullable<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<'de, T> Deserialize<'de> for Nullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

fn default_true() -> bool {
    true
}

/// Loads configuration from `path` (YAML), then applies `OVCC_CONFIG_`-prefixed
/// env overrides. Layered via figment.
///
/// The YAML file is the base layer; `OVCC_CONFIG_`-prefixed environment
/// variables are merged on top, so a deployment can override any value without
/// editing the file. Nested keys use `_` as the path separator (e.g.
/// `OVCC_CONFIG_TLS_ENABLED` overrides `tls.enabled`). Any figment failure
/// (missing/invalid file, type mismatch, missing required field) is surfaced as
/// [`ovcc_core::Error::config`].
///
/// The `OVCC_CONFIG_` prefix is deliberately narrower than `OVCC_`: a Service
/// named `ovcc` makes the kubelet inject service-link vars (`OVCC_SERVICE_PORT`,
/// `OVCC_PORT_…`, …) that a bare `OVCC_` prefix would read as config and
/// corrupt. None of those start with `OVCC_CONFIG_`, so figment ignores them; a
/// chart guard rejects any release name that would reintroduce the clash.
///
/// Backends are read from `nginx.backends` when an `nginx:` node is present
/// (compat mode) or from the top-level `backends` otherwise (normal mode); the
/// name-keyed map is flattened to [`Config::backends`], keeping only entries
/// whose `enabled` flag is set. See the module docs for the mode rule.
pub fn load(path: &Path) -> Result<Config> {
    let raw: RawConfig = Figment::new()
        .merge(Yaml::file(path))
        .merge(Env::prefixed("OVCC_CONFIG_").split('_'))
        .extract()
        .map_err(|err| Error::config(err.to_string()))?;

    // Compat mode (nginx node present) reads backends under the prefix and
    // ignores any top-level copies; normal mode reads the top level.
    let client_scheme = if raw.tls.enabled { "https" } else { "http" };
    let write_through = raw
        .nginx
        .as_ref()
        .map(|nginx| nginx.write_through.clone())
        .unwrap_or_default();
    let upstream_tls = raw
        .nginx
        .as_ref()
        .map(|nginx| UpstreamTls {
            trusted_certificate: nginx
                .ssl_trusted_certificate_env
                .clone()
                .unwrap_or_else(|| nginx.ssl_trusted_certificate.clone()),
            verify_depth: nginx.ssl_verify_depth_env.unwrap_or(nginx.ssl_verify_depth),
        })
        .unwrap_or_default();
    let origin_timeouts = raw
        .nginx
        .as_ref()
        .map(|nginx| {
            let connect = nginx
                .proxy_connect_timeout_env
                .as_deref()
                .unwrap_or(&nginx.proxy_connect_timeout);
            let read = nginx
                .proxy_read_timeout_env
                .as_deref()
                .unwrap_or(&nginx.proxy_read_timeout);
            let connect = parse_nonzero_duration(connect, "nginx.proxyConnectTimeout")?;
            let read = parse_nonzero_duration(read, "nginx.proxyReadTimeout")?;
            Ok::<_, Error>(OriginTimeouts {
                connect,
                response_header: read,
                body_stall: read,
            })
        })
        .transpose()?
        .unwrap_or_default();
    validate_write_through(&write_through)?;
    let service = raw.service.into_runtime();
    if service.local_mode && raw.peer.enabled {
        return Err(Error::config(
            "service.localMode cannot be combined with peer.enabled",
        ));
    }
    validate_peer(&raw.peer, raw.tls.enabled)?;
    let (entries, passthrough, upstream_balancer, proxy_cache) = match raw.nginx {
        Some(nginx) => {
            let mut entries = nginx.backends;
            let passthrough = entries
                .remove("passthrough")
                .map(|entry| parse_passthrough(&entry, client_scheme))
                .transpose()?
                .unwrap_or_default();
            (
                entries,
                passthrough,
                UpstreamBalancer {
                    keepalive: nginx.upstream_balancer.keepalive,
                    resolver: nginx.upstream_balancer.resolver.into(),
                },
                nginx.proxy_cache,
            )
        }
        None => (
            raw.backends,
            Passthrough::default(),
            UpstreamBalancer::default(),
            RawProxyCache::default(),
        ),
    };
    let validity_200_secs = parse_validity(&proxy_cache.validity, "200")?.unwrap_or(86_400);
    let validity_206_secs = parse_validity(&proxy_cache.validity, "206")?.unwrap_or(86_400);
    let lock_age_secs = parse_duration(&proxy_cache.lock_age)?;
    let lock_timeout_secs = parse_duration(&proxy_cache.lock_timeout)?;
    let paths: BTreeMap<_, _> = proxy_cache
        .paths
        .into_iter()
        .map(|path| (path.name.clone(), path))
        .collect();
    let backends = entries
        .into_iter()
        .filter(|(_, entry)| entry.enabled)
        .map(|(name, entry)| -> Result<Backend> {
            validate_transport_policy(&name, &entry)?;
            let canonical_route_present = entry.server_name.is_some()
                || entry.proxy_pass.is_some()
                || !matches!(&entry.proxy_auth_pass, Nullable::Missing)
                || entry.proxy_cache_name.is_some()
                || entry.enable_upstream_keepalive.is_some()
                || entry.allow_http2_clients.is_some()
                || entry.bridge_resolver.is_some();
            let allow_http2_clients = entry
                .allow_http2_clients
                .unwrap_or_else(|| canonical_http2_default(&name));
            // A backend is a registry vhost router when it declares upstreams;
            // otherwise registry routing is off and it uses the cloud/exact host
            // rules.
            let cacheable_uri = entry.cacheable_uri_env.or(entry.cacheable_uri);
            if cacheable_uri.is_some() && entry.upstreams.is_empty() {
                return Err(Error::config(format!(
                    "backends.{name}.cacheableUri requires registry upstreams"
                )));
            }
            let registry = (!entry.upstreams.is_empty()).then_some(ovcc_registry::RegistryConfig {
                matching: entry.matching,
                suffix: entry.suffix,
                cacheable_uri,
                upstreams: entry.upstreams,
            });
            let server_name = entry
                .server_name
                .filter(|value| !value.trim().is_empty())
                .map(ServerName::parse)
                .transpose()
                .map_err(Error::config)?;
            let proxy_pass = entry.proxy_pass.filter(|value| !value.trim().is_empty());
            if server_name.is_some() && proxy_pass.is_none() && entry.upstream.trim().is_empty() {
                return Err(Error::config(format!(
                    "nginx.backends.{name}.serverName requires a supported proxyPass"
                )));
            }
            if let Some(template) = proxy_pass.as_deref() {
                validate_proxy_pass(
                    &name,
                    "proxyPass",
                    template,
                    server_name.as_ref(),
                    registry.is_some(),
                )?;
            }
            let proxy_auth_pass = match entry.proxy_auth_pass {
                Nullable::Missing => proxy_pass.clone().or_else(|| {
                    (!entry.upstream.trim().is_empty()).then(|| entry.upstream.clone())
                }),
                Nullable::Null => {
                    if name != "nucleus" {
                        return Err(Error::config(format!(
                            "nginx.backends.{name}.proxyAuthPass cannot be null: cache HIT authorization probes must remain enabled"
                        )));
                    }
                    None
                }
                Nullable::Value(value) => {
                    validate_proxy_pass(
                        &name,
                        "proxyAuthPass",
                        &value,
                        server_name.as_ref(),
                        registry.is_some(),
                    )?;
                    Some(value)
                }
            };
            if entry
                .proxy_cache_name
                .as_deref()
                .is_some_and(|zone| zone.trim().is_empty())
            {
                return Err(Error::config(format!(
                    "nginx.backends.{name}.proxyCacheName cannot be empty"
                )));
            }
            let cache_root = entry
                .cache_root_env
                .or(entry.cache_root_compat)
                .or(entry.cache_root)
                .or_else(|| {
                    entry
                        .proxy_cache_name
                        .as_ref()
                        .filter(|zone| !zone.trim().is_empty())
                        .map(|zone| {
                            Path::new(&raw.cache.root)
                                .join(zone)
                                .to_string_lossy()
                                .into_owned()
                        })
                });
            // Retention looks up `nginx.proxyCache.paths` by the configured zone,
            // falling back to the backend name when the zone is omitted.
            let cache_path_name = entry
                .proxy_cache_name
                .as_deref()
                .filter(|zone| !zone.trim().is_empty())
                .unwrap_or(name.as_str());
            let cache_path = paths.get(cache_path_name);
            let max_idle_secs = cache_path
                .and_then(|path| path.max_idle_time.as_deref())
                .map(parse_duration)
                .transpose()?
                .or(Some(86_400));
            let peer_copy_max_idle_secs = cache_path
                .and_then(|path| path.peer_copy_max_idle_time.as_deref())
                .map(parse_duration)
                .transpose()?
                .or(Some(14_400));
            let backend_max_idle_secs = entry
                .max_idle_time_env
                .or(entry.max_idle_time)
                .as_deref()
                .map(parse_duration)
                .transpose()?;
            let cache_policy = BackendCachePolicy {
                validity_200_secs,
                validity_206_secs,
                max_idle_secs,
                peer_copy_max_idle_secs,
                backend_max_idle_secs,
                max_bytes: cache_path
                    .and_then(|path| path.max_size_gi)
                    .map(|gib| gib.saturating_mul(1024 * 1024 * 1024)),
                min_free_bytes: cache_path.and_then(|path| path.min_free_bytes),
                // Nucleus uses shorter effective limits on its dedicated
                // listener; other backends consume the global cache settings.
                lock_age_secs: if name == "nucleus" { 10 } else { lock_age_secs },
                lock_timeout_secs: if name == "nucleus" {
                    3
                } else {
                    lock_timeout_secs
                },
            };
            Ok(Backend {
                name,
                host: entry.host,
                upstream: entry.upstream,
                cache_root,
                cache_policy,
                requires_versioning: entry.requires_versioning,
                bursty: entry.bursty,
                registry,
                write_through: entry.write_through,
                allow_cache_reset: entry.allow_cache_reset,
                route: if canonical_route_present {
                    BackendRoute {
                        server_name,
                        proxy_pass,
                        proxy_auth_pass,
                        proxy_cache_name: entry.proxy_cache_name,
                        enable_upstream_keepalive: entry
                            .enable_upstream_keepalive
                            .unwrap_or(true),
                        allow_http2_clients,
                        resolver_override: entry
                            .bridge_resolver
                            .filter(|value| !value.trim().is_empty()),
                        scheme: client_scheme.to_owned(),
                    }
                } else {
                    BackendRoute::default()
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Config {
        backends,
        passthrough,
        tls: raw.tls,
        cache: raw.cache,
        write_through,
        service,
        peer: raw.peer,
        compat: raw.compat,
        telemetry: raw.telemetry,
        upstream_balancer,
        upstream_tls,
        origin_timeouts,
    })
}

fn validate_peer(peer: &PeerConfig, tls_enabled: bool) -> Result<()> {
    let dns = peer
        .discovery
        .dns
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if peer.discovery.dns.is_some() && dns.is_none() {
        return Err(Error::config("peer.discovery.dns cannot be empty"));
    }
    if dns.is_some() && !peer.discovery.peers.is_empty() {
        return Err(Error::config(
            "peer.discovery must configure either dns or peers, not both",
        ));
    }
    if peer.enabled && dns.is_none() && peer.discovery.peers.is_empty() {
        return Err(Error::config(
            "peer.enabled requires peer.discovery.dns or peer.discovery.peers",
        ));
    }
    for (name, value) in [
        ("budgetMs", peer.budget_ms),
        ("refreshSecs", peer.refresh_secs),
    ] {
        if value == 0 {
            return Err(Error::config(format!(
                "peer.{name} must be greater than zero"
            )));
        }
    }
    if peer
        .expected_server_name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(Error::config("peer.expectedServerName cannot be empty"));
    }
    if peer.enabled && tls_enabled && peer.expected_server_name.is_none() {
        return Err(Error::config(
            "peer.expectedServerName is required when peer fill uses TLS",
        ));
    }
    Ok(())
}

fn parse_validity(values: &BTreeMap<String, String>, status: &str) -> Result<Option<u64>> {
    values
        .get(status)
        .map(|value| parse_duration(value))
        .transpose()
}

fn parse_duration(value: &str) -> Result<u64> {
    let split = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number = number
        .parse::<u64>()
        .map_err(|_| Error::config(format!("invalid cache duration {value:?}")))?;
    let multiplier = match unit {
        "s" | "" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(Error::config(format!("invalid cache duration {value:?}"))),
    };
    Ok(number.saturating_mul(multiplier))
}

fn parse_nonzero_duration(value: &str, field: &str) -> Result<Duration> {
    let seconds = parse_duration(value).map_err(|_| {
        Error::config(format!(
            "{field} must be a duration using s, m, h, or d; got {value:?}"
        ))
    })?;
    if seconds == 0 {
        return Err(Error::config(format!("{field} must be greater than zero")));
    }
    Ok(Duration::from_secs(seconds))
}

fn validate_write_through(config: &WriteThroughConfig) -> Result<()> {
    if config.request_body_chunk_size == 0 {
        return Err(Error::config(
            "nginx.writeThrough.requestBodyChunkSize must be greater than zero",
        ));
    }
    let subdir = Path::new(&config.staging_subdir);
    if config.staging_subdir.trim().is_empty()
        || subdir.is_absolute()
        || subdir.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(Error::config(
            "nginx.writeThrough.stagingSubdir must be a relative path below each backend cache root",
        ));
    }
    Ok(())
}

fn parse_passthrough(entry: &BackendEntry, scheme: &str) -> Result<Passthrough> {
    validate_transport_policy("passthrough", entry)?;
    if entry.allow_cache_reset {
        return Err(Error::config(
            "nginx.backends.passthrough.allowCacheReset=true is not supported; keep the canonical false default",
        ));
    }
    let proxy_pass = entry
        .proxy_pass
        .clone()
        .unwrap_or_else(|| "http://$http_host".to_owned());
    validate_proxy_pass("passthrough", "proxyPass", &proxy_pass, None, false)?;
    Ok(Passthrough {
        proxy_pass,
        scheme: scheme.to_owned(),
    })
}

fn canonical_http2_default(name: &str) -> bool {
    !matches!(name, "nucleus")
}

fn validate_transport_policy(name: &str, entry: &BackendEntry) -> Result<()> {
    if entry.bridge_resolver.is_some() && name != "nucleus" {
        return Err(Error::config(format!(
            "nginx.backends.{name}.bridgeResolver is only valid for the nucleus backend"
        )));
    }
    if let Some(configured) = entry.allow_http2_clients {
        let canonical = canonical_http2_default(name);
        if configured != canonical {
            return Err(Error::config(format!(
                "nginx.backends.{name}.allowHttp2Clients={configured} is not supported; keep the canonical default {canonical}"
            )));
        }
    }
    Ok(())
}

fn validate_proxy_pass(
    backend: &str,
    field: &str,
    template: &str,
    server_name: Option<&ServerName>,
    registry: bool,
) -> Result<()> {
    let mut captures = BTreeMap::new();
    if let Some(ServerName {
        matcher: ServerNameMatcher::Regex(regex),
        ..
    }) = server_name
    {
        for name in regex.capture_names().flatten() {
            captures.insert(name.to_owned(), "capture.example".to_owned());
        }
    }
    if registry {
        captures.insert("upstream_host".to_owned(), "upstream.example".to_owned());
    }
    if render_proxy_pass(
        template,
        "https",
        "host.example",
        "host.example:443",
        &captures,
    )
    .is_none()
    {
        return Err(Error::config(format!(
            "nginx.backends.{backend}.{field}={template:?} is an unsupported proxy expression; use http(s) with $scheme, $host, $http_host, or a named serverName capture"
        )));
    }
    Ok(())
}

#[cfg(test)]
// `Jail::expect_with` forces the closure to return `Result<_, figment::Error>`,
// whose large `Err` variant trips `result_large_err`; it is the test harness's
// type, not ours, so the lint is irrelevant here.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;
    use ovcc_core::ErrorKind;

    // In normal mode, backends are a name-keyed map at the top level.
    const SAMPLE_YAML: &str = "\
backends:
  usd-assets:
    enabled: true
    host: assets.example.com
    upstream: https://origin.example.com
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
  max_bytes: 1073741824
";

    fn sample_config() -> Config {
        Config {
            backends: vec![Backend {
                name: "usd-assets".to_owned(),
                host: "assets.example.com".to_owned(),
                upstream: "https://origin.example.com".to_owned(),
                cache_root: None,
                cache_policy: BackendCachePolicy::default(),
                requires_versioning: true,
                bursty: None,
                registry: None,
                // SAMPLE_YAML omits `writeThrough`, so it defaults to disabled.
                write_through: WriteThrough::default(),
                allow_cache_reset: false,
                route: BackendRoute::default(),
            }],
            passthrough: Passthrough::default(),
            tls: Tls::default(),
            cache: CacheConfig {
                root: "/var/cache/ovcc".to_owned(),
                max_bytes: Some(1_073_741_824),
            },
            write_through: WriteThroughConfig::default(),
            // SAMPLE_YAML omits `service`, so the default bind applies.
            service: ServiceConfig::default(),
            // Peer fill is an explicit opt-in.
            peer: PeerConfig::default(),
            // Reset compatibility uses its defaults when omitted.
            compat: CompatConfig::default(),
            // SAMPLE_YAML omits `telemetry`, so defaults apply.
            telemetry: ovcc_telemetry::TelemetryConfig::default(),
            upstream_balancer: UpstreamBalancer::default(),
            upstream_tls: UpstreamTls::default(),
            origin_timeouts: OriginTimeouts::default(),
        }
    }

    #[test]
    fn general_get_local_access_config_defaults_and_override() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;
            jail.create_file(
                "local.yaml",
                &format!("service:\n  localMode: true\n{SAMPLE_YAML}"),
            )?;
            // Act.
            let defaulted = load(Path::new("values.yaml"))
                .map_err(|error| figment::Error::from(error.to_string()))?;
            let configured = load(Path::new("local.yaml"))
                .map_err(|error| figment::Error::from(error.to_string()))?;
            jail.set_env("OVCC_CONFIG_SERVICE_LOCALMODE", "false");
            let overridden = load(Path::new("local.yaml"))
                .map_err(|error| figment::Error::from(error.to_string()))?;
            // Assert.
            assert!(!defaulted.service.local_mode);
            assert!(configured.service.local_mode);
            assert!(!overridden.service.local_mode);
            assert!(configured.service.local_endpoint.is_none());
            Ok(())
        });
    }

    #[test]
    fn general_get_local_access_rejects_non_boolean_and_hides_endpoint() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "bad.yaml",
                &format!("service:\n  localMode: invalid\n{SAMPLE_YAML}"),
            )?;
            jail.create_file("compat.yaml", &format!("service:\n  localMode: true\n  localEndpoint: ignored\nnginx: {{}}\n{SAMPLE_YAML}"))?;
            // Act.
            let invalid = load(Path::new("bad.yaml"));
            let compat = load(Path::new("compat.yaml"))
                .map_err(|error| figment::Error::from(error.to_string()))?;
            // Assert.
            assert!(invalid.is_err());
            assert!(compat.service.local_mode);
            assert!(compat.service.local_endpoint.is_none());
            Ok(())
        });
    }

    #[test]
    fn general_get_local_mode_rejects_cooperative_peers() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", "service:\n  localMode: true\npeer:\n  enabled: true\ntls: {enabled: false}\ncache: {root: cache}\n")?;
            // Act.
            let result = load(Path::new("values.yaml"));
            // Assert.
            assert!(result.is_err_and(|e| e.message().contains("localMode cannot be combined")));
            Ok(())
        });
    }

    #[test]
    fn normal_mode_reads_top_level_backends() {
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert_eq!(cfg, sample_config());
            Ok(())
        });
    }

    #[test]
    fn tls_load_protocols_defaults_to_tls_1_3() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");

            // Assert.
            assert_eq!(cfg.tls.ssl_protocols, vec![TlsProtocol::Tls13]);
            Ok(())
        });
    }

    #[test]
    fn tls_load_protocols_accepts_tls_1_2_and_tls_1_3() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
tls:
  enabled: true
  sslProtocols: \"TLSv1.2 TLSv1.3\"
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");

            // Assert.
            assert_eq!(
                cfg.tls.ssl_protocols,
                vec![TlsProtocol::Tls12, TlsProtocol::Tls13]
            );
            Ok(())
        });
    }

    #[test]
    fn tls_secret_contract_accepts_snake_and_camel_case() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "snake.yaml",
                "\
backends: {}
tls:
  enabled: true
  certificate_path: /snake/tls.crt
  private_key_path: /snake/tls.key
  ca_certificate_path: /snake/ca.crt
  auto_reload_secret: true
cache:
  root: /var/cache/ovcc
",
            )?;
            jail.create_file(
                "camel.yaml",
                "\
backends: {}
tls:
  enabled: true
  certificatePath: /camel/tls.crt
  privateKeyPath: /camel/tls.key
  caCertificatePath: /camel/ca.crt
  autoReloadSecret: true
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let snake = load(Path::new("snake.yaml")).expect("snake config should load");
            let camel = load(Path::new("camel.yaml")).expect("camel config should load");

            // Assert.
            assert_eq!(
                snake.tls.certificate_path.as_deref(),
                Some("/snake/tls.crt")
            );
            assert_eq!(
                snake.tls.private_key_path.as_deref(),
                Some("/snake/tls.key")
            );
            assert_eq!(
                snake.tls.ca_certificate_path.as_deref(),
                Some("/snake/ca.crt")
            );
            assert!(snake.tls.auto_reload_secret);
            assert_eq!(
                camel.tls.certificate_path.as_deref(),
                Some("/camel/tls.crt")
            );
            assert_eq!(
                camel.tls.private_key_path.as_deref(),
                Some("/camel/tls.key")
            );
            assert_eq!(
                camel.tls.ca_certificate_path.as_deref(),
                Some("/camel/ca.crt")
            );
            assert!(camel.tls.auto_reload_secret);
            Ok(())
        });
    }

    #[test]
    fn tls_secret_contract_defaults_to_test_safe_self_signed_mode() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config should load");

            // Assert.
            assert_eq!(cfg.tls.certificate_path, None);
            assert_eq!(cfg.tls.private_key_path, None);
            assert_eq!(cfg.tls.ca_certificate_path, None);
            assert!(!cfg.tls.auto_reload_secret);
            Ok(())
        });
    }

    #[test]
    fn peer_config_defaults_to_disabled_safe_limits() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config should load");

            // Assert.
            assert_eq!(cfg.peer, PeerConfig::default());
            assert!(!cfg.peer.enabled);
            assert!(cfg.peer.replicate);
            assert_eq!(cfg.peer.budget_ms, 100);
            Ok(())
        });
    }

    #[test]
    fn peer_config_accepts_camel_case_dns_and_service_port() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends: {}
service:
  peerPort: 14130
peer:
  enabled: true
  replicate: false
  discovery:
    dns: ovcc-headless.default.svc.cluster.local
  expectedServerName: ovcc-headless.default.svc.cluster.local
  budgetMs: 51
  refreshSecs: 17
  minObjectBytes: 4096
tls:
  enabled: true
  caCertificatePath: /etc/ovcc/tls/ca.crt
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config should load");

            // Assert.
            assert_eq!(cfg.service.peer_port, Some(14_130));
            assert!(cfg.peer.enabled);
            assert!(!cfg.peer.replicate);
            assert_eq!(
                cfg.peer.discovery.dns.as_deref(),
                Some("ovcc-headless.default.svc.cluster.local")
            );
            assert!(cfg.peer.discovery.peers.is_empty());
            assert_eq!(cfg.peer.budget_ms, 51);
            assert_eq!(cfg.peer.refresh_secs, 17);
            assert_eq!(cfg.peer.min_object_bytes, 4096);
            assert_eq!(
                cfg.tls.ca_certificate_path.as_deref(),
                Some("/etc/ovcc/tls/ca.crt")
            );
            Ok(())
        });
    }

    #[test]
    fn peer_config_rejects_removed_serve_limit() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
peer:
  maxConcurrentServes: 8
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let error = load(Path::new("values.yaml")).expect_err("removed key must fail");

            // Assert.
            assert!(error.to_string().contains("maxConcurrentServes"));
            Ok(())
        });
    }

    #[test]
    fn peer_config_accepts_explicit_socket_addresses() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
peer:
  enabled: true
  discovery:
    peers:
      - 127.0.0.1:14130
      - '[2001:db8::1]:14130'
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config should load");

            // Assert.
            assert_eq!(
                cfg.peer.discovery.peers,
                vec![
                    "127.0.0.1:14130".parse().expect("IPv4 peer"),
                    "[2001:db8::1]:14130".parse().expect("IPv6 peer")
                ]
            );
            Ok(())
        });
    }

    #[test]
    fn peer_config_rejects_zero_budget() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
peer:
  budgetMs: 0
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let result = load(Path::new("values.yaml"));

            // Assert.
            assert!(
                result
                    .expect_err("zero peer budget should fail")
                    .to_string()
                    .contains("peer.budgetMs must be greater than zero")
            );
            Ok(())
        });
    }

    #[test]
    fn peer_config_rejects_ambiguous_or_missing_discovery() {
        // Arrange.
        Jail::expect_with(|jail| {
            let base = "\
backends: {}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
";
            jail.create_file(
                "ambiguous.yaml",
                &format!(
                    "peer:\n  enabled: true\n  discovery:\n    dns: peer.test\n    peers: [127.0.0.1:14130]\n{base}"
                ),
            )?;
            jail.create_file("missing.yaml", &format!("peer:\n  enabled: true\n{base}"))?;

            // Act.
            let ambiguous = load(Path::new("ambiguous.yaml"));
            let missing = load(Path::new("missing.yaml"));

            // Assert.
            assert!(
                ambiguous
                    .expect_err("ambiguous discovery should fail")
                    .to_string()
                    .contains("either dns or peers")
            );
            assert!(
                missing
                    .expect_err("enabled peer config needs discovery")
                    .to_string()
                    .contains("requires peer.discovery")
            );
            Ok(())
        });
    }

    #[test]
    fn peer_config_requires_expected_server_name_for_tls() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
peer:
  enabled: true
  discovery:
    dns: peer.test
tls:
  enabled: true
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let result = load(Path::new("values.yaml"));

            // Assert.
            assert!(
                result
                    .expect_err("TLS peers need a pinned ServerName")
                    .to_string()
                    .contains("expectedServerName is required")
            );
            Ok(())
        });
    }

    #[test]
    fn tls_load_protocols_rejects_unsupported_version() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
tls:
  enabled: true
  sslProtocols: \"TLSv1.1 TLSv1.3\"
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let result = load(Path::new("values.yaml"));

            // Assert.
            let error = result.expect_err("unsupported protocol should fail");
            assert!(
                error
                    .to_string()
                    .contains("unsupported downstream TLS protocol")
            );
            Ok(())
        });
    }

    #[test]
    fn compat_mode_reads_nginx_backends() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  sslTrustedCertificate: /etc/ovcc/origin-ca.pem
  sslVerifyDepth: 5
  proxyConnectTimeout: 7s
  proxyReadTimeout: 11s
  upstreamBalancer:
    keepalive:
      idleTimeoutSecs: 17
      maxRequestsPerConnection: 0
      maxCachedConnectionsPerWorker: 23
    resolver:
      cache:
        successTtlSecs: 31
        failureTtlSecs: 11
      query:
        timeoutMs: 700
        retries: 2
        random: false
      fallback: [192.0.2.53]
  backends:
    s3:
      enabled: true
      host: bucket.example.com
      upstream: https://origin.example.com
tls:
  enabled: true
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert_eq!(
                cfg.backends,
                vec![Backend {
                    name: "s3".to_owned(),
                    host: "bucket.example.com".to_owned(),
                    upstream: "https://origin.example.com".to_owned(),
                    cache_root: None,
                    cache_policy: BackendCachePolicy::default(),
                    requires_versioning: true,
                    bursty: None,
                    registry: None,
                    write_through: WriteThrough::default(),
                    allow_cache_reset: false,
                    route: BackendRoute::default(),
                }]
            );
            assert!(cfg.tls.enabled);
            assert_eq!(cfg.cache.root, "/var/cache/ovcc");
            assert_eq!(
                cfg.upstream_tls,
                UpstreamTls {
                    trusted_certificate: "/etc/ovcc/origin-ca.pem".to_owned(),
                    verify_depth: 5,
                }
            );
            assert_eq!(cfg.upstream_balancer.keepalive.idle_timeout_secs, 17);
            assert_eq!(cfg.upstream_balancer.keepalive.max_cached_connections, 23);
            assert_eq!(cfg.upstream_balancer.resolver.cache.success_ttl_secs, 31);
            assert_eq!(cfg.upstream_balancer.resolver.cache.failure_ttl_secs, 11);
            assert_eq!(cfg.upstream_balancer.resolver.query.timeout_ms, 700);
            assert_eq!(cfg.upstream_balancer.resolver.query.retries, 2);
            assert!(!cfg.upstream_balancer.resolver.query.random);
            assert_eq!(cfg.upstream_balancer.resolver.fallback, vec!["192.0.2.53"]);
            assert_eq!(cfg.origin_timeouts.connect, Duration::from_secs(7));
            assert_eq!(cfg.origin_timeouts.response_header, Duration::from_secs(11));
            assert_eq!(cfg.origin_timeouts.body_stall, Duration::from_secs(11));
            Ok(())
        });
    }

    #[test]
    fn upstream_security_and_dns_policy_default_safely() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx: {}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;

            let cfg = load(Path::new("values.yaml")).expect("defaults should load");

            assert_eq!(cfg.upstream_tls, UpstreamTls::default());
            assert_eq!(cfg.upstream_tls.verify_depth, 2);
            assert_eq!(cfg.origin_timeouts, OriginTimeouts::default());
            assert!(cfg.upstream_balancer.resolver.fallback.is_empty());
            Ok(())
        });
    }

    #[test]
    fn upstream_security_accepts_environment_overrides() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  sslTrustedCertificate: /etc/ovcc/file-ca.pem
  sslVerifyDepth: 2
  proxyConnectTimeout: 17s
  proxyReadTimeout: 19s
  upstreamBalancer:
    resolver:
      fallback: [192.0.2.53]
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            jail.set_env(
                "OVCC_CONFIG_NGINX_SSLTRUSTEDCERTIFICATE",
                "/etc/ovcc/env-ca.pem",
            );
            jail.set_env("OVCC_CONFIG_NGINX_SSLVERIFYDEPTH", "7");
            jail.set_env("OVCC_CONFIG_NGINX_PROXYCONNECTTIMEOUT", "29s");
            jail.set_env("OVCC_CONFIG_NGINX_PROXYREADTIMEOUT", "31s");

            let cfg = load(Path::new("values.yaml")).expect("env overrides should load");

            assert_eq!(cfg.upstream_tls.trusted_certificate, "/etc/ovcc/env-ca.pem");
            assert_eq!(cfg.upstream_tls.verify_depth, 7);
            assert_eq!(cfg.origin_timeouts.connect, Duration::from_secs(29));
            assert_eq!(cfg.origin_timeouts.response_header, Duration::from_secs(31));
            assert_eq!(cfg.origin_timeouts.body_stall, Duration::from_secs(31));
            assert_eq!(cfg.upstream_balancer.resolver.fallback, vec!["192.0.2.53"]);
            Ok(())
        });
    }

    #[test]
    fn upstream_timeouts_reject_zero_and_invalid_durations() {
        Jail::expect_with(|jail| {
            for (path, field, value) in [
                ("zero-connect.yaml", "proxyConnectTimeout", "0s"),
                ("invalid-read.yaml", "proxyReadTimeout", "forever"),
            ] {
                jail.create_file(
                    path,
                    &format!(
                        "nginx:\n  {field}: {value}\ntls:\n  enabled: false\ncache:\n  root: /var/cache/ovcc\n"
                    ),
                )?;

                let error = load(Path::new(path)).expect_err("invalid timeout must fail");

                assert!(error.to_string().contains(&format!("nginx.{field}")));
            }
            Ok(())
        });
    }

    #[test]
    fn upstream_security_accepts_zero_and_rejects_negative_verify_depth() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "zero.yaml",
                "\
nginx:
  sslVerifyDepth: 0
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            jail.create_file(
                "negative.yaml",
                "\
nginx:
  sslVerifyDepth: -1
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;

            let config =
                load(Path::new("zero.yaml")).expect("a direct-root chain needs depth zero");
            let error = load(Path::new("negative.yaml")).expect_err("negative depth must fail");

            assert_eq!(config.upstream_tls.verify_depth, 0);
            assert!(error.to_string().contains("sslVerifyDepth"));
            Ok(())
        });
    }

    #[test]
    fn nginx_presence_forces_compat_and_ignores_top_level() {
        Jail::expect_with(|jail| {
            // With both present, only nginx.backends is used (compat is exclusive).
            jail.create_file(
                "values.yaml",
                "\
backends:
  ignored:
    host: ignored.example.com
    upstream: https://ignored.example.com
nginx:
  backends:
    s3:
      host: bucket.example.com
      upstream: https://origin.example.com
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert_eq!(cfg.backends.len(), 1, "top-level backends must be ignored");
            assert_eq!(cfg.backends[0].name, "s3");
            Ok(())
        });
    }

    #[test]
    fn disabled_backend_is_excluded() {
        Jail::expect_with(|jail| {
            // `enabled: false` and the `include: false` alias both drop entries;
            // a default (no flag) entry stays enabled.
            jail.create_file(
                "values.yaml",
                "\
backends:
  off_explicit:
    enabled: false
    host: a.example.com
  off_alias:
    include: false
    host: b.example.com
  on_default:
    host: c.example.com
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            let names: Vec<_> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
            assert_eq!(names, vec!["on_default"]);
            Ok(())
        });
    }

    #[test]
    fn env_override_targets_active_root() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends:
    s3:
      enabled: true
      host: bucket.example.com
      upstream: https://origin.example.com
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            // Override a compat-rooted value to disable the s3 backend.
            jail.set_env("OVCC_CONFIG_NGINX_BACKENDS_S3_ENABLED", "false");
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert!(
                cfg.backends.is_empty(),
                "OVCC_CONFIG_NGINX_BACKENDS_S3_ENABLED should disable the backend"
            );
            Ok(())
        });
    }

    #[test]
    fn backend_cache_root_parses_normal_and_compat_modes() {
        Jail::expect_with(|jail| {
            // Arrange.
            jail.create_file(
                "normal.yaml",
                "\
backends:
  s3:
    cache_root: /proxy_cache_s3/rust
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;

            // Act.
            let normal = load(Path::new("normal.yaml")).expect("normal config loads");

            // Assert.
            assert_eq!(
                normal.backends[0].cache_root.as_deref(),
                Some("/proxy_cache_s3/rust")
            );

            // Arrange.
            jail.create_file(
                "compat.yaml",
                "\
nginx:
  backends:
    azure:
      cacheRoot: /proxy_cache_azure/rust
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;

            // Act.
            let compat = load(Path::new("compat.yaml")).expect("compat config loads");

            // Assert.
            assert_eq!(
                compat.backends[0].cache_root.as_deref(),
                Some("/proxy_cache_azure/rust")
            );
            Ok(())
        });
    }

    #[test]
    fn backend_cache_root_preserves_missing_and_empty_for_proxy_fallback() {
        Jail::expect_with(|jail| {
            // Arrange.
            jail.create_file(
                "values.yaml",
                "\
backends:
  missing: {}
  empty:
    cacheRoot: \"\"
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            let empty = cfg.backends.iter().find(|b| b.name == "empty").unwrap();
            let missing = cfg.backends.iter().find(|b| b.name == "missing").unwrap();

            // Assert.
            assert_eq!(empty.cache_root.as_deref(), Some(""));
            assert_eq!(missing.cache_root, None);
            Ok(())
        });
    }

    #[test]
    fn backend_cache_root_accepts_compact_env_override() {
        Jail::expect_with(|jail| {
            // Arrange.
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends:
    s3:
      cacheRoot: /from-yaml
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;
            // `_` is Figment's path separator, so the compact leaf spelling
            // keeps the override at the backend-map level.
            jail.set_env(
                "OVCC_CONFIG_NGINX_BACKENDS_S3_CACHEROOT",
                "/from-environment",
            );

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config loads");

            // Assert.
            assert_eq!(
                cfg.backends[0].cache_root.as_deref(),
                Some("/from-environment")
            );
            Ok(())
        });
    }

    #[test]
    fn proxy_cache_policy_reads_canonical_validity_and_path_limits() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  proxyCache:
    lockAge: 45s
    lockTimeout: 90s
    validity:
      \"200\": 1d
      \"206\": 12h
    paths:
      - name: s3
        maxIdleTime: 6h
        peerCopyMaxIdleTime: 2h
        maxSizeGi: 90
        minFreeBytes: 1073741824
  backends:
    s3:
      proxyCacheName: s3
      maxIdleTime: 30m
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("canonical policy loads");
            let policy = &cfg.backends[0].cache_policy;

            // Assert.
            assert_eq!(policy.validity_200_secs, 86_400);
            assert_eq!(policy.validity_206_secs, 43_200);
            assert_eq!(policy.max_idle_secs, Some(21_600));
            assert_eq!(policy.peer_copy_max_idle_secs, Some(7_200));
            assert_eq!(policy.backend_max_idle_secs, Some(1_800));
            assert_eq!(policy.max_bytes, Some(90 * 1024 * 1024 * 1024));
            assert_eq!(policy.min_free_bytes, Some(1_073_741_824));
            assert_eq!(policy.lock_age_secs, 45);
            assert_eq!(policy.lock_timeout_secs, 90);
            Ok(())
        });
    }

    #[test]
    fn backend_max_idle_time_accepts_compact_env_override() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends:
    azure:
      maxIdleTime: 30m
tls:
  enabled: false
cache:
  root: /fallback
",
            )?;
            jail.set_env("OVCC_CONFIG_NGINX_BACKENDS_AZURE_MAXIDLETIME", "10m");

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config loads");

            // Assert.
            assert_eq!(
                cfg.backends[0].cache_policy.backend_max_idle_secs,
                Some(600)
            );
            Ok(())
        });
    }

    #[test]
    fn proxy_cache_policy_applies_nucleus_effective_lock_limits() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  proxyCache:
    lockAge: 60s
    lockTimeout: 120s
  backends:
    nucleus:
      include: true
tls:
  enabled: true
cache:
  root: /tmp/cache
",
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("canonical policy loads");
            let policy = &cfg.backends[0].cache_policy;

            // Assert.
            assert_eq!(policy.lock_age_secs, 10);
            assert_eq!(policy.lock_timeout_secs, 3);
            assert_eq!(policy.peer_copy_max_idle_secs, Some(14_400));
            Ok(())
        });
    }

    #[test]
    fn env_var_overrides_yaml_value() {
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;
            // YAML has tls.enabled = false; the env override flips it.
            jail.set_env("OVCC_CONFIG_TLS_ENABLED", "true");
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert!(
                cfg.tls.enabled,
                "OVCC_CONFIG_TLS_ENABLED should override the file"
            );
            // Untouched values still come from the file.
            assert_eq!(cfg.cache.root, "/var/cache/ovcc");
            Ok(())
        });
    }

    #[test]
    fn peer_replicate_accepts_environment_override() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file("values.yaml", SAMPLE_YAML)?;
            jail.set_env("OVCC_CONFIG_PEER_REPLICATE", "false");

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");

            // Assert.
            assert!(!cfg.peer.replicate);
            Ok(())
        });
    }

    #[test]
    fn kubelet_service_link_vars_do_not_corrupt_config() {
        Jail::expect_with(|jail| {
            // A Service named `ovcc` makes the kubelet inject these service-link
            // vars. Under a bare `OVCC_` prefix, `OVCC_SERVICE_PORT` set
            // `service.port` to a scalar while `OVCC_SERVICE_PORT_CACHE` set it
            // to a map, so extraction into `u16` failed at startup. The
            // narrower `OVCC_CONFIG_` prefix ignores them.
            jail.create_file(
                "values.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
service:
  address: 0.0.0.0
  port: 14128
",
            )?;
            jail.set_env("OVCC_SERVICE_HOST", "10.0.0.1");
            jail.set_env("OVCC_SERVICE_PORT", "14128");
            jail.set_env("OVCC_SERVICE_PORT_CACHE", "14128");
            jail.set_env("OVCC_PORT", "tcp://10.0.0.1:14128");
            jail.set_env("OVCC_PORT_14128_TCP", "tcp://10.0.0.1:14128");
            let cfg = load(Path::new("values.yaml")).expect("kubelet vars must not break load");
            assert_eq!(cfg.service.port, 14128, "service.port stays the YAML value");
            assert_eq!(cfg.service.address, "0.0.0.0");
            Ok(())
        });
    }

    #[test]
    fn service_defaults_when_absent_and_parses_when_present() {
        Jail::expect_with(|jail| {
            // Absent -> default 0.0.0.0:14128.
            jail.create_file("default.yaml", SAMPLE_YAML)?;
            let cfg = load(Path::new("default.yaml")).expect("load should succeed");
            assert_eq!(cfg.service, ServiceConfig::default());
            assert_eq!(cfg.service.address, "0.0.0.0");
            assert_eq!(cfg.service.port, 14128);
            assert_eq!(cfg.service.metrics_port, None);

            // Present -> parsed (top level in both modes).
            jail.create_file(
                "explicit.yaml",
                "\
backends: {}
tls:
  enabled: true
cache:
  root: /tmp/ovcc
service:
  address: 127.0.0.1
  port: 0
",
            )?;
            let cfg = load(Path::new("explicit.yaml")).expect("load should succeed");
            assert_eq!(cfg.service.address, "127.0.0.1");
            assert_eq!(cfg.service.port, 0);
            Ok(())
        });
    }

    #[test]
    fn service_health_port_accepts_snake_and_camel_case() {
        Jail::expect_with(|jail| {
            // Arrange.
            jail.create_file(
                "snake.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /tmp/ovcc
service:
  health_port: 13129
",
            )?;
            jail.create_file(
                "camel.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /tmp/ovcc
service:
  healthPort: 13130
",
            )?;

            // Act.
            let snake = load(Path::new("snake.yaml")).expect("snake config should load");
            let camel = load(Path::new("camel.yaml")).expect("camel config should load");

            // Assert.
            assert_eq!(snake.service.health_port, Some(13129));
            assert_eq!(camel.service.health_port, Some(13130));
            Ok(())
        });
    }

    #[test]
    fn service_metrics_port_accepts_snake_and_camel_case() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "snake.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /tmp/ovcc
service:
  metrics_port: 9145
",
            )?;
            jail.create_file(
                "camel.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /tmp/ovcc
service:
  metricsPort: 9146
",
            )?;

            // Act.
            let snake = load(Path::new("snake.yaml")).expect("snake config should load");
            let camel = load(Path::new("camel.yaml")).expect("camel config should load");

            // Assert.
            assert_eq!(snake.service.metrics_port, Some(9145));
            assert_eq!(camel.service.metrics_port, Some(9146));
            Ok(())
        });
    }

    #[test]
    fn requires_versioning_defaults_true_and_parses_alias() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends:
  s3:
    host: s3.example.com
  gcs:
    host: gcs.example.com
    requiresVersioning: false
tls:
  enabled: false
cache:
  root: /tmp/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            let s3 = cfg.backends.iter().find(|b| b.name == "s3").unwrap();
            let gcs = cfg.backends.iter().find(|b| b.name == "gcs").unwrap();
            assert!(s3.requires_versioning, "default should be true");
            assert!(
                !gcs.requires_versioning,
                "requiresVersioning alias should override to false"
            );
            Ok(())
        });
    }

    #[test]
    fn write_through_is_per_backend_and_parses_compat_camelcase() {
        Jail::expect_with(|jail| {
            // Absent on a backend -> disabled.
            jail.create_file("default.yaml", SAMPLE_YAML)?;
            let cfg = load(Path::new("default.yaml")).expect("load should succeed");
            let assets = cfg
                .backends
                .iter()
                .find(|b| b.name == "usd-assets")
                .unwrap();
            assert_eq!(assets.write_through, WriteThrough::default());

            // Compat mode: write-through lives under each backend, with the chart's
            // camelCase keys. Azure carries the block-assembly opt-in; s3 does not.
            jail.create_file(
                "wt.yaml",
                "\
nginx:
  backends:
    s3:
      host: bucket.example.com
      upstream: https://origin.example.com
      writeThrough:
        enabled: true
        enableHttpChunkedUploads: true
    azure:
      host: az.example.com
      upstream: https://az.example.com
      writeThrough:
        enabled: true
        assembleBlockUploads: true
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("wt.yaml")).expect("load should succeed");
            let s3 = cfg.backends.iter().find(|b| b.name == "s3").unwrap();
            assert!(s3.write_through.enabled);
            assert!(s3.write_through.enable_http_chunked_uploads);
            assert!(!s3.write_through.assemble_block_uploads);

            let azure = cfg.backends.iter().find(|b| b.name == "azure").unwrap();
            assert!(azure.write_through.enabled);
            assert!(azure.write_through.assemble_block_uploads);
            assert!(!azure.write_through.enable_http_chunked_uploads);
            Ok(())
        });
    }

    #[test]
    fn global_write_through_defaults_and_overrides_use_canonical_wire_keys() {
        Jail::expect_with(|jail| {
            // Arrange.
            jail.create_file("default.yaml", SAMPLE_YAML)?;
            jail.create_file(
                "overrides.yaml",
                "\
nginx:
  writeThrough:
    internalPort: 23130
    requestBodyChunkSize: 4096
    stagingSubdir: uploads/staged
    stagingTokenTtlSeconds: 17
    stagingMetadataSize: 32m
  backends: {}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;

            // Act.
            let defaults = load(Path::new("default.yaml")).expect("defaults should load");
            let overrides = load(Path::new("overrides.yaml")).expect("overrides should load");

            // Assert.
            assert_eq!(defaults.write_through, WriteThroughConfig::default());
            assert_eq!(overrides.write_through.internal_port, 23_130);
            assert_eq!(overrides.write_through.request_body_chunk_size, 4_096);
            assert_eq!(overrides.write_through.staging_subdir, "uploads/staged");
            assert_eq!(overrides.write_through.staging_token_ttl_seconds, 17);
            assert_eq!(overrides.write_through.staging_metadata_size, "32m");
            Ok(())
        });
    }

    #[test]
    fn global_write_through_rejects_zero_chunks_and_staging_escape() {
        Jail::expect_with(|jail| {
            // Arrange.
            let config = |chunk_size: usize, staging_subdir: &str| {
                format!(
                    "\
nginx:
  writeThrough:
    requestBodyChunkSize: {chunk_size}
    stagingSubdir: {staging_subdir}
  backends: {{}}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
"
                )
            };
            jail.create_file("zero.yaml", &config(0, ".ovcc-staging"))?;
            jail.create_file("escape.yaml", &config(65_536, "../outside"))?;

            // Act.
            let zero = load(Path::new("zero.yaml")).expect_err("zero chunks must fail");
            let escape = load(Path::new("escape.yaml")).expect_err("escape must fail");

            // Assert.
            assert!(zero.to_string().contains("must be greater than zero"));
            assert!(escape.to_string().contains("relative path"));
            Ok(())
        });
    }

    #[test]
    fn telemetry_defaults_when_absent_and_parses_top_level_camelcase() {
        Jail::expect_with(|jail| {
            // Absent -> defaults.
            jail.create_file("default.yaml", SAMPLE_YAML)?;
            let cfg = load(Path::new("default.yaml")).expect("load should succeed");
            assert_eq!(cfg.telemetry, ovcc_telemetry::TelemetryConfig::default());

            // Present at the top level (both modes) with the chart's camelCase
            // `serviceName` and a nested OTLP endpoint.
            jail.create_file(
                "telemetry.yaml",
                "\
backends: {}
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
telemetry:
  serviceName: ovcc-edge
  format: json
  metricPrefix: ucc_
  otlp:
    endpoint: http://collector:4318
",
            )?;
            let cfg = load(Path::new("telemetry.yaml")).expect("load should succeed");
            assert_eq!(cfg.telemetry.service_name, "ovcc-edge");
            assert_eq!(cfg.telemetry.format, ovcc_telemetry::LogFormat::Json);
            assert_eq!(cfg.telemetry.otlp.endpoint, "http://collector:4318");
            assert_eq!(cfg.telemetry.metric_prefix, "ucc_");
            Ok(())
        });
    }

    #[test]
    fn bursty_cache_parses_camelcase_and_defaults_none() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends:
    s3:
      host: bucket.example.com
      upstream: https://origin.example.com
      burstyCache:
        ttlSec: 3
        rules:
          - method: GET
            uriStartsWith: \"/?list-type=\"
          - method: HEAD
            uriStartsWith: \"/\"
    gcs:
      host: gcs.example.com
      upstream: https://gcs.example.com
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            let s3 = cfg.backends.iter().find(|b| b.name == "s3").unwrap();
            let gcs = cfg.backends.iter().find(|b| b.name == "gcs").unwrap();

            let bursty = s3.bursty.as_ref().expect("s3 bursty parsed");
            assert_eq!(bursty.ttl_secs, 3);
            assert_eq!(bursty.rules.len(), 2);
            assert_eq!(bursty.rules[0].method, "GET");
            assert_eq!(bursty.rules[0].uri_starts_with, "/?list-type=");
            assert_eq!(bursty.rules[1].method, "HEAD");
            // A backend without burstyCache leaves it disabled (None).
            assert!(gcs.bursty.is_none());
            Ok(())
        });
    }

    #[test]
    fn service_self_names_parse() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
backends: {}
tls:
  enabled: true
cache:
  root: /tmp/ovcc
service:
  self_names:
    - ovcc.prod.svc.cluster.local
    - ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            assert_eq!(
                cfg.service.self_names,
                vec!["ovcc.prod.svc.cluster.local".to_owned(), "ovcc".to_owned()]
            );
            Ok(())
        });
    }

    fn backend(name: &str, host: &str, upstream: &str) -> Backend {
        Backend {
            name: name.to_owned(),
            host: host.to_owned(),
            upstream: upstream.to_owned(),
            cache_root: None,
            cache_policy: BackendCachePolicy::default(),
            requires_versioning: true,
            bursty: None,
            registry: None,
            write_through: WriteThrough::default(),
            allow_cache_reset: false,
            route: BackendRoute::default(),
        }
    }

    #[test]
    fn resolve_upstream_routes_cloud_hosts_to_themselves_over_tls() {
        // A real S3 bucket host matches the built-in s3 pattern -> proxy to
        // itself over TLS, ignoring the configured (mock) upstream.
        let s3 = backend("s3", "s3.mock.local", "http://mock.svc:4443");
        assert_eq!(
            s3.resolve_upstream("example-bucket.s3.us-west-2.amazonaws.com"),
            Some("https://example-bucket.s3.us-west-2.amazonaws.com".to_owned())
        );
        // GCS (with and without a bucket label) and Azure likewise.
        let gcs = backend("gcs", "gcs.mock.local", "http://mock.svc:4443");
        assert_eq!(
            gcs.resolve_upstream("storage.googleapis.com"),
            Some("https://storage.googleapis.com".to_owned())
        );
        assert_eq!(
            gcs.resolve_upstream("bucket.storage.googleapis.com"),
            Some("https://bucket.storage.googleapis.com".to_owned())
        );
        let azure = backend("azure", "az.mock.local", "http://mock.svc:4443");
        assert_eq!(
            azure.resolve_upstream("acct.blob.core.windows.net"),
            Some("https://acct.blob.core.windows.net".to_owned())
        );
    }

    #[test]
    fn canonical_backend_defaults_parse_and_route() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    azure:
      include: true
      serverName: '~^(?<container_name>[^\.]+)\.blob\.core\.windows\.net$'
      proxyCacheName: az
      proxyPass: '$scheme://$host'
      proxyAuthPass: '$scheme://$host'
      enableUpstreamKeepalive: false
      allowHttp2Clients: true
    gcs:
      include: true
      serverName: '~^([^.]+\.)?storage\.googleapis\.com$'
      proxyCacheName: gcs
      proxyPass: 'https://$host'
      proxyAuthPass: 'https://$host'
      enableUpstreamKeepalive: true
      allowHttp2Clients: true
    s3:
      include: true
      serverName: '~^[^.]+\.s3\.[^.]+\.amazonaws\.com$'
      proxyCacheName: s3
      proxyPass: '$scheme://$host'
      proxyAuthPass: '$scheme://$host'
      enableUpstreamKeepalive: true
      allowHttp2Clients: true
    nucleus:
      include: true
      serverName: _
      proxyCacheName: nucleus
      proxyPass: '$scheme://$host'
      proxyAuthPass: null
      enableUpstreamKeepalive: false
      allowHttp2Clients: false
      bridgeResolver: null
    passthrough:
      proxyPass: '$scheme://$http_host'
      allowHttp2Clients: true
tls:
  enabled: true
cache:
  root: /var/cache/ovcc
"#,
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("canonical defaults load");
            let s3 = cfg
                .backends
                .iter()
                .find(|backend| backend.name == "s3")
                .unwrap();
            let routed = s3
                .resolve_backend("bucket.s3.us-west-2.amazonaws.com")
                .expect("canonical S3 host routes");

            // Assert.
            assert_eq!(
                routed,
                ResolvedBackend {
                    upstream: "https://bucket.s3.us-west-2.amazonaws.com".to_owned(),
                    auth_upstream: Some("https://bucket.s3.us-west-2.amazonaws.com".to_owned()),
                }
            );
            // Compared as paths: the zone is joined with the platform separator.
            assert_eq!(
                s3.cache_root.as_deref().map(Path::new),
                Some(Path::new("/var/cache/ovcc/s3")),
                "proxyCacheName isolates the backend under the shared cache root"
            );
            assert!(s3.route.enable_upstream_keepalive);
            assert!(s3.route.allow_http2_clients);
            for (name, host, cache_name, keepalive) in [
                ("azure", "container.blob.core.windows.net", "az", false),
                ("gcs", "bucket.storage.googleapis.com", "gcs", true),
            ] {
                let backend = cfg
                    .backends
                    .iter()
                    .find(|backend| backend.name == name)
                    .unwrap();
                assert_eq!(
                    backend.resolve_backend(host),
                    Some(ResolvedBackend {
                        upstream: format!("https://{host}"),
                        auth_upstream: Some(format!("https://{host}")),
                    })
                );
                let expected_root = format!("/var/cache/ovcc/{cache_name}");
                assert_eq!(
                    backend.cache_root.as_deref().map(Path::new),
                    Some(Path::new(&expected_root))
                );
                assert_eq!(backend.route.enable_upstream_keepalive, keepalive);
                assert!(backend.route.allow_http2_clients);
            }
            let nucleus = cfg
                .backends
                .iter()
                .find(|backend| backend.name == "nucleus")
                .unwrap();
            assert_eq!(
                nucleus.resolve_nucleus_upstream("bridge.example:14129", "https"),
                "https://bridge.example"
            );
            assert!(!nucleus.route.enable_upstream_keepalive);
            assert_eq!(
                cfg.passthrough.resolve_upstream("origin.example:8443"),
                "https://origin.example:8443"
            );
            Ok(())
        });
    }

    #[test]
    fn canonical_backend_supported_overrides_drive_routing() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    s3:
      include: true
      serverName: '~^(?<upstream_host>[^.]+)\.cache\.example$'
      proxyCacheName: artifacts
      proxyPass: 'https://$upstream_host.origin.example:9443'
      proxyAuthPass: 'https://auth.example'
      enableUpstreamKeepalive: false
      allowHttp2Clients: true
    passthrough:
      proxyPass: 'https://$http_host'
      allowHttp2Clients: true
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("supported overrides load");
            let backend = &cfg.backends[0];
            let routed = backend
                .resolve_backend("packages.cache.example")
                .expect("override serverName routes");

            // Assert.
            assert_eq!(routed.upstream, "https://packages.origin.example:9443");
            assert_eq!(
                routed.auth_upstream.as_deref(),
                Some("https://auth.example")
            );
            assert_eq!(
                backend.cache_root.as_deref().map(Path::new),
                Some(Path::new("/cache/artifacts"))
            );
            assert!(!backend.route.enable_upstream_keepalive);
            assert_eq!(
                cfg.passthrough.resolve_upstream("other.example:7443"),
                "https://other.example:7443"
            );
            Ok(())
        });
    }

    #[test]
    fn canonical_registry_routes_content_and_auth_to_allowlisted_upstream() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    registry:
      include: true
      proxyCacheName: registry
      matching: suffix
      suffix: cache.svc.cluster.local
      proxyPass: 'https://$upstream_host'
      proxyAuthPass: 'https://$upstream_host'
      enableUpstreamKeepalive: false
      allowHttp2Clients: true
      upstreams:
        - name: npm
          hosts: [registry.npmjs.org]
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("registry values load");
            let registry = &cfg.backends[0];
            let routed = registry
                .resolve_backend("registry.npmjs.org.cache.svc.cluster.local")
                .expect("allowlisted registry routes");

            // Assert.
            assert_eq!(
                routed,
                ResolvedBackend {
                    upstream: "https://registry.npmjs.org".to_owned(),
                    auth_upstream: Some("https://registry.npmjs.org".to_owned()),
                }
            );
            assert_eq!(
                registry.cache_root.as_deref().map(Path::new),
                Some(Path::new("/cache/registry"))
            );
            assert!(!registry.route.enable_upstream_keepalive);
            assert!(
                registry
                    .resolve_backend("unlisted.example.cache.svc.cluster.local")
                    .is_none(),
                "registry routing remains restricted to the allowlist"
            );
            Ok(())
        });
    }

    #[test]
    fn canonical_backend_rejects_unsupported_expressions() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    s3:
      include: true
      serverName: '~^bucket\.example$'
      proxyPass: 'https://$unknown_variable'
      proxyAuthPass: 'https://$host'
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;

            // Act.
            let error = load(Path::new("values.yaml"))
                .expect_err("an unsupported proxy expression must fail");

            // Assert.
            assert!(error.to_string().contains("unsupported proxy expression"));
            Ok(())
        });
    }

    #[test]
    fn canonical_backend_rejects_missing_hit_authorization() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    s3:
      include: true
      serverName: '~^[^.]+\.s3\.[^.]+\.amazonaws\.com$'
      proxyPass: '$scheme://$host'
      proxyAuthPass: null
      allowHttp2Clients: true
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;

            // Act.
            let error =
                load(Path::new("values.yaml")).expect_err("missing HIT authorization must fail");

            // Assert.
            assert!(
                error
                    .to_string()
                    .contains("cache HIT authorization probes must remain enabled")
            );
            Ok(())
        });
    }

    #[test]
    fn canonical_backend_accepts_dns_override_and_rejects_http2_override() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "bridge.yaml",
                r#"
nginx:
  backends:
    nucleus:
      include: true
      bridgeResolver: unbound.example:53
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;
            jail.create_file(
                "http2.yaml",
                r#"
nginx:
  backends:
    s3:
      include: true
      allowHttp2Clients: false
tls:
  enabled: true
cache:
  root: /cache
"#,
            )?;

            // Act.
            let bridge = load(Path::new("bridge.yaml")).expect("DNS override must parse");
            let http2 = load(Path::new("http2.yaml")).expect_err("HTTP/2 override must fail");

            // Assert.
            assert_eq!(
                bridge.backends[0].route.resolver_override.as_deref(),
                Some("unbound.example:53")
            );
            assert!(
                http2
                    .to_string()
                    .contains("is not supported; keep the canonical default")
            );
            Ok(())
        });
    }

    #[test]
    fn resolve_upstream_uses_configured_upstream_for_exact_mock_host() {
        // The integration mock host doesn't match any cloud pattern, so it
        // takes the configured (mock) upstream — exact-match flavors unaffected.
        let s3 = backend("s3", "s3.mock.local", "http://mock.svc:4443");
        assert_eq!(
            s3.resolve_upstream("s3.mock.local"),
            Some("http://mock.svc:4443".to_owned())
        );
        // A `:port` on the Host is ignored for matching.
        assert_eq!(
            s3.resolve_upstream("s3.mock.local:14128"),
            Some("http://mock.svc:4443".to_owned())
        );
    }

    #[test]
    fn resolve_upstream_returns_none_for_unrelated_host() {
        // Neither the cloud pattern nor the exact host matches -> not ours
        // (the proxy then falls through to passthrough).
        let s3 = backend("s3", "s3.mock.local", "http://mock.svc:4443");
        assert_eq!(s3.resolve_upstream("example.com"), None);
        // A non-cloud backend (no built-in pattern) routes only on exact host.
        let nucleus = backend("nucleus", "nucleus.mock.local", "http://mock.svc:4443");
        assert_eq!(nucleus.resolve_upstream("acct.blob.core.windows.net"), None);
    }

    #[test]
    fn registry_backend_parses_matching_suffix_and_upstreams() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                "\
nginx:
  backends:
    registry:
      include: true
      requiresVersioning: false
      matching: suffix
      suffix: cache.svc.cluster.local
      upstreams:
        - name: npm
          hosts: [registry.npmjs.org]
        - name: pip
          hosts: [pypi.org, files.pythonhosted.org]
          contentRewrite:
            - uriStartsWith: /simple/
              types: [text/html]
              substitute:
                match: https://files.pythonhosted.org
                replace: http://files.pythonhosted.org.cache.svc.cluster.local:14128
tls:
  enabled: false
cache:
  root: /var/cache/ovcc
",
            )?;
            let cfg = load(Path::new("values.yaml")).expect("load should succeed");
            let registry = cfg.backends.iter().find(|b| b.name == "registry").unwrap();
            assert!(!registry.requires_versioning, "registries are mutable");
            let rc = registry.registry.as_ref().expect("registry config parsed");
            assert_eq!(rc.matching, ovcc_registry::Matching::Suffix);
            assert_eq!(rc.suffix, "cache.svc.cluster.local");
            assert_eq!(rc.upstreams.len(), 2);
            // The pip upstream carries its content rewrite rule.
            let pip = rc.upstreams.iter().find(|u| u.name == "pip").unwrap();
            assert_eq!(pip.hosts.len(), 2);
            assert_eq!(pip.content_rewrite.len(), 1);
            assert_eq!(pip.content_rewrite[0].uri_starts_with, "/simple/");
            Ok(())
        });
    }

    #[test]
    fn resolve_upstream_routes_registry_vhost_to_real_upstream_over_tls() {
        // A registry backend maps a suffixed virtual host to the real upstream
        // over TLS, taking precedence over the cloud/exact rules.
        let registry = Backend {
            name: "registry".to_owned(),
            host: String::new(),
            upstream: String::new(),
            cache_root: None,
            cache_policy: BackendCachePolicy::default(),
            requires_versioning: false,
            bursty: None,
            allow_cache_reset: false,
            registry: Some(ovcc_registry::RegistryConfig {
                matching: ovcc_registry::Matching::Suffix,
                suffix: "cache.svc.cluster.local".to_owned(),
                cacheable_uri: None,
                upstreams: vec![ovcc_registry::RegistryUpstream {
                    name: "npm".to_owned(),
                    hosts: vec!["registry.npmjs.org".to_owned()],
                    cacheable_uri: None,
                    content_rewrite: Vec::new(),
                }],
            }),
            write_through: WriteThrough::default(),
            route: BackendRoute::default(),
        };
        assert_eq!(
            registry.resolve_upstream("registry.npmjs.org.cache.svc.cluster.local"),
            Some("https://registry.npmjs.org".to_owned())
        );
        // An unlisted host is not served by the registry backend.
        assert_eq!(registry.resolve_upstream("example.com"), None);
        // `registry_match` exposes the matched upstream's (empty) rewrite rules.
        let m = registry
            .registry_match("registry.npmjs.org.cache.svc.cluster.local")
            .expect("registry match");
        assert!(m.content_rewrite.is_empty());
    }

    #[test]
    fn nucleus_upstream_is_configured_when_upstream_set() {
        let b = backend("nucleus", "nucleus.mock.local", "http://mock.svc:4443");
        assert_eq!(
            b.nucleus_upstream(),
            NucleusUpstream::Configured("http://mock.svc:4443".to_owned())
        );
    }

    #[test]
    fn nucleus_upstream_is_dynamic_host_when_upstream_empty() {
        let b = backend("nucleus", "_", "");
        assert_eq!(b.nucleus_upstream(), NucleusUpstream::DynamicHost);
    }

    #[test]
    fn allow_cache_reset_reads_chart_alias() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    nucleus:
      enabled: true
      host: "nucleus.mock.local"
      upstream: "http://mock.svc:4443"
      allowCacheReset: true
tls:
  enabled: false
cache:
  root: "/data"
"#,
            )?;
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            let nucleus = cfg
                .backends
                .iter()
                .find(|b| b.name == "nucleus")
                .expect("nucleus backend present");
            assert!(nucleus.allow_cache_reset);
            Ok(())
        });
    }

    #[test]
    fn allow_cache_reset_defaults_false() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
nginx:
  backends:
    nucleus:
      enabled: true
      host: "nucleus.mock.local"
      upstream: "http://mock.svc:4443"
tls:
  enabled: false
cache:
  root: "/data"
"#,
            )?;
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            let nucleus = cfg
                .backends
                .iter()
                .find(|b| b.name == "nucleus")
                .expect("nucleus backend present");
            assert!(!nucleus.allow_cache_reset);
            Ok(())
        });
    }

    #[test]
    fn compat_reads_configmap_engine_fields() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
backends: {}
tls:
  enabled: false
cache:
  root: "/data"
compat:
  useUccDefaults: true
  emitLegacyCacheReset: false
  cacheResetPath: "/primary/reset"
  instanceHeader: "Primary-Instance"
  legacyCacheResetPath: ""
  legacyInstanceHeader: ""
"#,
            )?;
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            assert_eq!(
                cfg.compat,
                CompatConfig {
                    use_ucc_defaults: true,
                    emit_legacy_cache_reset: false,
                    cache_reset_path: "/primary/reset".to_owned(),
                    instance_header: "Primary-Instance".to_owned(),
                    legacy_cache_reset_path: String::new(),
                    legacy_instance_header: String::new(),
                }
            );
            Ok(())
        });
    }

    #[test]
    fn compat_defaults_for_older_engine_config() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
backends: {}
tls:
  enabled: false
cache:
  root: "/data"
"#,
            )?;
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            assert_eq!(cfg.compat, CompatConfig::default());
            Ok(())
        });
    }

    #[test]
    fn registry_get_cacheable_uri_load_and_env_override() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
tls:
  enabled: false
cache:
  root: /data
nginx:
  backends:
    registry:
      cacheableUri: '!~ ^/v2/token(?:\?|$)'
      upstreams:
        - name: docker
          hosts: [registry.example]
          cacheableUri: '^/v2/'
        - name: packages
          hosts: [packages.example]
"#,
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("config loads");
            let route = cfg.backends[0].registry_match("registry.example").unwrap();

            // Assert.
            assert!(!route.is_cacheable_uri("/v2/token?service=registry"));
            assert!(!route.is_cacheable_uri("/packages"));
            assert!(route.is_cacheable_uri("/v2/test/blobs/sha256:abc"));
            assert!(
                cfg.backends[0]
                    .registry_match("packages.example")
                    .unwrap()
                    .is_cacheable_uri("/packages")
            );

            // Arrange.
            jail.set_env(
                "OVCC_CONFIG_NGINX_BACKENDS_REGISTRY_CACHEABLEURI",
                "!~ ^/v2/private",
            );

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("env override loads");
            let route = cfg.backends[0].registry_match("registry.example").unwrap();

            // Assert.
            assert!(route.is_cacheable_uri("/v2/token"));
            assert!(!route.is_cacheable_uri("/v2/private/blob"));
            assert!(!route.is_cacheable_uri("/packages"));
            Ok(())
        });
    }

    #[test]
    fn registry_get_cacheable_uri_combined_values_routes_and_boundaries() {
        // Loading the overlay that `tests/chart/test_chart_rendering.sh` also
        // renders proves every expression compiles as a Rust regex.
        let values = include_str!("../../../fixtures/registry-cacheable-uri.values.yaml");
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                &format!("tls:\n  enabled: false\ncache:\n  root: /data\n{values}"),
            )?;
            let cfg = load(Path::new("values.yaml")).expect("combined registry values load");
            let backend = &cfg.backends[0];
            assert_eq!(backend.registry.as_ref().unwrap().upstreams.len(), 5);

            let cases = [
                ("docker.artifactory.example.com", "/v2/", false),
                ("docker.artifactory.example.com", "/v2/?", false),
                ("docker.artifactory.example.com", "/v2/token", false),
                (
                    "docker.artifactory.example.com",
                    "/v2/token?service=docker",
                    false,
                ),
                (
                    "docker.artifactory.example.com",
                    "/v2/docker-local/token",
                    false,
                ),
                (
                    "docker.artifactory.example.com",
                    "/v2/team/image/blobs/uploads/uuid",
                    false,
                ),
                (
                    "docker.artifactory.example.com",
                    "/v2/team/image/blobs/uploads/?mount=sha256:abc",
                    false,
                ),
                (
                    "docker.artifactory.example.com",
                    "/v2/team/image/blobs/sha256:abc",
                    true,
                ),
                (
                    "docker.artifactory.example.com",
                    "/v2/team/image/manifests/sha256:abc",
                    true,
                ),
                ("docker.artifactory.example.com", "/v2/tokenize", true),
                (
                    "docker.artifactory.example.com",
                    "/v2/team/image/blobs/uploadsExtra",
                    true,
                ),
                (
                    "artifactory.example.com",
                    "/api/docker/null/v2/token",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/docker/repo/v2/token?scope=pull",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/docker/repo/v2/auth",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/npm/auth/",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/npm/repo/-/whoami",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/conan/repo/v2/users/authenticate",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/access/api/v1/tokens?page=1",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/api/security/token",
                    false,
                ),
                (
                    "artifactory.example.com",
                    "/artifactory/repo/package.tgz",
                    true,
                ),
                ("registry.npmjs.org", "/-/whoami", false),
                ("registry.npmjs.org", "/-/npm/v1/tokens?page=1", false),
                ("registry.npmjs.org", "/-/npm/v1/tokens/token/abc", false),
                ("registry.npmjs.org", "/-/v1/login/session", false),
                (
                    "registry.npmjs.org",
                    "/-/user/org.couchdb.user:alice",
                    false,
                ),
                ("registry.npmjs.org", "/lodash/-/lodash-4.17.21.tgz", true),
                ("registry.npmjs.org", "/-/whoamis", true),
                ("registry.npmjs.org", "/v2/token", true),
                ("center2.conan.io", "/v1/users/authenticate", false),
                ("center2.conan.io", "/v2/users/check_credentials", false),
                (
                    "center2.conan.io",
                    "/v2/conans/zlib/1.3.1/_/_/revisions/r/files/conan_package.tgz",
                    true,
                ),
                ("pypi.org", "/simple/idna/", true),
                ("files.pythonhosted.org", "/packages/idna.whl", true),
                (
                    "developer.download.nvidia.com",
                    "/compute/cuda/repos/ubuntu2204/x86_64/Packages.gz",
                    true,
                ),
                (
                    "developer.download.nvidia.com",
                    "/compute/cuda/repos/ubuntu2204/x86_64/cuda-keyring.deb",
                    true,
                ),
            ];
            for (host, uri, expected) in cases {
                let route = backend
                    .registry_match(host)
                    .expect("configured registry host");
                assert_eq!(route.is_cacheable_uri(uri), expected, "{host}{uri}");
            }
            assert!(backend.registry_match("other.example.com").is_none());
            Ok(())
        });
    }

    #[test]
    fn registry_get_cacheable_uri_native_alias_and_null() {
        // Arrange.
        Jail::expect_with(|jail| {
            jail.create_file(
                "values.yaml",
                r#"
tls:
  enabled: false
cache:
  root: /data
backends:
  registry:
    cacheable_uri: '!~ ^/v2/token$'
    upstreams:
      - name: docker
        hosts: [registry.example]
        cacheable_uri: null
"#,
            )?;

            // Act.
            let cfg = load(Path::new("values.yaml")).expect("native config loads");
            let route = cfg.backends[0].registry_match("registry.example").unwrap();

            // Assert.
            assert!(!route.is_cacheable_uri("/v2/token"));
            assert!(route.is_cacheable_uri("/other"));
            Ok(())
        });
    }

    #[test]
    fn registry_get_cacheable_uri_invalid_configuration_rejected() {
        // Arrange.
        Jail::expect_with(|jail| {
            for entry in [
                "cacheableUri: '!~ (?!/v2/token)'\n    upstreams: [{name: docker, hosts: [registry.example]}]",
                "upstreams: [{name: docker, hosts: [registry.example], cacheableUri: '['}]",
                "cacheableUri: '^/v2/'",
                "cacheableUri: false\n    upstreams: [{name: docker, hosts: [registry.example]}]",
                "upstreams: [{name: docker, hosts: [registry.example], cacheableUri: ['^/v2/']}]",
            ] {
                jail.create_file("values.yaml", &format!("tls:\n  enabled: false\ncache:\n  root: /data\nbackends:\n  registry:\n    {entry}\n"))?;

                // Act.
                let err =
                    load(Path::new("values.yaml")).expect_err("invalid rule must fail startup");

                // Assert.
                assert_eq!(err.kind(), ErrorKind::Config);
            }
            Ok(())
        });
    }

    #[test]
    fn missing_file_is_a_config_error() {
        Jail::expect_with(|_jail| {
            let err =
                load(Path::new("does-not-exist.yaml")).expect_err("a missing file must not load");
            assert_eq!(err.kind(), ErrorKind::Config);
            Ok(())
        });
    }

    #[test]
    fn invalid_yaml_is_a_config_error() {
        Jail::expect_with(|jail| {
            jail.create_file("bad.yaml", "backends: [unterminated\n")?;
            let err = load(Path::new("bad.yaml")).expect_err("invalid YAML must not load");
            assert_eq!(err.kind(), ErrorKind::Config);
            Ok(())
        });
    }
}
