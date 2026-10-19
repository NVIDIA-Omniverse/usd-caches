// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Upstream/origin HTTP client (plaintext **or** TLS).
//!
//! The contract is `OriginResponse` + `fetch`. A pooled `https_or_http` rustls
//! connector reaches both plaintext and TLS origins.
//!
//! Note that [`fetch`] returns the body buffered for simplicity against the
//! small-object mock origin and registry rewrite paths. [`open_fetch_with_headers`]
//! leaves the body unread for streaming fills (origin → storage → client).

mod resolver;

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::{BodyExt, Empty};
use hyper::Uri;
use hyper::header::{ETAG, HOST, HeaderName, HeaderValue};
use hyper::{Method, Request};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::{Connect, HttpConnector};
use hyper_util::rt::TokioExecutor;
use ovcc_core::{Error, Result};
use ovcc_tls::{load_root_store, load_root_store_with_system_policy};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::{mpsc::Receiver, oneshot};
use tokio::time::{Sleep, timeout};
use tower_service::Service as TowerService;

pub use resolver::ResolverOptions;

const DEFAULT_CA_BUNDLE_PATH: &str = ovcc_tls::SYSTEM_CA_BUNDLE_PATH;
const DEFAULT_TLS_VERIFY_DEPTH: usize = 2;

/// The pooled-client connector type, wrapping connection-time measurement around
/// a rustls TLS layer over the plaintext `HttpConnector`. The inner connector is
/// built with `https_or_http`, so the SAME client serves both a plaintext origin
/// (the integration mock over `http://`) and a real TLS origin (S3/GCS/Azure
/// over `https://$host`) — the engine picks per the URL scheme.
type OriginConnector = TimedConnector<HttpsConnector<HttpConnector<resolver::CachedResolver>>>;
type BodilessClient = Client<OriginConnector, Empty<Bytes>>;
type StreamingClient = Client<OriginConnector, ChannelBody>;
type BodilessClients = Mutex<std::collections::HashMap<(String, bool), Arc<BodilessClient>>>;
type StreamingClients = Mutex<std::collections::HashMap<(String, bool), Arc<StreamingClient>>>;

/// DNS and connection-pool policy installed before the first origin request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamOptions {
    pub resolver: ResolverOptions,
    pub ca_bundle_path: PathBuf,
    pub tls_verify_depth: usize,
    pub connect_timeout: Duration,
    pub response_header_timeout: Duration,
    pub body_stall_timeout: Duration,
    pub pool_idle_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub max_requests_per_connection: u64,
}

impl Default for UpstreamOptions {
    fn default() -> Self {
        Self {
            resolver: ResolverOptions::default(),
            ca_bundle_path: PathBuf::from(DEFAULT_CA_BUNDLE_PATH),
            tls_verify_depth: DEFAULT_TLS_VERIFY_DEPTH,
            connect_timeout: Duration::from_secs(60),
            response_header_timeout: Duration::from_secs(60),
            body_stall_timeout: Duration::from_secs(60),
            pool_idle_timeout: Duration::from_secs(60),
            pool_max_idle_per_host: 200,
            max_requests_per_connection: 0,
        }
    }
}

static UPSTREAM_OPTIONS: OnceLock<UpstreamOptions> = OnceLock::new();
static ORIGIN_REQUEST_COUNTS: OnceLock<Mutex<std::collections::HashMap<String, u64>>> =
    OnceLock::new();

/// Installs process-wide origin transport policy before clients are created.
pub fn configure(options: UpstreamOptions) -> Result<()> {
    validate_options(&options)?;
    install_options(&UPSTREAM_OPTIONS, options)
}

fn validate_options(options: &UpstreamOptions) -> Result<()> {
    load_root_store(&options.ca_bundle_path)?;
    for (name, value) in [
        ("connect_timeout", options.connect_timeout),
        ("response_header_timeout", options.response_header_timeout),
        ("body_stall_timeout", options.body_stall_timeout),
    ] {
        if value.is_zero() {
            return Err(Error::config(format!(
                "origin {name} must be greater than zero"
            )));
        }
    }
    resolver::validate_options(&options.resolver).map_err(|error| {
        Error::config(format!("invalid origin resolver configuration: {error}"))
    })?;
    Ok(())
}

fn install_options(cell: &OnceLock<UpstreamOptions>, options: UpstreamOptions) -> Result<()> {
    if let Some(existing) = cell.get() {
        if existing != &options {
            return Err(Error::config(
                "origin transport policy was already initialized with different values",
            ));
        }
        return Ok(());
    }
    match cell.set(options) {
        Ok(()) => Ok(()),
        Err(options) if cell.get() == Some(&options) => Ok(()),
        Err(_) => Err(Error::config(
            "origin transport policy was concurrently initialized with different values",
        )),
    }
}

/// Applies a nameserver override while an origin request future is polled.
pub fn with_nameserver_override<T>(
    nameserver: Option<String>,
    future: impl Future<Output = T>,
) -> impl Future<Output = T> {
    resolver::with_nameserver_override(nameserver, future)
}

/// Builds the shared HTTPS-capable connector from the configured PEM trust
/// bundle. `https_or_http` keeps plaintext origins working (the mock), while
/// rustls derives verification identity and SNI from the URI hostname even
/// after the resolver returns a TCP IP address.
fn origin_connector(options: &UpstreamOptions) -> Result<OriginConnector> {
    let resolver = resolver::CachedResolver::new(options.resolver.clone()).map_err(|error| {
        Error::config(format!("failed to construct origin DNS resolver: {error}"))
    })?;
    let mut http = HttpConnector::new_with_resolver(resolver);
    // `HttpConnector` rejects `https://` URIs by default. The rustls wrapper
    // owns scheme handling, so its inner TCP connector must accept both.
    http.enforce_http(false);
    let inner = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(build_tls_config(
            &options.ca_bundle_path,
            options.tls_verify_depth,
        )?)
        .https_or_http()
        .with_server_name_resolver(origin_server_name)
        .enable_http1()
        .wrap_connector(http);
    Ok(TimedConnector {
        inner,
        connect_timeout: options.connect_timeout,
    })
}

/// Resolves TLS verification identity and SNI explicitly from the origin URI.
/// `Uri::host` excludes the port; bracket trimming retains valid IPv6 literals.
fn origin_server_name(
    uri: &Uri,
) -> std::result::Result<ServerName<'static>, Box<dyn std::error::Error + Send + Sync>> {
    let host = uri.host().ok_or_else(|| {
        Box::<dyn std::error::Error + Send + Sync>::from("origin URI has no host")
    })?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    ServerName::try_from(host.to_owned()).map_err(|error| Box::new(error) as _)
}

fn build_tls_config(path: &Path, verify_depth: usize) -> Result<rustls::ClientConfig> {
    let (roots, system_policy) = load_root_store_with_system_policy(path)?;
    let roots = Arc::new(roots);
    let verifier = WebPkiServerVerifier::builder(roots)
        .build()
        .map_err(|error| Error::config(format!("failed to build origin TLS verifier: {error}")))?;
    let verifier = Arc::new(DepthLimitedServerVerifier {
        inner: verifier,
        max_intermediates: verify_depth,
        system_policy,
    });
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    // The HTTPS connector supplies the URI hostname as `ServerName`; keeping
    // SNI explicit prevents a resolved TCP address from becoming TLS identity.
    config.enable_sni = true;
    Ok(config)
}

#[derive(Debug)]
struct DepthLimitedServerVerifier {
    inner: Arc<WebPkiServerVerifier>,
    max_intermediates: usize,
    system_policy: bool,
}

impl ServerCertVerifier for DepthLimitedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if intermediates.len() > self.max_intermediates {
            return Err(rustls::Error::General(format!(
                "origin certificate chain has {} intermediates; configured verify depth is {}",
                intermediates.len(),
                self.max_intermediates
            )));
        }
        let verified = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        #[cfg(windows)]
        if self.system_policy {
            let intermediate_der = intermediates
                .iter()
                .map(CertificateDer::as_ref)
                .collect::<Vec<_>>();
            ovcc_platform::verify_system_certificate_chain(
                end_entity.as_ref(),
                &intermediate_der,
                server_name.to_str().as_ref(),
            )
            .map_err(|error| {
                rustls::Error::General(format!(
                    "Windows origin certificate policy rejected the chain: {error}"
                ))
            })?;
        }
        #[cfg(not(windows))]
        let _ = self.system_policy;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

tokio::task_local! {
    /// Per-fetch slot the [`TimedConnector`] writes the connect duration into.
    /// Scoped around a single `client.request` await so a freshly opened
    /// connection records its setup time here; a fetch that reuses a pooled
    /// keepalive connection never invokes the connector, leaving the slot empty.
    static CONNECT_SLOT: Arc<Mutex<Option<Duration>>>;
}

/// Stage whose no-progress budget expired on an origin request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginTimeoutStage {
    Connect,
    ResponseHeader,
    ResponseBody,
}

impl OriginTimeoutStage {
    /// Token used in the error text and the `ovcc.upstream.timeout_stage` span field.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::ResponseHeader => "response_header",
            Self::ResponseBody => "response_body",
        }
    }
}

/// Typed origin inactivity timeout retained through connector and body errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OriginTimeout {
    pub stage: OriginTimeoutStage,
    pub duration: Duration,
}

impl fmt::Display for OriginTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "origin {} inactivity timeout after {:?}",
            self.stage.as_str(),
            self.duration
        )
    }
}

impl std::error::Error for OriginTimeout {}

type BoxStdError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
enum TimedConnectError {
    Transport(BoxStdError),
    Timeout(OriginTimeout),
}

impl fmt::Display for TimedConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::Timeout(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for TimedConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error.as_ref()),
            Self::Timeout(error) => Some(error),
        }
    }
}

/// Connector decorator that times the underlying connect and stashes the
/// duration in [`CONNECT_SLOT`]. It returns the inner connection unchanged, so
/// it still satisfies hyper's [`Connect`] contract; only new connections are
/// timed (a pooled-connection reuse skips the connector entirely).
#[derive(Clone)]
struct TimedConnector<C> {
    inner: C,
    connect_timeout: Duration,
}

impl<C> TowerService<Uri> for TimedConnector<C>
where
    C: TowerService<Uri> + Clone + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<BoxStdError>,
{
    type Response = C::Response;
    type Error = TimedConnectError;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<C::Response, TimedConnectError>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), TimedConnectError>> {
        self.inner
            .poll_ready(cx)
            .map_err(|error| TimedConnectError::Transport(error.into()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        // Drive the connect on the `poll_ready`-ed inner service, leaving a
        // fresh clone for the next checkout (the canonical tower pattern).
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let connect_timeout = self.connect_timeout;
        Box::pin(async move {
            let started = Instant::now();
            let connection = timeout(connect_timeout, inner.call(uri))
                .await
                .map_err(|_| {
                    TimedConnectError::Timeout(OriginTimeout {
                        stage: OriginTimeoutStage::Connect,
                        duration: connect_timeout,
                    })
                })?
                .map_err(|error| TimedConnectError::Transport(error.into()))?;
            let elapsed = started.elapsed();
            let _ = CONNECT_SLOT.try_with(|slot| {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(elapsed);
                }
            });
            Ok(connection)
        })
    }
}

