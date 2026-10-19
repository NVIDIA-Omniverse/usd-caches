// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Peer discovery, gRPC presence queries, and the peer data client.
//!
//! Peers exchange cache identity components rather than rendered cache keys:
//! cache generation is local to each process, so the serving peer must render
//! its own key. [`PeerDirectory`] keeps discovery advisory and deterministic;
//! a failed peer is suppressed briefly while origin remains the caller's
//! fallback. [`PeerClient`] dials the selected socket address while TLS verifies
//! the configured DNS [`rustls::pki_types::ServerName`].

use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{CONNECTION, HOST, HeaderName, HeaderValue};
use http::{HeaderMap, Method, Request, StatusCode};
use http_body::{Body, Frame, SizeHint};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tokio::time::{Sleep, timeout};
use tokio_rustls::TlsConnector;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tower::service_fn;

/// A peer request may cross exactly one peer boundary.
pub const PEER_REQUEST_DEPTH: u8 = 1;
/// The only internal header permitted on a forwarded peer data request.
pub const X_OVCC_PEER_DEPTH: &str = "x-ovcc-peer-depth";
/// Failed peers are skipped briefly so rollout skew does not tax every MISS.
pub const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(1);

/// Types generated from the `ovcc.peer.v1` protobuf service definition.
pub mod proto {
    tonic::include_proto!("ovcc.peer.v1");
}

mod optional_keys;
pub use optional_keys::{
    OPTIONAL_KEY_CONTRACT, OptionalProbeResponse, OptionalRequest, optional_who_has_request,
    optional_who_has_response, validate_optional_request,
};

const MAX_HOST_BYTES: usize = 255;
const MAX_METHOD_BYTES: usize = 16;
const MAX_URI_BYTES: usize = 8 * 1024;
const MAX_ARGS_BYTES: usize = 8 * 1024;
const MAX_IF_MATCH_BYTES: usize = 1024;
const MAX_RANGE_BYTES: usize = 256;
const MAX_IDENTITY_BYTES: usize = 20 * 1024;

/// Result type for peer discovery, codec, and request setup.
pub type Result<T> = std::result::Result<T, PeerError>;

/// Timeout stage, kept explicit so proxy metrics remain bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutStage {
    Probe,
    Connect,
    FirstByte,
}

/// Peer operation failure.
#[derive(Debug)]
pub enum PeerError {
    InvalidConfig(String),
    InvalidHeader { name: &'static str, reason: String },
    Discovery(String),
    Io(io::Error),
    Tls(String),
    Http(String),
    Timeout(TimeoutStage),
}

impl PeerError {
    /// Which stage ran out of time, for the one variant that carries a stage.
    pub fn timeout_stage(&self) -> Option<TimeoutStage> {
        match self {
            Self::Timeout(stage) => Some(*stage),
            _ => None,
        }
    }
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => write!(f, "invalid peer configuration: {reason}"),
            Self::InvalidHeader { name, reason } => {
                write!(f, "invalid peer header {name}: {reason}")
            }
            Self::Discovery(reason) => write!(f, "peer discovery failed: {reason}"),
            Self::Io(error) => write!(f, "peer I/O failed: {error}"),
            Self::Tls(reason) => write!(f, "peer TLS failed: {reason}"),
            Self::Http(reason) => write!(f, "peer HTTP failed: {reason}"),
            Self::Timeout(stage) => write!(f, "peer {stage:?} timeout"),
        }
    }
}

impl std::error::Error for PeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PeerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Carries the peer failure into the shared error type without losing its
/// rendered detail, so a caller working in [`ovcc_core::Result`] can use `?`.
impl From<PeerError> for ovcc_core::Error {
    fn from(error: PeerError) -> Self {
        Self::upstream(error.to_string())
    }
}

/// Cache object identity carried as structured protobuf fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectIdentity {
    pub host: String,
    pub method: Method,
    pub uri: String,
    pub args: String,
    pub if_match: String,
    pub range: String,
}

/// Exact origin validator required for peer selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactValidator {
    Etag(String),
    LastModified(String),
}

impl ExactValidator {
    /// The validator token itself, with the ETag or Last-Modified distinction dropped.
    pub fn value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::LastModified(value) => value,
        }
    }
}

impl TryFrom<proto::CacheIdentity> for ObjectIdentity {
    type Error = PeerError;

    fn try_from(identity: proto::CacheIdentity) -> Result<Self> {
        let identity = Self {
            host: identity.host,
            method: Method::from_bytes(identity.method.as_bytes()).map_err(|error| {
                invalid_header("method", format!("invalid method token: {error}"))
            })?,
            uri: identity.uri,
            args: identity.filtered_query,
            if_match: identity.if_match,
            range: identity.range,
        };
        validate_identity(&identity)?;
        Ok(identity)
    }
}

impl From<&ObjectIdentity> for proto::CacheIdentity {
    fn from(identity: &ObjectIdentity) -> Self {
        Self {
            host: identity.host.clone(),
            method: identity.method.as_str().to_owned(),
            uri: identity.uri.clone(),
            filtered_query: identity.args.clone(),
            if_match: identity.if_match.clone(),
            range: identity.range.clone(),
        }
    }
}

impl TryFrom<proto::Validator> for ExactValidator {
    type Error = PeerError;

    fn try_from(validator: proto::Validator) -> Result<Self> {
        match validator.value {
            Some(proto::validator::Value::Etag(value)) if !value.is_empty() => {
                validate_bounded("validator.etag", &value, 1, MAX_IF_MATCH_BYTES)?;
                Ok(Self::Etag(value))
            }
            Some(proto::validator::Value::LastModified(value)) if !value.is_empty() => {
                validate_bounded("validator.last_modified", &value, 1, MAX_IF_MATCH_BYTES)?;
                Ok(Self::LastModified(value))
            }
            _ => Err(invalid_header("validator", "missing exact value")),
        }
    }
}

impl From<&ExactValidator> for proto::Validator {
    fn from(validator: &ExactValidator) -> Self {
        let value = match validator {
            ExactValidator::Etag(value) => proto::validator::Value::Etag(value.clone()),
            ExactValidator::LastModified(value) => {
                proto::validator::Value::LastModified(value.clone())
            }
        };
        Self { value: Some(value) }
    }
}