/// Process-wide client for bodiless (GET/HEAD/probe) origin requests.
///
/// The reuse policy selects either the shared pool or a pool-disabled client.
/// The latter guarantees a fresh connection even when an origin ignores
/// `Connection: close` and responds with keepalive.
fn bodiless_client(reuse_connections: bool) -> Result<Arc<BodilessClient>> {
    static POOLED: OnceLock<Arc<BodilessClient>> = OnceLock::new();
    static DIRECT: OnceLock<Arc<BodilessClient>> = OnceLock::new();
    static OVERRIDES: OnceLock<BodilessClients> = OnceLock::new();
    let Some(nameserver) = resolver::nameserver_override() else {
        let client = if reuse_connections { &POOLED } else { &DIRECT };
        if let Some(client) = client.get() {
            return Ok(Arc::clone(client));
        }
        let built = Arc::new(build_bodiless_client(None, reuse_connections)?);
        let _ = client.set(Arc::clone(&built));
        return Ok(Arc::clone(client.get().unwrap_or(&built)));
    };
    let clients = OVERRIDES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut clients = clients
        .lock()
        .map_err(|_| Error::config("origin client map lock was poisoned"))?;
    if let Some(client) = clients.get(&(nameserver.clone(), reuse_connections)) {
        return Ok(Arc::clone(client));
    }
    let built = Arc::new(build_bodiless_client(
        Some(nameserver.clone()),
        reuse_connections,
    )?);
    clients.insert((nameserver, reuse_connections), Arc::clone(&built));
    Ok(built)
}

/// Process-wide client for streamed-body (transparent-proxy) requests.
fn streaming_client(reuse_connections: bool) -> Result<Arc<StreamingClient>> {
    static POOLED: OnceLock<Arc<StreamingClient>> = OnceLock::new();
    static DIRECT: OnceLock<Arc<StreamingClient>> = OnceLock::new();
    static OVERRIDES: OnceLock<StreamingClients> = OnceLock::new();
    let Some(nameserver) = resolver::nameserver_override() else {
        let client = if reuse_connections { &POOLED } else { &DIRECT };
        if let Some(client) = client.get() {
            return Ok(Arc::clone(client));
        }
        let built = Arc::new(build_streaming_client(None, reuse_connections)?);
        let _ = client.set(Arc::clone(&built));
        return Ok(Arc::clone(client.get().unwrap_or(&built)));
    };
    let clients = OVERRIDES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut clients = clients
        .lock()
        .map_err(|_| Error::config("origin streaming client map lock was poisoned"))?;
    if let Some(client) = clients.get(&(nameserver.clone(), reuse_connections)) {
        return Ok(Arc::clone(client));
    }
    let built = Arc::new(build_streaming_client(
        Some(nameserver.clone()),
        reuse_connections,
    )?);
    clients.insert((nameserver, reuse_connections), Arc::clone(&built));
    Ok(built)
}

fn build_bodiless_client(
    nameserver: Option<String>,
    reuse_connections: bool,
) -> Result<BodilessClient> {
    let options = configured_options(nameserver);
    build_bodiless_client_with_options(&options, reuse_connections)
}

fn build_bodiless_client_with_options(
    options: &UpstreamOptions,
    reuse_connections: bool,
) -> Result<BodilessClient> {
    let mut builder = Client::builder(TokioExecutor::new());
    // Request replay belongs to the downstream client. In particular, do not
    // hide a stale pooled connection by replaying the request inside Hyper.
    builder.retry_canceled_requests(false);
    if reuse_connections {
        builder
            .pool_idle_timeout(options.pool_idle_timeout)
            .pool_max_idle_per_host(options.pool_max_idle_per_host);
    } else {
        builder.pool_max_idle_per_host(0);
    }
    Ok(builder.build(origin_connector(options)?))
}

fn build_streaming_client(
    nameserver: Option<String>,
    reuse_connections: bool,
) -> Result<StreamingClient> {
    let options = configured_options(nameserver);
    let mut builder = Client::builder(TokioExecutor::new());
    builder.retry_canceled_requests(false);
    if reuse_connections {
        builder
            .pool_idle_timeout(options.pool_idle_timeout)
            .pool_max_idle_per_host(options.pool_max_idle_per_host);
    } else {
        builder.pool_max_idle_per_host(0);
    }
    Ok(builder.build(origin_connector(&options)?))
}

fn configured_options(nameserver: Option<String>) -> UpstreamOptions {
    let mut options = UPSTREAM_OPTIONS
        .get_or_init(UpstreamOptions::default)
        .clone();
    options.resolver.nameserver_override = nameserver;
    options
}

/// A response fetched from the origin.
#[derive(Debug, Clone)]
pub struct OriginResponse {
    pub status: u16,
    pub etag: Option<String>,
    /// All response headers (name, value), in wire order. Relayed to the client
    /// (filtered for hop-by-hop) so origin metadata (`ETag`, `Content-Type`,
    /// custom backend headers) survives the proxy.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Per-stage latency of this fetch, for the access log and the
    /// `ovcc.upstream.fetch` span.
    pub timing: UpstreamTiming,
}

/// Origin request shape used before peer discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPreflight {
    pub method: Method,
    pub headers: Vec<(String, String)>,
}

/// Exact freshness witness returned by an authorized preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightValidator {
    Etag(String),
    LastModified(String),
}

/// Metadata required before WhoHas fanout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightMetadata {
    pub total_size: u64,
    pub validator: PreflightValidator,
}

/// Plans a mandatory peer-fill preflight without changing the eventual GET.
///
/// Public requests can use `HEAD`. Method-bound or authenticated requests keep
/// `GET` and use one finite byte range. An AWS presigned request whose signature
/// covers `range` must already carry one finite range because changing it would
/// invalidate the signature.
pub fn plan_peer_preflight(
    raw_query: &str,
    headers: &[(String, String)],
    method_bound_or_authenticated: bool,
) -> Option<PeerPreflight> {
    let mut planned = headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("range"))
        .cloned()
        .collect::<Vec<_>>();
    if !method_bound_or_authenticated {
        return Some(PeerPreflight {
            method: Method::HEAD,
            headers: planned,
        });
    }

    let original_range = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.as_str());
    let range_signed = url::form_urlencoded::parse(raw_query.as_bytes())
        .find(|(name, _)| name.eq_ignore_ascii_case("X-Amz-SignedHeaders"))
        .is_some_and(|(_, value)| {
            value
                .split(';')
                .any(|name| name.trim().eq_ignore_ascii_case("range"))
        });
    let range = if range_signed {
        let original = original_range.filter(|value| is_single_finite_range(value))?;
        original.to_owned()
    } else {
        "bytes=0-0".to_owned()
    };
    planned.push(("Range".to_owned(), range));
    Some(PeerPreflight {
        method: Method::GET,
        headers: planned,
    })
}

/// Extracts authorized object metadata from the exact preflight response shape.
pub fn peer_preflight_metadata(
    request: &PeerPreflight,
    response: &OriginResponse,
) -> Option<PreflightMetadata> {
    let total_size = match (&request.method, response.status) {
        (&Method::HEAD, 200) => response_header(&response.headers, "content-length")?
            .parse()
            .ok()?,
        (&Method::GET, 206) => {
            content_range_total(response_header(&response.headers, "content-range")?)?
        }
        _ => return None,
    };
    let validator = response_header(&response.headers, "etag")
        .map(|value| PreflightValidator::Etag(value.to_owned()))
        .or_else(|| {
            response_header(&response.headers, "last-modified")
                .map(|value| PreflightValidator::LastModified(value.to_owned()))
        })?;
    Some(PreflightMetadata {
        total_size,
        validator,
    })
}

fn is_single_finite_range(value: &str) -> bool {
    let Some(spec) = value.strip_prefix("bytes=") else {
        return false;
    };
    if spec.contains(',') {
        return false;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return false;
    };
    if start.is_empty()
        || end.is_empty()
        || !start.bytes().all(|byte| byte.is_ascii_digit())
        || !end.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    matches!(
        (start.parse::<u64>(), end.parse::<u64>()),
        (Ok(start), Ok(end)) if start <= end
    )
}

fn content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    (total != "*").then(|| total.parse().ok()).flatten()
}

fn response_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Granular origin-fetch latencies in seconds.
///
/// `connect` is the connection-establishment time and is `None` when the fetch
/// reused a pooled keepalive connection (no connect happened). `header` measures
/// up to the response status+headers, and `response` up to the fully-read body;
/// both are taken from the moment the request is handed to the client.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UpstreamTiming {
    pub connect: Option<f64>,
    pub header: Option<f64>,
    pub response: Option<f64>,
}

/// A streaming request body fed from an [`mpsc`](tokio::sync::mpsc) channel.
///
/// Lets the proxy forward a client upload to the origin **without buffering it
/// in RAM**: the proxy reads chunks off the client socket and sends them into
/// the channel (bounded, so a slow origin back-pressures the client), and this
/// body hands each chunk to hyper as it arrives. This is what keeps a multi-GB
/// `PUT`/`POST` constant-memory.
///
/// Dropping the sender marks end-of-stream, so it is reserved for a body that
/// arrived whole. An incomplete one sends [`Err`] first, which hyper sees as a
/// body error and tears the upstream request down.
pub struct ChannelBody {
    rx: Receiver<UploadItem>,
    upload_complete: Option<oneshot::Sender<()>>,
    /// Declared total body length, when known. Reported via [`Body::size_hint`]
    /// so hyper frames the upstream request with `Content-Length` instead of
    /// `Transfer-Encoding: chunked`. This matters for origins that reject a
    /// chunked body, since a real S3 `PUT` answers a chunked upload with `501 Not
    /// Implemented` (it requires `Content-Length` or `aws-chunked`), whereas the
    /// integration mock tolerates chunked. `None` keeps the chunked framing (a
    /// genuinely streamed, unknown-length body).
    content_length: Option<u64>,
    #[cfg(feature = "test-fault-injection")]
    fault: Option<ovcc_testing::FaultLease>,
    #[cfg(feature = "test-fault-injection")]
    fault_pending: bool,
    #[cfg(feature = "test-fault-injection")]
    forwarded: u64,
}

/// Why OVCC abandoned a streamed upload body before its natural end.
///
/// [`ChannelBody`] yields this as a **body error**, so hyper tears the upstream
/// request down instead of writing a terminating chunk. An origin therefore
/// cannot commit, or report success for, a prefix of a client upload that OVCC
/// never finished forwarding. A clean end of the upstream body means the
/// complete, well-formed client body crossed, and nothing else does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadAbort {
    /// The client's HTTP chunked framing was malformed.
    InvalidFraming,
    /// The client stopped sending before the body was complete.
    Truncated,
    /// The `test-fault-injection` upstream request-body fault fired.
    #[cfg(feature = "test-fault-injection")]
    #[doc(hidden)]
    Injected,
}

impl fmt::Display for UploadAbort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidFraming => "client upload framing is invalid",
            Self::Truncated => "client upload ended before the body was complete",
            #[cfg(feature = "test-fault-injection")]
            Self::Injected => "injected upstream request-body failure",
        })
    }
}

impl std::error::Error for UploadAbort {}

/// One item on a streamed upload channel, either object bytes or a terminal
/// abort. Sending [`Err`] and then dropping the sender is the only way to end
/// an upstream body *without* a clean end of message.
pub type UploadItem = std::result::Result<Bytes, UploadAbort>;

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = UploadAbort;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        #[cfg(feature = "test-fault-injection")]
        if this.fault_pending {
            this.fault_pending = false;
            if let Some(lease) = this.fault.take() {
                let generation = lease.generation();
                match lease.fire(this.forwarded) {
                    Ok(snapshot) => tracing::warn!(
                        event = "test_fault_fired",
                        fault = "upstream_request_body",
                        generation,
                        body_bytes_emitted = snapshot.body_bytes_emitted.unwrap_or_default(),
                    ),
                    Err(error) => tracing::error!(
                        event = "test_fault_receipt_failed",
                        fault = "upstream_request_body",
                        generation,
                        error = %error,
                    ),
                }
            }
            return Poll::Ready(Some(Err(UploadAbort::Injected)));
        }
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                #[cfg(feature = "test-fault-injection")]
                if let Some(lease) = this.fault.as_ref() {
                    let remaining = lease.after_bytes().saturating_sub(this.forwarded);
                    if remaining == 0 {
                        this.fault_pending = true;
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    if chunk.len() as u64 >= remaining {
                        let take = usize::try_from(remaining).unwrap_or(chunk.len());
                        let prefix = chunk.slice(..take);
                        this.forwarded = this.forwarded.saturating_add(prefix.len() as u64);
                        this.fault_pending = true;
                        return Poll::Ready(Some(Ok(Frame::data(prefix))));
                    }
                    this.forwarded = this.forwarded.saturating_add(chunk.len() as u64);
                }
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            // Surfacing the abort as a body error is what stops hyper from
            // writing the terminating chunk, so the origin never sees a valid
            // end of message. `upload_complete` stays unfired because an
            // abandoned upload did not complete.
            Poll::Ready(Some(Err(abort))) => Poll::Ready(Some(Err(abort))),
            Poll::Ready(None) => {
                if let Some(upload_complete) = this.upload_complete.take() {
                    let _ = upload_complete.send(());
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self.content_length {
            Some(len) => SizeHint::with_exact(len),
            None => SizeHint::default(),
        }
    }
}

/// Fetches `url` from the origin over plaintext HTTP/1.1.
///
/// Uses the `hyper-util` legacy client (pooled, `TokioExecutor` + the plaintext
/// HTTP connector). The body is fully buffered into `Vec<u8>` — acceptable for
/// the small-object origin. Any transport, URL, or body-read failure is
/// mapped to [`ovcc_core::Error::upstream`].
pub async fn fetch(url: &str) -> Result<OriginResponse> {
    fetch_with_headers(url, &[]).await
}

/// Like [`fetch`], but forwards the given client request `headers` to the
/// origin (the access check and content-shaping on real backends key on
/// headers such as `Authorization`, `If-Match`, and `Range`). Hop-by-hop
/// headers and `Content-Length` are dropped. `Host` preserves an explicit request
/// authority and otherwise uses the URL authority. Invalid authority is rejected;
/// other unparseable header names/values are skipped.
pub async fn fetch_with_headers(url: &str, headers: &[(String, String)]) -> Result<OriginResponse> {
    fetch_with_method(Method::GET, url, headers).await
}

/// Issues a `HEAD` to the origin, forwarding `headers`. The response has no
/// body; only status and `etag` are meaningful.
pub async fn fetch_head(url: &str, headers: &[(String, String)]) -> Result<OriginResponse> {
    fetch_with_method(Method::HEAD, url, headers).await
}

/// Re-validates caller access with a single-byte `Range: bytes=0-0` GET carrying
/// the caller's headers so the origin re-runs authorization. Returns the origin
/// status; a `403` must not be served from cache.
///
/// The `Range` header is forced to `bytes=0-0` (overriding any client value),
/// so on a real S3/Azure origin this returns `206 Partial Content` with one byte,
/// making the probe cheap even for multi-GB objects. The response body is
/// discarded — only the status matters for the access decision.
pub async fn probe_access(url: &str, headers: &[(String, String)]) -> Result<OriginResponse> {
    let mut probe_headers: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("range"))
        .cloned()
        .collect();
    probe_headers.push(("Range".to_string(), "bytes=0-0".to_string()));

    fetch_with_method(Method::GET, url, &probe_headers).await
}

/// Error while reading an origin response body.
#[derive(Debug)]
pub enum OriginBodyError {
    Transport(hyper::Error),
    Timeout(OriginTimeout),
}

impl OriginBodyError {
    /// Which stage expired, or `None` when the body failed for a transport reason.
    pub const fn timeout_stage(&self) -> Option<OriginTimeoutStage> {
        match self {
            Self::Transport(_) => None,
            Self::Timeout(timeout) => Some(timeout.stage),
        }
    }
}

impl fmt::Display for OriginBodyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::Timeout(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OriginBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Timeout(error) => Some(error),
        }
    }
}

/// A body read carries no evidence about whether the connection had been
/// established, so it converts to a plain upstream error rather than the
/// connection-failure classification `request_error` can reach.
impl From<OriginBodyError> for Error {
    fn from(error: OriginBodyError) -> Self {
        Self::upstream(error.to_string())
    }
}

/// Streaming origin body whose inactivity deadline resets on every frame.
pub struct OriginBody {
    inner: hyper::body::Incoming,
    stall_timeout: Duration,
    stall: Option<Pin<Box<Sleep>>>,
    last_frame: Option<tokio::time::Instant>,
    failed: bool,
}

impl OriginBody {
    fn new(inner: hyper::body::Incoming, stall_timeout: Duration) -> Self {
        Self {
            inner,
            stall_timeout,
            stall: None,
            last_frame: None,
            failed: false,
        }
    }

    fn stall_deadline(progress: tokio::time::Instant, timeout: Duration) -> tokio::time::Instant {
        // Oversized durations retain Tokio's far-future fallback policy.
        progress
            .checked_add(timeout)
            .unwrap_or_else(|| tokio::time::sleep(timeout).deadline())
    }

    fn reset_stall(&mut self) {
        let now = tokio::time::Instant::now();
        self.last_frame = Some(now);
        if let Some(stall) = &mut self.stall {
            stall
                .as_mut()
                .reset(Self::stall_deadline(now, self.stall_timeout));
        }
    }

    fn poll_stall(&mut self, context: &mut Context<'_>) -> Poll<()> {
        self.stall
            .get_or_insert_with(|| {
                let progress = self.last_frame.unwrap_or_else(tokio::time::Instant::now);
                Box::pin(tokio::time::sleep_until(Self::stall_deadline(
                    progress,
                    self.stall_timeout,
                )))
            })
            .as_mut()
            .poll(context)
    }
}

impl Body for OriginBody {
    type Data = Bytes;
    type Error = OriginBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                self.reset_stall();
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.failed = true;
                Poll::Ready(Some(Err(OriginBodyError::Transport(error))))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if self.poll_stall(context).is_ready() {
                    self.failed = true;
                    Poll::Ready(Some(Err(OriginBodyError::Timeout(OriginTimeout {
                        stage: OriginTimeoutStage::ResponseBody,
                        duration: self.stall_timeout,
                    }))))
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

/// Origin response with a streaming body that has not been read yet.
pub struct OpenOriginResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub headers: Vec<(String, String)>,
    pub timing: UpstreamTiming,
    pub body: OriginBody,
}

/// Like [`fetch_with_headers`], but leaves the response body unread so the
/// caller can stream it (origin → storage → client) without buffering the whole
/// object in RAM.
pub async fn open_fetch_with_headers(
    url: &str,
    headers: &[(String, String)],
) -> Result<OpenOriginResponse> {
    open_fetch_with_method(Method::GET, url, headers).await
}

/// Like [`fetch_head`], but returns an [`OpenOriginResponse`] whose body must
/// not be read (a `HEAD` response carries no entity body).
pub async fn open_fetch_head(
    url: &str,
    headers: &[(String, String)],
) -> Result<OpenOriginResponse> {
    open_fetch_with_method(Method::HEAD, url, headers).await
}

/// Like [`fetch_with_method`], but leaves the response body unread for
/// streaming consumption.
pub async fn open_fetch_with_method(
    method: Method,
    url: &str,
    headers: &[(String, String)],
) -> Result<OpenOriginResponse> {
    let client = bodiless_client(!connection_close_requested(headers))?;
    open_bodiless(client.as_ref(), method, url, headers).await
}

/// Like [`fetch_with_headers`], but issues `method` (e.g. `HEAD`) instead of
/// `GET`. A `HEAD` response carries no body; everything else is identical.
pub async fn fetch_with_method(
    method: Method,
    url: &str,
    headers: &[(String, String)],
) -> Result<OriginResponse> {
    let client = bodiless_client(!connection_close_requested(headers))?;
    send_bodiless(client.as_ref(), method, url, headers).await
}

/// Issues a bodiless request through the given pooled `client`. Factored out of
/// [`fetch_with_method`] so a test can drive it with a client trusting a
/// self-signed origin (exercising the real TLS path); production always passes
/// the shared [`bodiless_client`]. Generic over the connector so either a
/// webpki-roots client or a custom-trust test client fits.
#[tracing::instrument(
    name = "ovcc.upstream.fetch",
    skip_all,
    fields(
        http.response.status_code = tracing::field::Empty,
        ovcc.upstream.connect_time = tracing::field::Empty,
        ovcc.upstream.header_time = tracing::field::Empty,
        ovcc.upstream.response_time = tracing::field::Empty,
        ovcc.upstream.timeout_stage = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn send_bodiless<C>(
    client: &Client<C, Empty<Bytes>>,
    method: Method,
    url: &str,
    headers: &[(String, String)],
) -> Result<OriginResponse>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let options = configured_options(None);
    send_bodiless_with_timeouts(
        client,
        method,
        url,
        headers,
        options.response_header_timeout,
        options.body_stall_timeout,
    )
    .await
}

async fn send_bodiless_with_timeouts<C>(
    client: &Client<C, Empty<Bytes>>,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    response_header_timeout: Duration,
    body_stall_timeout: Duration,
) -> Result<OriginResponse>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let req = build_bodiless_request(method, url, headers)?;
    let (resp, started, timing) =
        match timed_request(client.request(req), response_header_timeout).await {
            Ok(triple) => triple,
            Err(e) => {
                // Flag the span ERROR so the gateway response produced for an
                // upstream transport failure shows red in a trace UI instead of
                // an otherwise-empty fetch span.
                let span = tracing::Span::current();
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_message", "upstream request failed");
                return Err(request_error(url, &e));
            }
        };

    tracing::Span::current().record("http.response.status_code", resp.status().as_u16());
    into_origin_response(url, resp, started, timing, body_stall_timeout).await
}

/// Issues a bodiless request and returns the response head + unread body.
#[tracing::instrument(
    name = "ovcc.upstream.fetch",
    skip_all,
    fields(
        http.response.status_code = tracing::field::Empty,
        ovcc.upstream.connect_time = tracing::field::Empty,
        ovcc.upstream.header_time = tracing::field::Empty,
        ovcc.upstream.response_time = tracing::field::Empty,
        ovcc.upstream.timeout_stage = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn open_bodiless<C>(
    client: &Client<C, Empty<Bytes>>,
    method: Method,
    url: &str,
    headers: &[(String, String)],
) -> Result<OpenOriginResponse>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let options = configured_options(None);
    open_bodiless_with_timeouts(
        client,
        method,
        url,
        headers,
        options.response_header_timeout,
        options.body_stall_timeout,
    )
    .await
}

async fn open_bodiless_with_timeouts<C>(
    client: &Client<C, Empty<Bytes>>,
    method: Method,
    url: &str,
    headers: &[(String, String)],
    response_header_timeout: Duration,
    body_stall_timeout: Duration,
) -> Result<OpenOriginResponse>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let req = build_bodiless_request(method, url, headers)?;
    let (resp, timing) = match timed_request(client.request(req), response_header_timeout).await {
        Ok((resp, _started, timing)) => (resp, timing),
        Err(e) => {
            let span = tracing::Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_message", "upstream request failed");
            return Err(request_error(url, &e));
        }
    };
    tracing::Span::current().record("http.response.status_code", resp.status().as_u16());
    Ok(into_open_origin_response(resp, timing, body_stall_timeout))
}