/// Decodes a received probe, rejecting a missing or invalid identity or validator.
pub fn validate_who_has_request(
    request: proto::WhoHasRequest,
) -> Result<(ObjectIdentity, ExactValidator)> {
    let identity = request
        .identity
        .ok_or_else(|| invalid_header("identity", "missing"))?
        .try_into()?;
    let validator = request
        .validator
        .ok_or_else(|| invalid_header("validator", "missing"))?
        .try_into()?;
    Ok((identity, validator))
}

/// Builds an outbound probe, refusing identities that exceed the wire size bound.
pub fn who_has_request(
    identity: &ObjectIdentity,
    validator: &ExactValidator,
) -> Result<proto::WhoHasRequest> {
    validate_identity(identity)?;
    let total = identity.host.len()
        + identity.method.as_str().len()
        + identity.uri.len()
        + identity.args.len()
        + identity.if_match.len()
        + identity.range.len()
        + validator.value().len();
    if total > MAX_IDENTITY_BYTES {
        return Err(PeerError::InvalidHeader {
            name: "identity",
            reason: format!("aggregate value exceeds {MAX_IDENTITY_BYTES} bytes"),
        });
    }
    Ok(proto::WhoHasRequest {
        identity: Some(identity.into()),
        validator: Some(validator.into()),
        optional_keys: None,
    })
}

fn validate_identity(identity: &ObjectIdentity) -> Result<()> {
    validate_bounded("identity.host", &identity.host, 1, MAX_HOST_BYTES)?;
    if identity.host.bytes().any(|byte| {
        !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']' | b'_'))
    }) {
        return Err(invalid_header(
            "identity.host",
            "contains a character outside DNS/IP host syntax",
        ));
    }
    validate_bounded(
        "identity.method",
        identity.method.as_str(),
        1,
        MAX_METHOD_BYTES,
    )?;
    if identity.method != Method::GET && identity.method != Method::HEAD {
        return Err(invalid_header(
            "identity.method",
            "only GET or HEAD identities are supported",
        ));
    }
    validate_bounded("identity.uri", &identity.uri, 1, MAX_URI_BYTES)?;
    if !identity.uri.starts_with('/') || identity.uri.contains('?') || identity.uri.contains('\\') {
        return Err(invalid_header(
            "identity.uri",
            "must be an origin-form path without query or backslash",
        ));
    }
    validate_bounded("identity.filtered_query", &identity.args, 0, MAX_ARGS_BYTES)?;
    if identity.args.starts_with('?') {
        return Err(invalid_header(
            "identity.filtered_query",
            "must not include the query '?' prefix",
        ));
    }
    validate_bounded(
        "identity.if_match",
        &identity.if_match,
        0,
        MAX_IF_MATCH_BYTES,
    )?;
    validate_bounded("identity.range", &identity.range, 0, MAX_RANGE_BYTES)?;
    if !identity.range.is_empty() && !identity.range.starts_with("bytes=") {
        return Err(invalid_header(
            "identity.range",
            "must be empty or use bytes= syntax",
        ));
    }
    Ok(())
}

fn validate_bounded(
    name: &'static str,
    value: &str,
    min_bytes: usize,
    max_bytes: usize,
) -> Result<()> {
    if value.len() < min_bytes {
        return Err(invalid_header(name, "cannot be empty"));
    }
    if value.len() > max_bytes {
        return Err(invalid_header(
            name,
            format!("value exceeds {max_bytes} bytes"),
        ));
    }
    if !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == 0x7f)
    {
        return Err(invalid_header(name, "contains non-ASCII or control bytes"));
    }
    Ok(())
}

fn invalid_header(name: &'static str, reason: impl Into<String>) -> PeerError {
    PeerError::InvalidHeader {
        name,
        reason: reason.into(),
    }
}

/// Peer source. DNS names are resolved to all pod addresses; explicit peers
/// bypass discovery and are useful in deterministic tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    Dns { host: String, port: u16 },
    Explicit(Vec<SocketAddr>),
}

/// Inputs used to remove this process from a resolved peer list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelfIdentity {
    pub pod_ip: Option<IpAddr>,
    pub pod_name: Option<String>,
    pub self_names: Vec<String>,
}

impl SelfIdentity {
    /// Reads Kubernetes downward-API values and appends configured Service names.
    pub fn from_env(self_names: Vec<String>) -> Self {
        Self {
            pod_ip: env::var("POD_IP")
                .ok()
                .and_then(|value| value.parse::<IpAddr>().ok()),
            pod_name: env::var("POD_NAME")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            self_names,
        }
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        self.pod_name
            .iter()
            .map(String::as_str)
            .chain(self.self_names.iter().map(String::as_str))
    }
}

/// Boxed DNS lookup future, exposed so tests can inject deterministic answers.
pub type ResolveFuture<'a> = Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send + 'a>>;

/// DNS seam used by [`PeerDirectory`].
pub trait PeerResolver: Send + Sync {
    /// Answers with every address for the name, leaving self-exclusion to the caller.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a>;
}

/// Production resolver backed by Tokio's `lookup_host` (`getaddrinfo`).
#[derive(Debug, Default)]
pub struct TokioPeerResolver;

impl PeerResolver for TokioPeerResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        Box::pin(async move { lookup_host((host, port)).await.map(Iterator::collect) })
    }
}

#[derive(Debug, Default)]
struct DirectoryState {
    peers: Vec<SocketAddr>,
    negative_until: HashMap<SocketAddr, Instant>,
    next_refresh: Option<Instant>,
}

struct DirectoryInner {
    discovery: Discovery,
    refresh_interval: Duration,
    negative_ttl: Duration,
    self_identity: SelfIdentity,
    resolver: Arc<dyn PeerResolver>,
    refresh_lock: AsyncMutex<()>,
    state: RwLock<DirectoryState>,
}

/// Refreshing, self-excluding peer directory with a short negative cache.
#[derive(Clone)]
pub struct PeerDirectory {
    inner: Arc<DirectoryInner>,
}

impl PeerDirectory {
    /// Builds a directory using the system resolver and the supplied replica identity.
    pub fn new(
        discovery: Discovery,
        refresh_interval: Duration,
        negative_ttl: Duration,
        self_identity: SelfIdentity,
    ) -> Self {
        Self::with_resolver(
            discovery,
            refresh_interval,
            negative_ttl,
            self_identity,
            Arc::new(TokioPeerResolver),
        )
    }

    /// Same as [`Self::new`], taking this replica's identity from `POD_IP` and `POD_NAME`.
    pub fn from_env(
        discovery: Discovery,
        refresh_interval: Duration,
        negative_ttl: Duration,
        self_names: Vec<String>,
    ) -> Self {
        Self::new(
            discovery,
            refresh_interval,
            negative_ttl,
            SelfIdentity::from_env(self_names),
        )
    }

    /// Builds a directory over an injected DNS seam rather than the system resolver.
    pub fn with_resolver(
        discovery: Discovery,
        refresh_interval: Duration,
        negative_ttl: Duration,
        self_identity: SelfIdentity,
        resolver: Arc<dyn PeerResolver>,
    ) -> Self {
        Self {
            inner: Arc::new(DirectoryInner {
                discovery,
                refresh_interval,
                negative_ttl,
                self_identity,
                resolver,
                refresh_lock: AsyncMutex::new(()),
                state: RwLock::new(DirectoryState::default()),
            }),
        }
    }

    /// Resolves and atomically publishes a stable sorted peer set.
    pub async fn refresh(&self) -> Result<Vec<SocketAddr>> {
        let _refresh = self.inner.refresh_lock.lock().await;
        let mut resolved = match &self.inner.discovery {
            Discovery::Dns { host, port } => self
                .inner
                .resolver
                .resolve(host, *port)
                .await
                .map_err(|error| {
                    PeerError::Discovery(format!("resolving {host}:{port}: {error}"))
                })?,
            Discovery::Explicit(peers) => peers.clone(),
        };

        let port = match &self.inner.discovery {
            Discovery::Dns { port, .. } => *port,
            Discovery::Explicit(peers) => peers.first().map_or(0, SocketAddr::port),
        };
        let self_ips = self.resolve_self_ips(port).await;
        resolved.retain(|peer| !self_ips.contains(&peer.ip()));
        resolved.sort_unstable();
        resolved.dedup();

        let now = Instant::now();
        let mut state = write_lock(&self.inner.state);
        state
            .negative_until
            .retain(|peer, until| resolved.binary_search(peer).is_ok() && *until > now);
        state.peers = resolved.clone();
        state.next_refresh = Some(now + self.inner.refresh_interval);
        Ok(resolved)
    }

    /// Returns up to `limit` healthy peers in stable address order. A failed
    /// refresh may use the prior non-empty snapshot; an empty first refresh
    /// reports the discovery error so the caller can record it.
    pub async fn select(&self, limit: usize) -> Result<Vec<SocketAddr>> {
        if self.refresh_due(Instant::now())
            && let Err(error) = self.refresh().await
            && self.snapshot().is_empty()
        {
            return Err(error);
        }
        Ok(self.select_at(limit, Instant::now()))
    }

    /// Returns every discovered replica, including addresses currently marked
    /// failed. The proxy races this complete snapshot under one attempt budget,
    /// so stale negative observations cannot suppress a current holder.
    pub async fn select_all(&self) -> Result<Vec<SocketAddr>> {
        if self.refresh_due(Instant::now())
            && let Err(error) = self.refresh().await
            && self.snapshot().is_empty()
        {
            return Err(error);
        }
        Ok(self.snapshot())
    }

    /// Current resolved addresses, including temporarily suppressed peers.
    pub fn snapshot(&self) -> Vec<SocketAddr> {
        read_lock(&self.inner.state).peers.clone()
    }

    /// Suppresses a failed peer for the configured short TTL.
    pub fn mark_failed(&self, peer: SocketAddr) {
        self.mark_failed_at(peer, Instant::now());
    }

    /// Clears a peer's negative-cache entry after a successful request.
    pub fn mark_healthy(&self, peer: SocketAddr) {
        write_lock(&self.inner.state).negative_until.remove(&peer);
    }

    fn refresh_due(&self, now: Instant) -> bool {
        read_lock(&self.inner.state)
            .next_refresh
            .is_none_or(|deadline| deadline <= now)
    }

    fn mark_failed_at(&self, peer: SocketAddr, now: Instant) {
        let mut state = write_lock(&self.inner.state);
        if state.peers.binary_search(&peer).is_ok() {
            state
                .negative_until
                .insert(peer, now + self.inner.negative_ttl);
        }
    }

    fn select_at(&self, limit: usize, now: Instant) -> Vec<SocketAddr> {
        let state = read_lock(&self.inner.state);
        state
            .peers
            .iter()
            .filter(|peer| {
                state
                    .negative_until
                    .get(peer)
                    .is_none_or(|until| *until <= now)
            })
            .take(limit)
            .copied()
            .collect()
    }

    async fn resolve_self_ips(&self, port: u16) -> HashSet<IpAddr> {
        let mut ips = HashSet::new();
        if let Some(pod_ip) = self.inner.self_identity.pod_ip {
            ips.insert(pod_ip);
        }
        for name in self.inner.self_identity.names() {
            if let Ok(ip) = name.parse::<IpAddr>() {
                ips.insert(ip);
                continue;
            }
            if let Ok(addresses) = self.inner.resolver.resolve(name, port).await {
                ips.extend(addresses.into_iter().map(|address| address.ip()));
            }
        }
        ips
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Peer transport. TLS roots include public WebPKI roots and, when supplied,
/// every certificate in the private CA PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTransport {
    Plaintext,
    Tls {
        ca_certificate_path: Option<PathBuf>,
        expected_server_name: String,
    },
}

/// Peer-client timeout policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerClientOptions {
    pub transport: PeerTransport,
    pub probe_timeout: Duration,
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub stall_timeout: Duration,
}