/// Selects HTTP authority without changing the URI used for pooling, DNS or TLS.
fn request_authority(uri: &hyper::Uri, headers: &[(String, String)]) -> Result<HeaderValue> {
    let authority = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.as_str())
        .or_else(|| uri.authority().map(|value| value.as_str()))
        .ok_or_else(|| Error::upstream("missing upstream authority"))?;
    let parsed = authority
        .parse::<hyper::http::uri::Authority>()
        .map_err(|_| Error::upstream("invalid upstream authority"))?;
    if parsed.as_str().contains('@') {
        return Err(Error::upstream("invalid upstream authority"));
    }
    HeaderValue::from_str(authority).map_err(|_| Error::upstream("invalid upstream Host"))
}

/// Builds a bodiless origin request, forwarding client headers and trace context.
fn build_bodiless_request(
    method: Method,
    url: &str,
    headers: &[(String, String)],
) -> Result<Request<Empty<Bytes>>> {
    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| Error::upstream(format!("invalid url {url:?}: {e}")))?;

    let close_requested = connection_close_requested(headers);
    let mut builder = Request::builder().method(method).uri(uri.clone());
    // The URI remains the connection and TLS identity.
    builder = builder.header(HOST, request_authority(&uri, headers)?);
    for (name, value) in headers {
        if !is_forwardable(name) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(hn, hv);
        }
    }
    if close_requested || connection_request_limit_reached(&uri) {
        builder = builder.header(hyper::header::CONNECTION, "close");
    }
    builder = inject_trace_context(builder);
    builder
        .body(Empty::<Bytes>::new())
        .map_err(|e| Error::upstream(format!("build request for {url:?}: {e}")))
}

/// Parses a hyper response head into an [`OpenOriginResponse`], leaving the body
/// unread for streaming consumption.
fn into_open_origin_response(
    resp: hyper::Response<hyper::body::Incoming>,
    timing: UpstreamTiming,
    body_stall_timeout: Duration,
) -> OpenOriginResponse {
    let status = resp.status().as_u16();
    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();
    OpenOriginResponse {
        status,
        etag,
        headers,
        timing,
        body: OriginBody::new(resp.into_body(), body_stall_timeout),
    }
}

#[derive(Debug)]
enum TimedRequestError<E> {
    Transport(E),
    Timeout(OriginTimeout),
}

impl<E: fmt::Display> fmt::Display for TimedRequestError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => error.fmt(formatter),
            Self::Timeout(error) => error.fmt(formatter),
        }
    }
}

impl<E> std::error::Error for TimedRequestError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Timeout(error) => Some(error),
        }
    }
}

/// Runs `request` inside a [`CONNECT_SLOT`] scope so a newly opened connection
/// records its setup time, and measures the time to the response head. Returns
/// the response, the start instant (so the caller can measure the body-read
/// time), and an [`UpstreamTiming`] with `connect`/`header` filled. `connect`
/// stays `None` when a pooled keepalive connection was reused.
async fn timed_request<F, B, E>(
    request: F,
    response_header_timeout: Duration,
) -> std::result::Result<(hyper::Response<B>, Instant, UpstreamTiming), TimedRequestError<E>>
where
    F: Future<Output = std::result::Result<hyper::Response<B>, E>>,
{
    let slot: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let started = Instant::now();
    let resp = timeout(
        response_header_timeout,
        CONNECT_SLOT.scope(Arc::clone(&slot), request),
    )
    .await
    .map_err(|_| {
        TimedRequestError::Timeout(OriginTimeout {
            stage: OriginTimeoutStage::ResponseHeader,
            duration: response_header_timeout,
        })
    })?
    .map_err(TimedRequestError::Transport)?;
    let header = started.elapsed().as_secs_f64();
    let connect = slot
        .lock()
        .ok()
        .and_then(|guard| *guard)
        .map(|d| d.as_secs_f64());
    let timing = UpstreamTiming {
        connect,
        header: Some(header),
        response: None,
    };
    Ok((resp, started, timing))
}

/// Drives a streamed upload immediately, then starts the response-header
/// inactivity budget only after Hyper consumes the complete request body.
/// This avoids turning a progressing multi-GB upload into a total-duration cap.
async fn timed_request_after_upload<F, B, E>(
    request: F,
    upload_complete: oneshot::Receiver<()>,
    response_header_timeout: Duration,
) -> std::result::Result<(hyper::Response<B>, Instant, UpstreamTiming), TimedRequestError<E>>
where
    F: Future<Output = std::result::Result<hyper::Response<B>, E>>,
{
    let slot: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
    let started = Instant::now();
    let request = CONNECT_SLOT.scope(Arc::clone(&slot), request);
    tokio::pin!(request);
    let resp = tokio::select! {
        result = &mut request => result.map_err(TimedRequestError::Transport)?,
        _ = upload_complete => timeout(response_header_timeout, &mut request)
            .await
            .map_err(|_| TimedRequestError::Timeout(OriginTimeout {
                stage: OriginTimeoutStage::ResponseHeader,
                duration: response_header_timeout,
            }))?
            .map_err(TimedRequestError::Transport)?,
    };
    let header = started.elapsed().as_secs_f64();
    let connect = slot
        .lock()
        .ok()
        .and_then(|guard| *guard)
        .map(|duration| duration.as_secs_f64());
    Ok((
        resp,
        started,
        UpstreamTiming {
            connect,
            header: Some(header),
            response: None,
        },
    ))
}

fn request_error<E>(url: &str, error: &TimedRequestError<E>) -> Error
where
    E: std::error::Error + 'static,
{
    if let Some(timeout) = find_origin_timeout(error) {
        tracing::Span::current().record("ovcc.upstream.timeout_stage", timeout.stage.as_str());
        return Error::upstream(format!("request to {url:?} failed: {timeout}"));
    }
    let message = format!("request to {url:?} failed: {error}");
    if absolute_origin_url(url) && established_connection_failed(error) {
        Error::upstream_connection(message)
    } else {
        Error::upstream(message)
    }
}

fn absolute_origin_url(url: &str) -> bool {
    url.parse::<Uri>()
        .is_ok_and(|uri| uri.scheme().is_some() && uri.authority().is_some())
}

fn established_connection_failed(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<hyper_util::client::legacy::Error>() {
            return !error.is_connect();
        }
        current = error.source();
    }
    false
}

fn find_origin_timeout(error: &(dyn std::error::Error + 'static)) -> Option<OriginTimeout> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(timeout) = error.downcast_ref::<OriginTimeout>() {
            return Some(*timeout);
        }
        current = error.source();
    }
    None
}

/// Extracts an origin timeout stage retained in an `ovcc_core::Error` message.
/// Existing fetch signatures return `ovcc_core::Result`; streamed body errors
/// retain [`OriginTimeout`] directly through their error source chain.
pub fn timeout_stage(error: &Error) -> Option<OriginTimeoutStage> {
    let message = error.to_string();
    [
        OriginTimeoutStage::Connect,
        OriginTimeoutStage::ResponseHeader,
        OriginTimeoutStage::ResponseBody,
    ]
    .into_iter()
    .find(|stage| message.contains(&format!("origin {} inactivity timeout", stage.as_str())))
}

/// Adds W3C `traceparent`/`tracestate` headers for the current span so the
/// origin sees one continuous trace. A no-op when there is no active span
/// context (e.g. unit tests with no subscriber).
fn inject_trace_context(mut builder: http::request::Builder) -> http::request::Builder {
    let mut propagated: Vec<(String, String)> = Vec::new();
    ovcc_telemetry::propagation::inject_current(&mut propagated);
    for (name, value) in &propagated {
        if let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(hn, hv);
        }
    }
    builder
}

/// Transparently proxies a streamed request body and leaves the response body
/// unread so the caller can relay it at constant memory.
///
/// An [`Err`] item on `body_rx` aborts the upstream body, so an incomplete
/// client upload can never be reported to the origin as a complete one.
pub async fn open_proxy_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_rx: Receiver<UploadItem>,
    content_length: Option<u64>,
) -> Result<OpenOriginResponse> {
    open_proxy_request_timed(method, url, headers, body_rx, content_length)
        .await
        .map(|(response, _started)| response)
}

/// Transparently proxies a streamed request body and buffers the response.
///
/// Write-through callers retain this wrapper because their mutation responses
/// are bounded. General passthrough uses [`open_proxy_request`] so downloads do
/// not accumulate in memory.
pub async fn proxy_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_rx: Receiver<UploadItem>,
    content_length: Option<u64>,
) -> Result<OriginResponse> {
    let redacted_url = url.split_once('?').map_or(url, |(path, _)| path);
    let (response, started) =
        open_proxy_request_timed(method, url, headers, body_rx, content_length).await?;
    collect_open_origin_response(redacted_url, response, started).await
}

async fn open_proxy_request_timed(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body_rx: Receiver<UploadItem>,
    content_length: Option<u64>,
) -> Result<(OpenOriginResponse, Instant)> {
    let redacted_url = url.split_once('?').map_or(url, |(path, _)| path);
    let uri: hyper::Uri = url
        .parse()
        .map_err(|e| Error::upstream(format!("invalid url {redacted_url:?}: {e}")))?;
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| Error::upstream(format!("invalid method {method:?}: {e}")))?;

    #[cfg(feature = "test-fault-injection")]
    let fault = claim_request_body_fault(&method, &uri)?;

    let close_requested = connection_close_requested(headers);
    let client = streaming_client(!close_requested)?;
    let options = configured_options(None);
    // Hyper treats unknown-length GET and HEAD bodies as empty unless their
    // framing is explicit. ChannelBody supplies the decoded client bytes.
    let chunked_get_or_head =
        content_length.is_none() && (method == Method::GET || method == Method::HEAD);
    let mut builder = Request::builder().method(method).uri(uri.clone());
    builder = builder.header(HOST, request_authority(&uri, headers)?);
    for (name, value) in headers {
        if !is_forwardable(name) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(hn, hv);
        }
    }
    if chunked_get_or_head {
        builder = builder.header(hyper::header::TRANSFER_ENCODING, "chunked");
    }
    if close_requested || connection_request_limit_reached(&uri) {
        builder = builder.header(hyper::header::CONNECTION, "close");
    }
    let (upload_complete_tx, upload_complete_rx) = oneshot::channel();
    let req = builder
        .body(ChannelBody {
            rx: body_rx,
            upload_complete: Some(upload_complete_tx),
            content_length,
            #[cfg(feature = "test-fault-injection")]
            fault,
            #[cfg(feature = "test-fault-injection")]
            fault_pending: false,
            #[cfg(feature = "test-fault-injection")]
            forwarded: 0,
        })
        .map_err(|e| Error::upstream(format!("build request for {redacted_url:?}: {e}")))?;

    let (resp, started, timing) = timed_request_after_upload(
        client.request(req),
        upload_complete_rx,
        options.response_header_timeout,
    )
    .await
    .map_err(|error| request_error(redacted_url, &error))?;
    tracing::Span::current().record("http.response.status_code", resp.status().as_u16());
    Ok((
        into_open_origin_response(resp, timing, options.body_stall_timeout),
        started,
    ))
}

#[cfg(feature = "test-fault-injection")]
fn claim_request_body_fault(
    method: &Method,
    uri: &Uri,
) -> Result<Option<ovcc_testing::FaultLease>> {
    if !is_azure_put_block_request(method, uri) {
        return Ok(None);
    }

    match ovcc_testing::global_controller()
        .claim_matching()
        .map_err(|error| Error::internal(format!("test fault controller failed: {error}")))?
    {
        ovcc_testing::MatchOutcome::NotClaimed => Ok(None),
        ovcc_testing::MatchOutcome::Skipped(_) => Ok(None),
        ovcc_testing::MatchOutcome::Claimed(lease) => {
            tracing::warn!(
                event = "test_fault_claimed",
                fault = "upstream_request_body",
                generation = lease.generation(),
                after_bytes = lease.after_bytes(),
            );
            Ok(Some(lease))
        }
    }
}

#[cfg(feature = "test-fault-injection")]
fn is_azure_put_block_request(method: &Method, uri: &Uri) -> bool {
    method == Method::PUT
        && url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .any(|(name, value)| name == "comp" && value == "block")
}

async fn collect_open_origin_response(
    url: &str,
    response: OpenOriginResponse,
    started: Instant,
) -> Result<OriginResponse> {
    let OpenOriginResponse {
        status,
        etag,
        headers,
        mut timing,
        body,
    } = response;
    let body = body
        .collect()
        .await
        .map_err(|error| {
            if let Some(stage) = error.timeout_stage() {
                tracing::Span::current().record("ovcc.upstream.timeout_stage", stage.as_str());
            }
            Error::upstream(format!("reading body from {url:?} failed: {error}"))
        })?
        .to_bytes()
        .to_vec();
    timing.response = Some(started.elapsed().as_secs_f64());
    record_timing(&timing);
    Ok(OriginResponse {
        status,
        etag,
        headers,
        body,
        timing,
    })
}

fn connection_close_requested(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("connection") && value.eq_ignore_ascii_case("close")
    })
}

fn connection_request_limit_reached(uri: &Uri) -> bool {
    let limit = UPSTREAM_OPTIONS
        .get_or_init(UpstreamOptions::default)
        .max_requests_per_connection;
    if limit == 0 {
        return false;
    }
    let Some(authority) = uri.authority() else {
        return false;
    };
    let counts = ORIGIN_REQUEST_COUNTS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let Ok(mut counts) = counts.lock() else {
        return false;
    };
    if counts.len() >= 4096 && !counts.contains_key(authority.as_str()) {
        counts.clear();
    }
    let count = counts.entry(authority.as_str().to_owned()).or_default();
    *count += 1;
    if *count >= limit {
        *count = 0;
        true
    } else {
        false
    }
}

/// Parses a hyper response into an [`OriginResponse`], capturing the status,
/// all headers, the convenience `etag`, and the (buffered) body. `started` and
/// the partial `timing` (connect/header) come from [`timed_request`]; the
/// full-body read time (`response`) is measured here and the
/// `ovcc.upstream.fetch` span is annotated.
async fn into_origin_response(
    url: &str,
    resp: hyper::Response<hyper::body::Incoming>,
    started: Instant,
    mut timing: UpstreamTiming,
    body_stall_timeout: Duration,
) -> Result<OriginResponse> {
    let status = resp.status().as_u16();
    let etag = resp
        .headers()
        .get(ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_owned(), v.to_owned()))
        })
        .collect();

    let body = OriginBody::new(resp.into_body(), body_stall_timeout)
        .collect()
        .await
        .map_err(|error| {
            if let Some(stage) = error.timeout_stage() {
                tracing::Span::current().record("ovcc.upstream.timeout_stage", stage.as_str());
            }
            Error::upstream(format!("reading body from {url:?} failed: {error}"))
        })?
        .to_bytes()
        .to_vec();
    timing.response = Some(started.elapsed().as_secs_f64());

    record_timing(&timing);

    Ok(OriginResponse {
        status,
        etag,
        headers,
        body,
        timing,
    })
}

fn record_timing(timing: &UpstreamTiming) {
    let span = tracing::Span::current();
    if let Some(connect) = timing.connect {
        span.record("ovcc.upstream.connect_time", connect);
    }
    if let Some(header) = timing.header {
        span.record("ovcc.upstream.header_time", header);
    }
    if let Some(response) = timing.response {
        span.record("ovcc.upstream.response_time", response);
    }
}