impl Default for PeerClientOptions {
    fn default() -> Self {
        Self {
            transport: PeerTransport::Plaintext,
            probe_timeout: Duration::from_millis(50),
            connect_timeout: Duration::from_millis(100),
            first_byte_timeout: Duration::from_millis(500),
            stall_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
struct TlsClient {
    connector: TlsConnector,
    server_name: ServerName<'static>,
    grpc: ClientTlsConfig,
}

/// Minimal HTTP/1.1 peer client. It deliberately owns no selection policy:
/// callers obtain addresses from [`PeerDirectory`] and can race probes.
#[derive(Clone)]
pub struct PeerClient {
    options: PeerClientOptions,
    tls: Option<TlsClient>,
    grpc_channels: Arc<RwLock<HashMap<SocketAddr, Arc<OnceCell<Channel>>>>>,
}

impl PeerClient {
    /// Rejects a zero timeout and prepares the TLS client the transport calls for.
    pub fn new(options: PeerClientOptions) -> Result<Self> {
        for (name, duration) in [
            ("probe_timeout", options.probe_timeout),
            ("connect_timeout", options.connect_timeout),
            ("first_byte_timeout", options.first_byte_timeout),
            ("stall_timeout", options.stall_timeout),
        ] {
            if duration.is_zero() {
                return Err(PeerError::InvalidConfig(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        let tls = match &options.transport {
            PeerTransport::Plaintext => None,
            PeerTransport::Tls {
                ca_certificate_path,
                expected_server_name,
            } => Some(build_tls_client(
                ca_certificate_path.as_ref(),
                expected_server_name,
            )?),
        };
        Ok(Self {
            options,
            tls,
            grpc_channels: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Deadline bounding one complete probe, channel setup included.
    pub fn probe_timeout(&self) -> Duration {
        self.options.probe_timeout
    }

    /// Unary gRPC WhoHas request bounded by one complete probe deadline.
    pub async fn probe(
        &self,
        peer: SocketAddr,
        identity: &ObjectIdentity,
        validator: &ExactValidator,
    ) -> Result<ProbeResponse> {
        let request = who_has_request(identity, validator)?;
        timeout(self.options.probe_timeout, async {
            let channel = self.grpc_channel(peer).await?;
            let response = proto::peer_control_client::PeerControlClient::new(channel)
                .who_has(request)
                .await
                .map_err(|error| PeerError::Http(format!("WhoHas RPC to {peer}: {error}")))?
                .into_inner();
            let object_status = proto::ObjectStatus::try_from(response.object_status)
                .unwrap_or(proto::ObjectStatus::Unspecified);
            let response_validator = response
                .validator
                .map(ExactValidator::try_from)
                .transpose()?;
            Ok(ProbeResponse {
                peer,
                object_status,
                http_status: response.http_status,
                object_size: response.object_size,
                validator: response_validator,
                max_idle_secs: response.max_idle_seconds,
            })
        })
        .await
        .map_err(|_| PeerError::Timeout(TimeoutStage::Probe))?
    }

    /// GET an object while leaving its body unread and stall-protected.
    pub async fn open_object(
        &self,
        peer: SocketAddr,
        request_target: &str,
        forwarded_headers: &[(String, String)],
    ) -> Result<OpenPeerResponse> {
        self.request_object(peer, request_target, forwarded_headers)
            .await
    }

    async fn grpc_channel(&self, peer: SocketAddr) -> Result<Channel> {
        let cell = {
            if let Some(cell) = read_lock(&self.grpc_channels).get(&peer) {
                Arc::clone(cell)
            } else {
                let mut channels = write_lock(&self.grpc_channels);
                Arc::clone(
                    channels
                        .entry(peer)
                        .or_insert_with(|| Arc::new(OnceCell::new())),
                )
            }
        };
        cell.get_or_try_init(|| self.connect_grpc_channel(peer))
            .await
            .cloned()
    }

    async fn connect_grpc_channel(&self, peer: SocketAddr) -> Result<Channel> {
        let authority = self.tls.as_ref().map_or_else(
            || peer.to_string(),
            |tls| tls.server_name.to_str().into_owned(),
        );
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        let mut endpoint = Endpoint::from_shared(format!("{scheme}://{authority}"))
            .map_err(|error| PeerError::Http(format!("building WhoHas endpoint: {error}")))?;
        if let Some(tls) = &self.tls {
            endpoint = endpoint
                .tls_config(tls.grpc.clone())
                .map_err(|error| PeerError::Tls(format!("configuring WhoHas TLS: {error}")))?;
        }
        endpoint
            .connect_with_connector(service_fn(move |_: http::Uri| async move {
                let tcp = TcpStream::connect(peer).await?;
                tcp.set_nodelay(true)?;
                Ok::<_, io::Error>(TokioIo::new(tcp))
            }))
            .await
            .map_err(|error| PeerError::Http(format!("connecting WhoHas RPC to {peer}: {error}")))
    }

    /// Drops channels for replicas no longer present in the discovery snapshot.
    pub fn retain_grpc_channels(&self, peers: &[SocketAddr]) {
        write_lock(&self.grpc_channels).retain(|peer, _| peers.contains(peer));
    }

    #[cfg(test)]
    fn grpc_channel_count(&self) -> usize {
        read_lock(&self.grpc_channels)
            .values()
            .filter(|channel| channel.get().is_some())
            .count()
    }

    async fn request_object(
        &self,
        peer: SocketAddr,
        request_target: &str,
        forwarded_headers: &[(String, String)],
    ) -> Result<OpenPeerResponse> {
        let method_name = Method::GET.as_str().to_owned();
        let request_started = Instant::now();
        tracing::debug!(
            event = "peer_request_stage",
            peer = %peer,
            method = %method_name,
            stage = "connect_start",
        );
        let connect_started = Instant::now();
        let io = timeout(self.options.connect_timeout, self.connect(peer))
            .await
            .map_err(|_| PeerError::Timeout(TimeoutStage::Connect))??;
        tracing::debug!(
            event = "peer_request_stage",
            peer = %peer,
            method = %method_name,
            stage = "transport_connected",
            duration_seconds = connect_started.elapsed().as_secs_f64(),
        );
        let handshake_started = Instant::now();
        let (mut sender, connection) = timeout(
            self.options.connect_timeout,
            http1::handshake(TokioIo::new(io)),
        )
        .await
        .map_err(|_| PeerError::Timeout(TimeoutStage::Connect))?
        .map_err(|error| PeerError::Http(format!("HTTP/1 handshake: {error}")))?;
        tracing::debug!(
            event = "peer_request_stage",
            peer = %peer,
            method = %method_name,
            stage = "http_ready",
            duration_seconds = handshake_started.elapsed().as_secs_f64(),
        );
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let mut request = Request::builder()
            .method(Method::GET)
            .uri(request_target)
            .body(Empty::<Bytes>::new())
            .map_err(|error| PeerError::Http(format!("building request: {error}")))?;
        let connection_tokens = forwarded_headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
            .flat_map(|(_, value)| value.split(','))
            .map(|value| value.trim().to_ascii_lowercase())
            .collect::<HashSet<_>>();
        for (name, value) in forwarded_headers {
            if is_peer_transport_header(name, &connection_tokens) {
                continue;
            }
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|error| PeerError::Http(format!("invalid forwarded header: {error}")))?;
            let value = HeaderValue::from_str(value).map_err(|error| {
                PeerError::Http(format!("invalid forwarded header value: {error}"))
            })?;
            request.headers_mut().append(name, value);
        }
        request
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
        request.headers_mut().insert(
            HOST,
            forwarded_headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("host"))
                .map(|(_, value)| HeaderValue::from_str(value))
                .unwrap_or_else(|| HeaderValue::from_str(&peer.to_string()))
                .map_err(|error| PeerError::Http(format!("invalid Host header: {error}")))?,
        );
        request.headers_mut().insert(
            HeaderName::from_static(X_OVCC_PEER_DEPTH),
            HeaderValue::from_static("1"),
        );

        let first_byte_started = Instant::now();
        let response = timeout(
            self.options.first_byte_timeout,
            sender.send_request(request),
        )
        .await
        .map_err(|_| PeerError::Timeout(TimeoutStage::FirstByte))?
        .map_err(|error| PeerError::Http(format!("request to {peer}: {error}")))?;
        let status = response.status();
        let headers = response.headers().clone();
        tracing::debug!(
            event = "peer_request_stage",
            peer = %peer,
            method = %method_name,
            stage = "response_head",
            status = status.as_u16(),
            first_byte_seconds = first_byte_started.elapsed().as_secs_f64(),
            total_seconds = request_started.elapsed().as_secs_f64(),
        );
        Ok(OpenPeerResponse {
            peer,
            status,
            headers,
            body: PeerBody::new(response.into_body(), self.options.stall_timeout),
        })
    }

    async fn connect(&self, peer: SocketAddr) -> Result<Box<dyn AsyncIo>> {
        let tcp = TcpStream::connect(peer).await?;
        tcp.set_nodelay(true)?;
        let Some(tls) = &self.tls else {
            return Ok(Box::new(tcp));
        };
        let stream = tls
            .connector
            .connect(tls.server_name.clone(), tcp)
            .await
            .map_err(|error| PeerError::Tls(format!("handshake with {peer}: {error}")))?;
        Ok(Box::new(stream))
    }
}

fn is_peer_transport_header(name: &str, connection_tokens: &HashSet<String>) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || connection_tokens.contains(&lower)
        || name.to_ascii_lowercase().starts_with("x-ovcc-")
}

fn build_tls_client(
    ca_certificate_path: Option<&PathBuf>,
    expected_server_name: &str,
) -> Result<TlsClient> {
    let server_name = ServerName::try_from(expected_server_name.to_owned())
        .map_err(|error| PeerError::InvalidConfig(format!("expected ServerName: {error}")))?;

    let mut roots =
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut grpc = ClientTlsConfig::new()
        .domain_name(expected_server_name.to_owned())
        .with_webpki_roots();
    if let Some(path) = ca_certificate_path {
        let pem = ovcc_tls::add_peer_ca(path, &mut roots).map_err(|error| match error {
            ovcc_tls::PeerCaError::Io(error) => PeerError::Io(error),
            ovcc_tls::PeerCaError::Tls(message) => PeerError::Tls(message),
        })?;
        grpc = grpc.ca_certificate(Certificate::from_pem(pem));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsClient {
        connector: TlsConnector::from(Arc::new(config)),
        server_name,
        grpc,
    })
}

trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// gRPC WhoHas response used for peer selection.
pub struct ProbeResponse {
    pub peer: SocketAddr,
    pub object_status: proto::ObjectStatus,
    pub http_status: u32,
    pub object_size: u64,
    pub validator: Option<ExactValidator>,
    pub max_idle_secs: Option<u64>,
}

/// Peer response with an unread streaming body.
pub struct OpenPeerResponse {
    pub peer: SocketAddr,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: PeerBody,
}

/// Body read error, including an explicit no-progress timeout.
#[derive(Debug)]
pub enum PeerBodyError {
    Transport(hyper::Error),
    StallTimeout(Duration),
}

impl fmt::Display for PeerBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "peer body transport failed: {error}"),
            Self::StallTimeout(duration) => {
                write!(f, "peer body made no progress for {duration:?}")
            }
        }
    }
}

impl std::error::Error for PeerBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::StallTimeout(_) => None,
        }
    }
}