/// Whether a client request header should be forwarded to the origin. Drops
/// `Host` (selected separately), `Content-Length` and the
/// standard hop-by-hop headers.
fn is_forwardable(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

    static TEMP_BUNDLE_ID: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone)]
    struct PendingConnector;

    impl TowerService<Uri> for PendingConnector {
        type Response = ();
        type Error = std::io::Error;
        type Future =
            Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(
            &mut self,
            _context: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _uri: Uri) -> Self::Future {
            Box::pin(std::future::pending())
        }
    }

    struct TempCaBundle {
        path: PathBuf,
    }

    impl TempCaBundle {
        fn new(contents: &str) -> std::io::Result<Self> {
            let id = TEMP_BUNDLE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("ovcc-upstream-ca-{}-{id}.pem", std::process::id()));
            std::fs::write(&path, contents)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempCaBundle {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct TestIdentity {
        root_pem: String,
        cert_chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
    }

    fn ca_params(name: &str) -> std::result::Result<CertificateParams, rcgen::Error> {
        let mut params = CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        Ok(params)
    }

    fn leaf_identity(
        dns_name: &str,
        issuer: &rcgen::Issuer<'_, KeyPair>,
    ) -> std::result::Result<(CertificateDer<'static>, PrivateKeyDer<'static>), rcgen::Error> {
        let key = KeyPair::generate()?;
        let cert = CertificateParams::new(vec![dns_name.to_owned()])?.signed_by(&key, issuer)?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        Ok((cert.der().clone(), key))
    }

    fn direct_private_ca_identity(dns_name: &str) -> TestResult<TestIdentity> {
        let root =
            CertifiedIssuer::self_signed(ca_params("OVCC test root")?, KeyPair::generate()?)?;
        let (leaf, key) = leaf_identity(dns_name, &root)?;
        Ok(TestIdentity {
            root_pem: root.pem(),
            cert_chain: vec![leaf],
            key,
        })
    }

    fn two_intermediate_identity(dns_name: &str) -> TestResult<TestIdentity> {
        let root =
            CertifiedIssuer::self_signed(ca_params("OVCC test root")?, KeyPair::generate()?)?;
        let intermediate_one = CertifiedIssuer::signed_by(
            ca_params("OVCC test intermediate one")?,
            KeyPair::generate()?,
            &root,
        )?;
        let intermediate_two = CertifiedIssuer::signed_by(
            ca_params("OVCC test intermediate two")?,
            KeyPair::generate()?,
            &intermediate_one,
        )?;
        let (leaf, key) = leaf_identity(dns_name, &intermediate_two)?;
        Ok(TestIdentity {
            root_pem: root.pem(),
            cert_chain: vec![
                leaf,
                intermediate_two.der().clone(),
                intermediate_one.der().clone(),
            ],
            key,
        })
    }

    async fn spawn_test_dns() -> TestResult<(String, tokio::task::JoinHandle<std::io::Result<()>>)>
    {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let address = socket.local_addr()?;
        let task = tokio::spawn(async move {
            let mut query = [0u8; 512];
            let (length, peer) = socket.recv_from(&mut query).await?;
            if length < 12 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "short DNS query",
                ));
            }
            let mut response = Vec::with_capacity(length + 16);
            response.extend_from_slice(&query[..2]);
            response.extend_from_slice(&[0x81, 0x80]);
            response.extend_from_slice(&[0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
            response.extend_from_slice(&query[12..length]);
            response.extend_from_slice(&[
                0xc0, 0x0c, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3c, 0x00, 0x04, 127, 0, 0,
                1,
            ]);
            socket.send_to(&response, peer).await?;
            Ok(())
        });
        Ok((address.to_string(), task))
    }

    async fn spawn_tls_origin(
        identity: TestIdentity,
    ) -> TestResult<(
        SocketAddr,
        tokio::task::JoinHandle<TestResult<Option<String>>>,
    )> {
        let mut server_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(identity.cert_chain, identity.key)?;
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut tls = acceptor.accept(tcp).await?;
            let sni = tls.get_ref().1.server_name().map(str::to_owned);
            let mut request = [0u8; 1024];
            let mut read = 0;
            while read < request.len() {
                let count = tls.read(&mut request[read..]).await?;
                if count == 0 {
                    break;
                }
                read += count;
                if request[..read]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            tls.write_all(
                b"HTTP/1.1 200 OK\r\nETag: \"tls\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            )
            .await?;
            tls.flush().await?;
            tls.shutdown().await?;
            Ok(sni)
        });
        Ok((address, task))
    }

    fn tls_test_options(bundle: &Path, verify_depth: usize, nameserver: String) -> UpstreamOptions {
        let mut options = UpstreamOptions {
            ca_bundle_path: bundle.to_owned(),
            tls_verify_depth: verify_depth,
            ..UpstreamOptions::default()
        };
        options.resolver.nameserver_override = Some(nameserver);
        options.resolver.retries = 0;
        options.resolver.random_nameserver_order = false;
        options
    }

    fn test_server_verifier(
        bundle: &Path,
        verify_depth: usize,
    ) -> TestResult<DepthLimitedServerVerifier> {
        let inner = WebPkiServerVerifier::builder(Arc::new(load_root_store(bundle)?)).build()?;
        Ok(DepthLimitedServerVerifier {
            inner,
            max_intermediates: verify_depth,
            system_policy: false,
        })
    }

    #[test]
    fn peer_preflight_public_uses_head_without_range() {
        // Arrange.
        let headers = vec![
            ("Host".to_owned(), "bucket.example.com".to_owned()),
            ("Range".to_owned(), "bytes=10-19".to_owned()),
        ];

        // Act.
        let plan = plan_peer_preflight("", &headers, false);

        // Assert.
        let plan = plan.as_ref().expect("public preflight is possible");
        assert_eq!(plan.method, Method::HEAD);
        assert!(response_header(&plan.headers, "range").is_none());
    }

    #[test]
    fn peer_preflight_authenticated_uses_single_byte_get() {
        // Arrange.
        let headers = vec![
            ("Authorization".to_owned(), "Bearer token".to_owned()),
            ("Range".to_owned(), "bytes=10-19".to_owned()),
        ];

        // Act.
        let plan = plan_peer_preflight("", &headers, true);

        // Assert.
        let plan = plan.as_ref().expect("authenticated preflight is possible");
        assert_eq!(plan.method, Method::GET);
        assert_eq!(response_header(&plan.headers, "range"), Some("bytes=0-0"));
        assert_eq!(
            response_header(&plan.headers, "authorization"),
            Some("Bearer token")
        );
    }

    #[test]
    fn peer_preflight_presigned_unsigned_range_is_replaced() {
        // Arrange.
        let query = "X-Amz-SignedHeaders=host&X-Amz-Signature=abc";
        let headers = vec![("Range".to_owned(), "bytes=10-19".to_owned())];

        // Act.
        let plan = plan_peer_preflight(query, &headers, true);

        // Assert.
        assert_eq!(
            plan.as_ref()
                .and_then(|plan| response_header(&plan.headers, "range")),
            Some("bytes=0-0")
        );
    }

    #[test]
    fn peer_preflight_presigned_signed_range_requires_finite_original() {
        // Arrange.
        let query = "X-Amz-SignedHeaders=host%3Brange&X-Amz-Signature=abc";
        let finite = vec![("Range".to_owned(), "bytes=10-19".to_owned())];
        let open_ended = vec![("Range".to_owned(), "bytes=10-".to_owned())];

        // Act.
        let preserved = plan_peer_preflight(query, &finite, true);
        let skipped = plan_peer_preflight(query, &open_ended, true);

        // Assert.
        assert_eq!(
            preserved
                .as_ref()
                .and_then(|plan| response_header(&plan.headers, "range")),
            Some("bytes=10-19")
        );
        assert_eq!(skipped, None);
    }

    #[test]
    fn peer_preflight_206_uses_content_range_total_and_prefers_etag() {
        // Arrange.
        let plan = PeerPreflight {
            method: Method::GET,
            headers: vec![("Range".to_owned(), "bytes=0-0".to_owned())],
        };
        let response = OriginResponse {
            status: 206,
            etag: Some("\"v1\"".to_owned()),
            headers: vec![
                ("Content-Range".to_owned(), "bytes 0-0/10485760".to_owned()),
                ("Content-Length".to_owned(), "1".to_owned()),
                ("ETag".to_owned(), "\"v1\"".to_owned()),
                (
                    "Last-Modified".to_owned(),
                    "Sat, 15 Aug 2026 00:00:00 GMT".to_owned(),
                ),
            ],
            body: vec![0],
            timing: UpstreamTiming::default(),
        };

        // Act.
        let metadata = peer_preflight_metadata(&plan, &response);

        // Assert.
        assert_eq!(
            metadata,
            Some(PreflightMetadata {
                total_size: 10_485_760,
                validator: PreflightValidator::Etag("\"v1\"".to_owned()),
            })
        );
    }

    #[test]
    fn connection_close_policy_survives_hop_by_hop_filtering() {
        // Arrange.
        let headers = vec![("Connection".to_owned(), "close".to_owned())];

        // Act.
        let request =
            build_bodiless_request(Method::GET, "https://origin.example/object", &headers)
                .expect("request builds");

        // Assert.
        assert_eq!(
            request
                .headers()
                .get(hyper::header::CONNECTION)
                .and_then(|value| value.to_str().ok()),
            Some("close")
        );
    }

    #[cfg(feature = "test-fault-injection")]
    #[tokio::test]
    async fn azure_put_block_test_fault_fires_after_configured_bytes() -> TestResult {
        // Arrange.
        let controller = Arc::new(ovcc_testing::FaultController::default());
        controller.arm(0, 4)?;
        let ovcc_testing::MatchOutcome::Claimed(lease) = controller.claim_matching()? else {
            return Err("armed request did not claim the test fault".into());
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"abcdefgh"))).await?;
        drop(tx);
        let body = ChannelBody {
            rx,
            upload_complete: None,
            content_length: Some(8),
            fault: Some(lease),
            fault_pending: false,
            forwarded: 0,
        };

        // Act.
        let result = body.collect().await;
        let snapshot = controller.snapshot()?;

        // Assert.
        assert_eq!(
            result.expect_err("injected body must fail").to_string(),
            "injected upstream request-body failure"
        );
        assert_eq!(
            snapshot.controller_state,
            ovcc_testing::ControllerState::Fired
        );
        assert_eq!(snapshot.body_bytes_emitted, Some(4));
        Ok(())
    }

    #[cfg(feature = "test-fault-injection")]
    #[tokio::test]
    async fn azure_put_block_test_fault_surfaces_hyper_sendrequest() -> TestResult {
        // Arrange.
        let controller = ovcc_testing::global_controller();
        struct ClearGlobalController;
        impl Drop for ClearGlobalController {
            fn drop(&mut self) {
                let _ = ovcc_testing::global_controller().clear();
            }
        }
        if controller.snapshot()?.controller_state != ovcc_testing::ControllerState::Idle {
            controller.clear()?;
        }
        let _clear = ClearGlobalController;
        controller.arm(0, 4)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut body = Vec::new();
            socket.read_to_end(&mut body).await?;
            Ok::<_, std::io::Error>(body)
        });
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"abcdefgh"))).await?;
        drop(tx);
        let url = format!("http://{address}/blob?comp=block&blockid=redacted");

        // Act.
        let result = proxy_request("PUT", &url, &[], rx, Some(8)).await;
        let snapshot = controller.snapshot()?;
        let received = origin.await??;

        // Assert.
        let error = result.expect_err("faulted Hyper request must fail");
        assert!(
            error.to_string().contains("client error (SendRequest)"),
            "unexpected Hyper error: {error}"
        );
        assert_eq!(
            snapshot.controller_state,
            ovcc_testing::ControllerState::Fired
        );
        assert_eq!(snapshot.body_bytes_emitted, Some(4));
        assert!(
            received
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map_or(received.is_empty(), |end| received.len() - (end + 4) <= 4),
            "Hyper must not write more than the emitted body prefix"
        );
        Ok(())
    }

    #[cfg(feature = "test-fault-injection")]
    #[test]
    fn azure_put_block_test_fault_selector_is_operation_scoped() -> TestResult {
        // Arrange.
        let block: Uri =
            "https://account.blob.core.windows.net/c/blob?comp=block&blockid=redacted".parse()?;
        let block_list: Uri =
            "https://account.blob.core.windows.net/c/blob?comp=blocklist".parse()?;
        let encoded: Uri = "https://example.test/c/blob?comp=block%20list".parse()?;

        // Act.
        let selected = [
            is_azure_put_block_request(&Method::PUT, &block),
            is_azure_put_block_request(&Method::GET, &block),
            is_azure_put_block_request(&Method::PUT, &block_list),
            is_azure_put_block_request(&Method::PUT, &encoded),
        ];

        // Assert.
        assert_eq!(selected, [true, false, false, false]);
        Ok(())
    }

    #[test]
    fn equal_transport_policy_initialization_is_concurrency_safe() {
        // Arrange.
        const THREADS: usize = 16;
        let cell = Arc::new(OnceLock::new());
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let cell = Arc::clone(&cell);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                install_options(&cell, UpstreamOptions::default())
            }));
        }

        // Act.
        let results = handles
            .into_iter()
            .map(|handle| handle.join().expect("initializer thread"))
            .collect::<Vec<_>>();

        // Assert.
        assert!(results.into_iter().all(|result| result.is_ok()));
    }

    #[test]
    fn different_transport_policy_reinitialization_is_rejected() {
        // Arrange.
        let cell = OnceLock::new();
        install_options(&cell, UpstreamOptions::default()).expect("initial policy");
        let mut changed = UpstreamOptions::default();
        changed.pool_max_idle_per_host += 1;

        // Act.
        let error = install_options(&cell, changed).expect_err("different policy must fail");

        // Assert.
        assert_eq!(error.kind(), ovcc_core::ErrorKind::Config);
    }

    #[test]
    fn upstream_scope_unpolled_drop_uses_eager_nameserver_scope() {
        struct OnDrop<'a>(&'a std::cell::RefCell<Option<String>>);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                *self.0.borrow_mut() = resolver::nameserver_override();
            }
        }
        let observed = std::cell::RefCell::new(None);
        let guard = OnDrop(&observed);
        let future = with_nameserver_override(Some("127.0.0.1:5353".to_owned()), async move {
            drop(guard);
        });
        assert_eq!(resolver::nameserver_override(), None);
        drop(future);
        assert_eq!(observed.borrow().as_deref(), Some("127.0.0.1:5353"));
        assert_eq!(resolver::nameserver_override(), None);
    }

    #[tokio::test]
    async fn upstream_scope_nameserver_restores_nested_and_interleaved_values() {
        let scoped = |name: &'static str| {
            with_nameserver_override(Some(name.to_owned()), async move {
                assert_eq!(resolver::nameserver_override().as_deref(), Some(name));
                tokio::task::yield_now().await;
                assert_eq!(resolver::nameserver_override().as_deref(), Some(name));
                with_nameserver_override(None, async {
                    tokio::task::yield_now().await;
                    assert_eq!(resolver::nameserver_override(), None);
                })
                .await;
                assert_eq!(resolver::nameserver_override().as_deref(), Some(name));
            })
        };
        let first = scoped("127.0.0.1:5353");
        let second = scoped("127.0.0.2:5353");
        assert_eq!(resolver::nameserver_override(), None);
        tokio::join!(first, second);
        assert_eq!(resolver::nameserver_override(), None);
    }

    #[tokio::test]
    async fn nameserver_override_uses_an_isolated_connection_pool() {
        let default = bodiless_client(true).expect("default client");
        let overridden = with_nameserver_override(Some("127.0.0.9:5353".to_owned()), async {
            bodiless_client(true)
        })
        .await
        .expect("overridden client");
        let reused = with_nameserver_override(Some("127.0.0.9:5353".to_owned()), async {
            bodiless_client(true)
        })
        .await
        .expect("reused client");

        assert!(!Arc::ptr_eq(&default, &overridden));
        assert!(Arc::ptr_eq(&overridden, &reused));
    }

    /// Spawns a one-shot origin that reads a request and writes a canned
    /// HTTP/1.1 response. The hyper "server" feature is not enabled, so the
    /// implementation writes the raw response bytes straight to the accepted `TcpStream`.
    async fn spawn_origin(response: &'static [u8]) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");

            // Drains the request headers so the client's write completes before
            // we reply (avoids a write-on-closed-socket race).
            let mut buf = [0u8; 1024];
            let mut total = 0;
            while total < buf.len() {
                let n = stream.read(&mut buf[total..]).await.expect("read request");
                if n == 0 {
                    break;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }

            stream.write_all(response).await.expect("write response");
            stream.flush().await.expect("flush");
        });
        addr
    }

    async fn spawn_header_stall_origin() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local_addr");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        (address, task)
    }

    async fn spawn_body_stall_origin() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local_addr");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nh")
                .await
                .expect("write partial response");
            stream.flush().await.expect("flush partial response");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        (address, task)
    }

    async fn spawn_gated_body_origin()
    -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local_addr");
        let (release, released) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response head");
            stream.flush().await.expect("flush response head");
            let _ = released.await;
            stream.write_all(b"hello").await.expect("write body");
            stream.flush().await.expect("flush body");
        });
        (address, release, task)
    }

    async fn spawn_progressing_origin() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local_addr");
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n")
                .await
                .expect("write response head");
            for byte in b"hello" {
                stream.write_all(&[*byte]).await.expect("write body byte");
                stream.flush().await.expect("flush body byte");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        (address, task)
    }

    async fn spawn_keepalive_origin(
        expected_requests: usize,
    ) -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let accepts = Arc::new(AtomicUsize::new(0));
        let observed_accepts = Arc::clone(&accepts);
        let handle = tokio::spawn(async move {
            let mut requests = 0;
            while requests < expected_requests {
                let (mut stream, _) = listener.accept().await.expect("accept");
                observed_accepts.fetch_add(1, Ordering::SeqCst);

                loop {
                    let mut buf = [0u8; 1024];
                    let mut total = 0;
                    let mut closed = false;
                    while total < buf.len() {
                        let n = stream.read(&mut buf[total..]).await.expect("read request");
                        if n == 0 {
                            closed = true;
                            break;
                        }
                        total += n;
                        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    if closed {
                        break;
                    }

                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              Content-Length: 2\r\n\
                              Connection: keep-alive\r\n\
                              \r\n\
                              ok",
                        )
                        .await
                        .expect("write response");
                    stream.flush().await.expect("flush");
                    requests += 1;
                    if requests == expected_requests {
                        return;
                    }
                }
            }
        });
        (addr, accepts, handle)
    }

    #[tokio::test]
    async fn upstream_get_keepalive_reuses_connection() {
        // Arrange.
        let (addr, accepts, origin) = spawn_keepalive_origin(2).await;
        let url = format!("http://{addr}/object");

        // Act.
        let first = fetch_with_headers(&url, &[])
            .await
            .expect("first fetch succeeds");
        let second = fetch_with_headers(&url, &[])
            .await
            .expect("second fetch succeeds");
        origin.await.expect("origin task");

        // Assert.
        assert_eq!(first.body, b"ok");
        assert_eq!(second.body, b"ok");
        assert!(first.timing.connect.is_some());
        assert_eq!(second.timing.connect, None);
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn upstream_get_keepalive_disabled_opens_connection_per_request() {
        // Arrange.
        let (addr, accepts, origin) = spawn_keepalive_origin(2).await;
        let url = format!("http://{addr}/object");
        let headers = vec![("Connection".to_owned(), "close".to_owned())];

        // Act.
        let first = fetch_with_headers(&url, &headers)
            .await
            .expect("first fetch succeeds");
        let second = fetch_with_headers(&url, &headers)
            .await
            .expect("second fetch succeeds");
        origin.await.expect("origin task");

        // Assert.
        assert_eq!(first.body, b"ok");
        assert_eq!(second.body, b"ok");
        assert_eq!(accepts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fetch_parses_status_etag_and_body() {
        let response = b"HTTP/1.1 200 OK\r\n\
            ETag: \"abc123\"\r\n\
            Content-Length: 13\r\n\
            Connection: close\r\n\
            \r\n\
            Hello, world!";
        let addr = spawn_origin(response).await;

        let url = format!("http://{addr}/object");
        let resp = fetch(&url).await.expect("fetch succeeds");

        assert_eq!(resp.status, 200);
        assert_eq!(resp.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(resp.body, b"Hello, world!");
    }

    #[tokio::test]
    async fn fetch_records_upstream_timings() {
        let response = b"HTTP/1.1 200 OK\r\n\
            Content-Length: 5\r\n\
            Connection: close\r\n\
            \r\n\
            hello";
        let addr = spawn_origin(response).await;
        let url = format!("http://{addr}/timed");

        // A fresh pooled client guarantees this first request opens a new
        // connection, so connect time is measured rather than reused from the
        // process-wide pool.
        let connector = origin_connector(&UpstreamOptions::default()).expect("origin connector");
        let client = Client::builder(TokioExecutor::new()).build(connector);
        let resp = send_bodiless(&client, Method::GET, &url, &[])
            .await
            .expect("fetch succeeds");

        assert_eq!(resp.status, 200);
        let timing = resp.timing;
        assert!(timing.header.is_some(), "header time recorded");
        assert!(timing.response.is_some(), "response time recorded");
        assert!(
            timing.connect.is_some(),
            "connect time recorded for a new connection"
        );
        // The body finishes no earlier than the headers; connect precedes both.
        assert!(timing.response.unwrap() >= timing.header.unwrap());
        assert!(timing.header.unwrap() >= timing.connect.unwrap());
    }

    #[tokio::test]
    async fn origin_connector_reports_typed_connect_timeout() {
        // Arrange.
        let mut connector = TimedConnector {
            inner: PendingConnector,
            connect_timeout: Duration::from_millis(10),
        };
        let uri: Uri = "https://origin.internal/object".parse().expect("URI");

        // Act.
        let error = connector
            .call(uri)
            .await
            .expect_err("connect must time out");

        // Assert.
        assert!(matches!(
            error,
            TimedConnectError::Timeout(OriginTimeout {
                stage: OriginTimeoutStage::Connect,
                ..
            })
        ));
    }

    #[test]
    fn origin_timeout_stage_survives_request_error_mapping() {
        // Arrange.
        let connect_timeout = OriginTimeout {
            stage: OriginTimeoutStage::Connect,
            duration: Duration::from_secs(60),
        };
        let nested = TimedRequestError::Transport(TimedConnectError::Timeout(connect_timeout));

        // Act.
        let error = request_error("https://origin.internal/object", &nested);

        // Assert.
        assert_eq!(timeout_stage(&error), Some(OriginTimeoutStage::Connect));
    }

    #[tokio::test]
    async fn general_put_relative_origin_is_not_an_established_connection_failure() -> TestResult {
        // Arrange.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);

        // Act.
        let error = proxy_request("PUT", "/relative/origin", &[], rx, Some(0))
            .await
            .expect_err("Hyper must reject a relative origin URI");

        // Assert.
        assert_eq!(error.kind(), ovcc_core::ErrorKind::Upstream);
        assert!(!error.is_upstream_connection_failure());
        Ok(())
    }

    #[test]
    fn origin_body_timeout_error_exposes_typed_stage() {
        // Arrange.
        let error = OriginBodyError::Timeout(OriginTimeout {
            stage: OriginTimeoutStage::ResponseBody,
            duration: Duration::from_secs(60),
        });

        // Act.
        let stage = error.timeout_stage();

        // Assert.
        assert_eq!(stage, Some(OriginTimeoutStage::ResponseBody));
    }

    #[tokio::test]
    async fn streamed_upload_starts_header_budget_after_upload_completion() {
        // Arrange.
        let (upload_complete, uploaded) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let _ = upload_complete.send(());
        });
        let request = async {
            tokio::time::sleep(Duration::from_millis(35)).await;
            Ok::<_, std::io::Error>(hyper::Response::new(()))
        };
        let started = Instant::now();

        // Act.
        let result = timed_request_after_upload(request, uploaded, Duration::from_millis(20)).await;

        // Assert.
        assert!(result.is_ok());
        assert!(started.elapsed() > Duration::from_millis(20));
    }

    #[tokio::test]
    async fn response_header_budget_reports_typed_timeout() {
        // Arrange.
        let request =
            std::future::pending::<std::result::Result<hyper::Response<()>, std::io::Error>>();

        // Act.
        let error = timed_request(request, Duration::from_millis(10))
            .await
            .expect_err("header must time out");

        // Assert.
        assert!(matches!(
            error,
            TimedRequestError::Timeout(OriginTimeout {
                stage: OriginTimeoutStage::ResponseHeader,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn origin_request_reports_typed_response_header_timeout() {
        // Arrange.
        let (address, origin) = spawn_header_stall_origin().await;
        let options = UpstreamOptions::default();
        let client = build_bodiless_client_with_options(&options, false).expect("client");
        let url = format!("http://{address}/header-stall");

        // Act.
        let error = send_bodiless_with_timeouts(
            &client,
            Method::GET,
            &url,
            &[],
            Duration::from_millis(20),
            Duration::from_secs(1),
        )
        .await
        .expect_err("response head must time out");
        origin.abort();

        // Assert.
        assert_eq!(
            timeout_stage(&error),
            Some(OriginTimeoutStage::ResponseHeader)
        );
    }

    async fn controlled_origin_body() -> (OriginBody, tokio::io::DuplexStream) {
        let (client, mut server) = tokio::io::duplex(1024);
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(client))
                .await
                .unwrap();
        tokio::spawn(async move {
            connection.await.unwrap();
        });
        let origin = tokio::spawn(async move {
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                server.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            server
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            server
        });
        let response = sender
            .send_request(
                Request::builder()
                    .uri("/")
                    .body(Empty::<Bytes>::new())
                    .unwrap(),
            )
            .await
            .unwrap();
        (
            OriginBody::new(response.into_body(), Duration::from_secs(10)),
            origin.await.unwrap(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn origin_get_body_oversized_stall_timeout_preserves_tokio_deadline() {
        let (mut body, mut origin) = controlled_origin_body().await;
        body.stall_timeout = Duration::MAX;
        let expected = tokio::time::sleep(Duration::MAX).deadline();
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(Pin::new(&mut body).poll_frame(&mut context).is_pending());
        assert_eq!(body.stall.as_ref().unwrap().deadline(), expected);
        tokio::time::advance(Duration::from_secs(60)).await;
        origin.write_all(b"1\r\na\r\n").await.unwrap();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            b"a".as_slice()
        );
        assert_eq!(
            body.stall.as_ref().unwrap().deadline(),
            tokio::time::sleep(Duration::MAX).deadline()
        );
        assert!(Pin::new(&mut body).poll_frame(&mut context).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn origin_get_body_stall_timer_is_lazy_and_reused_after_progress() {
        let (mut body, mut origin) = controlled_origin_body().await;
        assert!(body.stall.is_none());
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(Pin::new(&mut body).poll_frame(&mut context).is_pending());
        let timer = (&**body.stall.as_ref().unwrap()) as *const Sleep;
        tokio::time::advance(Duration::from_secs(5)).await;
        origin.write_all(b"1\r\na\r\n").await.unwrap();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            b"a".as_slice()
        );
        assert_eq!((&**body.stall.as_ref().unwrap()) as *const Sleep, timer);
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(
            Pin::new(&mut body).poll_frame(&mut context).is_pending(),
            "progress extends the registered deadline"
        );
        tokio::time::advance(Duration::from_secs(5)).await;
        let error = body.frame().await.unwrap().unwrap_err();
        assert_eq!(
            error.timeout_stage(),
            Some(OriginTimeoutStage::ResponseBody)
        );
        assert!(body.frame().await.is_none(), "timeout is terminal");
    }

    #[tokio::test(start_paused = true)]
    async fn origin_get_body_stall_deadline_retains_last_ready_frame() {
        let (mut body, mut origin) = controlled_origin_body().await;
        origin.write_all(b"1\r\na\r\n").await.unwrap();
        // The driver must publish the frame before the first body poll.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let frame = body.frame().await.unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap(), b"a".as_slice());
        assert!(
            body.stall.is_none(),
            "ready-only polling allocates no stall timer"
        );
        tokio::time::advance(Duration::from_secs(11)).await;
        let error = body.frame().await.unwrap().unwrap_err();
        assert_eq!(
            error.timeout_stage(),
            Some(OriginTimeoutStage::ResponseBody)
        );
    }

    #[tokio::test]
    async fn origin_body_reports_typed_no_progress_timeout() {
        // Arrange.
        let (address, origin) = spawn_body_stall_origin().await;
        let options = UpstreamOptions::default();
        let client = build_bodiless_client_with_options(&options, false).expect("client");
        let url = format!("http://{address}/body-stall");

        // Act.
        let error = send_bodiless_with_timeouts(
            &client,
            Method::GET,
            &url,
            &[],
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .await
        .expect_err("response body must time out");
        origin.abort();

        // Assert.
        assert_eq!(
            timeout_stage(&error),
            Some(OriginTimeoutStage::ResponseBody)
        );
    }

    #[tokio::test]
    async fn origin_get_body_timeout_arms_on_first_poll() {
        // Arrange.
        let (address, release, origin) = spawn_gated_body_origin().await;
        let options = UpstreamOptions::default();
        let client = build_bodiless_client_with_options(&options, false).expect("client");
        let url = format!("http://{address}/delayed-first-poll");
        let mut response = open_bodiless_with_timeouts(
            &client,
            Method::GET,
            &url,
            &[],
            Duration::from_secs(1),
            Duration::from_millis(50),
        )
        .await
        .expect("response head");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut first_frame = Box::pin(response.body.frame());

        // Act.
        let pending = timeout(Duration::from_millis(5), &mut first_frame).await;
        release.send(()).expect("release origin body");
        let frame = timeout(Duration::from_secs(1), first_frame)
            .await
            .expect("body arrives within the first-poll budget")
            .expect("body frame")
            .expect("body frame succeeds")
            .into_data()
            .expect("data frame");
        origin.await.expect("origin task");

        // Assert.
        assert!(
            pending.is_err(),
            "the first body poll must wait for progress"
        );
        assert_eq!(frame, b"hello".as_slice());
    }

    #[tokio::test]
    async fn origin_body_progress_resets_inactivity_timeout() {
        // Arrange.
        let (address, origin) = spawn_progressing_origin().await;
        let options = UpstreamOptions::default();
        let client = build_bodiless_client_with_options(&options, false).expect("client");
        let url = format!("http://{address}/slow-progress");
        let started = Instant::now();

        // Act.
        let response = send_bodiless_with_timeouts(
            &client,
            Method::GET,
            &url,
            &[],
            Duration::from_secs(1),
            Duration::from_millis(200),
        )
        .await
        .expect("progressing response must not time out");
        origin.await.expect("origin task");

        // Assert.
        assert_eq!(response.body, b"hello");
        assert!(started.elapsed() > Duration::from_millis(200));
    }

    #[tokio::test]
    async fn fetch_handles_missing_etag() {
        let response = b"HTTP/1.1 404 Not Found\r\n\
            Content-Length: 9\r\n\
            Connection: close\r\n\
            \r\n\
            not found";
        let addr = spawn_origin(response).await;

        let url = format!("http://{addr}/missing");
        let resp = fetch(&url).await.expect("fetch succeeds");

        assert_eq!(resp.status, 404);
        assert_eq!(resp.etag, None);
        assert_eq!(resp.body, b"not found");
    }

    #[tokio::test]
    async fn fetch_rejects_malformed_url() {
        let err = fetch("not a url").await.expect_err("must fail");
        assert_eq!(err.kind(), ovcc_core::ErrorKind::Upstream);
    }

    #[test]
    fn origin_tls_missing_ca_bundle_fails_client_construction() {
        // Arrange.
        let missing = std::env::temp_dir().join(format!(
            "ovcc-upstream-missing-ca-{}.pem",
            std::process::id()
        ));

        // Act.
        let result = load_root_store(&missing);

        // Assert.
        assert!(result.is_err());
    }

    #[test]
    fn origin_server_name_uses_uri_hostname_without_port_or_brackets() -> TestResult {
        // Arrange.
        let nucleus: Uri = "https://nucleus.internal:3443/omni/lft/object".parse()?;
        let passthrough: Uri = "https://packages.internal:8443/v2/object".parse()?;
        let ipv6: Uri = "https://[2001:db8::1]:443/object".parse()?;

        // Act.
        let nucleus_name = origin_server_name(&nucleus)?;
        let passthrough_name = origin_server_name(&passthrough)?;
        let ipv6_name = origin_server_name(&ipv6)?;

        // Assert.
        assert_eq!(nucleus_name.to_str(), "nucleus.internal");
        assert_eq!(passthrough_name.to_str(), "packages.internal");
        assert_eq!(ipv6_name.to_str(), "2001:db8::1");
        Ok(())
    }

    #[test]
    fn origin_transport_rejects_zero_timeout_values() -> TestResult {
        // Arrange.
        let identity = direct_private_ca_identity("origin.internal")?;
        let bundle = TempCaBundle::new(&identity.root_pem)?;

        for name in [
            "connect_timeout",
            "response_header_timeout",
            "body_stall_timeout",
        ] {
            let mut options = UpstreamOptions {
                ca_bundle_path: bundle.path.clone(),
                ..UpstreamOptions::default()
            };
            match name {
                "connect_timeout" => options.connect_timeout = Duration::ZERO,
                "response_header_timeout" => options.response_header_timeout = Duration::ZERO,
                "body_stall_timeout" => options.body_stall_timeout = Duration::ZERO,
                _ => unreachable!(),
            }

            // Act.
            let error = validate_options(&options).expect_err("zero timeout must fail");

            // Assert.
            assert!(error.to_string().contains(name));
        }
        Ok(())
    }

    #[test]
    fn origin_tls_malformed_ca_bundle_fails_client_construction() -> TestResult {
        // Arrange.
        let bundle = TempCaBundle::new(
            "-----BEGIN CERTIFICATE-----\nnot-valid-base64%%%\n-----END CERTIFICATE-----\n",
        )?;

        // Act.
        let result = load_root_store(&bundle.path);

        // Assert.
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn origin_tls_empty_ca_bundle_fails_client_construction() -> TestResult {
        // Arrange.
        let bundle = TempCaBundle::new("# no certificates\n")?;

        // Act.
        let result = load_root_store(&bundle.path);

        // Assert.
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn origin_tls_verifier_enforces_trust_hostname_and_depth() -> TestResult {
        // Arrange.
        let trusted = direct_private_ca_identity("origin.internal")?;
        let trusted_bundle = TempCaBundle::new(&trusted.root_pem)?;
        let trusted_verifier = test_server_verifier(&trusted_bundle.path, 2)?;
        let origin_name = ServerName::try_from("origin.internal")?;

        // Act.
        let accepted = trusted_verifier.verify_server_cert(
            &trusted.cert_chain[0],
            &trusted.cert_chain[1..],
            &origin_name,
            &[],
            UnixTime::now(),
        );

        // Assert.
        assert!(accepted.is_ok());

        // Arrange.
        let wrong_name = ServerName::try_from("wrong.internal")?;

        // Act.
        let hostname_mismatch = trusted_verifier.verify_server_cert(
            &trusted.cert_chain[0],
            &trusted.cert_chain[1..],
            &wrong_name,
            &[],
            UnixTime::now(),
        );

        // Assert.
        assert!(hostname_mismatch.is_err());

        // Arrange.
        let untrusted = direct_private_ca_identity("origin.internal")?;

        // Act.
        let untrusted_chain = trusted_verifier.verify_server_cert(
            &untrusted.cert_chain[0],
            &untrusted.cert_chain[1..],
            &origin_name,
            &[],
            UnixTime::now(),
        );

        // Assert.
        assert!(untrusted_chain.is_err());

        // Arrange.
        let deep = two_intermediate_identity("origin.internal")?;
        let deep_bundle = TempCaBundle::new(&deep.root_pem)?;
        let shallow_verifier = test_server_verifier(&deep_bundle.path, 1)?;
        let deep_verifier = test_server_verifier(&deep_bundle.path, 2)?;

        // Act.
        let too_deep = shallow_verifier.verify_server_cert(
            &deep.cert_chain[0],
            &deep.cert_chain[1..],
            &origin_name,
            &[],
            UnixTime::now(),
        );
        let allowed_depth = deep_verifier.verify_server_cert(
            &deep.cert_chain[0],
            &deep.cert_chain[1..],
            &origin_name,
            &[],
            UnixTime::now(),
        );

        // Assert.
        assert!(too_deep.is_err());
        assert!(allowed_depth.is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn origin_tls_private_ca_round_trip_preserves_hostname_sni() -> TestResult {
        // Arrange.
        let identity = direct_private_ca_identity("origin.internal")?;
        let bundle = TempCaBundle::new(&identity.root_pem)?;
        let (address, origin) = spawn_tls_origin(identity).await?;
        let (nameserver, dns) = spawn_test_dns().await?;
        let options = tls_test_options(&bundle.path, 2, nameserver);
        let client = build_bodiless_client_with_options(&options, false)?;
        let url = format!("https://origin.internal:{}/object", address.port());

        // Act.
        let response = send_bodiless(&client, Method::GET, &url, &[]).await?;
        let observed_sni = origin.await??;
        dns.await??;

        // Assert.
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");
        assert_eq!(observed_sni.as_deref(), Some("origin.internal"));
        Ok(())
    }

    #[tokio::test]
    async fn origin_tls_untrusted_private_ca_is_rejected() -> TestResult {
        // Arrange.
        let server_identity = direct_private_ca_identity("origin.internal")?;
        let untrusted_identity = direct_private_ca_identity("unused.internal")?;
        let bundle = TempCaBundle::new(&untrusted_identity.root_pem)?;
        let (address, origin) = spawn_tls_origin(server_identity).await?;
        let (nameserver, dns) = spawn_test_dns().await?;
        let options = tls_test_options(&bundle.path, 2, nameserver);
        let client = build_bodiless_client_with_options(&options, false)?;
        let url = format!("https://origin.internal:{}/object", address.port());

        // Act.
        let response = send_bodiless(&client, Method::GET, &url, &[]).await;
        let origin_result = origin.await?;
        dns.await??;

        // Assert.
        assert!(response.is_err());
        assert!(origin_result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn origin_tls_hostname_mismatch_is_rejected() -> TestResult {
        // Arrange.
        let identity = direct_private_ca_identity("right.internal")?;
        let bundle = TempCaBundle::new(&identity.root_pem)?;
        let (address, origin) = spawn_tls_origin(identity).await?;
        let (nameserver, dns) = spawn_test_dns().await?;
        let options = tls_test_options(&bundle.path, 2, nameserver);
        let client = build_bodiless_client_with_options(&options, false)?;
        let url = format!("https://wrong.internal:{}/object", address.port());

        // Act.
        let response = send_bodiless(&client, Method::GET, &url, &[]).await;
        let origin_result = origin.await?;
        dns.await??;

        // Assert.
        assert!(response.is_err());
        assert!(origin_result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn origin_tls_verify_depth_enforces_intermediate_limit() -> TestResult {
        // Arrange.
        let rejected_identity = two_intermediate_identity("origin.internal")?;
        let rejected_bundle = TempCaBundle::new(&rejected_identity.root_pem)?;
        let (rejected_address, rejected_origin) = spawn_tls_origin(rejected_identity).await?;
        let (rejected_nameserver, rejected_dns) = spawn_test_dns().await?;
        let rejected_options = tls_test_options(&rejected_bundle.path, 1, rejected_nameserver);
        let rejected_client = build_bodiless_client_with_options(&rejected_options, false)?;
        let rejected_url = format!("https://origin.internal:{}/object", rejected_address.port());

        // Act.
        let rejected = send_bodiless(&rejected_client, Method::GET, &rejected_url, &[]).await;
        let rejected_origin_result = rejected_origin.await?;
        rejected_dns.await??;

        // Assert.
        assert!(rejected.is_err());
        assert!(rejected_origin_result.is_err());

        // Arrange.
        let accepted_identity = two_intermediate_identity("origin.internal")?;
        let accepted_bundle = TempCaBundle::new(&accepted_identity.root_pem)?;
        let (accepted_address, accepted_origin) = spawn_tls_origin(accepted_identity).await?;
        let (accepted_nameserver, accepted_dns) = spawn_test_dns().await?;
        let accepted_options = tls_test_options(&accepted_bundle.path, 2, accepted_nameserver);
        let accepted_client = build_bodiless_client_with_options(&accepted_options, false)?;
        let accepted_url = format!("https://origin.internal:{}/object", accepted_address.port());

        // Act.
        let accepted = send_bodiless(&accepted_client, Method::GET, &accepted_url, &[]).await?;
        let accepted_sni = accepted_origin.await??;
        accepted_dns.await??;

        // Assert.
        assert_eq!(accepted.status, 200);
        assert_eq!(accepted_sni.as_deref(), Some("origin.internal"));
        Ok(())
    }

    #[tokio::test]
    async fn probe_access_returns_403_when_origin_forbids() {
        let response = b"HTTP/1.1 403 Forbidden\r\n\
            Content-Length: 9\r\n\
            Connection: close\r\n\
            \r\n\
            forbidden";
        let addr = spawn_origin(response).await;

        let url = format!("http://{addr}/object");
        let headers = vec![("Authorization".to_string(), "Bearer revoked".to_string())];
        let probe = probe_access(&url, &headers).await.expect("probe succeeds");
        assert_eq!(probe.status, 403);
    }

    #[tokio::test]
    async fn probe_access_returns_206_when_origin_authorizes_range() {
        let response = b"HTTP/1.1 206 Partial Content\r\n\
            Content-Range: bytes 0-0/13\r\n\
            Content-Length: 1\r\n\
            Connection: close\r\n\
            \r\n\
            H";
        let addr = spawn_origin(response).await;

        let url = format!("http://{addr}/object");
        let headers = vec![("Authorization".to_string(), "Bearer valid".to_string())];
        let probe = probe_access(&url, &headers).await.expect("probe succeeds");
        assert_eq!(probe.status, 206);
    }

    #[test]
    fn origin_body_failure_bridges_without_the_connection_classification() {
        // Arrange.
        let error = OriginBodyError::Timeout(OriginTimeout {
            stage: OriginTimeoutStage::ResponseBody,
            duration: Duration::from_secs(1),
        });

        // Act.
        let bridged = Error::from(error);

        // Assert.
        assert_eq!(bridged.kind(), ovcc_core::ErrorKind::Upstream);
        assert!(!bridged.is_upstream_connection_failure());
        assert_eq!(
            bridged.message(),
            "origin response_body inactivity timeout after 1s"
        );
    }
}