/// Reads of a peer body stay in the peer category, so a stall and a transport
/// failure classify the same way an aborted probe does.
impl From<PeerBodyError> for ovcc_core::Error {
    fn from(error: PeerBodyError) -> Self {
        Self::upstream(error.to_string())
    }
}

/// Streaming Hyper body that fails if no frame arrives within `stall_timeout`.
pub struct PeerBody {
    inner: Incoming,
    stall_timeout: Duration,
    stall: Pin<Box<Sleep>>,
    failed: bool,
}

impl PeerBody {
    fn new(inner: Incoming, stall_timeout: Duration) -> Self {
        Self {
            inner,
            stall_timeout,
            stall: Box::pin(tokio::time::sleep(stall_timeout)),
            failed: false,
        }
    }
}

impl Body for PeerBody {
    type Data = Bytes;
    type Error = PeerBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                let deadline = tokio::time::Instant::now() + self.stall_timeout;
                self.stall.as_mut().reset(deadline);
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.failed = true;
                Poll::Ready(Some(Err(PeerBodyError::Transport(error))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if self.stall.as_mut().poll(context).is_ready() {
                    self.failed = true;
                    Poll::Ready(Some(Err(PeerBodyError::StallTimeout(self.stall_timeout))))
                } else {
                    Poll::Pending
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.failed || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use http_body_util::BodyExt;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status as GrpcStatus};

    type TestResult = std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn identity() -> ObjectIdentity {
        ObjectIdentity {
            host: "bucket.example.com".to_owned(),
            method: Method::GET,
            uri: "/objects/a.usd".to_owned(),
            args: "part=1".to_owned(),
            if_match: "\"etag-1\"".to_owned(),
            range: "bytes=0-99".to_owned(),
        }
    }

    fn peer_error<T>(
        result: Result<T>,
        message: &str,
    ) -> std::result::Result<PeerError, io::Error> {
        match result {
            Ok(_) => Err(io::Error::other(message)),
            Err(error) => Ok(error),
        }
    }

    #[test]
    fn who_has_protobuf_round_trips_structured_identity_and_validator() -> TestResult {
        // Arrange.
        let expected_identity = identity();
        let expected_validator = ExactValidator::Etag("\"etag-1\"".to_owned());

        // Act.
        let request = who_has_request(&expected_identity, &expected_validator)?;
        let (actual_identity, actual_validator) = validate_who_has_request(request)?;

        // Assert.
        assert_eq!(actual_identity, expected_identity);
        assert_eq!(actual_validator, expected_validator);
        Ok(())
    }

    #[test]
    fn who_has_protobuf_rejects_missing_validator() -> TestResult {
        // Arrange.
        let request = proto::WhoHasRequest {
            optional_keys: None,
            identity: Some((&identity()).into()),
            validator: None,
        };

        // Act.
        let error = validate_who_has_request(request);

        // Assert.
        assert!(
            peer_error(error, "missing validator unexpectedly decoded")?
                .to_string()
                .contains("missing")
        );
        Ok(())
    }

    #[test]
    fn who_has_protobuf_rejects_oversized_and_hostile_values() -> TestResult {
        // Arrange.
        let mut oversized = identity();
        oversized.uri = format!("/{}", "a".repeat(MAX_URI_BYTES));
        let mut hostile = identity();
        hostile.uri = "/safe\\smuggled".to_owned();
        let validator = ExactValidator::LastModified("Sat, 15 Aug 2026 00:00:00 GMT".to_owned());

        // Act.
        let oversized_error = who_has_request(&oversized, &validator);
        let hostile_error = who_has_request(&hostile, &validator);

        // Assert.
        assert!(
            peer_error(oversized_error, "oversized URI unexpectedly decoded")?
                .to_string()
                .contains("exceeds")
        );
        assert!(
            peer_error(hostile_error, "hostile URI unexpectedly encoded")?
                .to_string()
                .contains("backslash")
        );
        Ok(())
    }

    #[derive(Default)]
    struct FakeResolver {
        answers: Mutex<HashMap<(String, u16), Vec<SocketAddr>>>,
    }

    impl FakeResolver {
        fn set(&self, host: &str, port: u16, addresses: Vec<SocketAddr>) {
            let mut answers = self
                .answers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            answers.insert((host.to_owned(), port), addresses);
        }
    }

    impl PeerResolver for FakeResolver {
        fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
            Box::pin(async move {
                self.answers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&(host.to_owned(), port))
                    .cloned()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, host.to_owned()))
            })
        }
    }

    #[tokio::test]
    async fn directory_dns_refresh_excludes_self_and_sorts_stably() -> TestResult {
        // Arrange.
        let resolver = Arc::new(FakeResolver::default());
        resolver.set(
            "peer.test",
            14_130,
            vec![
                "10.0.0.3:14130".parse()?,
                "10.0.0.1:14130".parse()?,
                "10.0.0.2:14130".parse()?,
                "10.0.0.3:14130".parse()?,
            ],
        );
        resolver.set("pod-2.peer.test", 14_130, vec!["10.0.0.2:14130".parse()?]);
        let directory = PeerDirectory::with_resolver(
            Discovery::Dns {
                host: "peer.test".to_owned(),
                port: 14_130,
            },
            Duration::from_secs(30),
            Duration::from_secs(1),
            SelfIdentity {
                pod_ip: Some("10.0.0.1".parse()?),
                pod_name: Some("pod-2.peer.test".to_owned()),
                self_names: Vec::new(),
            },
            resolver,
        );

        // Act.
        let peers = directory.refresh().await?;

        // Assert.
        assert_eq!(peers, vec!["10.0.0.3:14130".parse()?]);
        assert_eq!(directory.select(8).await?, peers);
        Ok(())
    }

    #[tokio::test]
    async fn directory_refresh_replaces_dns_membership_deterministically() -> TestResult {
        // Arrange.
        let resolver = Arc::new(FakeResolver::default());
        resolver.set("peer.test", 14_130, vec!["10.0.0.2:14130".parse()?]);
        let directory = PeerDirectory::with_resolver(
            Discovery::Dns {
                host: "peer.test".to_owned(),
                port: 14_130,
            },
            Duration::from_secs(30),
            Duration::from_secs(1),
            SelfIdentity::default(),
            resolver.clone(),
        );
        directory.refresh().await?;
        resolver.set(
            "peer.test",
            14_130,
            vec!["10.0.0.4:14130".parse()?, "10.0.0.3:14130".parse()?],
        );

        // Act.
        let refreshed = directory.refresh().await?;

        // Assert.
        assert_eq!(
            refreshed,
            vec!["10.0.0.3:14130".parse()?, "10.0.0.4:14130".parse()?]
        );
        Ok(())
    }

    #[tokio::test]
    async fn directory_negative_cache_is_per_peer_and_expires() -> TestResult {
        // Arrange.
        let first: SocketAddr = "10.0.0.2:14130".parse()?;
        let second: SocketAddr = "10.0.0.3:14130".parse()?;
        let directory = PeerDirectory::new(
            Discovery::Explicit(vec![second, first]),
            Duration::from_secs(30),
            Duration::from_secs(1),
            SelfIdentity::default(),
        );
        directory.refresh().await?;
        let now = Instant::now();
        directory.mark_failed_at(first, now);

        // Act.
        let suppressed = directory.select_at(8, now);
        let all = directory.select_all().await?;
        let restored = directory.select_at(8, now + Duration::from_secs(1));

        // Assert.
        assert_eq!(suppressed, vec![second]);
        assert_eq!(all, vec![first, second]);
        assert_eq!(restored, vec![first, second]);
        Ok(())
    }

    async fn read_request_head<S>(stream: &mut S) -> io::Result<()>
    where
        S: AsyncRead + Unpin,
    {
        let mut bytes = vec![0_u8; 4096];
        let mut read = 0;
        while read < bytes.len() {
            let count = stream.read(&mut bytes[read..]).await?;
            if count == 0 {
                break;
            }
            read += count;
            if bytes[..read].windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        Ok(())
    }

    async fn spawn_plain_response(response: &'static [u8]) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let result: io::Result<()> = async {
                let (mut stream, _) = listener.accept().await?;
                read_request_head(&mut stream).await?;
                stream.write_all(response).await?;
                stream.shutdown().await
            }
            .await;
            let _ = result;
        });
        Ok(address)
    }

    #[derive(Clone)]
    struct TestPeerControl {
        delay: Duration,
    }

    #[tonic::async_trait]
    impl proto::peer_control_server::PeerControl for TestPeerControl {
        async fn who_has(
            &self,
            request: GrpcRequest<proto::WhoHasRequest>,
        ) -> std::result::Result<GrpcResponse<proto::WhoHasResponse>, GrpcStatus> {
            tokio::time::sleep(self.delay).await;
            let validator = request.into_inner().validator;
            Ok(GrpcResponse::new(proto::WhoHasResponse {
                omitted_parts: Vec::new(),
                optional_keys_ack: String::new(),
                object_status: proto::ObjectStatus::Committed as i32,
                http_status: 200,
                object_size: 8192,
                validator,
                max_idle_seconds: Some(600),
            }))
        }
    }

    async fn spawn_grpc_peer(delay: Duration) -> io::Result<SocketAddr> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(proto::peer_control_server::PeerControlServer::new(
                    TestPeerControl { delay },
                ))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
        });
        Ok(address)
    }

    #[tokio::test]
    async fn peer_client_get_forwards_original_target_and_credentials() -> TestResult {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut bytes = vec![0_u8; 4096];
                let mut read = 0;
                while read < bytes.len() {
                    let Ok(count) = stream.read(&mut bytes[read..]).await else {
                        return;
                    };
                    if count == 0 {
                        break;
                    }
                    read += count;
                    if bytes[..read].windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = request_tx.send(String::from_utf8_lossy(&bytes[..read]).into_owned());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        });
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;
        let forwarded = vec![
            ("Host".to_owned(), "bucket.example.com".to_owned()),
            ("Authorization".to_owned(), "Bearer delegated".to_owned()),
            ("If-Match".to_owned(), "\"etag-1\"".to_owned()),
            (
                "Connection".to_owned(),
                "keep-alive, X-Remove-Me".to_owned(),
            ),
            ("X-Remove-Me".to_owned(), "secret".to_owned()),
            ("X-OVCC-Old".to_owned(), "remove".to_owned()),
        ];

        // Act.
        let response = client
            .open_object(address, "/objects/a.usd?signature=raw", &forwarded)
            .await?;
        let body = response.body.collect().await?.to_bytes();
        let request = request_rx.await?.to_ascii_lowercase();

        // Assert.
        assert!(request.starts_with("get /objects/a.usd?signature=raw http/1.1\r\n"));
        assert!(request.contains("\r\nauthorization: bearer delegated\r\n"));
        assert!(request.contains("\r\nif-match: \"etag-1\"\r\n"));
        assert!(request.contains("\r\nhost: bucket.example.com\r\n"));
        assert!(request.contains("\r\nx-ovcc-peer-depth: 1\r\n"));
        assert!(!request.contains("x-remove-me"));
        assert!(!request.contains("x-ovcc-old"));
        assert_eq!(body, Bytes::from_static(b"ok"));
        Ok(())
    }

    #[tokio::test]
    async fn peer_client_plaintext_returns_streaming_status_headers_and_body() -> TestResult {
        // Arrange.
        let address = spawn_plain_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"v1\"\r\nConnection: close\r\n\r\ndata",
        )
        .await?;
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;

        // Act.
        let response = client
            .open_object(address, "/objects/a.usd?part=1", &[])
            .await?;
        let status = response.status;
        let etag = response.headers.get("etag").cloned();
        let body = response.body.collect().await?.to_bytes();

        // Assert.
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            etag.as_ref().and_then(|value| value.to_str().ok()),
            Some("\"v1\"")
        );
        assert_eq!(body, Bytes::from_static(b"data"));
        Ok(())
    }

    #[tokio::test]
    async fn peer_grpc_who_has_returns_structured_metadata() -> TestResult {
        // Arrange.
        let address = spawn_grpc_peer(Duration::ZERO).await?;
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;
        let validator = ExactValidator::Etag("\"v2\"".to_owned());

        // Act.
        let probe = client.probe(address, &identity(), &validator).await?;
        let second = client.probe(address, &identity(), &validator).await?;

        // Assert.
        assert_eq!(probe.object_status, proto::ObjectStatus::Committed);
        assert_eq!(probe.http_status, 200);
        assert_eq!(probe.object_size, 8192);
        assert_eq!(probe.validator, Some(validator));
        assert_eq!(probe.max_idle_secs, Some(600));
        assert_eq!(second.object_status, proto::ObjectStatus::Committed);
        assert_eq!(client.grpc_channel_count(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn peer_grpc_channel_retention_accepts_shuffled_snapshot() -> TestResult {
        // Arrange.
        let first = spawn_grpc_peer(Duration::ZERO).await?;
        let second = spawn_grpc_peer(Duration::ZERO).await?;
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;
        let validator = ExactValidator::Etag("\"v2\"".to_owned());
        client.probe(first, &identity(), &validator).await?;
        client.probe(second, &identity(), &validator).await?;
        let mut shuffled = vec![first, second];
        shuffled.sort_unstable();
        shuffled.reverse();

        // Act.
        client.retain_grpc_channels(&shuffled);

        // Assert.
        assert_eq!(client.grpc_channel_count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn peer_grpc_who_has_enforces_complete_probe_timeout() -> TestResult {
        // Arrange.
        let address = spawn_grpc_peer(Duration::from_millis(100)).await?;
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_millis(10),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;

        // Act.
        let error = match client
            .probe(
                address,
                &identity(),
                &ExactValidator::Etag("\"v2\"".to_owned()),
            )
            .await
        {
            Ok(_) => return Err(io::Error::other("probe unexpectedly succeeded").into()),
            Err(error) => error,
        };

        // Assert.
        assert_eq!(error.timeout_stage(), Some(TimeoutStage::Probe));
        Ok(())
    }

    #[tokio::test]
    async fn peer_client_enforces_first_byte_timeout() -> TestResult {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = read_request_head(&mut stream).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_millis(10),
            stall_timeout: Duration::from_secs(1),
            ..PeerClientOptions::default()
        })?;

        // Act.
        let error = match client
            .open_object(address, "/objects/a.usd?part=1", &[])
            .await
        {
            Ok(_) => {
                return Err(io::Error::other("first-byte wait unexpectedly succeeded").into());
            }
            Err(error) => error,
        };

        // Assert.
        assert_eq!(error.timeout_stage(), Some(TimeoutStage::FirstByte));
        Ok(())
    }

    #[tokio::test]
    async fn peer_body_enforces_no_progress_stall_timeout() -> TestResult {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = read_request_head(&mut stream).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\na",
                    )
                    .await;
                let _ = stream.flush().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let client = PeerClient::new(PeerClientOptions {
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_millis(10),
            ..PeerClientOptions::default()
        })?;
        let mut response = client
            .open_object(address, "/objects/a.usd?part=1", &[])
            .await?;

        // Act.
        let first = response.body.frame().await.transpose()?;
        let stalled = response.body.frame().await;

        // Assert.
        assert_eq!(
            first.and_then(|frame| frame.into_data().ok()),
            Some(Bytes::from_static(b"a"))
        );
        assert!(matches!(
            stalled,
            Some(Err(PeerBodyError::StallTimeout(duration)))
                if duration == Duration::from_millis(10)
        ));
        Ok(())
    }

    async fn spawn_tls_response(
        dns_name: &str,
        response: &'static [u8],
        ca_path: &std::path::Path,
    ) -> std::result::Result<SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
        let issued = rcgen::generate_simple_self_signed(vec![dns_name.to_owned()])?;
        std::fs::write(ca_path, issued.cert.pem())?;
        let certificate = issued.cert.der().clone();
        let key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)?;
        let acceptor = TlsAcceptor::from(Arc::new(server));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await
                && let Ok(mut stream) = acceptor.accept(stream).await
            {
                let _ = read_request_head(&mut stream).await;
                let _ = stream.write_all(response).await;
                let _ = stream.shutdown().await;
            }
        });
        Ok(address)
    }

    #[tokio::test]
    async fn peer_client_tls_dials_ip_and_pins_expected_server_name() -> TestResult {
        // Arrange.
        let temp = tempfile::tempdir()?;
        let ca_path = temp.path().join("ca.crt");
        let address = spawn_tls_response(
            "peer.internal",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            &ca_path,
        )
        .await?;
        let client = PeerClient::new(PeerClientOptions {
            transport: PeerTransport::Tls {
                ca_certificate_path: Some(ca_path),
                expected_server_name: "peer.internal".to_owned(),
            },
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
        })?;

        // Act.
        let response = client
            .open_object(address, "/objects/a.usd?part=1", &[])
            .await?;
        let body = response.body.collect().await?.to_bytes();

        // Assert.
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(body, Bytes::from_static(b"ok"));
        Ok(())
    }

    #[tokio::test]
    async fn peer_client_tls_rejects_certificate_for_another_server_name() -> TestResult {
        // Arrange.
        let temp = tempfile::tempdir()?;
        let ca_path = temp.path().join("ca.crt");
        let address = spawn_tls_response(
            "peer.internal",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            &ca_path,
        )
        .await?;
        let client = PeerClient::new(PeerClientOptions {
            transport: PeerTransport::Tls {
                ca_certificate_path: Some(ca_path),
                expected_server_name: "other.internal".to_owned(),
            },
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
        })?;

        // Act.
        let error = peer_error(
            client
                .open_object(address, "/objects/a.usd?part=1", &[])
                .await,
            "wrong ServerName unexpectedly succeeded",
        )?;

        // Assert.
        assert!(matches!(error, PeerError::Tls(_)));
        Ok(())
    }

    #[tokio::test]
    async fn peer_client_connect_timeout_includes_tls_handshake() -> TestResult {
        // Arrange.
        let issued = rcgen::generate_simple_self_signed(vec!["peer.internal".to_owned()])?;
        let temp = tempfile::tempdir()?;
        let ca_path = temp.path().join("ca.crt");
        std::fs::write(&ca_path, issued.cert.pem())?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Ok((_stream, _)) = listener.accept().await {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let client = PeerClient::new(PeerClientOptions {
            transport: PeerTransport::Tls {
                ca_certificate_path: Some(ca_path),
                expected_server_name: "peer.internal".to_owned(),
            },
            probe_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_millis(10),
            first_byte_timeout: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(1),
        })?;

        // Act.
        let error = peer_error(
            client
                .open_object(address, "/objects/a.usd?part=1", &[])
                .await,
            "stalled TLS handshake unexpectedly connected",
        )?;

        // Assert.
        assert_eq!(error.timeout_stage(), Some(TimeoutStage::Connect));
        Ok(())
    }

    #[test]
    fn peer_and_peer_body_failures_bridge_into_the_upstream_error_kind() {
        // Arrange.
        let probe = PeerError::Tls("handshake refused".to_owned());
        let body = PeerBodyError::StallTimeout(Duration::from_secs(1));

        // Act.
        let probe = ovcc_core::Error::from(probe);
        let body = ovcc_core::Error::from(body);

        // Assert.
        assert_eq!(probe.kind(), ovcc_core::ErrorKind::Upstream);
        assert_eq!(probe.message(), "peer TLS failed: handshake refused");
        assert_eq!(body.kind(), ovcc_core::ErrorKind::Upstream);
        assert_eq!(body.message(), "peer body made no progress for 1s");
    }
}
