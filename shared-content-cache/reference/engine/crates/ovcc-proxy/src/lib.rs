// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OVCC reverse-proxy assembly.
//!
//! Wires the building-block crates into a working HTTPS reverse-proxy cache:
//! self-signed `rustls` TLS, kTLS-upgraded connections, presigned-URL aware
//! cache keys, disk-backed [`ovcc_storage`] storage, and an origin fetch on
//! MISS. The whole point is the **hit path**: a cached body is
//! served straight from the page cache to the encrypted socket via
//! `sendfile(2)` over kTLS — no userspace body copy.
//!
//! # Two response paths
//!
//! - **HIT** (`X-Cache-Status: HIT`): after the rustls handshake the connection
//!   is upgraded to kernel TLS (kTLS); the response head is written through the
//!   kTLS stream, then the leased file fd is `sendfile`'d directly to the socket
//!   fd. The kernel encrypts each chunk into TLS records — page cache → NIC with
//!   zero userspace body copies. This mirrors `tests/ktls_sendfile.rs`.
//! - **MISS** (`X-Cache-Status: MISS`): the object is fetched from the origin
//!   ([`ovcc_upstream`]), streamed into the disk cache and to the client in
//!   chunks (constant memory; registry rewrite paths still buffer small index
//!   bodies), then served from cache on the next request via the HIT path.
//!
//! # Presigned-URL handling
//!
//! Two presigned GETs for one object differ only in signature/date noise, which
//! [`ovcc_cachekey::filter_query_args`] strips so they share object bytes. Before
//! either request enters a shared-service cache, a separate positive-only digest
//! cache proves its exact credential was authorized. An **expired** presigned
//! URL cannot reuse a shared HIT and forces a MISS/refetch via
//! [`ovcc_presigned::validate`]. Workstation local mode permits private reuse
//! without that authorization witness; content eligibility remains required.

mod azure_optional_keys;
#[cfg(feature = "test-optional-keys")]
mod optional_key_fixture;

use std::collections::{HashMap, HashSet, hash_map::DefaultHasher};
use std::fs::File;
use std::future::Future;
use std::hash::Hasher;
use std::io::{IoSlice, Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use http_body_util::BodyExt as _;
#[cfg(target_os = "linux")]
use ktls::CorkStream;
use ovcc_auth_cache::{
    Acquire as AuthorizationAcquire, AuthorizationCache, CredentialDigest,
    ProbeOutcome as AuthorizationProbeOutcome,
};
use ovcc_blocklist::{
    BlockId, BlockMetadata, BlockMetadataSource, parse_block_list, plan_segments,
};
use ovcc_cache::{
    Cache, EvictionPolicy, FillDecision, FillFollower, FillPermit, FillProgressState,
};
use ovcc_cachekey::{BurstyKeyParts, CacheKey, KeyParts, if_match_for_seed};
use ovcc_config::{
    Backend, CompatConfig, Config, Passthrough, Tls, TlsProtocol, WriteThroughConfig,
};
use ovcc_core::{CacheStatus, Error, ErrorKind, Result};
use ovcc_peer::{
    Discovery as PeerDiscovery, ExactValidator, ObjectIdentity, PeerClient, PeerClientOptions,
    PeerDirectory, PeerTransport, proto as peer_proto,
};
use ovcc_presigned::{PresignKind, Presigned};
use ovcc_storage::{
    CacheProvenance, DiskStorage, Lease, LeaseLayout, StagedSegment, Storage, StoreWrite,
    StoredMeta,
};
use ovcc_telemetry::access::TimeoutStage;
use ovcc_telemetry::metrics::{
    AccessProbe, ConnectionState, HttpServerErrorType, HttpServerRequestGuard, PeerAttemptOutcome,
    PeerAttemptStage, PeerFillFollowerActiveGuard, PeerFillFollowerResult, PeerProbeInFlightGuard,
    PeerResult, PeerServeActiveGuard, PeerServeResult, WriteThroughActiveGuard, WriteThroughPhase,
    WriteThroughResult,
};
use ovcc_upstream::{UploadAbort, UploadItem, UpstreamTiming};
use rand::SeedableRng as _;
use rand::rngs::StdRng;
use rand::seq::SliceRandom as _;
use sha2::{Digest as _, Sha256};
#[cfg(target_os = "linux")]
use tokio::io::Interest;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, timeout_at};
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status as GrpcStatus};
use tower::{Layer, Service};
use tracing::Instrument as _;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Cap on the request head we will buffer before giving up — generous for a
/// well-formed GET, bounded so a malicious client cannot grow it without limit.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Cap on one chunked framing line, either the hex chunk-size line with its
/// extensions or a single trailer field line. Mirrors [`MAX_HEAD_BYTES`], so a
/// framing line that never terminates cannot grow the decoder's carry buffer
/// without limit.
const MAX_CHUNK_LINE_BYTES: usize = 8 * 1024;
/// Cap on the whole trailer section following the terminating zero-size chunk.
const MAX_CHUNK_TRAILER_BYTES: usize = 16 * 1024;
/// Bounds one ready socket's uninterrupted `sendfile` progress before the task
/// cooperates with other runnable transfers. This is a scheduling quantum, not
/// a throughput limit; socket backpressure remains governed by readiness.
#[cfg(target_os = "linux")]
const SENDFILE_CHUNK_BYTES: usize = 16 * 1024 * 1024;
/// On Windows one overlapped `TransmitFile` completes only once its whole
/// chunk is sent, so the chunk also bounds how much progress the
/// [`DOWNSTREAM_WRITE_STALL_TIMEOUT`] inactivity window must cover (1 MiB per
/// 60 s ≈ 17 KB/s), approximating Linux's per-`sendfile` progress granularity.
#[cfg(windows)]
const SENDFILE_CHUNK_BYTES: usize = 1024 * 1024;
/// Windows TLS HITs are read into userspace and encrypted by rustls (there is
/// no Winsock kernel TLS); reads are double-buffered in chunks of this size.
#[cfg(windows)]
const TLS_FILE_CHUNK_BYTES: usize = 256 * 1024;
/// Active requests, and the closes that end their connections, get a bounded
/// graceful-completion window after acceptance stops. Idle connections start
/// their graceful close at once.
const CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Process-wide connection serial. Relaxed ordering is sufficient:
/// only uniqueness is required and each connection owns its request counter.
static CONNECTION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

mod client_close;
use client_close::{CloseMode, close_client};

/// A running proxy's bound addresses plus a shutdown handle.
///
mod local;
mod request_lease;
use request_lease::{Lookup, RequestLease};

/// Returned by [`serve`]. Call [`Serving::shutdown`] to stop the accept loop(s)
/// and await the server task — the e2e harness uses this to start/stop the proxy
/// (e.g. to prove the on-disk index reloads across a restart).
///
/// The main cache listener always binds; the Nucleus listener binds only when
/// `service.nucleus_port` is set, in which case a single accept task selects
/// across both listeners and one shutdown signal stops both.
pub struct Serving {
    addr: SocketAddr,
    nucleus_addr: Option<SocketAddr>,
    peer_addr: Option<SocketAddr>,
    health_addr: Option<SocketAddr>,
    metrics_addr: Option<SocketAddr>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

impl Serving {
    /// The bound `127.0.0.1:<ephemeral>` cache address clients connect to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The bound Nucleus listener address, or `None` when Nucleus is disabled.
    pub fn nucleus_addr(&self) -> Option<SocketAddr> {
        self.nucleus_addr
    }

    /// The bound peer listener address, or `None` when peer fill is disabled.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    /// The bound plaintext health listener address, or `None` when disabled.
    pub fn health_addr(&self) -> Option<SocketAddr> {
        self.health_addr
    }

    /// The bound plaintext Prometheus listener address, or `None` when disabled.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self.metrics_addr
    }

    /// Stop accepting, signal owned workers, and await every connection and
    /// cache-manager task that can retain the cache database.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

/// Which listener a connection arrived on. The Nucleus listener routes to the
/// Nucleus serve arms; the cache listener keeps the existing host-based dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerKind {
    Cache,
    Nucleus,
}

const PEER_BODY_STALL_TIMEOUT: Duration = Duration::from_secs(5);
const PEER_H2_MAX_RESET_STREAMS: usize = 32_768;
const DOWNSTREAM_WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(60);
const BAD_GATEWAY_BODY: &[u8] = b"Bad Gateway\n";
const GATEWAY_TIMEOUT_BODY: &[u8] = b"Gateway Timeout\n";
const RANDOM_SEED_ENV: &str = "OVCC_RANDOM_SEED";
/// The cache stores only a SHA-256 credential identity and expiry. This
/// conservative entry cap bounds process memory without preallocating it.
const PRESIGNED_AUTH_MAX_ENTRIES: usize = 1_000_000;

#[derive(Clone, Copy)]
enum PresignedAuthorization {
    Cached,
    Authorized,
    AuthorizedUncached,
    Rejected,
    Failed,
}

impl PresignedAuthorization {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cache_hit",
            Self::Authorized => "authorized",
            Self::AuthorizedUncached => "authorized_uncached",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

/// Cache-bypass inputs owned by the calling serve arm. URI exclusions retain
/// presigned validation while skipping cache access and origin auth probes.
#[derive(Clone, Copy)]
struct BypassPolicy {
    local_mode: bool,
    is_azure: bool,
    versioning: bool,
    uri_excluded: bool,
}

/// One request's pre-cache access decision, established by [`evaluate_access`]
/// before any cache lookup.
enum AccessDecision {
    /// The request may consult the cache. A forced miss denies reuse of any
    /// existing entry, so an expired signature or an authorization that could
    /// not be established goes to the origin instead.
    Proceed { forced_miss: bool },
    /// Cache-bypass policy applies, so the request is fetched fresh from the
    /// origin and relayed without reading or writing an entry.
    Bypass,
    /// The request is answered locally, touching neither the cache nor the
    /// origin. Boxed because a [`Response`] dwarfs the other variants.
    Reject(Box<Response>),
}

struct RunRandom(Mutex<StdRng>);

impl RunRandom {
    fn from_env() -> Self {
        match std::env::var(RANDOM_SEED_ENV) {
            Ok(value) => Self::from_configured_seed(&value),
            Err(std::env::VarError::NotPresent) => Self::from_seed(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                tracing::warn!(
                    event = "random_seed_invalid",
                    environment_variable = RANDOM_SEED_ENV,
                    reason = "not_utf8",
                    fallback_seed = 0_u64,
                );
                Self::from_seed(Some(0))
            }
        }
    }

    fn from_configured_seed(value: &str) -> Self {
        match value.parse::<u64>() {
            Ok(seed) => Self::from_seed(Some(seed)),
            Err(error) => {
                tracing::warn!(
                    event = "random_seed_invalid",
                    environment_variable = RANDOM_SEED_ENV,
                    reason = "not_unsigned_64_bit_integer",
                    fallback_seed = 0_u64,
                    error = %error,
                );
                Self::from_seed(Some(0))
            }
        }
    }

    fn from_seed(seed: Option<u64>) -> Self {
        let rng = seed.map_or_else(StdRng::from_os_rng, StdRng::seed_from_u64);
        Self(Mutex::new(rng))
    }

    fn shuffle<T>(&self, values: &mut [T]) {
        values.shuffle(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()));
    }
}

/// Blocking entrypoint for the `ovcc` binary that builds a Tokio runtime,
/// [`serve`]s, and runs until `SIGINT` (Ctrl-C) or, on Unix, `SIGTERM`, then
/// shuts down gracefully.
///
/// Kept here (not in the `ovcc` crate) so the thin binary needs only path-dep
/// crates and no direct Tokio dependency.
pub fn run(config: Config, storage_root: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let scheme = if config.tls.enabled { "https" } else { "http" };
        let stop = StopSignals::register();
        let serving = serve(config, storage_root).await?;
        tracing::info!("ovcc-proxy listening on {scheme}://{}", serving.addr());
        stop.wait().await;
        serving.shutdown().await;
        Ok(())
    })
}

/// The `SIGTERM` registration that Kubernetes and `docker stop` rely on. It is
/// made before [`serve`] starts accepting, so a signal that arrives while the
/// proxy is still starting is held until `run` can shut down gracefully instead
/// of being lost, or ignored when the process is PID 1. `SIGINT` keeps its
/// default action until then, so Ctrl-C still interrupts a slow start.
struct StopSignals {
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl StopSignals {
    fn register() -> Self {
        Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .inspect_err(
                    |error| tracing::warn!(event = "sigterm_handler_unavailable", error = %error),
                )
                .ok(),
        }
    }

    /// Resolves on `SIGINT` (Ctrl-C) or, on Unix, `SIGTERM`.
    async fn wait(self) {
        #[cfg(unix)]
        if let Some(mut terminate) = self.terminate {
            let signal = tokio::select! {
                _ = tokio::signal::ctrl_c() => "SIGINT",
                _ = terminate.recv() => "SIGTERM",
            };
            tracing::info!(event = "shutdown_signal_received", signal);
            return;
        }
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(all(test, unix))]
mod stop_signals_tests {
    use super::StopSignals;
    use std::time::Duration;

    /// Arrange a registration, act by delivering SIGTERM before anything waits,
    /// and assert the wait still ends, as it must when the signal lands while
    /// `serve` is starting. A lazy registration would let the default action
    /// end the test process instead.
    #[tokio::test]
    async fn general_run_sigterm_before_wait_is_held_until_awaited() {
        // Arrange.
        let stop = StopSignals::register();

        // Act.
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("kill runs");

        // Assert.
        assert!(sent.success());
        tokio::time::timeout(Duration::from_secs(5), stop.wait())
            .await
            .expect("a held SIGTERM ends the wait");
    }
}

fn build_peer_runtime(config: &Config) -> Result<Option<Arc<PeerRuntime>>> {
    if !config.peer.enabled {
        return Ok(None);
    }
    let peer_port = config
        .service
        .peer_port
        .filter(|port| *port != 0)
        .ok_or_else(|| Error::config("peer.enabled requires service.peerPort"))?;
    let discovery = match (
        config.peer.discovery.dns.as_deref(),
        config.peer.discovery.peers.as_slice(),
    ) {
        (Some(host), []) => PeerDiscovery::Dns {
            host: host.to_owned(),
            port: peer_port,
        },
        (None, peers) if !peers.is_empty() => PeerDiscovery::Explicit(peers.to_vec()),
        _ => {
            return Err(Error::config(
                "peer.enabled requires exactly one discovery source",
            ));
        }
    };
    let transport = if config.tls.enabled {
        let expected_server_name = config
            .peer
            .expected_server_name
            .clone()
            .ok_or_else(|| Error::config("TLS peer fill requires peer.expectedServerName"))?;
        PeerTransport::Tls {
            ca_certificate_path: config.tls.ca_certificate_path.as_ref().map(PathBuf::from),
            expected_server_name,
        }
    } else {
        PeerTransport::Plaintext
    };
    let budget = Duration::from_millis(config.peer.budget_ms);
    let client = PeerClient::new(PeerClientOptions {
        transport,
        probe_timeout: budget,
        connect_timeout: budget,
        first_byte_timeout: budget,
        stall_timeout: PEER_BODY_STALL_TIMEOUT,
    })
    .map_err(|error| Error::config(error.to_string()))?;
    let directory = PeerDirectory::from_env(
        discovery,
        Duration::from_secs(config.peer.refresh_secs),
        ovcc_peer::DEFAULT_NEGATIVE_TTL,
        config.service.self_names.clone(),
    );
    Ok(Some(Arc::new(PeerRuntime {
        directory,
        client,
        random: RunRandom::from_env(),
        budget,
        replicate: config.peer.replicate,
        cache_port: AtomicU16::new(config.service.port),
        min_object_bytes: config.peer.min_object_bytes,
    })))
}

/// Binds an ephemeral TLS listener, opens the disk cache, and runs the accept loop
/// until shutdown. Returns the bound [`SocketAddr`] and a shutdown handle.
///
/// `config.backends` maps a request `Host` to an upstream origin base URL.
/// `storage_root` is the disk cache root handed to [`DiskStorage::open`], which
/// also runs crash recovery / startup classification; a previously populated
/// root yields HITs immediately (the redb metadata store is durable), which is
/// what makes the restart-persistence test pass.
pub async fn serve(config: Config, storage_root: PathBuf) -> Result<Serving> {
    if config.service.local_mode && config.peer.enabled {
        return Err(Error::config(
            "service.localMode cannot be combined with peer.enabled",
        ));
    }
    let mut local_listener = if config.service.local_mode {
        Some(match config.service.local_endpoint.as_ref() {
            Some(path) => ovcc_platform::local::Listener::bind(std::path::Path::new(path))?,
            None => ovcc_platform::local::Listener::bind_default()?,
        })
    } else {
        None
    };
    let local_scheme = if config.tls.enabled { "https" } else { "http" };
    let local_slots = Arc::new(tokio::sync::Semaphore::new(128));
    ovcc_upstream::configure(ovcc_upstream::UpstreamOptions {
        resolver: ovcc_upstream::ResolverOptions {
            success_ttl: Duration::from_secs(
                config.upstream_balancer.resolver.cache.success_ttl_secs,
            ),
            failure_ttl: Duration::from_secs(
                config.upstream_balancer.resolver.cache.failure_ttl_secs,
            ),
            query_timeout: Duration::from_millis(
                config.upstream_balancer.resolver.query.timeout_ms,
            ),
            retries: config.upstream_balancer.resolver.query.retries,
            random_nameserver_order: config.upstream_balancer.resolver.query.random,
            fallback_nameservers: config.upstream_balancer.resolver.fallback.clone(),
            nameserver_override: None,
        },
        ca_bundle_path: PathBuf::from(&config.upstream_tls.trusted_certificate),
        tls_verify_depth: config.upstream_tls.verify_depth,
        pool_idle_timeout: Duration::from_secs(
            config.upstream_balancer.keepalive.idle_timeout_secs,
        ),
        pool_max_idle_per_host: config.upstream_balancer.keepalive.max_cached_connections,
        max_requests_per_connection: config
            .upstream_balancer
            .keepalive
            .max_requests_per_connection,
        connect_timeout: config.origin_timeouts.connect,
        response_header_timeout: config.origin_timeouts.response_header,
        body_stall_timeout: config.origin_timeouts.body_stall,
    })?;
    let peer = build_peer_runtime(&config)?;
    let prometheus = config
        .service
        .metrics_port
        .map(|_| ovcc_telemetry::metrics::prometheus(&config.telemetry.metric_prefix));
    // Disabled TLS skips acceptor construction so certificate settings do not
    // affect the plaintext listener.
    let acceptor = if config.tls.enabled {
        Some(
            TlsAcceptorProvider::new(config.tls.clone())
                .map_err(|e| Error::internal(e.to_string()))?,
        )
    } else {
        None
    };
    let peer_acceptor = if config.tls.enabled && peer.is_some() {
        Some(
            TlsAcceptorProvider::peer_control(config.tls.clone())
                .map_err(|error| Error::internal(error.to_string()))?,
        )
    } else {
        None
    };
    let write_through = config.write_through;
    let backends = tokio::task::spawn_blocking(move || {
        open_backend_stores(config.backends, &storage_root, &write_through)
    })
    .await
    .map_err(|error| Error::internal(format!("storage startup task failed: {error}")))??;
    let manager_stores = unique_backend_stores(&backends);
    let backends = Arc::new(backends);
    let self_names = Arc::new(config.service.self_names);
    let reset_compat = Arc::new(config.compat);
    // A cache generation counter namespaces every cache key, so
    // bumping it (via POST /__/cache/reset?gen=N) invalidates the whole cache
    // without touching disk — subsequent requests key under the new generation
    // and MISS. In-process for now (resets on restart); a durable counter is a
    // later step.
    let generation = Arc::new(AtomicI64::new(0));
    let presigned_auth = Arc::new(AuthorizationCache::new(PRESIGNED_AUTH_MAX_ENTRIES));

    let ctx = ConnCtx {
        backends,
        passthrough: config.passthrough,
        self_names,
        generation,
        reset_compat,
        instance: Arc::new(INSTANCE_ID.clone()),
        presigned_auth,
        local_mode: config.service.local_mode,
        peer: peer.clone(),
    };

    // Binds the client-facing listener from config. Production defaults to
    // `0.0.0.0:14128` (reachable as a Service); tests use `127.0.0.1:0` for an
    // ephemeral port. `local_addr()` reports the actual bound address either way.
    let content_address = if config.service.local_mode {
        "127.0.0.1"
    } else {
        &config.service.address
    };
    let bind_addr = format!("{}:{}", content_address, config.service.port);
    let listener = TcpListener::bind(&bind_addr).await?;
    let addr = listener.local_addr()?;
    if let Some(peer) = &peer {
        peer.cache_port.store(addr.port(), Ordering::Release);
    }

    // Binds the dedicated Nucleus listener when configured. `None` leaves
    // Nucleus off, while port zero lets the OS select an ephemeral port for
    // isolated tests. Uses the same bind address as the cache listener.
    let nucleus_listener = match config.service.nucleus_port {
        Some(port) => {
            let bind = format!("{}:{}", content_address, port);
            Some(TcpListener::bind(&bind).await?)
        }
        None => None,
    };
    let nucleus_addr = match &nucleus_listener {
        Some(l) => Some(l.local_addr()?),
        None => None,
    };

    let peer_listener = if peer.is_some() {
        let port = config
            .service
            .peer_port
            .filter(|port| *port != 0)
            .ok_or_else(|| Error::config("peer.enabled requires service.peerPort"))?;
        let bind = format!("{}:{port}", config.service.address);
        Some(TcpListener::bind(&bind).await?)
    } else {
        None
    };
    let peer_addr = match &peer_listener {
        Some(listener) => Some(listener.local_addr()?),
        None => None,
    };

    // The health listener stays plaintext and independent of TLS/cache routing.
    // `Some(0)` binds an ephemeral port for tests.
    let health_listener = match config.service.health_port {
        Some(port) => {
            let bind = format!("{}:{}", config.service.address, port);
            Some(TcpListener::bind(&bind).await?)
        }
        None => None,
    };
    let health_addr = match &health_listener {
        Some(listener) => Some(listener.local_addr()?),
        None => None,
    };

    // Pull metrics stay plaintext and independent of cache/TLS/health routing.
    // `Some(0)` binds an ephemeral port for tests.
    let metrics_listener = match config.service.metrics_port {
        Some(port) => {
            let bind = format!("{}:{}", config.service.address, port);
            Some(TcpListener::bind(&bind).await?)
        }
        None => None,
    };
    let metrics_addr = match &metrics_listener {
        Some(listener) => Some(listener.local_addr()?),
        None => None,
    };

    let (worker_shutdown_tx, worker_shutdown_rx) = watch::channel(false);
    let mut manager_handles = spawn_cache_managers(manager_stores, worker_shutdown_rx.clone());
    let peer_control_handle = match (peer_listener, peer.as_ref()) {
        (Some(listener), Some(_)) => {
            let peer_ctx = ctx.clone();
            let peer_shutdown = worker_shutdown_rx.clone();
            Some(tokio::spawn(async move {
                if let Err(error) =
                    serve_peer_control(listener, peer_acceptor, peer_ctx, peer_shutdown).await
                {
                    tracing::warn!(event = "peer_control_server_failed", error = %error);
                }
            }))
        }
        _ => None,
    };
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let mut connection_handles = JoinSet::new();
        let mut local_handles = JoinSet::new();
        let mut local_retry_at = tokio::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                accepted = local::accept(&mut local_listener, &mut local_retry_at) => {
                    if let Ok(connection) = accepted
                        && let Ok(slot) = Arc::clone(&local_slots).try_acquire_owned()
                    {
                        let context = ctx.clone();
                        let shutdown = worker_shutdown_rx.clone();
                        local_handles.spawn(async move {
                            let _slot = slot;
                            if local::handle(connection, context, local_scheme, shutdown).await.is_err() {
                                tracing::debug!(event = "local_lease_request_failed");
                            }
                        });
                    }
                },
                accepted = listener.accept() => spawn_connection(
                    accepted,
                    &acceptor,
                    &ctx,
                    ListenerKind::Cache,
                    &mut connection_handles,
                    worker_shutdown_rx.clone(),
                ),
                // Only resolves when a Nucleus listener is bound; otherwise this
                // branch parks forever so the select waits on the cache listener.
                accepted = accept_optional(&nucleus_listener) => spawn_connection(
                    accepted,
                    &acceptor,
                    &ctx,
                    ListenerKind::Nucleus,
                    &mut connection_handles,
                    worker_shutdown_rx.clone(),
                ),
                accepted = accept_optional(&health_listener) => {
                    spawn_health_connection(accepted, &ctx);
                },
                accepted = accept_optional(&metrics_listener) => {
                    if let Some(prometheus) = prometheus.as_ref() {
                        spawn_metrics_connection(accepted, Arc::clone(prometheus));
                    }
                },
                completed = connection_handles.join_next(), if !connection_handles.is_empty() => {
                    log_connection_task_result(completed);
                },
                completed = local_handles.join_next(), if !local_handles.is_empty() => {
                    log_connection_task_result(completed);
                },
            }
        }

        // No listener can spawn more cache-owning work after this point.
        drop(listener);
        drop(nucleus_listener);
        drop(local_listener);
        let _ = worker_shutdown_tx.send(true);

        // Local requests cancel network/control waits themselves, then join
        // their storage workers. Aborting those joins could retain redb.
        while let Some(completed) = local_handles.join_next().await {
            log_connection_task_result(Some(completed));
        }

        let graceful_connections = async {
            while let Some(completed) = connection_handles.join_next().await {
                log_connection_task_result(Some(completed));
            }
        };
        if timeout(CONNECTION_SHUTDOWN_GRACE, graceful_connections)
            .await
            .is_err()
        {
            tracing::warn!(
                event = "connection_shutdown_grace_expired",
                grace_seconds = CONNECTION_SHUTDOWN_GRACE.as_secs(),
            );
            connection_handles.abort_all();
            while let Some(completed) = connection_handles.join_next().await {
                log_connection_task_result(Some(completed));
            }
        }

        if let Some(handle) = peer_control_handle
            && let Err(error) = handle.await
        {
            tracing::warn!(event = "peer_control_task_failed", error = %error);
        }

        // A manager may be awaiting spawn_blocking. Never abort it: dropping
        // that JoinHandle would detach the closure and preserve its redb owner.
        while let Some(completed) = manager_handles.join_next().await {
            if let Err(error) = completed {
                tracing::warn!(
                    event = "cache_manager_task_failed",
                    error = %error,
                );
            }
        }
    });

    Ok(Serving {
        addr,
        nucleus_addr,
        peer_addr,
        health_addr,
        metrics_addr,
        shutdown_tx: Some(shutdown_tx),
        handle,
    })
}

async fn cache_lookup_blocking(cache: &Arc<Cache<DiskStorage>>, key: &str) -> Result<Lookup> {
    if local::is_request() {
        local::retain_source(|| Some(cache.storage().local_path_source(key)));
        if let Some(candidate) = cache.storage().cached_local_path(key) {
            return Ok(Lookup::Hit(RequestLease::Path(candidate)));
        }
        let source = cache.storage().local_path_source(key);
        #[cfg(test)]
        let lookup_key = key.to_owned();
        return local::blocking(move || {
            #[cfg(test)]
            local::hold_lookup_for_test(&lookup_key);
            source.prepare()
        })
        .await
        .map_err(|error| Error::internal(error.to_string()))?
        .map(Into::into);
    }
    let cache = Arc::clone(cache);
    let key = key.to_owned();
    local::blocking(move || {
        #[cfg(test)]
        local::hold_lookup_for_test(&key);
        cache.lookup(&key).map(Into::into)
    })
    .await
    .map_err(|error| Error::internal(format!("cache lookup task failed: {error}")))?
}

async fn cache_refresh_blocking(
    cache: &Arc<Cache<DiskStorage>>,
    key: &str,
    expected: StoredMeta,
    permit: FillPermit,
    expiry: RevalidationExpiry,
) -> Result<Option<Lease>> {
    let cache = Arc::clone(cache);
    let key = key.to_owned();
    local::blocking(move || match expiry {
        RevalidationExpiry::CachePolicy => cache.refresh_acquired(&key, &expected, permit),
        RevalidationExpiry::Explicit(expires_at) => {
            cache.refresh_acquired_until(&key, &expected, permit, expires_at)
        }
    })
    .await
    .map_err(|error| Error::internal(format!("cache refresh task failed: {error}")))?
}

async fn cache_peek_blocking(
    cache: &Arc<Cache<DiskStorage>>,
    key: &str,
) -> Result<Option<StoredMeta>> {
    let cache = Arc::clone(cache);
    let key = key.to_owned();
    local::blocking(move || cache.peek(&key))
        .await
        .map_err(|error| Error::internal(format!("cache peek task failed: {error}")))?
}

async fn cache_remove_blocking(cache: &Arc<Cache<DiskStorage>>, key: &str) -> Result<()> {
    let cache = Arc::clone(cache);
    let key = key.to_owned();
    local::blocking(move || cache.remove(&key))
        .await
        .map_err(|error| Error::internal(format!("cache removal task failed: {error}")))?
}

/// Cache and write-through state owned by one canonical filesystem root.
///
/// Multiple backends may share this value, but a redb root is opened exactly
/// once. Staging remains beside the corresponding cache, so upload recovery and
/// Azure block fragments cannot cross backend storage boundaries.
struct BackendStore {
    cache: Arc<Cache<DiskStorage>>,
    write_through: WriteThroughState,
}

#[derive(Debug)]
struct WriteThroughState {
    staging_dir: PathBuf,
    chunk_size: usize,
    token_ttl: Duration,
}

/// One configured backend paired with its selected runtime store.
struct BackendRuntime {
    config: Backend,
    store: Arc<BackendStore>,
}

/// Opens one store per distinct canonical root and pairs each backend with it.
///
/// Missing and blank per-backend roots select `fallback_root`. Creating and
/// canonicalizing each root before opening redb deduplicates equivalent paths
/// and symlink aliases.
fn open_backend_stores(
    backends: Vec<Backend>,
    fallback_root: &Path,
    write_through: &WriteThroughConfig,
) -> Result<Vec<BackendRuntime>> {
    let mut stores: HashMap<PathBuf, Arc<BackendStore>> = HashMap::new();
    let mut runtimes = Vec::with_capacity(backends.len());

    #[cfg(not(feature = "test-optional-keys"))]
    let azure_policy = Arc::new(azure_optional_keys::AzurePolicy {
        backends: backends.clone(),
    });
    for backend in backends {
        let configured_root = backend
            .cache_root
            .as_deref()
            .filter(|root| !root.trim().is_empty())
            .map(Path::new)
            .unwrap_or(fallback_root);
        std::fs::create_dir_all(configured_root)?;
        let root = std::fs::canonicalize(configured_root)?;

        let store = match stores.get(&root) {
            Some(store) => Arc::clone(store),
            None => {
                let configured_staging = root.join(&write_through.staging_subdir);
                std::fs::create_dir_all(&configured_staging)?;
                let staging_dir = std::fs::canonicalize(&configured_staging)?;
                if !staging_dir.starts_with(&root) {
                    return Err(Error::config(format!(
                        "nginx.writeThrough.stagingSubdir resolves outside backend cache root {}",
                        root.display()
                    )));
                }
                let token_ttl = Duration::from_secs(write_through.staging_token_ttl_seconds);
                BlockStaging::discard_after_restart(&staging_dir)?;
                let store = Arc::new(BackendStore {
                    cache: Arc::new(Cache::with_policy(
                        {
                            #[cfg(feature = "test-optional-keys")]
                            let storage = DiskStorage::open_with_key_policy(
                                &root,
                                Some(Arc::new(optional_key_fixture::FixturePolicy)),
                            )?;
                            #[cfg(not(feature = "test-optional-keys"))]
                            let storage = DiskStorage::open_with_key_policy(
                                &root,
                                Some(azure_policy.clone()),
                            )?;
                            storage
                        },
                        EvictionPolicy {
                            validity_200_secs: Some(backend.cache_policy.validity_200_secs),
                            validity_206_secs: Some(backend.cache_policy.validity_206_secs),
                            max_idle_secs: backend.cache_policy.max_idle_secs,
                            peer_copy_max_idle_secs: backend.cache_policy.peer_copy_max_idle_secs,
                            max_bytes: backend.cache_policy.max_bytes,
                            min_free_bytes: backend.cache_policy.min_free_bytes,
                            lock_age_secs: backend.cache_policy.lock_age_secs,
                            lock_timeout_secs: backend.cache_policy.lock_timeout_secs,
                        },
                    )),
                    write_through: WriteThroughState {
                        staging_dir,
                        chunk_size: write_through.request_body_chunk_size,
                        token_ttl,
                    },
                });
                stores.insert(root, Arc::clone(&store));
                store
            }
        };
        runtimes.push(BackendRuntime {
            config: backend,
            store,
        });
    }

    Ok(runtimes)
}

/// Returns each canonical-root store exactly once for manager scheduling.
fn unique_backend_stores(backends: &[BackendRuntime]) -> Vec<Arc<BackendStore>> {
    let mut seen = HashSet::new();
    backends
        .iter()
        .filter_map(|runtime| {
            let identity = Arc::as_ptr(&runtime.store);
            seen.insert(identity).then(|| Arc::clone(&runtime.store))
        })
        .collect()
}

#[cfg(test)]
struct ManagerStepTestHook {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static MANAGER_STEP_TEST_HOOK: std::sync::OnceLock<Mutex<Option<ManagerStepTestHook>>> =
    std::sync::OnceLock::new();

/// Runs one independently tracked cache manager per canonical storage root.
fn spawn_cache_managers(
    stores: Vec<Arc<BackendStore>>,
    shutdown: watch::Receiver<bool>,
) -> JoinSet<()> {
    let mut managers = JoinSet::new();
    for store in stores {
        let lease_store = Arc::clone(&store);
        let mut lease_shutdown = shutdown.clone();
        managers.spawn(async move {
            loop {
                let store = Arc::clone(&lease_store);
                let delay = match tokio::task::spawn_blocking(move || store.cache.storage().maintain_local_leases()).await {
                    Ok(Ok(())) => Duration::from_secs(1),
                    Ok(Err(error)) => {
                        tracing::warn!(event = "local_lease_maintenance_failed", error = %error);
                        Duration::from_secs(10)
                    }
                    Err(error) => {
                        tracing::warn!(event = "local_lease_maintenance_task_failed", error = %error);
                        Duration::from_secs(10)
                    }
                };
                if *lease_shutdown.borrow() { break; }
                tokio::select! {
                    _ = wait_for_shutdown(&mut lease_shutdown) => break,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            match tokio::task::spawn_blocking(move || lease_store.cache.storage().maintain_local_leases()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(event = "local_lease_shutdown_failed", error = %error),
                Err(error) => tracing::warn!(event = "local_lease_shutdown_task_failed", error = %error),
            }
        });
        let mut shutdown = shutdown.clone();
        managers.spawn(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }

                // Await this closure unconditionally. Cancelling the async
                // waiter does not cancel spawn_blocking and would detach a
                // Cache -> DiskStorage -> redb owner.
                let managed_store = Arc::clone(&store);
                let step = tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(hook) = MANAGER_STEP_TEST_HOOK
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .expect("manager test hook lock")
                        .take()
                    {
                        let _ = hook.started.send(());
                        let _ = hook.release.recv();
                    }
                    managed_store.cache.manager_step()
                })
                .await;
                let delay = match step {
                    Ok(Ok(report)) => {
                        tracing::debug!(
                            event = "cache_manager_step",
                            evicted_count = report.evicted_count,
                            origin_idle_evicted_count = report.origin_idle_evicted_count,
                            peer_copy_idle_evicted_count = report.peer_copy_idle_evicted_count,
                            pressure_evicted_count = report.pressure_evicted_count,
                            next_delay_ms =
                                u64::try_from(report.next_delay.as_millis()).unwrap_or(u64::MAX),
                        );
                        report.next_delay
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            event = "cache_manager_step_failed",
                            error = %error,
                        );
                        Duration::from_secs(10)
                    }
                    Err(error) => {
                        tracing::warn!(
                            event = "cache_manager_blocking_task_failed",
                            error = %error,
                        );
                        Duration::from_secs(10)
                    }
                };

                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown) => break,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        });
    }
    managers
}

/// Accepts from an optional listener, parking forever when it is absent so a
/// `tokio::select!` arm can wait on it unconditionally. Used so the Nucleus
/// accept branch is a no-op when Nucleus is disabled.
async fn accept_optional(
    listener: &Option<TcpListener>,
) -> std::io::Result<(TcpStream, SocketAddr)> {
    match listener {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
static CONNECTION_SPAWN_TEST_REPORTER: std::sync::OnceLock<
    Mutex<Option<std::sync::mpsc::Sender<()>>>,
> = std::sync::OnceLock::new();

/// Spawns a per-connection task for an accepted socket, tagging it with the
/// listener it arrived on. Accept errors are logged and dropped (the loop
/// continues), matching the previous single-listener behavior.
fn spawn_connection(
    accepted: std::io::Result<(TcpStream, SocketAddr)>,
    acceptor: &Option<TlsAcceptorProvider>,
    ctx: &ConnCtx,
    listener_kind: ListenerKind,
    connections: &mut JoinSet<()>,
    shutdown: watch::Receiver<bool>,
) {
    match accepted {
        Ok((tcp, _peer)) => {
            #[cfg(test)]
            if let Some(reporter) = CONNECTION_SPAWN_TEST_REPORTER
                .get_or_init(|| Mutex::new(None))
                .lock()
                .expect("connection reporter lock")
                .take()
            {
                let _ = reporter.send(());
            }
            let acceptor = acceptor.as_ref().map(TlsAcceptorProvider::acceptor);
            let ctx = ctx.clone();
            connections.spawn(async move {
                let result = handle_connection(tcp, acceptor, ctx, listener_kind, shutdown).await;
                if let Err(err) = result {
                    tracing::warn!("ovcc-proxy connection error: {err}");
                }
            });
        }
        Err(err) => tracing::warn!("ovcc-proxy accept error: {err}"),
    }
}

fn log_connection_task_result(completed: Option<std::result::Result<(), tokio::task::JoinError>>) {
    if let Some(Err(error)) = completed {
        tracing::warn!(
            event = "connection_task_failed",
            error = %error,
        );
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Tracks each accepted connection as active and as one of
/// reading/writing/waiting. Task exit removes both counters.
struct ConnectionMetrics {
    state: ConnectionState,
}

impl ConnectionMetrics {
    fn accepted() -> Self {
        ovcc_telemetry::metrics::connection_delta(ConnectionState::Active, 1);
        ovcc_telemetry::metrics::connection_delta(ConnectionState::Reading, 1);
        Self {
            state: ConnectionState::Reading,
        }
    }

    fn transition(&mut self, next: ConnectionState) {
        if self.state == next {
            return;
        }
        ovcc_telemetry::metrics::connection_delta(self.state, -1);
        ovcc_telemetry::metrics::connection_delta(next, 1);
        self.state = next;
    }
}

impl Drop for ConnectionMetrics {
    fn drop(&mut self) {
        ovcc_telemetry::metrics::connection_delta(self.state, -1);
        ovcc_telemetry::metrics::connection_delta(ConnectionState::Active, -1);
    }
}

fn is_health_path(path: &str) -> bool {
    path.starts_with("/healthz")
}

#[cfg(feature = "test-fault-injection")]
const TEST_FAULT_PATH: &str = "/__ovcc/testing/faults/upstream-request-body";

#[cfg(feature = "test-fault-injection")]
fn test_control_response(
    request: &[u8],
    controller: &Arc<ovcc_testing::FaultController>,
) -> Option<Vec<u8>> {
    let first_line = request
        .split(|byte| *byte == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())?;
    let mut fields = first_line.trim_end_matches('\r').split_ascii_whitespace();
    let method = fields.next()?;
    let target = fields.next()?;
    let version = fields.next()?;
    if !version.starts_with("HTTP/") || fields.next().is_some() {
        return None;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != TEST_FAULT_PATH {
        return None;
    }
    if method == "HEAD" {
        let response = Response::head(405, "Method Not Allowed", None, Vec::new(), None);
        return Some(render_head(&response.head, false).into_bytes());
    }

    let response = match method {
        "GET" if query.is_empty() => controller
            .snapshot()
            .map(|snapshot| (200, "OK", snapshot))
            .map_err(|error| (500, "Internal Server Error", error.to_string())),
        "DELETE" if query.is_empty() => controller
            .clear()
            .map(|snapshot| {
                tracing::warn!(
                    event = "test_fault_cleared",
                    generation = snapshot.generation,
                );
                (200, "OK", snapshot)
            })
            .map_err(|error| match error {
                ovcc_testing::ControllerError::Idle => (409, "Conflict", error.to_string()),
                _ => (500, "Internal Server Error", error.to_string()),
            }),
        "POST" => match parse_test_fault_query(query) {
            Ok((skip_matches, after_bytes)) => controller
                .arm(skip_matches, after_bytes)
                .map(|snapshot| {
                    tracing::warn!(
                        event = "test_fault_armed",
                        fault = "upstream_request_body",
                        generation = snapshot.generation,
                        skips_remaining = snapshot.skips_remaining,
                        after_bytes = snapshot.after_bytes.unwrap_or_default(),
                    );
                    (201, "Created", snapshot)
                })
                .map_err(|error| match error {
                    ovcc_testing::ControllerError::Busy => (409, "Conflict", error.to_string()),
                    ovcc_testing::ControllerError::SkipMatchesOutOfRange
                    | ovcc_testing::ControllerError::AfterBytesOutOfRange => {
                        (400, "Bad Request", error.to_string())
                    }
                    _ => (500, "Internal Server Error", error.to_string()),
                }),
            Err(error) => Err((400, "Bad Request", error.to_owned())),
        },
        _ => {
            return Some(test_control_error_response(
                405,
                "Method Not Allowed",
                "unsupported control method",
            ));
        }
    };

    Some(match response {
        Ok((status, reason, snapshot)) => match serde_json::to_vec(&snapshot) {
            Ok(body) => test_control_http_response(status, reason, &body),
            Err(error) => {
                tracing::error!(event = "test_fault_response_failed", error = %error);
                test_control_error_response(
                    500,
                    "Internal Server Error",
                    "snapshot serialization failed",
                )
            }
        },
        Err((status, reason, error)) => test_control_error_response(status, reason, &error),
    })
}

#[cfg(feature = "test-fault-injection")]
fn parse_test_fault_query(query: &str) -> std::result::Result<(u64, u64), &'static str> {
    let mut skip_matches = None;
    let mut after_bytes = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let parsed = value
            .parse::<u64>()
            .map_err(|_| "control values must be unsigned integers")?;
        match name.as_ref() {
            "skip_matches" if skip_matches.is_none() => skip_matches = Some(parsed),
            "after_bytes" if after_bytes.is_none() => after_bytes = Some(parsed),
            _ => return Err("control query contains an unknown or duplicate field"),
        }
    }
    let after_bytes = after_bytes.ok_or("after_bytes is required")?;
    Ok((skip_matches.unwrap_or(0), after_bytes))
}

#[cfg(feature = "test-fault-injection")]
fn test_control_http_response(status: u16, reason: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[cfg(feature = "test-fault-injection")]
fn test_control_error_response(status: u16, reason: &str, message: &str) -> Vec<u8> {
    let body = serde_json::json!({ "error": message }).to_string();
    test_control_http_response(status, reason, body.as_bytes())
}

/// Serves the plaintext health endpoint without entering the TLS/cache path.
fn spawn_health_connection(accepted: std::io::Result<(TcpStream, SocketAddr)>, _ctx: &ConnCtx) {
    #[cfg(feature = "test-optional-keys")]
    let fixture_ctx = _ctx.clone();
    match accepted {
        Ok((mut tcp, _peer)) => {
            tokio::spawn(async move {
                let mut request = Vec::with_capacity(1024);
                let mut chunk = [0_u8; 1024];
                while request.len() < MAX_HEAD_BYTES {
                    let count = match tcp.read(&mut chunk).await {
                        Ok(0) => return,
                        Ok(count) => count,
                        Err(err) => {
                            tracing::warn!("ovcc-proxy health connection error: {err}");
                            return;
                        }
                    };
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                #[cfg(feature = "test-optional-keys")]
                if request.starts_with(b"GET /__/test/optional-keys HTTP/") {
                    let response = optional_key_fixture::diagnostics(fixture_ctx).await;
                    if let Err(error) = tcp.write_all(&response).await {
                        tracing::warn!(event = "fixture_diagnostics_write_failed", %error);
                    }
                    return;
                }

                #[cfg(feature = "test-fault-injection")]
                if let Some(response) =
                    test_control_response(&request, ovcc_testing::global_controller())
                {
                    if let Err(err) = tcp.write_all(&response).await {
                        tracing::warn!(event = "test_fault_control_response_error", error = %err);
                    }
                    return;
                }

                let healthy = request
                    .split(|byte| *byte == b'\n')
                    .next()
                    .and_then(|line| std::str::from_utf8(line).ok())
                    .and_then(|line| {
                        let mut fields = line.trim_end_matches('\r').split_ascii_whitespace();
                        Some((fields.next()?, fields.next()?, fields.next()?))
                    })
                    .is_some_and(|(method, path, version)| {
                        method == "GET" && is_health_path(path) && version.starts_with("HTTP/")
                    });
                let head_only = parse_request_head(&request)
                    .is_none_or(|head| head.method.eq_ignore_ascii_case("HEAD"));
                let response = if head_only {
                    b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n".as_slice()
                } else if healthy {
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 8\r\nConnection: close\r\n\r\nhealthy\n".as_slice()
                } else {
                    b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 10\r\nConnection: close\r\n\r\nNot Found\n".as_slice()
                };
                if let Err(err) = tcp.write_all(response).await {
                    tracing::warn!("ovcc-proxy health response error: {err}");
                }
            });
        }
        Err(err) => tracing::warn!("ovcc-proxy health accept error: {err}"),
    }
}

/// Serves only the canonical plaintext Prometheus endpoint.
fn spawn_metrics_connection(
    accepted: std::io::Result<(TcpStream, SocketAddr)>,
    prometheus: Arc<ovcc_telemetry::metrics::Prometheus>,
) {
    match accepted {
        Ok((mut tcp, _peer)) => {
            tokio::spawn(async move {
                let mut request = Vec::with_capacity(1024);
                let mut chunk = [0_u8; 1024];
                while request.len() < MAX_HEAD_BYTES {
                    let count = match tcp.read(&mut chunk).await {
                        Ok(0) => return,
                        Ok(count) => count,
                        Err(err) => {
                            tracing::warn!("ovcc-proxy metrics connection error: {err}");
                            return;
                        }
                    };
                    request.extend_from_slice(&chunk[..count]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                let scrape = request
                    .split(|byte| *byte == b'\n')
                    .next()
                    .and_then(|line| std::str::from_utf8(line).ok())
                    .and_then(|line| {
                        let mut fields = line.trim_end_matches('\r').split_ascii_whitespace();
                        Some((fields.next()?, fields.next()?, fields.next()?))
                    })
                    .is_some_and(|(method, path, version)| {
                        method == "GET" && path == "/cache_metrics" && version.starts_with("HTTP/")
                    });
                let (status, reason, content_type, body) = if scrape {
                    (
                        200,
                        "OK",
                        "text/plain; version=0.0.4; charset=utf-8",
                        prometheus.scrape(),
                    )
                } else {
                    (404, "Not Found", "text/plain", "Not Found\n".to_owned())
                };
                let head_only = parse_request_head(&request)
                    .is_none_or(|head| head.method.eq_ignore_ascii_case("HEAD"));
                let head = if head_only {
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                };
                if tcp.write_all(head.as_bytes()).await.is_ok() && !head_only {
                    let _ = tcp.write_all(body.as_bytes()).await;
                }
            });
        }
        Err(err) => tracing::warn!("ovcc-proxy metrics accept error: {err}"),
    }
}

struct TlsAcceptorProvider(ovcc_tls::TlsAcceptorProvider);

impl TlsAcceptorProvider {
    fn new(tls: Tls) -> std::result::Result<Self, BoxError> {
        Self::with_alpn(tls, Vec::new())
    }

    fn peer_control(tls: Tls) -> std::result::Result<Self, BoxError> {
        Self::with_alpn(tls, vec![b"h2".to_vec()])
    }

    fn with_alpn(tls: Tls, alpn_protocols: Vec<Vec<u8>>) -> std::result::Result<Self, BoxError> {
        ovcc_tls::TlsAcceptorProvider::new(ovcc_tls::ServerOptions {
            certificate_path: tls.certificate_path.map(PathBuf::from),
            private_key_path: tls.private_key_path.map(PathBuf::from),
            auto_reload_secret: tls.auto_reload_secret,
            ssl_protocols: tls
                .ssl_protocols
                .iter()
                .map(|protocol| match protocol {
                    TlsProtocol::Tls12 => ovcc_tls::TlsProtocol::Tls12,
                    TlsProtocol::Tls13 => ovcc_tls::TlsProtocol::Tls13,
                })
                .collect(),
            alpn_protocols,
        })
        .map(Self)
    }

    fn acceptor(&self) -> TlsAcceptor {
        self.0.acceptor()
    }
}

/// Parsed request line plus the headers needed to route, key, and forward.
#[derive(Clone)]
struct RequestHead {
    method: String,
    path: String,
    query: String,
    /// Distinguishes `/path?` from `/path` when the raw query is empty.
    query_present: bool,
    /// HTTP version token from the request line (e.g. `HTTP/1.1`), for the
    /// access log's `http_protocol`.
    http_version: String,
    host: Option<String>,
    /// All request headers (name, value), in wire order. Forwarded to the
    /// origin on a MISS so header-keyed backends (auth, If-Match, Range) behave.
    headers: Vec<(String, String)>,
    /// Declared request-body length (`Content-Length`), or 0 when absent. Used
    /// by the transparent-proxy path to stream exactly this many body bytes.
    content_length: usize,
    /// Exact wire bytes from the request line through the final `\r\n\r\n`.
    head_length: usize,
}

/// Where a response's body bytes come from. Kept separate from the head so the
/// driver ([`drive_response`]) — not the serve arm — performs the write, and the
/// kTLS HIT body stays a leased file fd rather than a buffered response body
/// (the zero-copy guarantee, since it is `sendfile`'d and never routed through
/// userspace).
enum BodySource {
    LocalPath(ovcc_storage::PathCandidate),
    /// A zero-copy HIT that `sendfile(2)`s the leased file fd straight to the
    /// kTLS socket.
    Sendfile(ovcc_storage::Lease),
    /// A small body buffered in memory (MISS / bypass / operator responses).
    Buffered(Vec<u8>),
    /// The serve arm already wrote the whole response to the socket. Used by the
    /// transparent-proxy, write-through, and MISS-fill arms, which interleave
    /// upload or origin reads with client writes. The connection is reused only
    /// when the arm marks its announced keep-alive framing complete.
    Streamed,
}

/// The TLS client stream after the handshake: kernel TLS on Linux, so HIT
/// bodies stay zero-copy `sendfile`; rustls on Windows, which has no Winsock
/// kernel TLS.
#[cfg(target_os = "linux")]
type TlsClientStream = ktls::KtlsStream<TcpStream>;
#[cfg(windows)]
type TlsClientStream = Box<tokio_rustls::server::TlsStream<TcpStream>>;

/// The transport a rustls handshake runs over. Linux drains the handshake
/// through `ktls::CorkStream` so kTLS can take over the record stream.
#[cfg(target_os = "linux")]
type HandshakeIo = CorkStream<TcpStream>;
#[cfg(windows)]
type HandshakeIo = TcpStream;

fn handshake_io(tcp: TcpStream) -> HandshakeIo {
    #[cfg(target_os = "linux")]
    return CorkStream::new(tcp);
    #[cfg(windows)]
    tcp
}

/// Hands a completed server handshake to the platform's client TLS stream.
async fn into_client_tls(
    tls: tokio_rustls::server::TlsStream<HandshakeIo>,
) -> std::io::Result<TlsClientStream> {
    #[cfg(target_os = "linux")]
    return ktls::config_ktls_server(tls)
        .await
        .map_err(std::io::Error::other);
    #[cfg(windows)]
    Ok(Box::new(tls))
}

/// The client-facing HTTP/1 stream after transport setup.
///
/// TLS keeps the existing rustls → kTLS stream intact on Linux; plaintext uses
/// the accepted TCP socket directly. Routing and response code operate on this
/// one transport-neutral type, while [`sendfile_lease`] still selects the
/// concrete zero-copy primitive for the socket.
enum ClientStream {
    Tls(TlsClientStream),
    Plain(TcpStream),
    Local(&'static str),
}

impl ClientStream {
    fn scheme(&self) -> &'static str {
        match self {
            Self::Tls(_) => "https",
            Self::Plain(_) => "http",
            Self::Local(scheme) => scheme,
        }
    }

    /// The underlying TCP socket.
    fn tcp(&self) -> std::io::Result<&TcpStream> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Tls(stream) => Ok(stream.get_ref()),
            #[cfg(windows)]
            Self::Tls(stream) => Ok(stream.get_ref().0),
            Self::Plain(stream) => Ok(stream),
            Self::Local(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "local leases have no TCP body transport",
            )),
        }
    }

    /// Forces an abortive close after a downstream inactivity timeout.
    fn reset_on_drop(&self) {
        if let Ok(socket) = self.tcp() {
            let _ = socket.set_zero_linger();
        }
    }
}

tokio::task_local! {
    static DOWNSTREAM_TIMED_OUT: std::cell::Cell<bool>;
    static DOWNSTREAM_RESPONSE_STATUS: std::cell::Cell<u16>;
    static DOWNSTREAM_RESPONSE_STARTED: std::cell::Cell<bool>;
    static DOWNSTREAM_BODY_BYTES: std::cell::Cell<u64>;
    static DOWNSTREAM_REUSE_ALLOWED: std::cell::Cell<bool>;
}

fn with_downstream_request_state<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    DOWNSTREAM_TIMED_OUT.scope(
        std::cell::Cell::new(false),
        DOWNSTREAM_RESPONSE_STATUS.scope(
            std::cell::Cell::new(500),
            DOWNSTREAM_RESPONSE_STARTED.scope(
                std::cell::Cell::new(false),
                DOWNSTREAM_BODY_BYTES.scope(
                    std::cell::Cell::new(0),
                    DOWNSTREAM_REUSE_ALLOWED.scope(std::cell::Cell::new(false), future),
                ),
            ),
        ),
    )
}

/// Records whether the client permits reuse and sent no bytes beyond its head
/// that the serve arms leave unread. Inline arms announce
/// `Connection: keep-alive` only under this permission, which [`route`]
/// derives from the client's own head because the arms see an upstream policy
/// head.
fn set_downstream_reuse_allowed(allowed: bool) {
    let _ = DOWNSTREAM_REUSE_ALLOWED.try_with(|reuse| reuse.set(allowed));
}

fn downstream_reuse_allowed() -> bool {
    DOWNSTREAM_REUSE_ALLOWED
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
}

fn mark_downstream_timeout() {
    let _ = DOWNSTREAM_TIMED_OUT.try_with(|timed_out| timed_out.set(true));
}

fn downstream_timed_out() -> bool {
    DOWNSTREAM_TIMED_OUT
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
}

fn downstream_response_status() -> u16 {
    DOWNSTREAM_RESPONSE_STATUS
        .try_with(std::cell::Cell::get)
        .unwrap_or(500)
}

fn mark_downstream_response_started(status: u16) {
    let _ = DOWNSTREAM_RESPONSE_STATUS.try_with(|current| current.set(status));
    let _ = DOWNSTREAM_RESPONSE_STARTED.try_with(|started| started.set(true));
}

fn downstream_response_started() -> bool {
    DOWNSTREAM_RESPONSE_STARTED
        .try_with(std::cell::Cell::get)
        .unwrap_or(false)
}

fn downstream_body_bytes() -> u64 {
    DOWNSTREAM_BODY_BYTES
        .try_with(std::cell::Cell::get)
        .unwrap_or(0)
}

fn add_downstream_body_bytes(bytes: u64) {
    let _ = DOWNSTREAM_BODY_BYTES.try_with(|total| total.set(total.get().saturating_add(bytes)));
}

impl AsyncRead for ClientStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Local(_) => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for ClientStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Local(_) => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Local(_) => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Local(_) => Poll::Ready(Ok(())),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tls(stream) => stream.is_write_vectored(),
            Self::Plain(stream) => stream.is_write_vectored(),
            Self::Local(_) => true,
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Self::Plain(stream) => Pin::new(stream).poll_write_vectored(cx, bufs),
            Self::Local(_) => Poll::Ready(Ok(bufs.iter().map(|buf| buf.len()).sum())),
        }
    }
}

/// The response head a serve arm produced; [`render_head`] turns it into wire
/// bytes. Held as metadata so the telemetry [`TelemetryService`] can log status
/// and cache status without touching the body.
///
/// Public for the `hit_serve` bench.
#[doc(hidden)]
pub struct ResponseHead {
    pub status: u16,
    pub reason: &'static str,
    /// `X-Cache-Status` value (HIT/MISS/BYPASS), or `None` for paths that carry
    /// none (the transparent proxy, a 421).
    pub cache_status: Option<&'static str>,
    /// Extra response headers in wire order (`ETag`, `Content-Type`,
    /// `Accept-Ranges`, relayed origin headers, ...).
    pub headers: Vec<(String, String)>,
    /// `Some(n)` emits `Content-Length: n`; `None` omits it (e.g. a `304`, which
    /// must not carry one).
    pub content_length: Option<u64>,
}

/// Bounded access-log outcome for local storage after a MISS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheStoreResult {
    Stored,
    NotStored,
    LockBypass,
    Incomplete,
    StorageError,
}

impl CacheStoreResult {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "STORED",
            Self::NotStored => "NOT_STORED",
            Self::LockBypass => "LOCK_BYPASS",
            Self::Incomplete => "INCOMPLETE",
            Self::StorageError => "STORAGE_ERROR",
        }
    }
}

/// What a serve arm produced — the response head, where its body comes from,
/// and the access-log metadata. The driver writes the head + body; the telemetry
/// Layer reads only the metadata (never the body), so the kTLS sendfile HIT path
/// is observed but not routed through this type.
struct Response {
    head: ResponseHead,
    body: BodySource,
    /// `$body_bytes_sent`: known up front for every variant (the leased object
    /// size, the buffered length, or the count the streaming arm wrote).
    bytes_sent: u64,
    /// Origin status when an upstream was contacted (absent on a pure HIT).
    upstream_status: Option<u16>,
    /// Per-stage origin latencies, when an upstream was contacted.
    upstream_timing: Option<UpstreamTiming>,
    /// Origin `Location` header, including for inline streaming arms.
    upstream_http_location: String,
    cached_etag: Option<String>,
    /// Distinguishes an inspected entry without an ETag from no cache observation.
    cache_entry_observed: bool,
    upstream_etag: Option<String>,
    /// Store outcome for this MISS, or `None` when no store operation applied.
    cache_store_result: Option<CacheStoreResult>,
    /// Cache tier that supplied a cacheable MISS body.
    cache_tier: Option<&'static str>,
    /// Bounded inactivity stage that terminated this response.
    timeout_stage: Option<TimeoutStage>,
    /// Whether a `Streamed` arm announced `Connection: keep-alive` and then
    /// wrote its complete, self-delimited body, so the connection can carry
    /// another request.
    framing_complete: bool,
    /// Whether a `Streamed` arm's body failed after a head without
    /// `Content-Length` or chunked coding. Only an abortive close shows that
    /// failure, since `close_notify` would mark the body complete.
    abort_close: bool,
}

impl Response {
    /// A buffered response the driver writes (head + body); `Content-Length` is
    /// the body length.
    fn buffered(
        status: u16,
        reason: &'static str,
        cache_status: Option<&'static str>,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Self {
        let len = body.len() as u64;
        Self {
            head: ResponseHead {
                status,
                reason,
                cache_status,
                headers,
                content_length: Some(len),
            },
            body: BodySource::Buffered(body),
            bytes_sent: len,
            upstream_status: None,
            upstream_timing: None,
            upstream_http_location: String::new(),
            cached_etag: None,
            cache_entry_observed: false,
            upstream_etag: None,
            cache_store_result: None,
            cache_tier: None,
            timeout_stage: None,
            framing_complete: false,
            abort_close: false,
        }
    }

    /// A bodiless response that omits `Content-Length` (a `304 Not Modified`).
    fn headers_only(
        status: u16,
        reason: &'static str,
        cache_status: Option<&'static str>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            head: ResponseHead {
                status,
                reason,
                cache_status,
                headers,
                content_length: None,
            },
            body: BodySource::Buffered(Vec::new()),
            bytes_sent: 0,
            upstream_status: None,
            upstream_timing: None,
            upstream_http_location: String::new(),
            cached_etag: None,
            cache_entry_observed: false,
            upstream_etag: None,
            cache_store_result: None,
            cache_tier: None,
            timeout_stage: None,
            framing_complete: false,
            abort_close: false,
        }
    }

    /// A response with no wire body, preserving a HEAD or 304 representation
    /// length when it is known.
    fn head(
        status: u16,
        reason: &'static str,
        cache_status: Option<&'static str>,
        headers: Vec<(String, String)>,
        content_length: Option<u64>,
    ) -> Self {
        Self {
            head: ResponseHead {
                status,
                reason,
                cache_status,
                headers,
                content_length,
            },
            body: BodySource::Buffered(Vec::new()),
            bytes_sent: 0,
            upstream_status: None,
            upstream_timing: None,
            upstream_http_location: String::new(),
            cached_etag: None,
            cache_entry_observed: false,
            upstream_etag: None,
            cache_store_result: None,
            cache_tier: None,
            timeout_stage: None,
            framing_complete: false,
            abort_close: false,
        }
    }

    /// A zero-copy HIT where the driver writes the head, then `sendfile`s the lease.
    fn sendfile(
        status: u16,
        reason: &'static str,
        headers: Vec<(String, String)>,
        lease: impl Into<RequestLease>,
    ) -> Self {
        let lease = lease.into();
        let size = lease.meta().size;
        let cached_etag = lease.meta().etag.clone();
        Self {
            head: ResponseHead {
                status,
                reason,
                cache_status: Some("HIT"),
                headers,
                content_length: Some(size),
            },
            body: lease.into_body(),
            bytes_sent: size,
            upstream_status: None,
            upstream_timing: None,
            upstream_http_location: String::new(),
            cached_etag,
            cache_entry_observed: true,
            upstream_etag: None,
            cache_store_result: None,
            cache_tier: None,
            timeout_stage: None,
            framing_complete: false,
            abort_close: false,
        }
    }

    /// A response the serve arm already wrote to the socket itself (the
    /// streaming arms). The driver leaves the socket untouched; only the
    /// metadata flows on for logging.
    fn streamed(status: u16, cache_status: Option<&'static str>, bytes_sent: u64) -> Self {
        Self {
            head: ResponseHead {
                status,
                reason: "",
                cache_status,
                headers: Vec::new(),
                content_length: None,
            },
            body: BodySource::Streamed,
            bytes_sent,
            upstream_status: None,
            upstream_timing: None,
            upstream_http_location: String::new(),
            cached_etag: None,
            cache_entry_observed: false,
            upstream_etag: None,
            cache_store_result: None,
            cache_tier: None,
            timeout_stage: None,
            framing_complete: false,
            abort_close: false,
        }
    }

    /// Records the contacted origin's status + per-stage timing for the access
    /// log (`$upstream_status` and the `upstream_*_time` fields).
    fn with_upstream(
        mut self,
        status: u16,
        timing: UpstreamTiming,
        headers: &[(String, String)],
    ) -> Self {
        self.upstream_status = Some(status);
        self.upstream_timing = Some(timing);
        self.upstream_http_location = header_value(headers, "location")
            .unwrap_or_default()
            .to_owned();
        self.upstream_etag = header_value(headers, "etag").map(str::to_owned);
        self
    }

    /// Marks a `Streamed` response whose announced keep-alive framing completed.
    fn with_framing_complete(mut self, complete: bool) -> Self {
        self.framing_complete = complete;
        self
    }

    /// Marks a `Streamed` response whose unframed body failed, which only an
    /// abortive close shows the client.
    fn with_abort_close(mut self, abort: bool) -> Self {
        self.abort_close = abort;
        self
    }

    fn with_cached_etag(mut self, etag: Option<String>) -> Self {
        // A fallback's later cache observation takes precedence over its caller's.
        if !self.cache_entry_observed {
            self.cached_etag = etag;
            self.cache_entry_observed = true;
        }
        self
    }

    /// Retains the checked entry while a fallback fetch owns its upstream metadata.
    fn with_probe_etags(mut self, cached: Option<String>, upstream: Option<String>) -> Self {
        self = self.with_cached_etag(cached);
        if self.upstream_status.is_none() {
            self.upstream_etag = upstream;
        }
        self
    }

    /// Attaches a bounded store outcome to the flat access-log metadata.
    fn with_cache_store_result(mut self, result: CacheStoreResult) -> Self {
        self.cache_store_result = Some(result);
        self
    }

    /// Marks which tier supplied a cacheable MISS without changing the existing
    /// `X-Cache-Status` vocabulary.
    fn with_cache_tier(mut self, tier: &'static str) -> Self {
        self.head
            .headers
            .push(("X-Cache-Tier-Hit".to_owned(), tier.to_owned()));
        self.cache_tier = Some(tier);
        self
    }

    fn with_timeout_stage(mut self, stage: TimeoutStage) -> Self {
        self.timeout_stage = Some(stage);
        self
    }
}

/// OVCC-specific values known to the router (not the serve arm) that the access
/// log records — the resolved backend/upstream and the cache-decision flags.
#[derive(Default)]
struct RequestFlags {
    backend: String,
    /// Upstream the request was routed to (logged only when an origin was
    /// actually contacted — empty on a HIT, matching nginx `$upstream_addr`).
    upstream: String,
    bursty_cache: bool,
    mutation_detected: bool,
    disable_cache: bool,
    presigned_auth_outcome: &'static str,
}

/// Pod hostname (`$hostname`) and engine version, resolved once. `version` can
/// be overridden by the chart via `OVCC_SERVICE_VERSION`; otherwise it is the
/// engine crate version.
static POD_NAME: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| std::env::var("HOSTNAME").unwrap_or_default());
/// Cache-reset identity, resolved once and stable for the process lifetime.
/// Kubernetes supplies the pod hostname; a PID-based value keeps isolated
/// process tests non-empty without inventing persistence across restarts.
static INSTANCE_ID: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or_else(|| format!("ovcc-{}", std::process::id()))
});
static ENGINE_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    std::env::var("OVCC_SERVICE_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_owned())
});

struct AccessConnection<'a> {
    remote_addr: &'a str,
    scheme: &'a str,
    ssl_protocol: &'a str,
    request: RequestConnection,
}

/// Builds the flat access-log record from the connection info, request fields,
/// router flags, and the arm's [`Response`] metadata. `request_time` and the
/// trace ids are left for the telemetry [`TelemetryService`] / emit step; this
/// is pure (no span recording, no I/O) so the router can assemble it and the
/// Layer logs strictly from the resulting [`ServeOutcome`].
fn build_access_log(
    head: &RequestHead,
    host: &str,
    connection: AccessConnection<'_>,
    flags: &RequestFlags,
    response: &Response,
) -> ovcc_telemetry::access::AccessLog {
    // The upstream address is logged only when an origin was actually contacted
    // (matches nginx `$upstream_addr`, which is empty on a cache HIT).
    let upstream = if response.upstream_status.is_some() {
        flags.upstream.clone()
    } else {
        String::new()
    };
    let timing = response.upstream_timing.unwrap_or_default();
    ovcc_telemetry::access::AccessLog {
        access_time: ovcc_telemetry::access::now_access_time(),
        remote_addr: connection.remote_addr.to_owned(),
        request: head.method.clone(),
        scheme: connection.scheme.to_owned(),
        http_protocol: head.http_version.clone(),
        status: response.head.status,
        request_uri: request_uri(head),
        host: host.to_owned(),
        http_user_agent: header_value(&head.headers, "user-agent")
            .unwrap_or_default()
            .to_owned(),
        connection: connection.request.id,
        connection_requests: connection.request.requests,
        ssl_protocol: connection.ssl_protocol.to_owned(),
        etag: header_value(&head.headers, "if-match")
            .unwrap_or_default()
            .to_owned(),
        cached_etag: response.cached_etag.clone(),
        upstream_etag: response.upstream_etag.clone(),
        request_range: header_value(&head.headers, "range")
            .unwrap_or_default()
            .to_owned(),
        http_x_request_id: header_value(&head.headers, "x-request-id")
            .unwrap_or_default()
            .to_owned(),
        upstream,
        upstream_status: response.upstream_status,
        upstream_http_location: response.upstream_http_location.clone(),
        upstream_connect_time: timing.connect,
        upstream_header_time: timing.header,
        upstream_response_time: timing.response,
        timeout_stage: response.timeout_stage,
        upstream_cache_status: response.head.cache_status.unwrap_or_default().to_owned(),
        cache_store_result: response
            .cache_store_result
            .map(CacheStoreResult::as_str)
            .unwrap_or_default()
            .to_owned(),
        cache_tier: response.cache_tier.unwrap_or_default().to_owned(),
        bytes_sent: response.bytes_sent,
        request_length: head.head_length as u64,
        has_auth_header: header_value(&head.headers, "authorization").is_some(),
        presigned_auth_outcome: flags.presigned_auth_outcome.to_owned(),
        bursty_cache: flags.bursty_cache,
        mutation_detected: flags.mutation_detected,
        disable_cache: flags.disable_cache,
        pod_name: POD_NAME.clone(),
        pid: std::process::id(),
        version: ENGINE_VERSION.clone(),
        otel_parent_id: ovcc_telemetry::propagation::parent_span_id(&head.headers),
        ..Default::default()
    }
}

/// Per-connection shared state, cheaply cloned (Arc bumps) into each accepted
/// connection's task and carried into the tower stack via [`ProxyRequest`].
struct PeerRuntime {
    directory: PeerDirectory,
    client: PeerClient,
    random: RunRandom,
    budget: Duration,
    replicate: bool,
    cache_port: AtomicU16,
    min_object_bytes: u64,
}

#[derive(Clone)]
struct ConnCtx {
    backends: Arc<Vec<BackendRuntime>>,
    passthrough: Passthrough,
    self_names: Arc<Vec<String>>,
    generation: Arc<AtomicI64>,
    reset_compat: Arc<CompatConfig>,
    instance: Arc<String>,
    /// Positive authorization witnesses are process-local and keyed only by a
    /// non-reversible digest of the complete presigned credential identity.
    presigned_auth: Arc<AuthorizationCache>,
    local_mode: bool,
    peer: Option<Arc<PeerRuntime>>,
}

/// The unit the tower stack serves — the kTLS stream plus the parsed request and
/// connection context. Owned (no borrows) so the [`Service`] future is `'static`
/// and `Send`. The stream rides along so the router can stream the request body
/// (transparent/write-through arms) and the driver can write the response.
#[derive(Clone, Copy)]
struct RequestConnection {
    id: u64,
    requests: u64,
}

struct ProxyRequest {
    stream: ClientStream,
    head: RequestHead,
    body_prefix: Vec<u8>,
    /// Whether the next request would start right after `body_prefix`.
    head_delimited: bool,
    host: String,
    remote_addr: String,
    scheme: &'static str,
    ssl_protocol: String,
    connection: RequestConnection,
    ctx: ConnCtx,
    /// Which listener this request arrived on. The Nucleus listener routes to
    /// the Nucleus serve arms; the cache listener keeps host-based dispatch.
    listener_kind: ListenerKind,
    peer_forwarded: bool,
}

/// What the router service hands back — the stream (for the driver to finish the
/// write), the [`Response`] (head + body source + metadata), and the assembled
/// access-log record (missing only `request_time`, which the Layer stamps). The
/// telemetry Layer reads the metadata; only the driver touches the body.
struct ServeOutcome {
    stream: ClientStream,
    response: Response,
    access: ovcc_telemetry::access::AccessLog,
    /// Whether the connection carries another request or closes after this one.
    next: ConnectionNext,
}

/// What follows a routed response on its connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionNext {
    /// The client allows reuse and the response framing completed on the wire.
    KeepAlive,
    /// The connection loop closes the stream after the access record.
    Close(CloseMode),
}

type ServeFuture =
    Pin<Box<dyn Future<Output = std::result::Result<ServeOutcome, BoxError>> + Send>>;

/// The request-routing tower service that maps a [`ProxyRequest`] to a
/// [`ServeOutcome`] by picking a serve arm. Stateless — all per-connection state
/// rides in the request's [`ConnCtx`].
#[derive(Clone, Copy)]
struct Router;

impl Service<ProxyRequest> for Router {
    type Response = ServeOutcome;
    type Error = BoxError;
    type Future = ServeFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ProxyRequest) -> Self::Future {
        Box::pin(route(req))
    }
}

/// The first (outermost) tower `Layer`: it times the request, lets the inner
/// service produce a [`ServeOutcome`], runs the [`drive_response`] driver to
/// write the body (the kTLS sendfile HIT body never enters this layer — only its
/// metadata does), records the terminal status/cache-status on the root span,
/// and emits the access-log line.
#[derive(Clone, Copy)]
struct TelemetryLayer;

impl<S> Layer<S> for TelemetryLayer {
    type Service = TelemetryService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TelemetryService { inner }
    }
}

#[derive(Clone)]
struct TelemetryService<S> {
    inner: S,
}

impl<S> Service<ProxyRequest> for TelemetryService<S>
where
    S: Service<ProxyRequest, Response = ServeOutcome, Error = BoxError>,
    S::Future: Send + 'static,
{
    /// The stream with its next step. Closing stays with the connection loop,
    /// so a lingering close never counts toward the request's telemetry.
    type Response = (ClientStream, ConnectionNext);
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: ProxyRequest) -> Self::Future {
        // `request_time` spans routing + the driver write; the health/cache-reset
        // operator paths short-circuit before the stack and are not logged.
        let start = Instant::now();
        let peer_forwarded = req.peer_forwarded;
        let http_metrics = HttpServerRequestGuard::start(
            &req.head.method,
            req.scheme,
            &req.head.http_version,
            http_request_body_size(&req.head),
        );
        let fut = self.inner.call(req);
        Box::pin(with_downstream_request_state(async move {
            let _peer_serve_guard = peer_forwarded.then(PeerServeActiveGuard::start);
            let ServeOutcome {
                mut stream,
                response,
                mut access,
                next,
            } = match fut.await {
                Ok(outcome) => outcome,
                Err(error) => {
                    if peer_forwarded {
                        ovcc_telemetry::metrics::record_peer_serve(
                            PeerServeResult::IoFailure,
                            0,
                            0.0,
                        );
                    }
                    let failure = if is_downstream_timeout(&error) {
                        HttpServerErrorType::Timeout
                    } else {
                        HttpServerErrorType::Other
                    };
                    let status = downstream_response_started().then(downstream_response_status);
                    http_metrics.finish(status, downstream_body_bytes(), Some(failure));
                    return Err(error);
                }
            };
            // Record the terminal status + cache status on the root span from the
            // response metadata only — never the body.
            let span = tracing::Span::current();
            span.record("http.response.status_code", response.head.status);
            if let Some(cache_status) = response.head.cache_status {
                span.record("ovcc.cache.status", cache_status);
            }
            let status = response.head.status;
            let keep_alive = next == ConnectionNext::KeepAlive;
            let drive = match drive_response(&mut stream, response, keep_alive).await {
                Ok(report) => report,
                Err((report, error)) => {
                    if peer_forwarded {
                        ovcc_telemetry::metrics::record_peer_serve(
                            classify_peer_serve_error(&error),
                            report.body_bytes,
                            report.socket_writable_wait.as_secs_f64(),
                        );
                    }
                    access.request_time = start.elapsed().as_secs_f64();
                    access.bytes_sent = report.body_bytes;
                    let timed_out = is_downstream_timeout(&error);
                    if timed_out {
                        access.timeout_stage = Some(TimeoutStage::DownstreamWrite);
                    }
                    ovcc_telemetry::metrics::record(&access);
                    ovcc_telemetry::access::emit(&access);
                    let failure = if timed_out {
                        HttpServerErrorType::Timeout
                    } else {
                        HttpServerErrorType::DownstreamWrite
                    };
                    http_metrics.finish(Some(status), report.body_bytes, Some(failure));
                    // A write timeout is a close outcome rather than a connection
                    // error, so the connection loop resets and records it.
                    if timed_out {
                        return Ok((stream, ConnectionNext::Close(CloseMode::Reset)));
                    }
                    return Err(error);
                }
            };
            if peer_forwarded {
                let result = match status {
                    200 | 206 => PeerServeResult::Complete,
                    404 => PeerServeResult::NotFound,
                    412 => PeerServeResult::Unauthorized,
                    _ => PeerServeResult::IoFailure,
                };
                ovcc_telemetry::metrics::record_peer_serve(
                    result,
                    drive.body_bytes,
                    drive.socket_writable_wait.as_secs_f64(),
                );
            }
            access.request_time = start.elapsed().as_secs_f64();
            // Metrics and the access log both read the outcome metadata only; the
            // kTLS sendfile HIT body was already written by the driver above and
            // never enters this layer.
            ovcc_telemetry::metrics::record(&access);
            ovcc_telemetry::access::emit(&access);
            http_metrics.finish(Some(status), drive.body_bytes, None);
            Ok((stream, next))
        }))
    }
}

enum PeerControlIo {
    Tls(Box<tokio_rustls::server::TlsStream<HandshakeIo>>),
    Plain(TcpStream),
}

impl AsyncRead for PeerControlIo {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for PeerControlIo {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_write(context, buffer),
            Self::Plain(stream) => Pin::new(stream).poll_write(context, buffer),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_flush(context),
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}

impl tonic::transport::server::Connected for PeerControlIo {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

#[derive(Clone)]
struct PeerControlService {
    ctx: ConnCtx,
}

#[tonic::async_trait]
impl peer_proto::peer_control_server::PeerControl for PeerControlService {
    async fn who_has(
        &self,
        request: GrpcRequest<peer_proto::WhoHasRequest>,
    ) -> std::result::Result<GrpcResponse<peer_proto::WhoHasResponse>, GrpcStatus> {
        let request = request.into_inner();
        #[cfg(feature = "test-optional-keys")]
        if let Some(response) = optional_key_fixture::who_has(&self.ctx, &request)
            .await
            .map_err(|error| GrpcStatus::invalid_argument(error.to_string()))?
        {
            return Ok(GrpcResponse::new(response));
        }
        if let Some(response) = azure_optional_keys::who_has(&self.ctx, &request)
            .await
            .map_err(|error| GrpcStatus::invalid_argument(error.to_string()))?
        {
            return Ok(GrpcResponse::new(response));
        }
        let (identity, validator) = ovcc_peer::validate_who_has_request(request)
            .map_err(|error| GrpcStatus::invalid_argument(error.to_string()))?;
        if identity.method != http::Method::GET {
            return Err(GrpcStatus::invalid_argument(
                "WhoHas supports GET identities only",
            ));
        }
        let Some(runtime) = self
            .ctx
            .backends
            .iter()
            .find(|runtime| runtime.config.resolve_backend(&identity.host).is_some())
        else {
            return Ok(GrpcResponse::new(absent_who_has()));
        };
        let generation = self.ctx.generation.load(Ordering::SeqCst).to_string();
        let key = KeyParts {
            generation: &generation,
            host: &identity.host,
            method: identity.method.as_str(),
            uri: &identity.uri,
            filtered_args: &identity.args,
            if_match: &identity.if_match,
            range: &identity.range,
        }
        .render();
        let meta = cache_peek_blocking(&runtime.store.cache, key.as_str())
            .await
            .map_err(|error| GrpcStatus::internal(error.to_string()))?;
        let Some(meta) = meta.filter(|meta| {
            matches!(meta.status, 200 | 206) && stored_validator_matches(meta, &validator)
        }) else {
            tracing::debug!(event = "peer_who_has_served", result = "absent");
            return Ok(GrpcResponse::new(absent_who_has()));
        };
        tracing::debug!(
            event = "peer_who_has_served",
            result = "committed",
            status = meta.status,
            object_size = meta.size,
        );
        Ok(GrpcResponse::new(committed_who_has(&meta, &validator)))
    }
}

fn committed_who_has(meta: &StoredMeta, validator: &ExactValidator) -> peer_proto::WhoHasResponse {
    peer_proto::WhoHasResponse {
        omitted_parts: Vec::new(),
        optional_keys_ack: String::new(),
        object_status: peer_proto::ObjectStatus::Committed as i32,
        http_status: u32::from(meta.status),
        object_size: meta.size,
        validator: Some(validator.into()),
        max_idle_seconds: meta.max_idle_secs,
    }
}

fn absent_who_has() -> peer_proto::WhoHasResponse {
    peer_proto::WhoHasResponse {
        omitted_parts: Vec::new(),
        optional_keys_ack: String::new(),
        object_status: peer_proto::ObjectStatus::Absent as i32,
        http_status: 0,
        object_size: 0,
        validator: None,
        max_idle_seconds: None,
    }
}

fn stored_validator_matches(meta: &StoredMeta, validator: &ExactValidator) -> bool {
    match validator {
        ExactValidator::Etag(expected) => meta.etag.as_ref() == Some(expected),
        ExactValidator::LastModified(expected) => meta.last_modified.as_ref() == Some(expected),
    }
}

async fn serve_peer_control(
    listener: TcpListener,
    acceptor: Option<TlsAcceptorProvider>,
    ctx: ConnCtx,
    shutdown: watch::Receiver<bool>,
) -> std::result::Result<(), BoxError> {
    let (incoming_tx, incoming_rx) = mpsc::channel(32);
    let mut accept_shutdown = shutdown.clone();
    let accept = async move {
        let mut handshakes = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut accept_shutdown) => break,
                accepted = listener.accept() => {
                    let (tcp, _) = accepted?;
                    tcp.set_nodelay(true)?;
                    let sender = incoming_tx.clone();
                    let acceptor = acceptor.as_ref().map(TlsAcceptorProvider::acceptor);
                    handshakes.spawn(async move {
                        let io = match acceptor {
                            Some(acceptor) => {
                                let tls = acceptor.accept(handshake_io(tcp)).await?;
                                PeerControlIo::Tls(Box::new(tls))
                            }
                            None => PeerControlIo::Plain(tcp),
                        };
                        let _ = sender.send(Ok::<_, std::io::Error>(io)).await;
                        Ok::<_, BoxError>(())
                    });
                }
                completed = handshakes.join_next(), if !handshakes.is_empty() => {
                    if let Some(Ok(Err(error))) = completed {
                        tracing::warn!(event = "peer_control_handshake_failed", error = %error);
                    }
                }
            }
        }
        drop(incoming_tx);
        handshakes.abort_all();
        while handshakes.join_next().await.is_some() {}
        Ok::<_, BoxError>(())
    };
    let mut server_shutdown = shutdown;
    let server = tonic::transport::Server::builder()
        .http2_max_pending_accept_reset_streams(Some(PEER_H2_MAX_RESET_STREAMS))
        .add_service(peer_proto::peer_control_server::PeerControlServer::new(
            PeerControlService { ctx },
        ))
        .serve_with_incoming_shutdown(
            ReceiverStream::new(incoming_rx),
            wait_for_shutdown(&mut server_shutdown),
        );
    let (accept_result, server_result) = tokio::join!(accept, server);
    accept_result?;
    server_result?;
    Ok(())
}

/// Handles one connection through rustls handshake → kTLS upgrade → one HTTP/1.1
/// request → response. The operator endpoints (`/healthz`, `/__/cache/reset`)
/// and the plaintext-on-TLS-port refusal answer inline and are not access-logged;
/// every routed request goes through the tower stack (telemetry Layer over the
/// router), which drives the write (HIT via `sendfile`) and emits one log line.
#[tracing::instrument(
    name = "http.server",
    skip_all,
    fields(
        http.request.method = tracing::field::Empty,
        url.path = tracing::field::Empty,
        server.address = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        ovcc.cache.status = tracing::field::Empty,
    )
)]
async fn handle_connection(
    tcp: TcpStream,
    acceptor: Option<TlsAcceptor>,
    ctx: ConnCtx,
    listener_kind: ListenerKind,
    mut shutdown: watch::Receiver<bool>,
) -> std::result::Result<(), BoxError> {
    let mut connection_metrics = ConnectionMetrics::accepted();
    // Client IP for the access log (`$remote_addr`), captured before `tcp` is
    // consumed by the TLS stack.
    let remote_addr = tcp
        .peer_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_default();

    // Disables Nagle so a kept-alive connection does not stall each small
    // head-then-body response by ~40ms on Nagle plus the peer's delayed ACK.
    let _ = tcp.set_nodelay(true);

    let (mut k, scheme, ssl_protocol) = if let Some(acceptor) = acceptor {
        // Rejects non-TLS input with an HTTP response before starting the
        // handshake.
        let mut first = [0u8; 1];
        let peek = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            peek = tcp.peek(&mut first) => peek,
        };
        match peek {
            Ok(0) | Err(_) => return Ok(()),
            Ok(_) if !looks_like_tls_client_hello(first[0]) => {
                let mut tcp = tcp;
                let parsed = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
                    parsed = timeout(KEEPALIVE_IDLE_TIMEOUT, read_request_head(&mut tcp)) => parsed,
                };
                // Unknown methods use the bodyless form; parsed GET errors retain their diagnostic.
                let method = match &parsed {
                    Ok(Ok(Some(read))) => read.head.method.as_str(),
                    _ => "HEAD",
                };
                connection_metrics.transition(ConnectionState::Writing);
                let _ = write_method_response_with(
                    &mut tcp,
                    method,
                    400,
                    "Bad Request",
                    &[("Content-Type", "text/plain")],
                    None,
                    b"Plain HTTP request sent to HTTPS port\n",
                )
                .await;
                let _ = tcp.shutdown().await;
                return Ok(());
            }
            Ok(_) => {}
        }

        // Preserve the HTTP/1 TLS path unchanged: rustls handshake through
        // CorkStream, capture the negotiated protocol, then (on Linux) hand
        // encryption to kTLS before any HTTP bytes are read.
        let tls = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            accepted = acceptor.accept(handshake_io(tcp)) => accepted?,
        };
        let ssl_protocol = tls
            .get_ref()
            .1
            .protocol_version()
            .map(tls_version_str)
            .unwrap_or_default()
            .to_owned();
        (
            ClientStream::Tls(into_client_tls(tls).await?),
            "https",
            ssl_protocol,
        )
    } else {
        (ClientStream::Plain(tcp), "http", String::new())
    };
    let mut service = TelemetryLayer.layer(Router);
    let connection = CONNECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut connection_requests = 0u64;

    // Serves requests until the connection closes. A response with complete
    // framing returns the stream, so the next request reuses the connection and
    // its TLS handshake; any other response closes it after the access record.
    let mut first = true;
    let idle = tokio::time::sleep(KEEPALIVE_IDLE_TIMEOUT);
    tokio::pin!(idle);
    loop {
        if first {
            connection_metrics.transition(ConnectionState::Reading);
        } else {
            connection_metrics.transition(ConnectionState::Waiting);
        }
        let parsed =
            read_before_idle(&mut shutdown, idle.as_mut(), read_request_head(&mut k)).await;
        let parsed = match parsed {
            // Idle expiry or the shutdown signal ends a kept-alive connection
            // like a response close, since the previous response may still sit
            // in kernel queues.
            None => {
                close_client(k, CloseMode::Graceful).await;
                return Ok(());
            }
            Some(Err(err)) => return Err(err.into()),
            Some(Ok(Some(parsed))) => parsed,
            Some(Ok(None)) => {
                // A malformed first request gets a `400`; a `None` on a later
                // read is the client closing a kept-alive connection.
                if first {
                    // A failed parse cannot establish whether a response body is permitted.
                    timed_write_all(
                        &mut k,
                        b"HTTP/1.1 400 Bad Request\r\nX-Cache-Status: BYPASS\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
                    close_client(k, CloseMode::Graceful).await;
                }
                return Ok(());
            }
        };
        connection_metrics.transition(ConnectionState::Reading);
        first = false;
        let ReadHead {
            head,
            body_prefix,
            delimited: head_delimited,
        } = parsed;
        connection_requests = connection_requests.saturating_add(1);
        let peer_forwarded = match peer_forwarded_request(&head) {
            Ok(peer_forwarded) => peer_forwarded,
            Err(()) => {
                write_method_response_with(
                    &mut k,
                    &head.method,
                    400,
                    "Bad Request",
                    &[],
                    Some("BYPASS"),
                    b"Bad peer depth\n",
                )
                .await?;
                close_client(k, CloseMode::Graceful).await;
                return Ok(());
            }
        };

        // Record the request fields on the root span and adopt any inbound W3C
        // trace context so this span is a child of the caller's trace (a no-op
        // for an un-traced client). Status/cache-status are recorded by the
        // telemetry Layer once the response is determined.
        let span = tracing::Span::current();
        span.record("http.request.method", head.method.as_str());
        span.record("url.path", head.path.as_str());
        ovcc_telemetry::propagation::set_parent_from_headers(&span, &head.headers);

        // Health check.
        if is_health_path(&head.path) {
            connection_metrics.transition(ConnectionState::Writing);
            write_method_response_with(
                &mut k,
                &head.method,
                200,
                "OK",
                &[("Content-Type", "text/plain")],
                None,
                b"healthy\n",
            )
            .await?;
            close_client(k, CloseMode::Graceful).await;
            return Ok(());
        }

        // Cache reset is exposed only for the selected backend when its
        // `allowCacheReset` gate is on. Config selects the accepted paths and
        // emitted instance headers.
        let primary_reset = head.path == ctx.reset_compat.cache_reset_path;
        let legacy_reset = ctx.reset_compat.emit_legacy_cache_reset
            && !ctx.reset_compat.legacy_cache_reset_path.is_empty()
            && head.path == ctx.reset_compat.legacy_cache_reset_path;
        let host = head.host.clone().unwrap_or_default();
        if (primary_reset || legacy_reset) && cache_reset_enabled(&ctx, listener_kind, &host) {
            let requested = query_param_decoded(&head.query, "gen");
            let (generation, old_generation, error) =
                apply_generation(&ctx.generation, requested.as_deref());
            let body = generation_json(&ctx.instance, generation, old_generation, error);
            let mut headers = vec![
                ("Content-Type", "application/json"),
                (
                    ctx.reset_compat.instance_header.as_str(),
                    ctx.instance.as_str(),
                ),
            ];
            if ctx.reset_compat.emit_legacy_cache_reset
                && !ctx.reset_compat.legacy_instance_header.is_empty()
            {
                headers.push((
                    ctx.reset_compat.legacy_instance_header.as_str(),
                    ctx.instance.as_str(),
                ));
            }
            connection_metrics.transition(ConnectionState::Writing);
            write_method_response_with(
                &mut k,
                &head.method,
                200,
                "OK",
                &headers,
                None,
                body.as_bytes(),
            )
            .await?;
            close_client(k, CloseMode::Graceful).await;
            return Ok(());
        }

        span.record("server.address", host.as_str());

        // Runs the tower stack (router + telemetry Layer) to write the response;
        // returns the stream when the connection is kept alive.
        let request = ProxyRequest {
            stream: k,
            head,
            body_prefix,
            head_delimited,
            host,
            remote_addr: remote_addr.clone(),
            scheme,
            ssl_protocol: ssl_protocol.clone(),
            connection: RequestConnection {
                id: connection,
                requests: connection_requests,
            },
            ctx: ctx.clone(),
            listener_kind,
            peer_forwarded,
        };
        connection_metrics.transition(ConnectionState::Writing);
        match service.call(request).await? {
            (returned, ConnectionNext::KeepAlive) => k = returned,
            (returned, ConnectionNext::Close(mode)) => {
                close_client(returned, mode).await;
                return Ok(());
            }
        }
    }
}

/// Routes one request to a serve arm and assembles its [`ServeOutcome`]. The
/// streaming arms (transparent/write-through) read the client upload through
/// `stream`; the deferring arms only return a [`Response`] for the driver.
async fn route(req: ProxyRequest) -> std::result::Result<ServeOutcome, BoxError> {
    let ProxyRequest {
        mut stream,
        head,
        body_prefix,
        head_delimited,
        host,
        remote_addr,
        scheme,
        ssl_protocol,
        connection,
        ctx,
        listener_kind,
        peer_forwarded: _,
    } = req;

    let reuse_allowed = client_allows_reuse(&head, &body_prefix, head_delimited);
    set_downstream_reuse_allowed(reuse_allowed);
    let mut flags = RequestFlags::default();
    let mut reset = false;
    let mut response = match compute_response(
        &mut stream,
        &head,
        &host,
        body_prefix,
        &ctx,
        &mut flags,
        listener_kind,
    )
    .await
    {
        Ok(response) => response,
        Err(error) if is_downstream_timeout(&error) => {
            stream.reset_on_drop();
            reset = true;
            Response::streamed(downstream_response_status(), None, 0)
                .with_timeout_stage(TimeoutStage::DownstreamWrite)
        }
        Err(error) => return Err(error),
    };
    if downstream_timed_out() {
        stream.reset_on_drop();
        reset = true;
        response = response.with_timeout_stage(TimeoutStage::DownstreamWrite);
    }
    // RFC 9110 §9.3.2 applies to every HEAD status, including local rejections.
    // Inline streaming arms enforce this before writing their response.
    if head.method.eq_ignore_ascii_case("HEAD") && !matches!(response.body, BodySource::Streamed) {
        response.body = BodySource::Buffered(Vec::new());
        response.bytes_sent = 0;
    }
    let access = build_access_log(
        &head,
        &host,
        AccessConnection {
            remote_addr: &remote_addr,
            scheme,
            ssl_protocol: &ssl_protocol,
            request: connection,
        },
        &flags,
        &response,
    );
    let next = connection_next(reset, reuse_allowed, &response);
    Ok(ServeOutcome {
        stream,
        response,
        access,
        next,
    })
}

/// Routes self-host checks, backend selection, write-through, transparent
/// methods, bursty policy, bypasses, HIT rechecks, and MISS fills in order.
/// Pure arms return a [`Response`]; streaming arms write through `stream` and
/// return [`BodySource::Streamed`].
async fn compute_response(
    stream: &mut ClientStream,
    head: &RequestHead,
    host: &str,
    body_prefix: Vec<u8>,
    ctx: &ConnCtx,
    flags: &mut RequestFlags,
    listener_kind: ListenerKind,
) -> std::result::Result<Response, BoxError> {
    let peer_forwarded = peer_forwarded_request(head).unwrap_or(false);
    // Rejects a request addressed to the proxy itself (its own Service FQDN):
    // it is not meant for any backend, so it gets 421 Misdirected Request. This
    // check runs first on both cache and Nucleus listeners.
    if is_self_host(host, &ctx.self_names) {
        return Ok(Response::buffered(
            421,
            "Misdirected Request",
            None,
            vec![("Content-Type".to_string(), "text/plain".to_string())],
            b"Misdirected Request\n".to_vec(),
        ));
    }

    // The Nucleus listener is a dedicated `server_name _` block: it selects the
    // `nucleus` backend by listener (not host resolution) and routes to the
    // Nucleus serve arms. This bypasses the cloud/host dispatch entirely.
    if matches!(listener_kind, ListenerKind::Nucleus) {
        let scheme = stream.scheme();
        let nameserver = ctx
            .backends
            .iter()
            .find(|runtime| runtime.config.name == "nucleus")
            .and_then(|runtime| runtime.config.route.resolver_override.clone());
        return ovcc_upstream::with_nameserver_override(
            nameserver,
            serve_nucleus(stream, head, host, body_prefix, ctx, flags, scheme),
        )
        .await;
    }

    // Resolves the backend by Host header and the origin URL to proxy to. A
    // known cloud host (S3/GCS/Azure pattern) routes to itself over TLS
    // (`https://$host`); the integration mock's exact host uses the configured
    // upstream (see `Backend::resolve_upstream`). An unmatched host falls
    // through to the passthrough path, proxied verbatim to the requested host
    // (method + body + relayed response), never cached.
    let Some((runtime, resolved)) = ctx.backends.iter().find_map(|runtime| {
        runtime
            .config
            .resolve_backend(host)
            .map(|resolved| (runtime, resolved))
    }) else {
        let upstream = ctx.passthrough.resolve_upstream(host);
        flags.upstream = upstream.clone();
        return serve_transparent(stream, &upstream, head, body_prefix, None).await;
    };
    let backend = &runtime.config;
    let store = &runtime.store;
    let upstream = resolved.upstream;
    let auth_upstream = resolved.auth_upstream.as_deref().unwrap_or(&upstream);
    // Content rewrite rules for the matched registry upstream, threaded into the
    // MISS fill; empty for non-registry backends.
    let registry_match = backend.registry_match(host);
    // A registry origin is addressed by its matched upstream host, whichever
    // virtual host the client used; `head.host` keeps the client value for the
    // cache key, logs, and peer identity.
    let policy_head = upstream_policy_head(
        head,
        &backend.route,
        registry_match.as_ref().map(|m| m.upstream_host()),
    );
    let head = policy_head.as_ref().unwrap_or(head);
    let requires_versioning = backend.requires_versioning;
    flags.backend = backend.name.clone();
    flags.upstream = upstream.clone();

    let rewrite_rules: &[ovcc_rewrite::RewriteRule] = registry_match
        .as_ref()
        .map(|m| m.content_rewrite)
        .unwrap_or(&[]);
    // Registry fetches force an uncompressed origin body; see
    // [`MissFill::force_identity`].
    let is_registry = backend.registry.is_some();

    // An excluded URI must bypass lookup and admission, including entries
    // cached before the rule was enabled.
    if registry_match
        .as_ref()
        .is_some_and(|route| route.has_uri_rules() && !route.is_cacheable_uri(&request_uri(head)))
    {
        flags.disable_cache = true;
        if head.method.eq_ignore_ascii_case("GET") || head.method.eq_ignore_ascii_case("HEAD") {
            if let AccessDecision::Reject(response) = evaluate_access(
                &ctx.presigned_auth,
                head,
                host,
                auth_upstream,
                flags,
                BypassPolicy {
                    local_mode: ctx.local_mode,
                    is_azure: false,
                    versioning: false,
                    uri_excluded: true,
                },
            )
            .await
            {
                return Ok(*response);
            }
            if head.content_length > 0 || header_value(&head.headers, "transfer-encoding").is_some()
            {
                return serve_transparent(stream, &upstream, head, body_prefix, Some("BYPASS"))
                    .await;
            }
            return serve_bypass(stream, &upstream, head).await;
        }
        return serve_transparent(stream, &upstream, head, body_prefix, None).await;
    }

    // The Azure backend selects its own cache variant, optional-key lookup, and
    // customer-provided-key, tag, and lease bypasses, so the predicate is
    // resolved above the bursty arm where both access-gate call sites reach it.
    let is_azure = backend.name == "azure";

    // A `PUT` to a write-through-enabled backend uploads to the origin AND seeds
    // the cache so the immediate consumer `GET` is a HIT. This is distinct from
    // the transparent `PUT` proxy below (which relays but never caches). Gated on
    // the backend's own `writeThrough.enabled`, so registry/cloud backends opt in
    // independently.
    if backend.write_through.enabled && head.method.eq_ignore_ascii_case("PUT") {
        // Azure block-blob uploads arrive as many `comp=block` PUTs then a
        // `comp=blocklist` commit. When block assembly is enabled, stage each
        // block and assemble the object on the commit; other `comp` operations
        // (lease, metadata, ...) are proxied without seeding, and a single-shot
        // PUT (no `comp`) falls through to the direct seed below.
        if backend.write_through.assemble_block_uploads {
            match comp_param(&head.query) {
                Some("block") => {
                    return serve_write_through_block(
                        stream,
                        &upstream,
                        head,
                        body_prefix,
                        &store.write_through,
                        backend.write_through.enable_http_chunked_uploads,
                    )
                    .await;
                }
                Some("blocklist") => {
                    return serve_write_through_blocklist(
                        stream,
                        &upstream,
                        head,
                        body_prefix,
                        &store.cache,
                        backend,
                        ctx.generation.load(Ordering::SeqCst),
                        &store.write_through,
                    )
                    .await;
                }
                Some(other) if !other.is_empty() => {
                    return serve_transparent(stream, &upstream, head, body_prefix, None).await;
                }
                _ => {}
            }
        }
        return serve_write_through(
            stream,
            &upstream,
            head,
            body_prefix,
            &store.cache,
            backend,
            ctx.generation.load(Ordering::SeqCst),
            &store.write_through,
            backend.write_through.enable_http_chunked_uploads,
        )
        .await;
    }

    // Any method other than GET/HEAD is transparently reverse-proxied, so the
    // method + streamed request body are forwarded and the response is relayed,
    // never cached. This is plain reverse-proxying (e.g. a PUT/DELETE), not
    // write-through caching.
    if !head.method.eq_ignore_ascii_case("GET") && !head.method.eq_ignore_ascii_case("HEAD") {
        return serve_transparent(stream, &upstream, head, body_prefix, None).await;
    }

    // Bursty list/HEAD caching (short-TTL, tenant-adaptive), when enabled for
    // this backend. A list `GET` (e.g. `/?list-type=`) or any `HEAD` carries no
    // `If-Match`, so the normal path would bypass the cache; bursty caches it
    // per tenant (keyed by request method, so GET and HEAD never collide) so an
    // immediate repeat HITs. This runs before the HEAD bypass and normal GET
    // path.
    if let Some(bursty) = &backend.bursty {
        let method = canonical_method(&head.method);
        if ovcc_bursty::matches(&bursty.rules, method, &request_uri(head)) {
            flags.bursty_cache = true;
            // Bursty serves shared cached bytes and metadata, so it runs the
            // same pre-cache access gate as the ordinary path before it may
            // select bursty service. `requiresVersioning` is deliberately not a
            // bursty bypass, since bursty exists to cache the validator-less
            // list and HEAD requests the ordinary path would bypass.
            let forced_miss = match evaluate_access(
                &ctx.presigned_auth,
                head,
                host,
                auth_upstream,
                flags,
                BypassPolicy {
                    local_mode: ctx.local_mode,
                    is_azure,
                    versioning: false,
                    uri_excluded: false,
                },
            )
            .await
            {
                AccessDecision::Proceed { forced_miss } => forced_miss,
                AccessDecision::Bypass => return serve_bypass(stream, &upstream, head).await,
                AccessDecision::Reject(response) => return Ok(*response),
            };
            return serve_bursty(
                stream,
                &store.cache,
                &upstream,
                head,
                host,
                ctx.generation.load(Ordering::SeqCst),
                method,
                bursty.ttl_secs,
                backend.cache_policy.backend_max_idle_secs,
                forced_miss,
            )
            .await;
        }
    }

    // HEAD is never cached on the normal path, since it carries no body, so a HEAD
    // that is not bursty-eligible is proxied through with `X-Cache-Status:
    // BYPASS`, relaying the origin response headers.
    if head.method.eq_ignore_ascii_case("HEAD") {
        return serve_head(&upstream, head).await;
    }

    // Computes the cache key. Presigned/SAS signature + time noise is stripped
    // per backend (Azure SAS uses its own param set), so two signed requests
    // for one object share a key. `If-Match` and `Range` are part of the key:
    // distinct validators/byte ranges are distinct cache entries.
    let strip = if is_azure {
        ovcc_cachekey::STRIPPED_PARAMS_AZURE
    } else {
        ovcc_cachekey::STRIPPED_PARAMS
    };
    let filtered = ovcc_cachekey::filter_query_args_with(&head.query, strip);
    let if_match = header_value(&head.headers, "if-match").unwrap_or("");
    let range = effective_range(&head.headers, is_azure);
    let cache_variant = if is_azure {
        azure_get_variant(&head.headers, range)
    } else {
        range.to_owned()
    };
    let generation_str = ctx.generation.load(Ordering::SeqCst).to_string();
    let key = KeyParts {
        generation: &generation_str,
        host,
        method: "GET",
        uri: &head.path,
        filtered_args: &filtered,
        if_match,
        range: &cache_variant,
    }
    .render();

    // The ordinary GET path runs the same pre-cache access gate as bursty, and
    // adds the `requiresVersioning`-without-`If-Match` bypass, since such a GET
    // carries no validator and cannot be safely cached, so it is served fresh
    // from the origin as cloud storage does by default.
    let forced_miss = match evaluate_access(
        &ctx.presigned_auth,
        head,
        host,
        auth_upstream,
        flags,
        BypassPolicy {
            local_mode: ctx.local_mode,
            is_azure,
            versioning: backend.requires_versioning && if_match.is_empty(),
            uri_excluded: false,
        },
    )
    .await
    {
        AccessDecision::Proceed { forced_miss } => forced_miss,
        AccessDecision::Bypass => return serve_bypass(stream, &upstream, head).await,
        AccessDecision::Reject(response) => return Ok(*response),
    };

    let mut fill = object_miss_fill(
        head,
        requires_versioning,
        forced_miss,
        is_registry,
        rewrite_rules,
        backend.cache_policy.backend_max_idle_secs,
        auth_upstream,
    );
    fill.recheck_auth &= !ctx.local_mode;

    // Preserve an expired representation long enough to issue the conditional
    // request required by the global cache-revalidation policy. A forced MISS
    // deliberately skips both fresh lookup and stale revalidation.
    let lookup = if forced_miss {
        Lookup::Miss
    } else {
        cache_lookup_blocking(&store.cache, key.as_str())
            .instrument(tracing::info_span!("ovcc.cache.lookup"))
            .await?
    };
    #[cfg(feature = "test-optional-keys")]
    let (key, lookup) =
        if !forced_miss && is_azure && matches!(lookup, Lookup::Miss | Lookup::SoftMiss(_)) {
            optional_key_fixture::lookup(&store.cache, &key).await?
        } else {
            (key, lookup)
        };
    #[cfg(not(feature = "test-optional-keys"))]
    let (key, lookup) =
        if !forced_miss && is_azure && matches!(lookup, Lookup::Miss | Lookup::SoftMiss(_)) {
            azure_optional_keys::lookup(&store.cache, &key).await?
        } else {
            (key, lookup)
        };
    local::retain_source(|| Some(store.cache.storage().local_path_source(key.as_str())));
    let hit = match lookup {
        Lookup::Hit(lease) if cache_entry_is_fresh(lease.meta(), now_unix()) => Some(lease),
        Lookup::Hit(lease)
            if !peer_forwarded
                && head.method.eq_ignore_ascii_case("GET")
                && has_revalidation_validator(lease.meta()) =>
        {
            drop(lease);
            return revalidate_entry(
                stream,
                &store.cache,
                &key,
                &upstream,
                head,
                fill,
                backend.name == "azure",
                RevalidationExpiry::CachePolicy,
            )
            .await;
        }
        Lookup::Hit(lease) => {
            drop(lease);
            None
        }
        Lookup::SoftMiss(_) | Lookup::Miss => None,
    };

    let Some(lease) = hit else {
        if peer_forwarded {
            return Ok(Response::buffered(
                404,
                "Not Found",
                Some("MISS"),
                Vec::new(),
                b"Peer object unavailable\n".to_vec(),
            ));
        }
        if !peer_forwarded
            && (!is_azure
                || cache_variant == range
                || azure_optional_keys::AzurePolicy::from_key(key.as_str()).is_some()
                || cfg!(feature = "test-optional-keys") && optional_fixture_key(key.as_str()))
            && let Some(response) = try_peer_fill(
                stream,
                &store.cache,
                &key,
                &upstream,
                head,
                &filtered,
                range,
                fill,
                ctx,
            )
            .await?
        {
            return Ok(response);
        }
        return serve_miss(stream, &store.cache, &key, &upstream, head, fill).await;
    };

    if peer_forwarded {
        let cached_etag = lease.meta().etag.clone();
        let Some(preflight) = peer_preflight(head, auth_upstream).await else {
            drop(lease);
            return Ok(Response::buffered(
                412,
                "Precondition Failed",
                Some("HIT"),
                Vec::new(),
                b"Peer preflight failed\n".to_vec(),
            )
            .with_cached_etag(cached_etag));
        };
        let upstream_etag = match &preflight.validator {
            ovcc_upstream::PreflightValidator::Etag(etag) => Some(etag.clone()),
            ovcc_upstream::PreflightValidator::LastModified(_) => None,
        };
        let validator = preflight_validator(&preflight);
        if !stored_validator_matches(lease.meta(), &validator) {
            drop(lease);
            return Ok(Response::buffered(
                412,
                "Precondition Failed",
                Some("HIT"),
                Vec::new(),
                b"Peer validator mismatch\n".to_vec(),
            )
            .with_probe_etags(cached_etag, upstream_etag));
        }
        return Ok(serve_hit_for_request(lease, head, backend.name == "azure")
            .with_probe_etags(cached_etag, upstream_etag));
    }

    // Decides whether this HIT needs an upstream re-check before serving.
    //  - I18 in shared mode: an authenticated request (`Authorization`, non-presigned) is
    //    re-authorized with a cheap byte-range probe — a `403` is relayed and
    //    the body is never served. The probe also surfaces origin validators.
    //  - a public request on a non-versioned backend (`requiresVersioning=
    //    false`) gets a `HEAD` to surface origin validators; without it, a mutated
    //    object would be served stale forever.
    // In both cases a changed origin ETag, with Last-Modified as a fallback,
    // marks the cached object stale and returns `503` after purging it.
    let recheck = if !ctx.local_mode && needs_auth_recheck(head) {
        let url = origin_url(auth_upstream, head);
        let started = Instant::now();
        let result = ovcc_upstream::probe_access(&url, &head.headers)
            .instrument(tracing::info_span!("ovcc.access.probe"))
            .await;
        ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
            host,
            method: canonical_method(&head.method),
            status: result.as_ref().ok().map(|origin| origin.status),
            duration_seconds: started.elapsed().as_secs_f64(),
        });
        Some(result)
    } else if !requires_versioning {
        let url = origin_url(auth_upstream, head);
        let started = Instant::now();
        let headers = freshness_probe_headers(&head.headers);
        let result = ovcc_upstream::fetch_head(&url, &headers)
            .instrument(tracing::info_span!("ovcc.mutation.recheck"))
            .await;
        ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
            host,
            method: "HEAD",
            status: result.as_ref().ok().map(|origin| origin.status),
            duration_seconds: started.elapsed().as_secs_f64(),
        });
        Some(result)
    } else {
        None
    };

    let cached_etag = lease.meta().etag.clone();
    let upstream_etag = recheck
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|origin| header_value(&origin.headers, "etag"))
        .map(str::to_owned);
    let response = match recheck {
        // Access revoked for this caller — refuse, never serve the cache.
        Some(Ok(origin)) if origin.status == 403 => Ok(Response::buffered(
            403,
            "Forbidden",
            Some("HIT"),
            Vec::new(),
            b"Forbidden\n".to_vec(),
        )),
        // Re-check authorized (S3/Azure answer the range probe with 206; the
        // mock origin answers 200). Serve the HIT unless the object mutated.
        Some(Ok(origin)) if origin.status == 200 || origin.status == 206 => {
            if is_mutated(lease.meta(), &origin.headers) {
                drop(lease);
                cache_remove_blocking(&store.cache, key.as_str()).await?;
                flags.mutation_detected = true;
                Ok(Response::buffered(
                    503,
                    "Service Unavailable",
                    Some("HIT"),
                    Vec::new(),
                    b"Stale\n".to_vec(),
                ))
            } else {
                Ok(serve_hit_conditional(lease, head, backend.name == "azure"))
            }
        }
        // Any other status (404/410/5xx) or a probe transport error means
        // the cached entry can no longer be vouched for, so fall back to a
        // fresh origin fetch, which re-runs auth and relays the result.
        Some(Ok(_)) | Some(Err(_)) => {
            drop(lease);
            serve_miss(
                stream,
                &store.cache,
                &key,
                &upstream,
                head,
                MissFill {
                    snapshot_response: false,
                    head_only: false,
                    expires_at: None,
                    admit_not_found: false,
                    honor_origin_cacheability: false,
                    add_lft_cors: false,
                    cache_status: CacheStatus::Miss,
                    normal_validity: true,
                    max_idle_secs: backend.cache_policy.backend_max_idle_secs,
                    rewrite: rewrite_rules,
                    force_identity: is_registry,
                    force_buffered: false,
                    recheck_auth: !ctx.local_mode && needs_auth_recheck(head),
                    recheck_mutation: !requires_versioning,
                    share_existing: false,
                    key_authorizes_followers: false,
                    recheck_upstream: auth_upstream,
                },
            )
            .await
        }
        // No re-check needed (versioned backend, no auth): serve directly.
        None => Ok(serve_hit_conditional(lease, head, backend.name == "azure")),
    };
    response.map(|response| response.with_probe_etags(cached_etag, upstream_etag))
}

/// The driver that writes a [`Response`] to the socket. A `Buffered` body is written
/// head-then-bytes; a `Sendfile` body writes the head then `sendfile(2)`s the
/// leased fd straight to the kTLS socket (the zero-copy HIT path); a `Streamed`
/// response was already written by its serve arm, so the driver only logs by
/// way of the metadata the caller already holds.
#[derive(Clone, Copy, Debug, Default)]
struct DriveReport {
    body_bytes: u64,
    socket_writable_wait: Duration,
}

async fn drive_response(
    stream: &mut ClientStream,
    response: Response,
    keep_alive: bool,
) -> std::result::Result<DriveReport, (DriveReport, BoxError)> {
    let expected_body_bytes = response.bytes_sent;
    match response.body {
        BodySource::LocalPath(_) => Err((
            DriveReport::default(),
            std::io::Error::other("local path reached HTTP driver").into(),
        )),
        BodySource::Streamed => Ok(DriveReport {
            body_bytes: expected_body_bytes,
            ..DriveReport::default()
        }),
        BodySource::Buffered(body) => {
            let head = render_head(&response.head, keep_alive);
            timed_write_all(stream, head.as_bytes())
                .await
                .map_err(|error| (DriveReport::default(), error.into()))?;
            let body_bytes = write_body_counted(stream, &body).await.map_err(|failure| {
                (
                    DriveReport {
                        body_bytes: failure.body_bytes,
                        ..DriveReport::default()
                    },
                    failure.error.into(),
                )
            })?;
            timed_flush(stream).await.map_err(|error| {
                (
                    DriveReport {
                        body_bytes,
                        ..DriveReport::default()
                    },
                    error.into(),
                )
            })?;
            Ok(DriveReport {
                body_bytes,
                ..DriveReport::default()
            })
        }
        BodySource::Sendfile(lease) => {
            let head = render_head(&response.head, keep_alive);
            timed_write_all(stream, head.as_bytes())
                .await
                .map_err(|error| (DriveReport::default(), error.into()))?;
            timed_flush(stream)
                .await
                .map_err(|error| (DriveReport::default(), error.into()))?;
            sendfile_lease(stream, &lease).await
        }
    }
}

fn classify_peer_serve_error(error: &BoxError) -> PeerServeResult {
    let Some(error) = error.downcast_ref::<std::io::Error>() else {
        return PeerServeResult::IoFailure;
    };
    match error.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::NotConnected
        | std::io::ErrorKind::UnexpectedEof => PeerServeResult::ClientAbort,
        _ => PeerServeResult::IoFailure,
    }
}

// A ready request retains priority over idle expiry; shutdown wins both.
async fn read_before_idle<F: Future>(
    shutdown: &mut watch::Receiver<bool>,
    mut idle: Pin<&mut tokio::time::Sleep>,
    read: F,
) -> Option<F::Output> {
    idle.as_mut()
        .reset(tokio::time::Instant::now() + KEEPALIVE_IDLE_TIMEOUT);
    tokio::select! {
        biased;
        _ = wait_for_shutdown(shutdown) => None,
        parsed = read => Some(parsed),
        _ = idle => None,
    }
}

/// How long a kept-alive connection waits for the next request before closing.
const KEEPALIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

fn peer_forwarded_request(head: &RequestHead) -> std::result::Result<bool, ()> {
    let internal = head
        .headers
        .iter()
        .filter(|(name, _)| name.to_ascii_lowercase().starts_with("x-ovcc-"))
        .collect::<Vec<_>>();
    if internal.is_empty() {
        return Ok(false);
    }
    if !head.method.eq_ignore_ascii_case("GET")
        || internal.len() != 1
        || !internal[0]
            .0
            .eq_ignore_ascii_case(ovcc_peer::X_OVCC_PEER_DEPTH)
        || internal[0].1 != ovcc_peer::PEER_REQUEST_DEPTH.to_string()
    {
        return Err(());
    }
    Ok(true)
}

/// Whether the client allows connection reuse. HTTP/1.1 keeps the connection
/// alive unless it sends `Connection: close`; HTTP/1.0 reuses only when it sends
/// `Connection: keep-alive`. Every `Connection` field counts, so a `close` in a
/// later field is never missed.
fn client_keep_alive(head: &RequestHead) -> bool {
    let tokens = head
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if tokens.iter().any(|token| token == "close") {
        return false;
    }
    head.http_version.eq_ignore_ascii_case("HTTP/1.1")
        || tokens.iter().any(|token| token == "keep-alive")
}

/// What follows a routed response on its connection. A downstream write timeout,
/// or a failed body whose head carried no framing, resets the connection.
/// Otherwise the connection carries another request only when the client
/// allows reuse and the response framing completed, by a driver-written body
/// or by a streamed arm that announced keep-alive and finished its own body.
fn connection_next(reset: bool, reuse_allowed: bool, response: &Response) -> ConnectionNext {
    if reset || response.abort_close {
        ConnectionNext::Close(CloseMode::Reset)
    } else if reuse_allowed
        && (!matches!(response.body, BodySource::Streamed) || response.framing_complete)
    {
        ConnectionNext::KeepAlive
    } else {
        ConnectionNext::Close(CloseMode::Graceful)
    }
}

/// Whether the connection may carry another request once this response's
/// framing completes. Beyond the client's permission, no request bytes may
/// remain past a strictly delimited head. The cache arms never read a declared
/// body and drop bytes already read past a bodiless head, and an oversized or
/// loosely delimited head leaves bytes another parser frames differently, so a
/// reused connection would misparse or stall.
fn client_allows_reuse(head: &RequestHead, body_prefix: &[u8], head_delimited: bool) -> bool {
    head_delimited
        && client_keep_alive(head)
        && !request_declares_body(head)
        && body_prefix.is_empty()
}

/// Whether the request declares a body. Any length other than an exact `0`,
/// including an invalid one, counts as a body.
fn request_declares_body(head: &RequestHead) -> bool {
    head.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value != "0")
    })
}

/// Renders a [`ResponseHead`] as HTTP/1.1 wire bytes, emitting the status line,
/// optional `X-Cache-Status`, the extra headers in order, an optional
/// `Content-Length`, and the `Connection` token (`keep-alive` reuses the
/// connection, `close` ends it).
///
/// Public for the `hit_serve` bench.
#[doc(hidden)]
pub fn render_head(head: &ResponseHead, keep_alive: bool) -> String {
    let mut out = format!("HTTP/1.1 {} {}\r\n", head.status, head.reason);
    if let Some(cache_status) = head.cache_status {
        out.push_str(&format!("X-Cache-Status: {cache_status}\r\n"));
    }
    for (name, value) in &head.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(content_length) = head.content_length {
        out.push_str(&format!("Content-Length: {content_length}\r\n"));
    }
    let connection = if keep_alive { "keep-alive" } else { "close" };
    out.push_str(&format!("Connection: {connection}\r\n\r\n"));
    out
}

/// `sendfile(2)`s a leased cache file straight to the kTLS socket fd, so the
/// kernel encrypts each chunk into TLS records and the body goes page cache → NIC
/// with no userspace copy. This is the engine's zero-copy HIT guarantee (mirrors
/// `tests/ktls_sendfile.rs`).
async fn sendfile_lease(
    stream: &mut ClientStream,
    lease: &ovcc_storage::Lease,
) -> std::result::Result<DriveReport, (DriveReport, BoxError)> {
    match lease.layout() {
        LeaseLayout::Contiguous => {
            sendfile_file(stream, &lease.file, lease.body_offset, lease.meta().size).await
        }
        LeaseLayout::Segmented => {
            let started = Instant::now();
            let descriptors = lease
                .segments()
                .ok_or("segmented lease has no manifest descriptors")
                .map_err(|error| (DriveReport::default(), error.into()))?;
            let segment_count = descriptors.len();
            let opener = lease
                .segment_opener()
                .map_err(|error| (DriveReport::default(), error.into()))?;
            let mut sent = 0u64;
            let mut socket_writable_wait = Duration::ZERO;
            let result = async {
                for (index, descriptor) in descriptors.enumerate() {
                    if descriptor.size() == 0 {
                        // Empty segments contribute ordering but need no fd.
                        // Yield periodically so a maximal empty-only manifest
                        // cannot monopolize a Tokio worker.
                        if index % 256 == 255 {
                            tokio::task::yield_now().await;
                        }
                        continue;
                    }
                    // Open only the current immutable fragment. It is dropped before
                    // the next descriptor, bounding the chain at directory + one fd.
                    // Path lookup/stat is blocking filesystem work.
                    let file = open_segment_async(opener.clone(), descriptor.clone())
                        .await
                        .map_err(|error| {
                            (
                                DriveReport {
                                    body_bytes: sent,
                                    socket_writable_wait,
                                },
                                error,
                            )
                        })?;
                    match sendfile_file(stream, &file, 0, descriptor.size()).await {
                        Ok(report) => {
                            sent = sent.checked_add(report.body_bytes).ok_or_else(|| {
                                (
                                    DriveReport {
                                        body_bytes: sent,
                                        socket_writable_wait,
                                    },
                                    "segmented send length overflow".into(),
                                )
                            })?;
                            socket_writable_wait += report.socket_writable_wait;
                        }
                        Err((report, error)) => {
                            sent = sent.saturating_add(report.body_bytes);
                            socket_writable_wait += report.socket_writable_wait;
                            return Err((
                                DriveReport {
                                    body_bytes: sent,
                                    socket_writable_wait,
                                },
                                error,
                            ));
                        }
                    }
                }
                if sent != lease.meta().size {
                    return Err((
                        DriveReport {
                            body_bytes: sent,
                            socket_writable_wait,
                        },
                        "segmented send length differs from Content-Length".into(),
                    ));
                }
                Ok(DriveReport {
                    body_bytes: sent,
                    socket_writable_wait,
                })
            }
            .await;
            tracing::info!(
                event = "segmented_sendfile_complete",
                segment_count,
                bytes = sent,
                expected_bytes = lease.meta().size,
                duration_seconds = started.elapsed().as_secs_f64(),
                success = result.is_ok(),
                "completed segmented sendfile chain"
            );
            result
        }
    }
}

#[cfg(test)]
static SEGMENT_OPEN_THREAD_REPORTER: std::sync::OnceLock<
    Mutex<Option<std::sync::mpsc::Sender<std::thread::ThreadId>>>,
> = std::sync::OnceLock::new();

async fn open_segment_async(
    opener: ovcc_storage::SegmentOpener,
    descriptor: ovcc_storage::SegmentDescriptor,
) -> std::result::Result<File, BoxError> {
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if let Some(reporter) = SEGMENT_OPEN_THREAD_REPORTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            let _ = reporter.send(std::thread::current().id());
        }
        opener.open(&descriptor)
    })
    .await
    .map_err(|error| Error::internal(format!("segment open task failed: {error}")))?
    .map_err(Into::into)
}

async fn sendfile_file(
    stream: &mut ClientStream,
    file: &File,
    start: u64,
    length: u64,
) -> std::result::Result<DriveReport, (DriveReport, BoxError)> {
    sendfile_file_with_cooperation(stream, file, start, length, tokio::task::yield_now).await
}

fn sendfile_chunk_len(file_remaining: u64) -> usize {
    usize::try_from(file_remaining.min(SENDFILE_CHUNK_BYTES as u64)).unwrap()
}

async fn sendfile_file_with_cooperation<Cooperate, CooperateFuture>(
    stream: &mut ClientStream,
    file: &File,
    start: u64,
    length: u64,
    mut cooperate: Cooperate,
) -> std::result::Result<DriveReport, (DriveReport, BoxError)>
where
    Cooperate: FnMut() -> CooperateFuture,
    CooperateFuture: Future<Output = ()>,
{
    #[cfg(windows)]
    if let ClientStream::Tls(tls) = stream {
        return send_file_through_tls(tls, file, start, length, cooperate).await;
    }
    // One overlapped reopen per body, reused by every chunk.
    #[cfg(windows)]
    let file = &ovcc_platform::TransmitSource::new(file)
        .map_err(|error| (DriveReport::default(), error.into()))?;
    let mut sent = 0u64;
    let mut socket_writable_wait = Duration::ZERO;
    while sent < length {
        let remaining = sendfile_chunk_len(length - sent);
        let socket = stream
            .tcp()
            .map_err(|error| (DriveReport::default(), error.into()))?;
        let chunk = send_file_chunk(socket, file, start + sent, remaining).await;
        socket_writable_wait += chunk.waited;
        let n = chunk.sent;
        if let Some(error) = chunk.error {
            return Err((
                DriveReport {
                    body_bytes: sent + n as u64,
                    socket_writable_wait,
                },
                error.into(),
            ));
        }
        if n == 0 {
            return Err((
                DriveReport {
                    body_bytes: sent,
                    socket_writable_wait,
                },
                "sendfile returned 0 before the whole file was sent".into(),
            ));
        }
        sent += n as u64;
        cooperate().await;
    }
    Ok(DriveReport {
        body_bytes: sent,
        socket_writable_wait,
    })
}

#[cfg(target_os = "linux")]
fn sendfile_stall_error() -> std::io::Error {
    mark_downstream_timeout();
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "downstream sendfile inactivity timeout",
    )
}

/// One zero-copy send: the bytes the kernel accepted, the error that ended it
/// (if any, after those bytes), and the time spent waiting on the socket.
struct ChunkOutcome {
    sent: usize,
    error: Option<std::io::Error>,
    waited: Duration,
}

impl ChunkOutcome {
    fn from_result(result: std::io::Result<usize>, waited: Duration) -> Self {
        match result {
            Ok(sent) => Self {
                sent,
                error: None,
                waited,
            },
            Err(error) => Self {
                sent: 0,
                error: Some(error),
                waited,
            },
        }
    }
}

/// Sends up to `count` bytes of `file` at `offset` with one non-blocking
/// `sendfile(2)`, awaiting writability when the socket buffer is full.
/// Partial progress arrives as a successful short count, so an error here
/// always follows zero bytes.
#[cfg(target_os = "linux")]
async fn send_file_chunk(tcp: &TcpStream, file: &File, offset: u64, count: usize) -> ChunkOutcome {
    use std::os::fd::AsFd as _;
    let mut offset = offset;
    let mut send_once = || ovcc_platform::sendfile(tcp.as_fd(), file, &mut offset, count);
    match send_once() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let started = Instant::now();
            let result = timeout(
                DOWNSTREAM_WRITE_STALL_TIMEOUT,
                tcp.async_io(Interest::WRITABLE, send_once),
            )
            .await
            .unwrap_or_else(|_| Err(sendfile_stall_error()));
            ChunkOutcome::from_result(result, started.elapsed())
        }
        result => ChunkOutcome::from_result(result, Duration::ZERO),
    }
}

/// Sends `count` bytes of `file` at `offset` with one overlapped
/// `TransmitFile`: the kernel reads through the system cache and sends
/// without a user-mode copy. The time the transfer is pending (backpressure
/// plus transmission) is reported as the writability wait; a failure keeps
/// the bytes already sent.
#[cfg(windows)]
async fn send_file_chunk(
    tcp: &TcpStream,
    source: &ovcc_platform::TransmitSource,
    offset: u64,
    count: usize,
) -> ChunkOutcome {
    send_file_chunk_with_timeout(tcp, source, offset, count, DOWNSTREAM_WRITE_STALL_TIMEOUT).await
}

#[cfg(windows)]
async fn send_file_chunk_with_timeout(
    tcp: &TcpStream,
    source: &ovcc_platform::TransmitSource,
    offset: u64,
    count: usize,
    inactivity_timeout: Duration,
) -> ChunkOutcome {
    let started = Instant::now();
    let count = u32::try_from(count).expect("chunk bounded by SENDFILE_CHUNK_BYTES");
    match ovcc_platform::transmit_file_with_inactivity_timeout(
        tcp,
        source,
        offset,
        count,
        inactivity_timeout,
    )
    .await
    {
        Ok(sent) => ChunkOutcome::from_result(Ok(sent as usize), started.elapsed()),
        Err(failure) => {
            if failure.error.kind() == std::io::ErrorKind::TimedOut {
                mark_downstream_timeout();
            }
            ChunkOutcome {
                sent: failure.sent as usize,
                error: Some(failure.error),
                waited: started.elapsed(),
            }
        }
    }
}

/// Windows TLS HIT body: Winsock has no kernel TLS, so the body is read from
/// the system cache into userspace and encrypted by rustls. The next chunk's
/// read runs on the blocking pool while the current chunk is written.
#[cfg(windows)]
async fn send_file_through_tls<Cooperate, CooperateFuture>(
    tls: &mut TlsClientStream,
    file: &File,
    start: u64,
    length: u64,
    mut cooperate: Cooperate,
) -> std::result::Result<DriveReport, (DriveReport, BoxError)>
where
    Cooperate: FnMut() -> CooperateFuture,
    CooperateFuture: Future<Output = ()>,
{
    let failed = |body_bytes: u64, socket_writable_wait: Duration, error: BoxError| {
        (
            DriveReport {
                body_bytes,
                socket_writable_wait,
            },
            error,
        )
    };
    let file = Arc::new(
        file.try_clone()
            .map_err(|error| failed(0, Duration::ZERO, error.into()))?,
    );
    let read = |offset: u64, sent: u64| {
        let file = Arc::clone(&file);
        let len = (length - sent).min(TLS_FILE_CHUNK_BYTES as u64) as usize;
        tokio::task::spawn_blocking(move || {
            let mut buffer = vec![0u8; len];
            ovcc_platform::read_exact_at(&file, &mut buffer, offset).map(|()| buffer)
        })
    };

    let mut sent = 0u64;
    let mut socket_writable_wait = Duration::ZERO;
    let mut pending = (length > 0).then(|| read(start, 0));
    while let Some(reading) = pending.take() {
        let buffer = match reading.await {
            Ok(Ok(buffer)) => buffer,
            Ok(Err(error)) => return Err(failed(sent, socket_writable_wait, error.into())),
            Err(error) => {
                return Err(failed(
                    sent,
                    socket_writable_wait,
                    Error::internal(format!("file read task failed: {error}")).into(),
                ));
            }
        };
        let chunk_end = sent + buffer.len() as u64;
        if chunk_end < length {
            pending = Some(read(start + chunk_end, chunk_end));
        }
        let started = Instant::now();
        let mut writer = ProgressTimeoutWriter::new(tls);
        let mut written = 0usize;
        while written < buffer.len() {
            match writer.write(&buffer[written..]).await {
                Ok(0) => {
                    return Err(failed(
                        sent + written as u64,
                        socket_writable_wait + started.elapsed(),
                        std::io::Error::from(std::io::ErrorKind::WriteZero).into(),
                    ));
                }
                Ok(bytes) => written += bytes,
                Err(error) => {
                    return Err(failed(
                        sent + written as u64,
                        socket_writable_wait + started.elapsed(),
                        error.into(),
                    ));
                }
            }
        }
        socket_writable_wait += started.elapsed();
        sent = chunk_end;
        cooperate().await;
    }
    // rustls may still hold records; the body is sent only once flushed.
    let started = Instant::now();
    let flushed = timed_flush(tls).await;
    socket_writable_wait += started.elapsed();
    flushed.map_err(|error| failed(sent, socket_writable_wait, error.into()))?;
    Ok(DriveReport {
        body_bytes: sent,
        socket_writable_wait,
    })
}

/// On the HIT path, builds the response head and hands the lease to the driver as
/// a [`BodySource::Sendfile`], so the body is `sendfile`'d from the page cache to
/// the kTLS socket without ever entering userspace (the zero-copy guarantee).
///
/// Carries its own span so a HIT trace is more than a lone `http.server` root;
/// it records metadata only (the body never enters userspace).
#[tracing::instrument(name = "ovcc.serve_hit", skip_all)]
fn serve_hit(lease: impl Into<RequestLease>) -> Response {
    let lease = lease.into();
    // Replay the origin response headers captured on the fill (ETag, Content-Type,
    // Content-Encoding, Content-Range, and any backend-specific header such as a
    // registry capability header) so the HIT is header-faithful to the MISS.
    // Replays the admitted origin status. This is normally `200`/`206`; only a
    // canonical bursty fill can persist a short-lived `404`.
    let code = lease.meta().status;
    let mut headers = lease.meta().headers.clone();
    // A complete cached representation can satisfy byte-range requests. A
    // stored 206 is only one slice, so it may advertise ranges only when the
    // origin explicitly supplied that capability header.
    if code == 200
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("accept-ranges"))
    {
        headers.push(("Accept-Ranges".to_owned(), "bytes".to_owned()));
    }
    let reason = miss_reason(code);
    Response::sendfile(code, reason, headers, lease)
}

fn serve_bursty_hit(lease: impl Into<RequestLease>, head: &RequestHead) -> Response {
    let lease = lease.into();
    let code = lease.meta().status;
    let cached_etag = lease.meta().etag.clone();
    let reason = miss_reason(code);
    let mut response = if head.method.eq_ignore_ascii_case("HEAD") {
        let headers = lease.meta().headers.clone();
        let representation_length = lease.meta().representation_length;
        drop(lease);
        Response::head(code, reason, Some("HIT"), headers, representation_length)
    } else {
        let headers = lease.meta().headers.clone();
        Response::sendfile(code, reason, headers, lease)
    };
    response
        .head
        .headers
        .retain(|(name, _)| !is_client_scoped_response_header(name));
    if is_azure_blob_request(head)
        && let Some(value) = azure_client_request_id(&head.headers)
    {
        response
            .head
            .headers
            .push(("x-ms-client-request-id".to_owned(), value.to_owned()));
    }
    response.with_cached_etag(cached_etag)
}

/// Replays Azure's request-scoped response overrides after a normalized cache
/// lookup. The cache identity excludes these fields because they do not change
/// object bytes, while the response still carries the caller's requested
/// headers.
fn serve_hit_for_request(
    lease: impl Into<RequestLease>,
    head: &RequestHead,
    is_azure: bool,
) -> Response {
    let mut response = serve_hit(lease);
    if !is_azure {
        return response;
    }

    // Get Blob reports the service version used for this request, not the
    // version that originally populated the shared object body.
    // https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob#response-headers
    if let Some(version) = azure_response_version(head) {
        response
            .head
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("x-ms-version"));
        response
            .head
            .headers
            .push(("x-ms-version".to_owned(), version));
    }

    for (name, value) in azure_response_overrides(&head.query) {
        response
            .head
            .headers
            .retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        response.head.headers.push((name.to_owned(), value));
    }
    response
        .head
        .headers
        // Microsoft: x-ms-request-id uniquely identifies the request that the
        // service made; a cache hit must not replay an earlier origin request.
        // https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob#response-headers
        .retain(|(name, _)| {
            !name.eq_ignore_ascii_case("x-ms-client-request-id")
                && !name.eq_ignore_ascii_case("x-ms-request-id")
        });
    if let Some(value) = azure_client_request_id(&head.headers) {
        response
            .head
            .headers
            .push(("x-ms-client-request-id".to_owned(), value.to_owned()));
    }
    response
}

fn azure_response_version(head: &RequestHead) -> Option<String> {
    // An explicit request header selects the service version. Headerless SAS
    // requests use api-version, falling back to the signed version. Without
    // either, the account's origin-selected default remains authoritative.
    // https://learn.microsoft.com/en-us/rest/api/storageservices/versioning-for-the-azure-storage-services
    header_value(&head.headers, "x-ms-version")
        .map(str::to_owned)
        .or_else(|| query_param_decoded(&head.query, "api-version"))
        .or_else(|| query_param_decoded(&head.query, "sv"))
        .filter(|value| !value.contains(['\r', '\n']))
}

fn azure_client_request_id(headers: &[(String, String)]) -> Option<&str> {
    header_value(headers, "x-ms-client-request-id")
        .filter(|value| value.len() <= 1024 && value.bytes().all(|byte| (32..=126).contains(&byte)))
}

/// Extracts the Azure overrides whose values are safe for an HTTP response
/// field. Query parsing decodes the values just as Azure does; rejecting CR/LF
/// prevents a signed query from changing the response-header structure.
fn azure_response_overrides(query: &str) -> Vec<(&'static str, String)> {
    [
        ("rscc", "Cache-Control"),
        ("rscd", "Content-Disposition"),
        ("rsce", "Content-Encoding"),
        ("rscl", "Content-Language"),
        ("rsct", "Content-Type"),
    ]
    .into_iter()
    .filter_map(|(param, header)| {
        let value = query_param_decoded(query, param)?;
        (!value.contains(['\r', '\n'])).then_some((header, value))
    })
    .collect()
}

fn is_azure_blob_request(head: &RequestHead) -> bool {
    head.host
        .as_deref()
        .is_some_and(|host| host.ends_with(".blob.core.windows.net"))
}

/// Serves a HIT while applying HTTP conditions to the cached representation.
/// Content freshness is decided before this function: immutable entries need
/// no origin check, while mutable entries reach this point only after their
/// origin validators still match.
fn serve_hit_conditional(
    lease: impl Into<RequestLease>,
    head: &RequestHead,
    is_azure: bool,
) -> Response {
    let lease = lease.into();
    let cached_etag = lease.meta().etag.clone();
    let last_modified = lease.meta().last_modified.as_deref();
    if if_unmodified_since_failed(
        header_value(&head.headers, "if-match"),
        header_value(&head.headers, "if-unmodified-since"),
        last_modified,
    ) {
        return serve_precondition_failed().with_cached_etag(cached_etag);
    }

    let inm = header_value(&head.headers, "if-none-match");
    let not_modified = if_none_match_matches(inm, lease.meta().etag.as_deref())
        || (inm.is_none()
            && if_modified_since_not_modified(
                header_value(&head.headers, "if-modified-since"),
                last_modified,
            ));
    if not_modified {
        serve_not_modified(lease.meta().etag.as_deref(), last_modified)
            .with_cached_etag(cached_etag)
    } else {
        serve_hit_for_request(lease, head, is_azure)
    }
}

fn if_modified_since_not_modified(
    if_modified_since: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    let (Some(condition), Some(last_modified)) = (if_modified_since, last_modified) else {
        return false;
    };
    let (Ok(condition), Ok(last_modified)) = (
        httpdate::parse_http_date(condition),
        httpdate::parse_http_date(last_modified),
    ) else {
        return false;
    };
    last_modified <= condition
}

fn if_unmodified_since_failed(
    if_match: Option<&str>,
    if_unmodified_since: Option<&str>,
    last_modified: Option<&str>,
) -> bool {
    if if_match.is_some() {
        return false;
    }
    let (Some(condition), Some(last_modified)) = (if_unmodified_since, last_modified) else {
        return false;
    };
    let (Ok(condition), Ok(last_modified)) = (
        httpdate::parse_http_date(condition),
        httpdate::parse_http_date(last_modified),
    ) else {
        return false;
    };
    last_modified > condition
}

fn freshness_probe_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            !name.eq_ignore_ascii_case("if-match")
                && !name.eq_ignore_ascii_case("if-modified-since")
                && !name.eq_ignore_ascii_case("if-none-match")
                && !name.eq_ignore_ascii_case("if-unmodified-since")
        })
        .cloned()
        .collect()
}

/// Whether an `If-None-Match` header value matches `etag`: a literal `*` always
/// matches a present validator, otherwise any comma-separated entry equal to
/// `etag` (compared verbatim, quotes included) matches. Absent header or absent
/// cached `ETag` never matches (the HIT is served in full).
///
/// Public for the `hit_serve` bench.
#[doc(hidden)]
pub fn if_none_match_matches(if_none_match: Option<&str>, etag: Option<&str>) -> bool {
    let (Some(inm), Some(etag)) = (if_none_match, etag) else {
        return false;
    };
    inm.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag
    })
}

/// Builds a `304 Not Modified` (no body, no `Content-Length`) relaying the
/// cached `ETag`. Used when a conditional request's `If-None-Match` matches.
fn serve_not_modified(etag: Option<&str>, last_modified: Option<&str>) -> Response {
    let mut headers = Vec::new();
    if let Some(etag) = etag {
        headers.push(("ETag".to_string(), etag.to_string()));
    }
    if let Some(last_modified) = last_modified {
        headers.push(("Last-Modified".to_string(), last_modified.to_string()));
    }
    Response::headers_only(304, "Not Modified", Some("HIT"), headers)
}

fn serve_precondition_failed() -> Response {
    Response::headers_only(412, "Precondition Failed", Some("HIT"), Vec::new())
}

#[derive(Clone, Copy)]
enum RevalidationExpiry {
    /// Refresh according to the backend's normal status validity.
    CachePolicy,
    /// Refresh to a location-specific absolute deadline.
    Explicit(i64),
}

/// Fill knobs for a cache-fill MISS.
#[derive(Clone, Copy)]
struct MissFill<'a> {
    /// Replays a bursty response snapshot without request-conditional or
    /// normal-HIT header synthesis.
    snapshot_response: bool,
    /// Serves the bursty HEAD arm, probing the origin with `HEAD` and admitting
    /// a zero-byte entry so the next HEAD HITs while the client response omits
    /// the body.
    head_only: bool,
    /// Bursty TTL deadline stamped on the admitted entry, or `None` for an entry
    /// that never expires.
    expires_at: Option<i64>,
    /// Admits an origin `404` only for the canonical bursty path. Every normal
    /// fill leaves this false so OVCC has no general negative cache.
    admit_not_found: bool,
    /// Applies origin response cache-control semantics before admission. This
    /// is enabled for Nucleus, whose cacheability is origin-controlled, and
    /// disabled for cloud and registry policies that own admission locally.
    honor_origin_cacheability: bool,
    /// Adds the LFT CORS policy to the client response before a streaming MISS
    /// writes its head. Stored metadata remains origin-only because HITs apply
    /// the same policy at serve time.
    add_lft_cors: bool,
    /// Client-visible cache outcome for an origin response. An ordinary fill is
    /// `MISS`; a changed response fetched while revalidating is `EXPIRED`.
    cache_status: CacheStatus,
    /// Applies the backend's normal 200/206 validity when `expires_at` is
    /// absent. Bursty owns its expiry, including `None` meaning never.
    normal_validity: bool,
    /// Most-nested backend inactivity ceiling. Peer fills additionally clamp
    /// this against the ceiling advertised by the selected source instance.
    max_idle_secs: Option<u64>,
    /// Content rewrite rules applied to a matching `200` body before admit, so a
    /// rewritten index is cached and served as a zero-copy HIT. Empty for every
    /// non-registry backend.
    rewrite: &'a [ovcc_rewrite::RewriteRule],
    /// Requests an uncompressed origin body. A registry HIT does not replay
    /// `Content-Encoding`, so a cached compressed body would reach the client
    /// mislabeled, and identity also lets content rewrite see the real index bytes.
    force_identity: bool,
    /// Keeps the buffered fill path when a registry rewrite needs the complete
    /// body before storage and relay.
    force_buffered: bool,
    /// Re-authorizes a shared fill before serving its bytes.
    recheck_auth: bool,
    /// Revalidates a shared fill against origin mutation before serving it.
    recheck_mutation: bool,
    /// Shares a committed entry found while coordinating this miss.
    share_existing: bool,
    /// Lets the cache key alone admit a follower to an active fill, as it already
    /// admits a waiter to a committed fill, so joining needs no origin preflight.
    key_authorizes_followers: bool,
    /// Origin used for authorization and mutation checks on a shared fill.
    recheck_upstream: &'a str,
}

enum CacheFillStart {
    Writer(Box<dyn StoreWrite>),
    Skipped(CacheStoreResult),
    StorageFailure(Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FillSource {
    Origin,
    Peer {
        replicate: bool,
        max_idle_secs: Option<u64>,
    },
}

impl FillSource {
    const fn cache_tier(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Peer { .. } => "peer",
        }
    }

    const fn store_locally(self) -> bool {
        match self {
            Self::Origin => true,
            Self::Peer { replicate, .. } => replicate,
        }
    }

    const fn is_peer(self) -> bool {
        matches!(self, Self::Peer { .. })
    }

    const fn is_shareable_peer(self) -> bool {
        matches!(
            self,
            Self::Peer {
                replicate: true,
                ..
            }
        )
    }

    const fn cache_provenance(self) -> CacheProvenance {
        match self {
            Self::Origin => CacheProvenance::Origin,
            Self::Peer { .. } => CacheProvenance::PeerCopy,
        }
    }

    fn max_idle_secs(self, backend_max_idle_secs: Option<u64>) -> Option<u64> {
        match self {
            Self::Origin => backend_max_idle_secs,
            Self::Peer { max_idle_secs, .. } => [backend_max_idle_secs, max_idle_secs]
                .into_iter()
                .flatten()
                .min(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerFallbackReason {
    BodyReadFailed,
    BodyIncomplete,
}

impl PeerFallbackReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BodyReadFailed => "body_read_failed_before_response",
            Self::BodyIncomplete => "body_incomplete_before_response",
        }
    }
}

// Keeps the common response inline so a rare peer fallback does not add one
// heap allocation to every MISS.
#[allow(clippy::large_enum_variant)]
enum FillOutcome {
    Served(Response),
    RetryOrigin(PeerFallbackReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowerClientFailureStage {
    ResponseHeadWrite,
    ResponseHeadFlush,
}

impl FollowerClientFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ResponseHeadWrite => "response_head_write",
            Self::ResponseHeadFlush => "response_head_flush",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowerIoFailureStage {
    ReadableWindow,
    Sendfile,
    AdvanceAfterSendfile,
    AdvanceAfterSendfileFailure,
}

impl FollowerIoFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadableWindow => "readable_window",
            Self::Sendfile => "sendfile",
            Self::AdvanceAfterSendfile => "advance_after_sendfile",
            Self::AdvanceAfterSendfileFailure => "advance_after_sendfile_failure",
        }
    }
}

// Keeps the successful response inline so joining a fill does not allocate a
// second box solely for this short-lived control result.
#[allow(clippy::large_enum_variant)]
enum FollowerFillOutcome {
    Served(Response, PeerFillFollowerResult),
    RetryNormal(PeerFillFollowerResult),
    ClientAbort {
        error: BoxError,
        bytes_sent: u64,
        result: PeerFillFollowerResult,
        stage: FollowerClientFailureStage,
    },
    IoFailure {
        error: BoxError,
        bytes_sent: u64,
        result: PeerFillFollowerResult,
        stage: FollowerIoFailureStage,
    },
}

struct ServingFollower {
    inner: FillFollower,
    #[cfg(test)]
    fault: Option<FollowerFault>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FollowerFault {
    ReadableWindow,
    Advance,
}

impl From<FillFollower> for ServingFollower {
    fn from(inner: FillFollower) -> Self {
        Self {
            inner,
            #[cfg(test)]
            fault: None,
        }
    }
}

impl ServingFollower {
    #[cfg(test)]
    fn with_fault(inner: FillFollower, fault: FollowerFault) -> Self {
        Self {
            inner,
            fault: Some(fault),
        }
    }

    fn meta(&self) -> &StoredMeta {
        self.inner.meta()
    }

    fn expected_size(&self) -> u64 {
        self.inner.expected_size()
    }

    fn offset(&self) -> u64 {
        self.inner.offset()
    }

    fn progress(&self) -> ovcc_cache::FillProgress {
        self.inner.progress()
    }

    fn readable_window(&self) -> Result<Option<ovcc_cache::FillReadWindow>> {
        #[cfg(test)]
        if self.fault == Some(FollowerFault::ReadableWindow) {
            return Err(Error::internal("injected follower readable-window failure"));
        }
        self.inner.readable_window()
    }

    fn advance(&mut self, bytes: u64) -> Result<()> {
        #[cfg(test)]
        if self.fault == Some(FollowerFault::Advance) {
            return Err(Error::internal("injected follower advance failure"));
        }
        self.inner.advance(bytes)
    }

    async fn wait_for_progress(
        &mut self,
    ) -> std::result::Result<ovcc_cache::FillProgress, watch::error::RecvError> {
        self.inner.wait_for_progress().await
    }
}

impl FollowerFillOutcome {
    fn served(response: Response, result: PeerFillFollowerResult, is_peer: bool) -> Self {
        if is_peer {
            ovcc_telemetry::metrics::record_peer_fill_follower_terminal(result);
        }
        Self::Served(response, result)
    }

    fn retry_normal(is_peer: bool) -> Self {
        let result = PeerFillFollowerResult::PrebyteFallback;
        if is_peer {
            ovcc_telemetry::metrics::record_peer_fill_follower_terminal(result);
        }
        Self::RetryNormal(result)
    }

    fn client_abort(
        error: BoxError,
        bytes_sent: u64,
        stage: FollowerClientFailureStage,
        is_peer: bool,
    ) -> Self {
        let result = PeerFillFollowerResult::ClientAbort;
        if is_peer {
            ovcc_telemetry::metrics::record_peer_fill_follower_terminal(result);
        }
        Self::ClientAbort {
            error,
            bytes_sent,
            result,
            stage,
        }
    }

    fn io_failure(
        error: BoxError,
        bytes_sent: u64,
        stage: FollowerIoFailureStage,
        is_peer: bool,
    ) -> Self {
        add_downstream_body_bytes(bytes_sent);
        let result = PeerFillFollowerResult::IoFailure;
        if is_peer {
            ovcc_telemetry::metrics::record_peer_fill_follower_terminal(result);
        }
        Self::IoFailure {
            error,
            bytes_sent,
            result,
            stage,
        }
    }
}

enum ReplicatedFillDecision {
    Owner(FillPermit),
    Shared(Lease),
    Follower(FillFollower),
    Timeout,
    Bypass,
}

#[cfg(test)]
struct FillLockWaitTestHook {
    key: String,
    blocked: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static FILL_LOCK_WAIT_TEST_HOOK: std::sync::OnceLock<Mutex<Option<FillLockWaitTestHook>>> =
    std::sync::OnceLock::new();

fn begin_cache_fill<S>(
    cache: &Cache<S>,
    key: &ovcc_cachekey::CacheKey,
    meta: StoredMeta,
    normal_validity: bool,
    permit: Option<FillPermit>,
    shareable: bool,
) -> std::result::Result<CacheFillStart, BoxError>
where
    S: Storage,
{
    let Some(permit) = permit else {
        return Ok(CacheFillStart::Skipped(CacheStoreResult::LockBypass));
    };
    let result = if shareable {
        debug_assert!(
            normal_validity,
            "shareable fills always use normal cache validity"
        );
        cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)
    } else if normal_validity {
        cache.begin_acquired_fill(key.as_str(), meta, permit)
    } else {
        cache.begin_acquired_fill_preserving_expiry(key.as_str(), meta, permit)
    };
    match result {
        Ok(writer) => Ok(CacheFillStart::Writer(writer)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::Cache | ErrorKind::Storage | ErrorKind::Io
            ) =>
        {
            Ok(CacheFillStart::StorageFailure(err))
        }
        Err(err) => Err(Box::new(err)),
    }
}

/// Writes and commits one complete buffered body without making cache I/O
/// failures client-visible. The origin response remains authoritative.
fn store_complete_body(start: CacheFillStart, body: &[u8]) -> CacheStoreResult {
    store_complete_chunks(start, &mut [IoSlice::new(body)])
}

fn store_complete_chunks(start: CacheFillStart, chunks: &mut [IoSlice<'_>]) -> CacheStoreResult {
    match start {
        CacheFillStart::Writer(mut writer) => {
            if writer.write_chunks(chunks).is_err() {
                writer.abort();
                return CacheStoreResult::StorageError;
            }
            match writer.commit() {
                Ok(lease) => {
                    local::retain_committed(lease);
                    CacheStoreResult::Stored
                }
                Err(_) => CacheStoreResult::StorageError,
            }
        }
        CacheFillStart::Skipped(result) => result,
        CacheFillStart::StorageFailure(_) => CacheStoreResult::StorageError,
    }
}

async fn write_follower_response_head<S>(
    stream: &mut S,
    meta: &StoredMeta,
    expected_size: u64,
    add_lft_cors: bool,
    keep_alive: bool,
) -> std::result::Result<(), (FollowerClientFailureStage, std::io::Error)>
where
    S: AsyncWrite + Unpin,
{
    let client_headers = miss_client_headers(&meta.headers, meta.status, add_lft_cors);
    let headers = client_headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain(std::iter::once((
            "X-Cache-Tier-Hit",
            match meta.provenance {
                CacheProvenance::Origin => "origin",
                CacheProvenance::PeerCopy => "peer",
            },
        )))
        .collect::<Vec<_>>();
    write_streaming_head(
        stream,
        meta.status,
        miss_reason(meta.status),
        Some("MISS"),
        &headers,
        Some(expected_size),
        false,
        keep_alive,
    )
    .await
    .map_err(|error| (FollowerClientFailureStage::ResponseHeadWrite, error))?;
    timed_flush(stream)
        .await
        .map_err(|error| (FollowerClientFailureStage::ResponseHeadFlush, error))
}

/// A MISS body at or below this known length is buffered and committed to the
/// cache before the response is written to the client, so an immediate
/// re-request is a deterministic HIT (bursty list re-GETs, presigned re-GETs).
/// A larger or unknown-length body streams progressively instead
/// (see [`serve_miss_streaming`]). Eligible known-length fills also publish
/// staging bytes for independently authorized same-process followers.
const MISS_BUFFER_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Whether an origin fill publishes its staging bytes to followers. Only large,
/// known-length, validated responses qualify, and a response echoing
/// caller-specific headers such as `x-ms-client-request-id` never does.
fn origin_fill_is_shareable(
    fill: &MissFill<'_>,
    headers: &[(String, String)],
    stored_headers: &[(String, String)],
    has_etag: bool,
) -> bool {
    fill.share_existing
        && fill.normal_validity
        && !fill.snapshot_response
        && !fill.force_identity
        && fill.rewrite.is_empty()
        && parse_content_length(headers).is_some_and(|len| len > MISS_BUFFER_MAX_BYTES)
        && (has_etag || header_value(headers, "last-modified").is_some())
        && !stored_headers
            .iter()
            .any(|(name, _)| is_client_scoped_response_header(name))
}

/// Plans the origin request that authorizes a peer or follower before it may
/// share bytes. Signed or authenticated requests keep a method-bound ranged `GET`.
fn origin_preflight_plan(head: &RequestHead) -> Option<ovcc_upstream::PeerPreflight> {
    let method_bound_or_authenticated = header_value(&head.headers, "authorization").is_some()
        || ovcc_presigned::classify(&head.query) != PresignKind::None;
    ovcc_upstream::plan_peer_preflight(&head.query, &head.headers, method_bound_or_authenticated)
}

async fn peer_preflight(
    head: &RequestHead,
    upstream: &str,
) -> Option<ovcc_upstream::PreflightMetadata> {
    let plan = origin_preflight_plan(head)?;
    let url = origin_url(upstream, head);
    let started = Instant::now();
    let result = ovcc_upstream::fetch_with_method(plan.method.clone(), &url, &plan.headers).await;
    ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
        host: head.host.as_deref().unwrap_or_default(),
        method: if plan.method == http::Method::HEAD {
            "HEAD"
        } else {
            "GET"
        },
        status: result.as_ref().ok().map(|origin| origin.status),
        duration_seconds: started.elapsed().as_secs_f64(),
    });
    let response = result.ok()?;
    ovcc_upstream::peer_preflight_metadata(&plan, &response)
}

async fn origin_follower_preflight(
    head: &RequestHead,
    upstream: &str,
    auth_upstream: &str,
) -> Option<ovcc_upstream::PreflightMetadata> {
    if auth_upstream != upstream {
        let plan = origin_preflight_plan(head)?;
        let url = origin_url(auth_upstream, head);
        let started = Instant::now();
        let result =
            ovcc_upstream::fetch_with_method(plan.method.clone(), &url, &plan.headers).await;
        ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
            host: head.host.as_deref().unwrap_or_default(),
            method: if plan.method == http::Method::HEAD {
                "HEAD"
            } else {
                "GET"
            },
            status: result.as_ref().ok().map(|origin| origin.status),
            duration_seconds: started.elapsed().as_secs_f64(),
        });
        let status = result.ok()?.status;
        if (plan.method == http::Method::HEAD && status != 200)
            || (plan.method == http::Method::GET && status != 206)
        {
            return None;
        }
    }
    peer_preflight(head, upstream).await
}

fn preflight_validator(metadata: &ovcc_upstream::PreflightMetadata) -> ExactValidator {
    match &metadata.validator {
        ovcc_upstream::PreflightValidator::Etag(value) => ExactValidator::Etag(value.clone()),
        ovcc_upstream::PreflightValidator::LastModified(value) => {
            ExactValidator::LastModified(value.clone())
        }
    }
}

/// Parses a concrete `Content-Range: bytes start-end/total` into response length
/// and total object size. Wildcards and malformed/overflowing values are not
/// exact enough to authorize an in-progress follower.
fn content_range_shape(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (bounds, total) = value.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    if end < start || end >= total {
        return None;
    }
    Some((end.checked_sub(start)?.checked_add(1)?, total))
}

fn response_shape_matches_preflight(
    status: u16,
    expected_size: u64,
    content_range: Option<&str>,
    preflight: &ovcc_upstream::PreflightMetadata,
) -> bool {
    match status {
        200 => content_range.is_none() && expected_size == preflight.total_size,
        206 => {
            content_range_shape(content_range.unwrap_or_default()).is_some_and(|(length, total)| {
                length == expected_size && total == preflight.total_size
            })
        }
        _ => false,
    }
}

/// Returns a follower only when this exact canonical key names an active
/// shareable fill whose immutable response metadata agrees with the
/// requester's independent origin preflight. The registry contains no request
/// headers or credentials; the key supplies canonical URI/Range identity.
fn matching_in_progress_fill<S>(
    cache: &Cache<S>,
    key: &ovcc_cachekey::CacheKey,
    preflight: &ovcc_upstream::PreflightMetadata,
) -> Option<FillFollower>
where
    S: Storage,
{
    let follower = cache.join_in_progress(key.as_str())?;
    if !stored_fill_matches_preflight(follower.meta(), follower.expected_size(), preflight) {
        return None;
    }
    Some(follower)
}

fn stored_fill_matches_preflight(
    meta: &StoredMeta,
    expected_size: u64,
    preflight: &ovcc_upstream::PreflightMetadata,
) -> bool {
    let validator = preflight_validator(preflight);
    expected_size != 0
        && stored_validator_matches(meta, &validator)
        && response_shape_matches_preflight(
            meta.status,
            expected_size,
            meta.content_range.as_deref(),
            preflight,
        )
        && (meta.status != 206
            || header_value(&meta.headers, "content-range") == meta.content_range.as_deref())
}

fn origin_decision_after_preflight(
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    preflight: Option<&ovcc_upstream::PreflightMetadata>,
    decision: FillDecision,
) -> ReplicatedFillDecision {
    match decision {
        FillDecision::Owner(permit) => ReplicatedFillDecision::Owner(permit),
        FillDecision::Shared(lease) => {
            if preflight.is_some_and(|preflight| {
                stored_fill_matches_preflight(&lease.meta, lease.meta.size, preflight)
            }) {
                ReplicatedFillDecision::Shared(lease)
            } else {
                ReplicatedFillDecision::Bypass
            }
        }
        FillDecision::Bypass => preflight
            .and_then(|preflight| matching_in_progress_fill(cache, key, preflight))
            .map(ReplicatedFillDecision::Follower)
            .unwrap_or(ReplicatedFillDecision::Bypass),
    }
}

/// Streams one authorized non-HIT follower directly from frontier-bounded
/// staging-file windows. The response head is withheld until at least one byte
/// is published, leaving an abort before byte visibility eligible for the
/// ordinary peer/origin path.
async fn serve_in_progress_fill(
    stream: &mut ClientStream,
    follower: impl Into<ServingFollower>,
    add_lft_cors: bool,
) -> std::result::Result<FollowerFillOutcome, BoxError> {
    let mut follower: ServingFollower = follower.into();
    let cache_tier = match follower.meta().provenance {
        CacheProvenance::Origin => "origin",
        CacheProvenance::PeerCopy => "peer",
    };
    let is_peer = cache_tier == "peer";
    let _peer_active = is_peer.then(PeerFillFollowerActiveGuard::start);
    // The head declares the exact expected size, so a fully sent body is
    // complete framing.
    let keep_alive = downstream_reuse_allowed() && status_has_body(follower.meta().status);
    let mut head_written = false;
    loop {
        let progress = follower.progress();
        if progress.state() == FillProgressState::Aborted {
            if !head_written {
                return Ok(FollowerFillOutcome::retry_normal(is_peer));
            }
            let response =
                Response::streamed(follower.meta().status, Some("MISS"), follower.offset())
                    .with_cache_tier(cache_tier)
                    .with_cache_store_result(CacheStoreResult::Incomplete);
            return Ok(FollowerFillOutcome::served(
                response,
                PeerFillFollowerResult::Partial,
                is_peer,
            ));
        }

        let window = match follower.readable_window() {
            Ok(window) => window,
            Err(error) => {
                return Ok(FollowerFillOutcome::io_failure(
                    error.into(),
                    follower.offset(),
                    FollowerIoFailureStage::ReadableWindow,
                    is_peer,
                ));
            }
        };
        if let Some(window) = window {
            // `readable_window` is frontier-bounded, but an abort also revokes
            // further follower sends from an already-published backlog.
            if follower.progress().state() == FillProgressState::Aborted {
                continue;
            }
            if !head_written {
                if let Err((stage, error)) = write_follower_response_head(
                    stream,
                    follower.meta(),
                    follower.expected_size(),
                    add_lft_cors,
                    keep_alive,
                )
                .await
                {
                    return Ok(FollowerFillOutcome::client_abort(
                        error.into(),
                        follower.offset(),
                        stage,
                        is_peer,
                    ));
                }
                head_written = true;
            }
            if follower.progress().state() == FillProgressState::Aborted {
                continue;
            }

            match sendfile_file(stream, window.file(), window.file_offset(), window.len()).await {
                Ok(report) => {
                    if cache_tier == "peer" {
                        ovcc_telemetry::metrics::record_peer_fill_follower_bytes(report.body_bytes);
                    }
                    if let Err(error) = follower.advance(report.body_bytes) {
                        let bytes_sent = follower.offset().saturating_add(report.body_bytes);
                        return Ok(FollowerFillOutcome::io_failure(
                            error.into(),
                            bytes_sent,
                            FollowerIoFailureStage::AdvanceAfterSendfile,
                            is_peer,
                        ));
                    }
                }
                Err((report, error)) => {
                    if cache_tier == "peer" {
                        ovcc_telemetry::metrics::record_peer_fill_follower_bytes(report.body_bytes);
                    }
                    if let Err(advance_error) = follower.advance(report.body_bytes) {
                        let bytes_sent = follower.offset().saturating_add(report.body_bytes);
                        tracing::warn!(
                            event = "peer_fill_follower_compound_io_failure",
                            stage = FollowerIoFailureStage::AdvanceAfterSendfileFailure.as_str(),
                            bytes_sent,
                            sendfile_error = %error,
                            advance_error = %advance_error,
                        );
                        return Ok(FollowerFillOutcome::io_failure(
                            advance_error.into(),
                            bytes_sent,
                            FollowerIoFailureStage::AdvanceAfterSendfileFailure,
                            is_peer,
                        ));
                    }
                    return Ok(FollowerFillOutcome::io_failure(
                        error,
                        follower.offset(),
                        FollowerIoFailureStage::Sendfile,
                        is_peer,
                    ));
                }
            }
            continue;
        }

        match progress.state() {
            FillProgressState::Committed | FillProgressState::CommitFailed
                if follower.offset() == follower.expected_size() =>
            {
                let response =
                    Response::streamed(follower.meta().status, Some("MISS"), follower.offset())
                        .with_cache_tier(cache_tier)
                        .with_cache_store_result(CacheStoreResult::NotStored)
                        .with_framing_complete(keep_alive && head_written);
                return Ok(FollowerFillOutcome::served(
                    response,
                    PeerFillFollowerResult::Complete,
                    is_peer,
                ));
            }
            FillProgressState::Aborted => unreachable!("aborted state handled before window read"),
            FillProgressState::Receiving
            | FillProgressState::BodyComplete
            | FillProgressState::Committed
            | FillProgressState::CommitFailed => {
                if follower.wait_for_progress().await.is_err() {
                    if head_written {
                        let response = Response::streamed(
                            follower.meta().status,
                            Some("MISS"),
                            follower.offset(),
                        )
                        .with_cache_tier(cache_tier)
                        .with_cache_store_result(CacheStoreResult::Incomplete);
                        return Ok(FollowerFillOutcome::served(
                            response,
                            PeerFillFollowerResult::Partial,
                            is_peer,
                        ));
                    }
                    return Ok(FollowerFillOutcome::retry_normal(is_peer));
                }
            }
        }
    }
}

async fn acquire_replicated_fill<S>(
    cache: &Cache<S>,
    key: &ovcc_cachekey::CacheKey,
    preflight: &ovcc_upstream::PreflightMetadata,
    budget_deadline: tokio::time::Instant,
) -> Result<ReplicatedFillDecision>
where
    S: Storage,
{
    let acquire = cache.acquire_fill(key.as_str());
    tokio::pin!(acquire);

    // Deterministically establishes the otherwise tiny test interleaving:
    // second join check -> acquire_fill pending -> owner registry publication.
    #[cfg(test)]
    let test_hook = {
        let mut hook = FILL_LOCK_WAIT_TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if hook.as_ref().is_some_and(|hook| hook.key == key.as_str()) {
            hook.take()
        } else {
            None
        }
    };
    #[cfg(test)]
    if let Some(hook) = test_hook {
        std::future::poll_fn(|context| {
            assert!(
                acquire.as_mut().poll(context).is_pending(),
                "fill-lock test hook requires a contended acquire"
            );
            Poll::Ready(())
        })
        .await;
        let _ = hook.blocked.send(());
        let _ = hook.release.await;
    }

    match timeout_at(budget_deadline, acquire).await {
        Ok(Ok(FillDecision::Owner(permit))) => Ok(ReplicatedFillDecision::Owner(permit)),
        Ok(Ok(FillDecision::Shared(lease))) => Ok(ReplicatedFillDecision::Shared(lease)),
        Ok(Ok(FillDecision::Bypass)) => Ok(matching_in_progress_fill(cache, key, preflight)
            .map(ReplicatedFillDecision::Follower)
            .unwrap_or(ReplicatedFillDecision::Bypass)),
        Ok(Err(error)) => Err(error),
        Err(_) => Ok(matching_in_progress_fill(cache, key, preflight)
            .map(ReplicatedFillDecision::Follower)
            .unwrap_or(ReplicatedFillDecision::Timeout)),
    }
}

/// Whether a waiter may stream another request's active fill instead of waiting
/// for its commit. Rewritten, identity-forced, snapshot, and HEAD fills only share
/// committed entries.
fn can_follow_origin_fill(fill: &MissFill<'_>) -> bool {
    fill.share_existing
        && fill.normal_validity
        && !fill.head_only
        && !fill.force_buffered
        && !fill.force_identity
        && fill.rewrite.is_empty()
        && !fill.snapshot_response
}

async fn acquire_origin_fill(
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    head: &RequestHead,
    upstream: &str,
    auth_upstream: &str,
    key_authorizes_followers: bool,
) -> Result<ReplicatedFillDecision> {
    let mut registration = cache.subscribe_fill_registration(key.as_str());
    let acquire = cache.acquire_fill(key.as_str());
    tokio::pin!(acquire);
    let initial = std::future::poll_fn(|context| Poll::Ready(acquire.as_mut().poll(context))).await;
    let mut immediate_bypass = match initial {
        Poll::Ready(Ok(FillDecision::Owner(permit))) => {
            return Ok(ReplicatedFillDecision::Owner(permit));
        }
        Poll::Ready(Ok(FillDecision::Shared(lease))) => {
            return Ok(ReplicatedFillDecision::Shared(lease));
        }
        Poll::Ready(Ok(FillDecision::Bypass)) => true,
        Poll::Ready(Err(error)) => return Err(error),
        Poll::Pending => false,
    };

    // Buffered and ineligible owners preserve ordinary single-flight sharing
    // without paying for a follower-only origin metadata probe.
    let follower = loop {
        if let Some(follower) = cache.join_in_progress(key.as_str()) {
            break follower;
        }
        if immediate_bypass {
            return Ok(ReplicatedFillDecision::Bypass);
        }
        tokio::select! {
            decision = &mut acquire => match decision? {
                FillDecision::Owner(permit) => return Ok(ReplicatedFillDecision::Owner(permit)),
                FillDecision::Shared(lease) => return Ok(ReplicatedFillDecision::Shared(lease)),
                FillDecision::Bypass => immediate_bypass = true,
            },
            changed = registration.changed() => {
                changed.map_err(|_| Error::internal("fill registration watcher closed"))?;
            }
        }
    };
    if key_authorizes_followers {
        return Ok(ReplicatedFillDecision::Follower(follower));
    }
    drop(follower);

    let Some(preflight) = origin_follower_preflight(head, upstream, auth_upstream).await else {
        if immediate_bypass {
            return Ok(ReplicatedFillDecision::Bypass);
        }
        return acquire
            .await
            .map(|decision| origin_decision_after_preflight(cache, key, None, decision));
    };
    if let Some(follower) = matching_in_progress_fill(cache, key, &preflight) {
        return Ok(ReplicatedFillDecision::Follower(follower));
    }
    if immediate_bypass {
        return Ok(ReplicatedFillDecision::Bypass);
    }
    loop {
        tokio::select! {
            decision = &mut acquire => {
                return decision.map(|decision| {
                    origin_decision_after_preflight(cache, key, Some(&preflight), decision)
                });
            }
            changed = registration.changed() => {
                if changed.is_err() {
                    break;
                }
                if let Some(follower) = matching_in_progress_fill(cache, key, &preflight) {
                    return Ok(ReplicatedFillDecision::Follower(follower));
                }
            }
        }
    }
    acquire
        .await
        .map(|decision| origin_decision_after_preflight(cache, key, Some(&preflight), decision))
}

fn record_peer_attempt(stage: PeerAttemptStage, outcome: PeerAttemptOutcome, started: Instant) {
    ovcc_telemetry::metrics::record_peer_attempt(stage, outcome, started.elapsed().as_secs_f64());
}

fn record_peer_budget_exhausted(stage: PeerAttemptStage, started: Instant) {
    let elapsed = started.elapsed();
    tracing::info!(
        event = "peer_fill_fallback",
        reason = "budget_exhausted",
        stage = stage.as_str(),
        duration_seconds = elapsed.as_secs_f64(),
    );
    ovcc_telemetry::metrics::record_peer_attempt(
        stage,
        PeerAttemptOutcome::Timeout,
        elapsed.as_secs_f64(),
    );
    ovcc_telemetry::metrics::record_peer_fill(PeerResult::Timeout, 0, elapsed.as_secs_f64());
}

fn log_peer_fill_follower_client_abort(
    stage: FollowerClientFailureStage,
    bytes_sent: u64,
    error: &dyn std::fmt::Display,
) {
    tracing::info!(
        event = "peer_fill_follower_client_abort",
        stage = stage.as_str(),
        bytes_sent,
        error = %error,
    );
}

fn log_peer_fill_follower_io_failure(
    stage: FollowerIoFailureStage,
    bytes_sent: u64,
    error: &dyn std::fmt::Display,
) {
    tracing::warn!(
        event = "peer_fill_follower_io_failure",
        stage = stage.as_str(),
        bytes_sent,
        error = %error,
    );
}

/// Attempts a peer-backed MISS before opening the origin body. Replicating
/// requests acquire the per-key fill lock after probing, while relay-only
/// requests avoid local fill coordination.
#[allow(clippy::too_many_arguments)]
async fn try_peer_fill(
    stream: &mut ClientStream,
    cache: &Arc<Cache<DiskStorage>>,
    key: &ovcc_cachekey::CacheKey,
    upstream: &str,
    head: &RequestHead,
    filtered_args: &str,
    effective_range: &str,
    fill: MissFill<'_>,
    ctx: &ConnCtx,
) -> std::result::Result<Option<Response>, BoxError> {
    // Staging followers and relay-only peers cannot grant a committed file.
    if matches!(stream, ClientStream::Local(_)) {
        return Ok(None);
    }
    let Some(peer) = ctx.peer.as_ref() else {
        return Ok(None);
    };
    if peer_forwarded_request(head).unwrap_or(false)
        || fill.head_only
        || fill.force_buffered
        || fill.force_identity
        || !fill.rewrite.is_empty()
        || !fill.share_existing
    {
        return Ok(None);
    }
    let budget_started = Instant::now();
    let budget_deadline = tokio::time::Instant::now() + peer.budget;
    let preflight =
        match timeout_at(budget_deadline, peer_preflight(head, fill.recheck_upstream)).await {
            Ok(Some(preflight)) => preflight,
            Ok(None) => {
                tracing::info!(event = "peer_fill_fallback", reason = "preflight_failed");
                record_peer_attempt(
                    PeerAttemptStage::Preflight,
                    PeerAttemptOutcome::Failed,
                    budget_started,
                );
                return Ok(None);
            }
            Err(_) => {
                record_peer_budget_exhausted(PeerAttemptStage::Preflight, budget_started);
                return Ok(None);
            }
        };
    if preflight.total_size < peer.min_object_bytes {
        tracing::info!(
            event = "peer_fill_fallback",
            reason = "object_too_small",
            object_size = preflight.total_size,
        );
        record_peer_attempt(
            PeerAttemptStage::Preflight,
            PeerAttemptOutcome::Ineligible,
            budget_started,
        );
        return Ok(None);
    }
    let validator = preflight_validator(&preflight);
    if peer.replicate
        && let Some(follower) = matching_in_progress_fill(cache, key, &preflight)
    {
        match serve_in_progress_fill(stream, follower, false).await? {
            FollowerFillOutcome::Served(response, result) => {
                debug_assert!(matches!(
                    result,
                    PeerFillFollowerResult::Complete | PeerFillFollowerResult::Partial
                ));
                return Ok(Some(response));
            }
            FollowerFillOutcome::RetryNormal(result) => {
                debug_assert_eq!(result, PeerFillFollowerResult::PrebyteFallback);
            }
            FollowerFillOutcome::ClientAbort {
                error,
                bytes_sent,
                result,
                stage,
            } => {
                debug_assert_eq!(result, PeerFillFollowerResult::ClientAbort);
                log_peer_fill_follower_client_abort(stage, bytes_sent, error.as_ref());
                return Err(error);
            }
            FollowerFillOutcome::IoFailure {
                error,
                bytes_sent,
                result,
                stage,
            } => {
                debug_assert_eq!(result, PeerFillFollowerResult::IoFailure);
                log_peer_fill_follower_io_failure(stage, bytes_sent, error.as_ref());
                return Err(error);
            }
        }
    }
    let identity = ObjectIdentity {
        host: head.host.clone().unwrap_or_default(),
        method: http::Method::GET,
        uri: head.path.clone(),
        args: filtered_args.to_owned(),
        if_match: header_value(&head.headers, "if-match")
            .unwrap_or_default()
            .to_owned(),
        range: effective_range.to_owned(),
    };
    let selection = match timeout_at(
        budget_deadline,
        first_peer_probe(
            peer,
            &identity,
            &validator,
            ctx.backends
                .iter()
                .find(|runtime| runtime.config.resolve_backend(&identity.host).is_some())
                .filter(|runtime| runtime.config.name == "azure")
                .map(|_| key.as_str()),
        ),
    )
    .await
    {
        Ok(Some(selection)) => selection,
        Ok(None) => {
            tracing::info!(event = "peer_fill_fallback", reason = "no_eligible_peer",);
            record_peer_attempt(
                PeerAttemptStage::Fanout,
                PeerAttemptOutcome::Miss,
                budget_started,
            );
            return Ok(None);
        }
        Err(_) => {
            record_peer_budget_exhausted(PeerAttemptStage::Fanout, budget_started);
            return Ok(None);
        }
    };
    let probe = selection.probe;
    tracing::info!(
        event = "peer_selected",
        peer = %probe.peer,
        status = probe.http_status,
        object_size = probe.object_size,
        cancelled_losers = selection.cancelled_losers,
    );

    // The first check above avoids duplicate control/data-plane work when the
    // owner was already registered. This second check closes the race where a
    // peer owner registers while this request's WhoHas fanout is in flight.
    if peer.replicate
        && let Some(follower) = matching_in_progress_fill(cache, key, &preflight)
    {
        match serve_in_progress_fill(stream, follower, false).await? {
            FollowerFillOutcome::Served(response, result) => {
                debug_assert!(matches!(
                    result,
                    PeerFillFollowerResult::Complete | PeerFillFollowerResult::Partial
                ));
                return Ok(Some(response));
            }
            FollowerFillOutcome::RetryNormal(result) => {
                debug_assert_eq!(result, PeerFillFollowerResult::PrebyteFallback);
            }
            FollowerFillOutcome::ClientAbort {
                error,
                bytes_sent,
                result,
                stage,
            } => {
                debug_assert_eq!(result, PeerFillFollowerResult::ClientAbort);
                log_peer_fill_follower_client_abort(stage, bytes_sent, error.as_ref());
                return Err(error);
            }
            FollowerFillOutcome::IoFailure {
                error,
                bytes_sent,
                result,
                stage,
            } => {
                debug_assert_eq!(result, PeerFillFollowerResult::IoFailure);
                log_peer_fill_follower_io_failure(stage, bytes_sent, error.as_ref());
                return Err(error);
            }
        }
    }

    let permit = if peer.replicate {
        let fill_decision =
            match acquire_replicated_fill(cache, key, &preflight, budget_deadline).await {
                Ok(decision) => decision,
                Err(error) => {
                    record_peer_attempt(
                        PeerAttemptStage::FillLock,
                        PeerAttemptOutcome::Failed,
                        budget_started,
                    );
                    return Err(error.into());
                }
            };
        match fill_decision {
            ReplicatedFillDecision::Owner(permit) => Some(permit),
            ReplicatedFillDecision::Shared(lease) => {
                record_peer_attempt(
                    PeerAttemptStage::FillLock,
                    PeerAttemptOutcome::Shared,
                    budget_started,
                );
                return serve_shared_fill(stream, cache, key, upstream, head, fill, lease)
                    .await
                    .map(Some);
            }
            ReplicatedFillDecision::Follower(follower) => {
                match serve_in_progress_fill(stream, follower, false).await? {
                    FollowerFillOutcome::Served(response, result) => {
                        debug_assert!(matches!(
                            result,
                            PeerFillFollowerResult::Complete | PeerFillFollowerResult::Partial
                        ));
                        return Ok(Some(response));
                    }
                    FollowerFillOutcome::RetryNormal(result) => {
                        debug_assert_eq!(result, PeerFillFollowerResult::PrebyteFallback);
                        return Ok(None);
                    }
                    FollowerFillOutcome::ClientAbort {
                        error,
                        bytes_sent,
                        result,
                        stage,
                    } => {
                        debug_assert_eq!(result, PeerFillFollowerResult::ClientAbort);
                        log_peer_fill_follower_client_abort(stage, bytes_sent, error.as_ref());
                        return Err(error);
                    }
                    FollowerFillOutcome::IoFailure {
                        error,
                        bytes_sent,
                        result,
                        stage,
                    } => {
                        debug_assert_eq!(result, PeerFillFollowerResult::IoFailure);
                        log_peer_fill_follower_io_failure(stage, bytes_sent, error.as_ref());
                        return Err(error);
                    }
                }
            }
            ReplicatedFillDecision::Timeout => {
                record_peer_budget_exhausted(PeerAttemptStage::FillLock, budget_started);
                return Ok(None);
            }
            ReplicatedFillDecision::Bypass => {
                record_peer_attempt(
                    PeerAttemptStage::FillLock,
                    PeerAttemptOutcome::Bypass,
                    budget_started,
                );
                return Ok(None);
            }
        }
    } else {
        None
    };

    let fill_started = Instant::now();
    let cache_peer = SocketAddr::new(probe.peer.ip(), peer.cache_port.load(Ordering::Acquire));
    let opened = match timeout_at(
        budget_deadline,
        peer.client
            .open_object(cache_peer, &request_uri(head), &head.headers),
    )
    .await
    {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => {
            peer.directory.mark_failed(probe.peer);
            let attempt_outcome = if error.timeout_stage().is_some() {
                PeerAttemptOutcome::Timeout
            } else {
                PeerAttemptOutcome::Refused
            };
            tracing::warn!(
                event = "peer_open_failed",
                peer = %probe.peer,
                timeout_stage = ?error.timeout_stage(),
                duration_seconds = fill_started.elapsed().as_secs_f64(),
                error = %error,
            );
            record_peer_attempt(PeerAttemptStage::PeerOpen, attempt_outcome, budget_started);
            ovcc_telemetry::metrics::record_peer_fill(
                if error.timeout_stage().is_some() {
                    PeerResult::Timeout
                } else {
                    PeerResult::Refused
                },
                0,
                fill_started.elapsed().as_secs_f64(),
            );
            drop(permit);
            return Ok(None);
        }
        Err(_) => {
            record_peer_budget_exhausted(PeerAttemptStage::PeerOpen, budget_started);
            drop(permit);
            return Ok(None);
        }
    };
    peer.directory.mark_healthy(probe.peer);
    let status = opened.status.as_u16();
    let response_length = parse_peer_content_length(&opened.headers);
    let response_validator = peer_response_validator(&opened.headers);
    let response_content_range = opened
        .headers
        .get(http::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok());
    if !matches!(status, 200 | 206)
        || response_length != Some(probe.object_size)
        || response_validator.as_ref() != Some(&validator)
        || !response_shape_matches_preflight(
            status,
            probe.object_size,
            response_content_range,
            &preflight,
        )
    {
        tracing::warn!(
            event = "peer_open_rejected",
            peer = %probe.peer,
            status,
            content_length = response_length,
            expected_size = probe.object_size,
        );
        record_peer_attempt(
            PeerAttemptStage::PeerOpen,
            PeerAttemptOutcome::Rejected,
            budget_started,
        );
        ovcc_telemetry::metrics::record_peer_fill(
            PeerResult::Refused,
            0,
            fill_started.elapsed().as_secs_f64(),
        );
        drop(permit);
        return Ok(None);
    }
    tracing::info!(
        event = "peer_open_complete",
        peer = %probe.peer,
        status,
        content_length = response_length,
        duration_seconds = fill_started.elapsed().as_secs_f64(),
    );
    let headers = match peer_header_vec(&opened.headers) {
        Ok(headers) => headers,
        Err(error) => {
            peer.directory.mark_failed(probe.peer);
            tracing::warn!(
                event = "peer_open_rejected",
                peer = %probe.peer,
                reason = "invalid_response_headers",
                duration_seconds = fill_started.elapsed().as_secs_f64(),
                error = %error,
            );
            record_peer_attempt(
                PeerAttemptStage::PeerOpen,
                PeerAttemptOutcome::Rejected,
                budget_started,
            );
            ovcc_telemetry::metrics::record_peer_fill(
                PeerResult::Refused,
                0,
                fill_started.elapsed().as_secs_f64(),
            );
            drop(permit);
            return Ok(None);
        }
    };
    record_peer_attempt(
        PeerAttemptStage::PeerOpen,
        PeerAttemptOutcome::Success,
        budget_started,
    );
    let etag = header_value(&headers, "etag").map(str::to_owned);
    let outcome = fill_from_open_response(
        stream,
        cache,
        key,
        head,
        fill,
        permit,
        status,
        etag,
        headers,
        None,
        opened.body,
        FillSource::Peer {
            replicate: peer.replicate,
            max_idle_secs: probe.max_idle_secs,
        },
    )
    .await?;
    let response = match outcome {
        FillOutcome::Served(response) => response,
        FillOutcome::RetryOrigin(reason) => {
            peer.directory.mark_failed(probe.peer);
            tracing::warn!(
                event = "peer_fill_fallback",
                reason = reason.as_str(),
                peer = %probe.peer,
                replicate = peer.replicate,
                duration_seconds = fill_started.elapsed().as_secs_f64(),
            );
            ovcc_telemetry::metrics::record_peer_fill(
                PeerResult::Truncated,
                0,
                fill_started.elapsed().as_secs_f64(),
            );
            return Ok(None);
        }
    };
    let fill_result = match (response.head.status, response.cache_store_result) {
        (403, _) => PeerResult::Unauthorized,
        (_, Some(CacheStoreResult::Stored | CacheStoreResult::NotStored)) => PeerResult::Hit,
        (_, Some(CacheStoreResult::Incomplete)) => PeerResult::Truncated,
        (_, Some(CacheStoreResult::LockBypass | CacheStoreResult::StorageError) | None) => {
            PeerResult::Refused
        }
    };
    ovcc_telemetry::metrics::record_peer_fill(
        fill_result,
        response.bytes_sent,
        fill_started.elapsed().as_secs_f64(),
    );
    tracing::info!(
        event = "peer_fill_complete",
        peer = %probe.peer,
        result = ?fill_result,
        status = response.head.status,
        bytes = response.bytes_sent,
        replicate = peer.replicate,
        duration_seconds = fill_started.elapsed().as_secs_f64(),
    );
    Ok(Some(response))
}

struct PeerProbeSelection {
    probe: ovcc_peer::ProbeResponse,
    cancelled_losers: usize,
}

async fn abort_join_set<T: 'static>(tasks: &mut JoinSet<T>) -> usize {
    tasks.abort_all();
    let mut cancelled = 0;
    while let Some(task) = tasks.join_next().await {
        if task.is_err_and(|error| error.is_cancelled()) {
            cancelled += 1;
        }
    }
    cancelled
}

fn optional_fixture_key(_key: &str) -> bool {
    #[cfg(feature = "test-optional-keys")]
    {
        optional_key_fixture::FixturePolicy::from_key(_key).is_some()
    }
    #[cfg(not(feature = "test-optional-keys"))]
    {
        false
    }
}

async fn first_peer_probe(
    peer: &PeerRuntime,
    identity: &ObjectIdentity,
    validator: &ExactValidator,
    _physical_key: Option<&str>,
) -> Option<PeerProbeSelection> {
    let directory_started = Instant::now();
    let mut peers = match peer.directory.select_all().await {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!(
                event = "peer_directory_select_failed",
                duration_seconds = directory_started.elapsed().as_secs_f64(),
                error = %error,
            );
            return None;
        }
    };
    peer.random.shuffle(&mut peers);
    peer.client.retain_grpc_channels(&peers);
    ovcc_telemetry::metrics::record_peer_directory(peers.len());
    tracing::info!(
        event = "peer_directory_selected",
        peer_count = peers.len(),
        duration_seconds = directory_started.elapsed().as_secs_f64(),
    );
    let mut probes = JoinSet::new();
    for address in peers {
        let client = peer.client.clone();
        let identity = identity.clone();
        let validator = validator.clone();
        #[cfg(feature = "test-optional-keys")]
        let optional = _physical_key.and_then(optional_key_fixture::FixturePolicy::wire);
        #[cfg(not(feature = "test-optional-keys"))]
        let optional = _physical_key.and_then(azure_optional_keys::AzurePolicy::wire);
        let physical_key = _physical_key.map(str::to_owned);
        probes.spawn(async move {
            let _metrics = PeerProbeInFlightGuard::start();
            let started = Instant::now();
            let result = if let Some(logical) = optional {
                match client.probe_optional(address, &identity, &validator, &logical).await {
                    Ok(ovcc_peer::OptionalProbeResponse::Supported { response, omitted_parts }) => {
                        tracing::info!(event = "optional_key_probe", acknowledged = true, omissions = ?omitted_parts);
                        Ok(response)
                    }
                    Ok(ovcc_peer::OptionalProbeResponse::Unsupported) => {
                        tracing::info!(event = "optional_key_probe", acknowledged = false);
                        if physical_key.as_deref().and_then(|key| key.rsplit('|').next()) == Some(identity.range.as_str()) {
                            client.probe(address, &identity, &validator).await
                        } else {
                            Ok(ovcc_peer::ProbeResponse { peer: address, object_status: peer_proto::ObjectStatus::Absent,
                                http_status: 0, object_size: 0, validator: None, max_idle_secs: None })
                        }
                    }
                    Err(error) => Err(error),
                }
            } else { client.probe(address, &identity, &validator).await };
            (address, started, result)
        });
    }
    while let Some(completed) = probes.join_next().await {
        let Ok((address, started, result)) = completed else {
            continue;
        };
        match result {
            Ok(probe) => {
                peer.directory.mark_healthy(address);
                let eligible = probe.object_status == peer_proto::ObjectStatus::Committed
                    && matches!(probe.http_status, 200 | 206)
                    && probe.validator.as_ref() == Some(validator);
                tracing::info!(
                    event = "peer_probe_complete",
                    peer = %address,
                    result = if eligible { "hit" } else { "miss" },
                    status = probe.http_status,
                    object_size = probe.object_size,
                    duration_seconds = started.elapsed().as_secs_f64(),
                );
                let duration_seconds = started.elapsed().as_secs_f64();
                if eligible {
                    ovcc_telemetry::metrics::record_peer_probe(PeerResult::Hit, duration_seconds);
                    let cancelled_losers = abort_join_set(&mut probes).await;
                    ovcc_telemetry::metrics::record_peer_probe_cancellations(cancelled_losers);
                    return Some(PeerProbeSelection {
                        probe,
                        cancelled_losers,
                    });
                }
                ovcc_telemetry::metrics::record_peer_probe(PeerResult::Miss, duration_seconds);
            }
            Err(error) => {
                peer.directory.mark_failed(address);
                tracing::warn!(
                    event = "peer_probe_complete",
                    peer = %address,
                    result = if error.timeout_stage().is_some() {
                        "timeout"
                    } else {
                        "refused"
                    },
                    timeout_stage = ?error.timeout_stage(),
                    duration_seconds = started.elapsed().as_secs_f64(),
                    error = %error,
                );
                ovcc_telemetry::metrics::record_peer_probe(
                    if error.timeout_stage().is_some() {
                        PeerResult::Timeout
                    } else {
                        PeerResult::Refused
                    },
                    started.elapsed().as_secs_f64(),
                );
            }
        }
    }
    None
}

fn parse_peer_content_length(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn peer_response_validator(headers: &http::HeaderMap) -> Option<ExactValidator> {
    headers
        .get(http::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(|value| ExactValidator::Etag(value.to_owned()))
        .or_else(|| {
            headers
                .get(http::header::LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(|value| ExactValidator::LastModified(value.to_owned()))
        })
}

fn peer_header_vec(
    headers: &http::HeaderMap,
) -> std::result::Result<Vec<(String, String)>, BoxError> {
    headers
        .iter()
        .map(|(name, value)| Ok((name.as_str().to_owned(), value.to_str()?.to_owned())))
        .collect()
}

/// On the MISS path, fetches from the origin; admits `200`/`206`, plus `404`
/// only when the caller explicitly marks a canonical bursty fill, and relays
/// the body to the client. Registry rewrite and HEAD fills buffer the (small)
/// body; the default GET path streams origin → storage → client without holding
/// the whole object in RAM. An unreachable origin becomes a `502`.
#[tracing::instrument(name = "ovcc.serve_miss", skip_all)]
async fn serve_miss(
    stream: &mut ClientStream,
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    upstream: &str,
    head: &RequestHead,
    fill: MissFill<'_>,
) -> std::result::Result<Response, BoxError> {
    // Local paths require a committed generation; waiters retain commit-based sharing.
    let decision = if !matches!(stream, ClientStream::Local(_)) && can_follow_origin_fill(&fill) {
        acquire_origin_fill(
            cache,
            key,
            head,
            upstream,
            fill.recheck_upstream,
            fill.key_authorizes_followers,
        )
        .await?
    } else if fill.share_existing {
        match cache.acquire_fill(key.as_str()).await? {
            FillDecision::Owner(permit) => ReplicatedFillDecision::Owner(permit),
            FillDecision::Shared(lease) => ReplicatedFillDecision::Shared(lease),
            FillDecision::Bypass => ReplicatedFillDecision::Bypass,
        }
    } else {
        match cache.acquire_refill(key.as_str()).await? {
            FillDecision::Owner(permit) => ReplicatedFillDecision::Owner(permit),
            FillDecision::Shared(lease) => ReplicatedFillDecision::Shared(lease),
            FillDecision::Bypass => ReplicatedFillDecision::Bypass,
        }
    };
    let permit = match decision {
        ReplicatedFillDecision::Owner(permit) => Some(permit),
        ReplicatedFillDecision::Shared(lease) => {
            return serve_shared_fill(stream, cache, key, upstream, head, fill, lease).await;
        }
        ReplicatedFillDecision::Follower(follower) => {
            match serve_in_progress_fill(stream, follower, fill.add_lft_cors).await? {
                FollowerFillOutcome::Served(response, _) => return Ok(response),
                FollowerFillOutcome::RetryNormal(_) => {
                    match cache.acquire_fill(key.as_str()).await? {
                        FillDecision::Owner(permit) => Some(permit),
                        FillDecision::Shared(lease) => {
                            return serve_shared_fill(
                                stream, cache, key, upstream, head, fill, lease,
                            )
                            .await;
                        }
                        FillDecision::Bypass => None,
                    }
                }
                FollowerFillOutcome::ClientAbort { error, .. }
                | FollowerFillOutcome::IoFailure { error, .. } => return Err(error),
            }
        }
        ReplicatedFillDecision::Timeout | ReplicatedFillDecision::Bypass => None,
    };
    if fill.head_only || !fill.rewrite.is_empty() || fill.force_buffered {
        return serve_miss_buffered(stream, cache, key, upstream, head, fill, permit).await;
    }
    serve_miss_streaming(stream, cache, key, upstream, head, fill, permit).await
}

async fn serve_shared_fill(
    stream: &mut ClientStream,
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    upstream: &str,
    head: &RequestHead,
    fill: MissFill<'_>,
    lease: impl Into<RequestLease>,
) -> std::result::Result<Response, BoxError> {
    let lease = lease.into();
    let recheck = if fill.recheck_auth {
        let url = origin_url(fill.recheck_upstream, head);
        let started = Instant::now();
        let result = ovcc_upstream::probe_access(&url, &head.headers)
            .instrument(tracing::info_span!("ovcc.access.probe"))
            .await;
        ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
            host: head.host.as_deref().unwrap_or_default(),
            method: canonical_method(&head.method),
            status: result.as_ref().ok().map(|origin| origin.status),
            duration_seconds: started.elapsed().as_secs_f64(),
        });
        Some(result)
    } else if fill.recheck_mutation {
        let url = origin_url(fill.recheck_upstream, head);
        let started = Instant::now();
        let headers = freshness_probe_headers(&head.headers);
        let result = ovcc_upstream::fetch_head(&url, &headers)
            .instrument(tracing::info_span!("ovcc.mutation.recheck"))
            .await;
        ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
            host: head.host.as_deref().unwrap_or_default(),
            method: "HEAD",
            status: result.as_ref().ok().map(|origin| origin.status),
            duration_seconds: started.elapsed().as_secs_f64(),
        });
        Some(result)
    } else {
        None
    };

    let cached_etag = lease.meta().etag.clone();
    let upstream_etag = recheck
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .and_then(|origin| header_value(&origin.headers, "etag"))
        .map(str::to_owned);
    let response = match recheck {
        Some(Ok(origin)) if origin.status == 403 => Ok(Response::buffered(
            403,
            "Forbidden",
            Some("HIT"),
            Vec::new(),
            b"Forbidden\n".to_vec(),
        )),
        Some(Ok(origin)) if origin.status == 200 || origin.status == 206 => {
            if is_mutated(lease.meta(), &origin.headers) {
                drop(lease);
                cache.remove(key.as_str())?;
                Ok(Response::buffered(
                    503,
                    "Service Unavailable",
                    Some("HIT"),
                    Vec::new(),
                    b"Stale\n".to_vec(),
                ))
            } else {
                Ok(serve_fill_hit(lease, head, fill))
            }
        }
        Some(Ok(_)) | Some(Err(_)) => {
            drop(lease);
            serve_bypass(stream, upstream, head).await
        }
        None => Ok(serve_fill_hit(lease, head, fill)),
    };
    response.map(|response| response.with_probe_etags(cached_etag, upstream_etag))
}

fn serve_fill_hit(
    lease: impl Into<RequestLease>,
    head: &RequestHead,
    fill: MissFill<'_>,
) -> Response {
    if fill.snapshot_response {
        serve_bursty_hit(lease, head)
    } else {
        serve_hit_conditional(lease, head, is_azure_blob_request(head))
    }
}

enum BufferedOriginBody {
    Complete(Vec<u8>, UpstreamTiming),
    Failed(Box<Response>),
}

async fn collect_buffered_origin_body<S, B>(
    stream: &mut S,
    permit: &mut Option<FillPermit>,
    cache_status: CacheStatus,
    status: u16,
    headers: &[(String, String)],
    mut timing: UpstreamTiming,
    body: B,
) -> std::result::Result<BufferedOriginBody, BoxError>
where
    S: AsyncWrite + Unpin,
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let body_started = Instant::now();
    let body = match body.collect().await {
        Ok(body) => body.to_bytes().to_vec(),
        Err(error) => {
            timing.response = Some(body_started.elapsed().as_secs_f64());
            record_upstream_fetch_timing(&timing);
            drop(permit.take());
            let relayed = relay_headers(headers);
            let header_refs: Vec<(&str, &str)> = relayed
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let reason = hyper::StatusCode::from_u16(status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("");
            let content_length = parse_content_length(headers);
            write_streaming_head(
                stream,
                status,
                reason,
                Some(cache_status.as_str()),
                &header_refs,
                content_length,
                false,
                false,
            )
            .await?;
            timed_flush(stream).await?;
            // Without `Content-Length` the empty body ends at the close, which
            // must not carry `close_notify`.
            let response = Response::streamed(status, Some(cache_status.as_str()), 0)
                .with_cache_tier("origin")
                .with_upstream(status, timing, headers)
                .with_abort_close(content_length.is_none() && status_has_body(status));
            return Ok(BufferedOriginBody::Failed(Box::new(
                if origin_body_timed_out(&error) {
                    response.with_timeout_stage(TimeoutStage::OriginResponseBody)
                } else {
                    response
                },
            )));
        }
    };
    timing.response = Some(body_started.elapsed().as_secs_f64());
    record_upstream_fetch_timing(&timing);
    Ok(BufferedOriginBody::Complete(body, timing))
}

/// Buffered MISS fill for HEAD responses and registry index rewrite (small bodies).
async fn serve_miss_buffered<S>(
    stream: &mut S,
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    upstream: &str,
    head: &RequestHead,
    fill: MissFill<'_>,
    mut permit: Option<FillPermit>,
) -> std::result::Result<Response, BoxError>
where
    S: AsyncWrite + Unpin,
{
    let url = origin_url(upstream, head);

    // A failure before an origin response head becomes a gateway response. For
    // GET, retain the open response while buffering so a later body failure can
    // preserve the status the origin already supplied.
    let (status, etag, headers, upstream_timing, body) = if fill.head_only {
        let origin = match ovcc_upstream::fetch_head(&url, &head.headers).await {
            Ok(origin) => origin,
            Err(error) => {
                return Ok(abandon_fill_after_origin_error(
                    permit,
                    &error,
                    fill.cache_status.as_str(),
                    fill.head_only,
                ));
            }
        };
        (
            origin.status,
            origin.etag,
            origin.headers,
            origin.timing,
            origin.body,
        )
    } else {
        let fetch_headers = if fill.force_identity {
            identity_encoding_headers(&head.headers)
        } else {
            head.headers.clone()
        };
        let origin = match ovcc_upstream::open_fetch_with_headers(&url, &fetch_headers).await {
            Ok(origin) => origin,
            Err(error) => {
                return Ok(abandon_fill_after_origin_error(
                    permit,
                    &error,
                    fill.cache_status.as_str(),
                    fill.head_only,
                ));
            }
        };
        let ovcc_upstream::OpenOriginResponse {
            status,
            etag,
            headers,
            timing,
            body,
        } = origin;
        match collect_buffered_origin_body(
            stream,
            &mut permit,
            fill.cache_status,
            status,
            &headers,
            timing,
            body,
        )
        .await?
        {
            BufferedOriginBody::Complete(body, timing) => (status, etag, headers, timing, body),
            BufferedOriginBody::Failed(response) => return Ok(*response),
        }
    };

    finish_buffered_origin_response(
        cache,
        key,
        head,
        fill,
        permit,
        status,
        etag,
        headers,
        upstream_timing,
        body,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_buffered_origin_response(
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    head: &RequestHead,
    fill: MissFill<'_>,
    permit: Option<FillPermit>,
    status: u16,
    etag: Option<String>,
    headers: Vec<(String, String)>,
    upstream_timing: UpstreamTiming,
    body: Vec<u8>,
) -> std::result::Result<Response, BoxError> {
    local::retain_source(|| Some(cache.storage().local_path_source(key.as_str())));
    let upstream_status = status;
    // Relays the origin response headers (ETag, Content-Type, ...) alongside the
    // body so a MISS is byte- and metadata-faithful to the origin.
    let relayed = relay_headers(&headers);
    let representation_length = parse_content_length(&headers);
    let head_only = fill.head_only && fill.snapshot_response;
    let bodyless = !status_has_body(status);
    let response_content_length = if status == 304 || (head_only && !bodyless) {
        representation_length
    } else {
        None
    };
    let stored_headers = if fill.snapshot_response {
        cache_snapshot_headers(&relayed)
    } else {
        relayed.clone()
    };

    // The normal path admits only `200`/`206`; `404` is admitted only when the
    // bursty caller opts in, matching `@bursty_cache proxy_cache_valid 404`.
    if should_admit(status, fill.admit_not_found)
        && (!fill.honor_origin_cacheability
            || origin_response_is_cacheable(&headers, SystemTime::now()))
    {
        let content_range = if status == 206 {
            header_value(&headers, "content-range").map(str::to_owned)
        } else {
            None
        };
        // Rewrites a matching `200` index body before admit, so the stored bytes
        // and the served length already reflect the substitution and the HIT
        // stays a `sendfile`. `apply` is a fast no-op when `rewrite` is empty
        // (every non-registry backend) or the body does not match a rule.
        let body = if status == 200 && !fill.rewrite.is_empty() {
            let content_type = header_value(&headers, "content-type").unwrap_or("");
            match ovcc_rewrite::apply(fill.rewrite, &head.path, content_type, &body) {
                Some(rewritten) => rewritten.into_owned(),
                None => body,
            }
        } else {
            body
        };
        let meta = StoredMeta {
            logical_key: None,
            size: body.len() as u64,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: fill.max_idle_secs,
            status,
            etag,
            // Persists the origin `Last-Modified` so a later re-check can detect
            // a mutated object and purge the stale entry.
            last_modified: header_value(&headers, "last-modified").map(str::to_owned),
            content_type: None,
            // Replays the full origin response headers on a HIT, so the cached
            // response stays byte- and header-faithful to the MISS.
            headers: stored_headers,
            representation_length,
            expires_at: fill.expires_at,
            versionable: false,
            content_range,
        };
        // For a HEAD the body is empty, so the entry is a zero-byte placeholder
        // that records presence (and the TTL) under the HEAD-method key.
        let cache_store_result = store_complete_body(
            begin_cache_fill(cache, key, meta, fill.normal_validity, permit, false)?,
            &body,
        );
        let reason = miss_reason(status);
        let response = if head_only || bodyless {
            Response::head(
                status,
                reason,
                Some(fill.cache_status.as_str()),
                relayed,
                response_content_length,
            )
        } else {
            Response::buffered(
                status,
                reason,
                Some(fill.cache_status.as_str()),
                relayed,
                body,
            )
        };
        Ok(response
            .with_cache_tier("origin")
            .with_cache_store_result(cache_store_result)
            .with_upstream(upstream_status, upstream_timing, &headers))
    } else {
        let response = if head_only || bodyless {
            Response::head(
                status,
                miss_reason(status),
                Some(fill.cache_status.as_str()),
                relayed,
                response_content_length,
            )
        } else {
            Response::buffered(
                status,
                miss_reason(status),
                Some(fill.cache_status.as_str()),
                relayed,
                body,
            )
        };
        Ok(response.with_cache_tier("origin").with_upstream(
            upstream_status,
            upstream_timing,
            &headers,
        ))
    }
}

/// Streaming MISS fill: origin body → `ovcc-storage` writer → client socket in
/// chunks (constant memory). Writes inline and returns [`BodySource::Streamed`].
///
/// Generic over the client writer so a unit test can drive it with an in-memory
/// duplex stream; production passes the concrete kTLS stream.
async fn serve_miss_streaming<S>(
    stream: &mut S,
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    upstream: &str,
    head: &RequestHead,
    fill: MissFill<'_>,
    permit: Option<FillPermit>,
) -> std::result::Result<Response, BoxError>
where
    S: AsyncWrite + Unpin,
{
    let url = origin_url(upstream, head);
    let fetch_headers = if fill.force_identity {
        identity_encoding_headers(&head.headers)
    } else {
        head.headers.clone()
    };
    let origin = match ovcc_upstream::open_fetch_with_headers(&url, &fetch_headers).await {
        Ok(origin) => origin,
        Err(error) => {
            let failure = upstream_failure_response(&error);
            write_response_with(
                stream,
                failure.status,
                failure.reason,
                failure.headers,
                Some(fill.cache_status.as_str()),
                failure.body,
            )
            .await?;
            let response = Response::streamed(
                failure.status,
                Some(fill.cache_status.as_str()),
                failure.body.len() as u64,
            );
            return Ok(match failure.timeout_stage {
                Some(stage) => response.with_timeout_stage(stage),
                None => response,
            });
        }
    };

    let outcome = fill_from_open_response(
        stream,
        cache,
        key,
        head,
        fill,
        permit,
        origin.status,
        origin.etag,
        origin.headers,
        Some(origin.timing),
        origin.body,
        FillSource::Origin,
    )
    .await?;
    match outcome {
        FillOutcome::Served(response) => Ok(response),
        FillOutcome::RetryOrigin(_) => {
            Err(Error::internal("origin fill requested peer fallback").into())
        }
    }
}

fn log_peer_fill_data_path_failure(
    source: FillSource,
    stage: &'static str,
    bytes_received: u64,
    bytes_sent: u64,
    client_detached: bool,
    error: &dyn std::fmt::Display,
) {
    if source.is_peer() {
        tracing::warn!(
            event = "peer_fill_data_path_failure",
            stage,
            replicate = source.is_shareable_peer(),
            bytes_received,
            bytes_sent,
            client_detached,
            error = %error,
        );
    }
}

struct BodyWriteFailure {
    error: std::io::Error,
    body_bytes: u64,
}

async fn write_body_counted<S>(
    stream: &mut S,
    mut body: &[u8],
) -> std::result::Result<u64, BodyWriteFailure>
where
    S: AsyncWrite + Unpin,
{
    write_buf_counted(stream, &mut body).await
}

async fn write_buf_counted<S, B>(
    stream: &mut S,
    body: &mut B,
) -> std::result::Result<u64, BodyWriteFailure>
where
    S: AsyncWrite + Unpin,
    B: Buf,
{
    let mut writer = ProgressTimeoutWriter::new(stream);
    let mut written = 0usize;
    while body.has_remaining() {
        match writer.write_buf(body).await {
            Ok(0) => {
                return Err(BodyWriteFailure {
                    error: std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "failed to write the complete response body",
                    ),
                    body_bytes: written as u64,
                });
            }
            Ok(bytes) => {
                written += bytes;
                add_downstream_body_bytes(bytes as u64);
            }
            Err(error) => {
                return Err(BodyWriteFailure {
                    error,
                    body_bytes: written as u64,
                });
            }
        }
    }
    Ok(written as u64)
}

async fn write_http_chunk_counted<S>(
    stream: &mut S,
    chunk: &[u8],
) -> std::result::Result<u64, BodyWriteFailure>
where
    S: AsyncWrite + Unpin,
{
    let header = format!("{:x}\r\n", chunk.len());
    timed_write_all(stream, header.as_bytes())
        .await
        .map_err(|error| BodyWriteFailure {
            error,
            body_bytes: 0,
        })?;
    let body_bytes = write_body_counted(stream, chunk).await?;
    timed_write_all(stream, b"\r\n")
        .await
        .map_err(|error| BodyWriteFailure { error, body_bytes })?;
    Ok(body_bytes)
}

#[allow(clippy::too_many_arguments)]
async fn relay_fill_chunk<S>(
    stream: &mut S,
    chunk: &Bytes,
    writer: &mut Option<Box<dyn StoreWrite>>,
    cache_store_result: &mut Option<CacheStoreResult>,
    client_writable: &mut bool,
    retain_fill: bool,
    use_chunked: bool,
    source: FillSource,
    bytes_received: u64,
    bytes_sent: u64,
) -> std::result::Result<u64, BoxError>
where
    S: AsyncWrite + Unpin,
{
    let store_write_error = writer
        .as_mut()
        .and_then(|writer| writer.write_chunk(chunk).err());
    if let Some(error) = store_write_error {
        log_peer_fill_data_path_failure(
            source,
            "staging_write",
            bytes_received,
            bytes_sent,
            !*client_writable,
            &error,
        );
        if let Some(writer) = writer.take() {
            writer.abort();
        }
        *cache_store_result = Some(CacheStoreResult::StorageError);
    }
    if !*client_writable {
        return Ok(0);
    }
    let written = if use_chunked {
        write_http_chunk_counted(stream, chunk).await
    } else {
        write_body_counted(stream, chunk).await
    };
    match written {
        Ok(body_bytes) => Ok(body_bytes),
        Err(failure) => {
            let bytes_sent = bytes_sent.saturating_add(failure.body_bytes);
            log_peer_fill_data_path_failure(
                source,
                "downstream_body_write",
                bytes_received,
                bytes_sent,
                retain_fill,
                &failure.error,
            );
            if retain_fill {
                *client_writable = false;
                Ok(failure.body_bytes)
            } else {
                Err(failure.error.into())
            }
        }
    }
}

/// Failure while collecting a known-length MISS body.
#[doc(hidden)]
#[derive(Debug)]
pub enum BufferedMissError<E> {
    Body(E),
    TooLong,
}

/// Collects a small MISS body. Public for the allocation guard and microbenchmark.
#[doc(hidden)]
pub async fn collect_miss_body<B>(
    body: B,
    len: usize,
) -> std::result::Result<impl Buf, BufferedMissError<B::Error>>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut remaining = len;
    let frames = tokio_stream::StreamExt::map(body.into_data_stream(), move |data| {
        let data = data.map_err(BufferedMissError::Body)?;
        if data.len() > remaining {
            return Err(BufferedMissError::TooLong);
        }
        remaining -= data.len();
        Ok(hyper::body::Frame::data(data))
    });
    Ok(http_body_util::StreamBody::new(frames)
        .collect()
        .await?
        .aggregate())
}

fn miss_body_views(body: &impl Buf) -> Vec<IoSlice<'_>> {
    let size = body.remaining();
    if size == 0 {
        return Vec::new();
    }
    let mut views = vec![IoSlice::new(&[]); 4];
    loop {
        let count = body.chunks_vectored(&mut views);
        if views[..count]
            .iter()
            .map(|chunk| chunk.len())
            .sum::<usize>()
            == size
        {
            views.truncate(count);
            return views;
        }
        views.resize(views.len() * 2, IoSlice::new(&[]));
    }
}

/// Applies client relay plus optional local admission to an opened response.
#[allow(clippy::too_many_arguments)]
async fn fill_from_open_response<S, B, C>(
    stream: &mut S,
    cache: &Cache<C>,
    key: &ovcc_cachekey::CacheKey,
    head: &RequestHead,
    fill: MissFill<'_>,
    mut permit: Option<FillPermit>,
    status: u16,
    etag: Option<String>,
    headers: Vec<(String, String)>,
    timing: Option<UpstreamTiming>,
    mut body: B,
    source: FillSource,
) -> std::result::Result<FillOutcome, BoxError>
where
    S: AsyncWrite + Unpin,
    B: hyper::body::Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::error::Error + 'static,
    C: Storage,
{
    local::retain_source(|| cache.storage().local_path_source(key.as_str()));
    let cache_tier = source.cache_tier();
    let cache_status = fill.cache_status.as_str();
    let store_locally = source.store_locally();
    let mut upstream_timing = timing.unwrap_or_default();
    let relayed = relay_headers(&headers);
    let stored_headers = if fill.snapshot_response {
        cache_snapshot_headers(&relayed)
    } else {
        relayed.clone()
    };
    let client_headers = miss_client_headers(&relayed, status, fill.add_lft_cors);
    let reason = miss_reason(status);
    let admit = should_admit(status, fill.admit_not_found)
        && (!fill.honor_origin_cacheability
            || origin_response_is_cacheable(&headers, SystemTime::now()));
    let store = admit && store_locally;
    // Client-abort tolerance applies only while populating a cache writer.
    // Error responses and bypass arms retain abort-on-disconnect.
    let retain_fill = store;
    let content_range = if status == 206 {
        header_value(&headers, "content-range").map(str::to_owned)
    } else {
        None
    };
    let content_length = parse_content_length(&headers);
    let use_chunked = status_has_body(status) && content_length.is_none();
    if !status_has_body(status) {
        let body_started = Instant::now();
        let header_refs: Vec<(&str, &str)> = client_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain(std::iter::once(("X-Cache-Tier-Hit", cache_tier)))
            .collect();
        write_streaming_head(
            stream,
            status,
            reason,
            Some(cache_status),
            &header_refs,
            content_length,
            false,
            false,
        )
        .await?;
        timed_flush(stream).await?;
        upstream_timing.response = Some(body_started.elapsed().as_secs_f64());
        if timing.is_some() {
            record_upstream_fetch_timing(&upstream_timing);
        }
        let response =
            Response::streamed(status, Some(cache_status), 0).with_cache_tier(cache_tier);
        let response = local::response_headers(response, &client_headers);
        return Ok(FillOutcome::Served(annotate_fill_upstream(
            response,
            status,
            timing.map(|_| upstream_timing),
            &headers,
        )));
    }
    let shareable_origin = source == FillSource::Origin
        && origin_fill_is_shareable(&fill, &headers, &stored_headers, etag.is_some());

    // A small, known-length local store commits BEFORE the response reaches the
    // client, keeping an immediate re-request a deterministic HIT. Relay-only,
    // larger, and unknown-length bodies stream below.
    if store
        && !source.is_shareable_peer()
        && let Some(len) = content_length
        && len <= MISS_BUFFER_MAX_BYTES
    {
        let body_started = Instant::now();
        let mut body = match collect_miss_body(body, len as usize).await {
            Ok(body) => body,
            Err(_) if source.is_peer() => {
                return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyReadFailed));
            }
            Err(error) => {
                let timed_out = matches!(&error, BufferedMissError::Body(error) if origin_body_timed_out(error));
                let header_refs: Vec<(&str, &str)> = client_headers
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .chain(std::iter::once(("X-Cache-Tier-Hit", cache_tier)))
                    .collect();
                write_streaming_head(
                    stream,
                    status,
                    reason,
                    Some(cache_status),
                    &header_refs,
                    content_length,
                    false,
                    false,
                )
                .await?;
                timed_flush(stream).await?;
                let response =
                    Response::streamed(status, Some(cache_status), 0).with_cache_tier(cache_tier);
                return Ok(FillOutcome::Served(if timed_out {
                    response.with_timeout_stage(TimeoutStage::OriginResponseBody)
                } else {
                    response
                }));
            }
        };
        let body_len = body.remaining() as u64;
        if source.is_peer() && body_len != len {
            return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyIncomplete));
        }
        let meta = StoredMeta {
            logical_key: None,
            size: body_len,
            revision: 0,
            provenance: source.cache_provenance(),
            max_idle_secs: source.max_idle_secs(fill.max_idle_secs),
            status,
            etag: etag.clone(),
            last_modified: header_value(&headers, "last-modified").map(str::to_owned),
            content_type: None,
            headers: stored_headers.clone(),
            representation_length: content_length,
            expires_at: fill.expires_at,
            versionable: false,
            content_range: content_range.clone(),
        };
        let mut views = miss_body_views(&body);
        let cache_store_result = store_complete_chunks(
            begin_cache_fill(cache, key, meta, fill.normal_validity, permit.take(), false)?,
            &mut views,
        );
        drop(views);
        upstream_timing.response = Some(body_started.elapsed().as_secs_f64());
        if timing.is_some() {
            record_upstream_fetch_timing(&upstream_timing);
        }
        let header_refs: Vec<(&str, &str)> = client_headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .chain(std::iter::once(("X-Cache-Tier-Hit", cache_tier)))
            .collect();
        // The admitted status carries a body whose length the head declares,
        // so a successful write completes the framing.
        let keep_alive = downstream_reuse_allowed() && status_has_body(status);
        write_response_buf_with(
            stream,
            status,
            reason,
            &header_refs,
            Some(cache_status),
            &mut body,
            keep_alive,
        )
        .await?;
        let response = Response::streamed(status, Some(cache_status), body_len)
            .with_cache_tier(cache_tier)
            .with_cache_store_result(cache_store_result)
            .with_framing_complete(keep_alive);
        let response = local::response_headers(response, &client_headers);
        return Ok(FillOutcome::Served(annotate_fill_upstream(
            response,
            status,
            timing.map(|_| upstream_timing),
            &headers,
        )));
    }

    // A peer response is not committed to the client until one body frame is
    // available. This leaves transport failure eligible for the origin arm
    // without buffering the remaining object.
    let first_peer_chunk = if source.is_peer() {
        loop {
            match body.frame().await {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(chunk) if chunk.is_empty() => continue,
                    Ok(chunk)
                        if content_length.is_some_and(|expected| chunk.len() as u64 > expected) =>
                    {
                        return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyIncomplete));
                    }
                    Ok(chunk) => break Some(chunk),
                    Err(_) => {
                        tracing::warn!(
                            event = "peer_fill_data_path_failure",
                            stage = "first_body_non_data_frame",
                            replicate = source.is_shareable_peer(),
                            bytes_received = 0_u64,
                            bytes_sent = 0_u64,
                            client_detached = false,
                        );
                        return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyReadFailed));
                    }
                },
                Some(Err(error)) => {
                    log_peer_fill_data_path_failure(source, "first_body_read", 0, 0, false, &error);
                    return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyReadFailed));
                }
                None if content_length == Some(0) => break None,
                None => {
                    return Ok(FillOutcome::RetryOrigin(PeerFallbackReason::BodyIncomplete));
                }
            }
        }
    } else {
        None
    };

    // An unknown-length response selected for local storage still gets a
    // staging writer. Filesystem I/O decides whether it stores.
    let first_peer_bytes = first_peer_chunk
        .as_ref()
        .map_or(0, |chunk| chunk.len() as u64);
    let (mut writer, mut cache_store_result) = if store {
        let meta = StoredMeta {
            logical_key: None,
            size: content_length.unwrap_or(0),
            revision: 0,
            provenance: source.cache_provenance(),
            max_idle_secs: source.max_idle_secs(fill.max_idle_secs),
            status,
            etag: etag.clone(),
            last_modified: header_value(&headers, "last-modified").map(str::to_owned),
            content_type: None,
            headers: stored_headers,
            representation_length: content_length,
            expires_at: fill.expires_at,
            versionable: false,
            content_range,
        };
        match begin_cache_fill(
            cache,
            key,
            meta,
            fill.normal_validity,
            permit.take(),
            source.is_shareable_peer() || shareable_origin,
        )? {
            CacheFillStart::Writer(writer) => (Some(writer), None),
            CacheFillStart::Skipped(result) => (None, Some(result)),
            CacheFillStart::StorageFailure(error) => {
                log_peer_fill_data_path_failure(
                    source,
                    "staging_begin",
                    first_peer_bytes,
                    0,
                    false,
                    &error,
                );
                (None, Some(CacheStoreResult::StorageError))
            }
        }
    } else {
        (None, None)
    };

    let header_refs: Vec<(&str, &str)> = client_headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .chain(std::iter::once(("X-Cache-Tier-Hit", cache_tier)))
        .collect();
    // Keep-alive needs framing the client can delimit, which takes a
    // body-bearing status and chunked coding only toward HTTP/1.1 clients.
    let keep_alive = downstream_reuse_allowed()
        && status_has_body(status)
        && (!use_chunked || head.http_version.eq_ignore_ascii_case("HTTP/1.1"));
    let mut client_writable = match write_streaming_head(
        stream,
        status,
        reason,
        Some(cache_status),
        &header_refs,
        content_length,
        use_chunked,
        keep_alive,
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            log_peer_fill_data_path_failure(
                source,
                "downstream_response_head",
                0,
                0,
                retain_fill,
                &error,
            );
            if retain_fill {
                false
            } else {
                return Err(error.into());
            }
        }
    };

    let body_started = Instant::now();
    let mut total = 0u64;
    let mut client_bytes = 0u64;
    let mut body_ok = true;
    let mut origin_body_timeout = false;

    if let Some(chunk) = first_peer_chunk {
        total += chunk.len() as u64;
        client_bytes += relay_fill_chunk(
            stream,
            &chunk,
            &mut writer,
            &mut cache_store_result,
            &mut client_writable,
            retain_fill,
            use_chunked,
            source,
            total,
            client_bytes,
        )
        .await?;
    }

    while let Some(frame) = body.frame().await {
        let chunk = match frame {
            Ok(frame) => match frame.into_data() {
                Ok(data) => data,
                Err(_) => {
                    if source.is_peer() {
                        tracing::warn!(
                            event = "peer_fill_data_path_failure",
                            stage = "body_non_data_frame",
                            replicate = source.is_shareable_peer(),
                            bytes_received = total,
                            bytes_sent = client_bytes,
                            client_detached = !client_writable,
                        );
                    }
                    body_ok = false;
                    break;
                }
            },
            Err(error) => {
                origin_body_timeout = origin_body_timed_out(&error);
                log_peer_fill_data_path_failure(
                    source,
                    "body_read",
                    total,
                    client_bytes,
                    !client_writable,
                    &error,
                );
                body_ok = false;
                break;
            }
        };
        if chunk.is_empty() {
            continue;
        }
        total += chunk.len() as u64;
        client_bytes += relay_fill_chunk(
            stream,
            &chunk,
            &mut writer,
            &mut cache_store_result,
            &mut client_writable,
            retain_fill,
            use_chunked,
            source,
            total,
            client_bytes,
        )
        .await?;
    }

    let complete = body_ok && content_length.is_none_or(|expected| total == expected);
    if client_writable
        && use_chunked
        && complete
        && let Err(error) = timed_write_all(stream, b"0\r\n\r\n").await
    {
        log_peer_fill_data_path_failure(
            source,
            "downstream_chunk_terminator",
            total,
            client_bytes,
            retain_fill,
            &error,
        );
        if retain_fill {
            client_writable = false;
        } else {
            return Err(error.into());
        }
    }
    if client_writable && let Err(error) = timed_flush(stream).await {
        log_peer_fill_data_path_failure(
            source,
            "downstream_final_flush",
            total,
            client_bytes,
            retain_fill,
            &error,
        );
        if retain_fill {
            client_writable = false;
        } else {
            return Err(error.into());
        }
    }

    upstream_timing.response = Some(body_started.elapsed().as_secs_f64());
    if timing.is_some() {
        record_upstream_fetch_timing(&upstream_timing);
    }

    if source.is_peer() && !complete {
        tracing::warn!(
            event = "peer_fill_data_path_failure",
            stage = "source_body_incomplete",
            replicate = source.is_shareable_peer(),
            bytes_received = total,
            bytes_sent = client_bytes,
            expected_bytes = content_length,
            client_detached = !client_writable,
        );
    }
    if let Some(writer) = writer.take() {
        let result = if complete {
            match writer.commit() {
                Ok(lease) => {
                    local::retain_committed(lease);
                    CacheStoreResult::Stored
                }
                Err(error) => {
                    // A racing purge revoking the fill is the correct outcome
                    // rather than a data-path failure worth alarming on.
                    if !error.is_fill_revoked() {
                        log_peer_fill_data_path_failure(
                            source,
                            "staging_commit",
                            total,
                            client_bytes,
                            !client_writable,
                            &error,
                        );
                    }
                    CacheStoreResult::StorageError
                }
            }
        } else {
            writer.abort();
            CacheStoreResult::Incomplete
        };
        cache_store_result = Some(result);
    } else if admit && !store_locally {
        cache_store_result = Some(if complete {
            CacheStoreResult::NotStored
        } else {
            CacheStoreResult::Incomplete
        });
    }

    // Every relayed byte, the terminator, and the flush reached the client
    // only when it stayed writable through a complete body.
    let mut response = Response::streamed(status, Some(cache_status), client_bytes)
        .with_cache_tier(cache_tier)
        .with_framing_complete(keep_alive && complete && client_writable);
    response = local::response_headers(response, &client_headers);
    if let Some(result) = cache_store_result {
        response = response.with_cache_store_result(result);
    }
    if origin_body_timeout {
        response = response.with_timeout_stage(TimeoutStage::OriginResponseBody);
    }
    Ok(FillOutcome::Served(annotate_fill_upstream(
        response,
        status,
        timing.map(|_| upstream_timing),
        &headers,
    )))
}

fn annotate_fill_upstream(
    mut response: Response,
    status: u16,
    timing: Option<UpstreamTiming>,
    headers: &[(String, String)],
) -> Response {
    match timing {
        Some(timing) => response.with_upstream(status, timing, headers),
        None => {
            response.upstream_etag = header_value(headers, "etag").map(str::to_owned);
            response
        }
    }
}

/// On the bursty path, caches a list `GET` or a `HEAD` under a short-TTL,
/// tenant-adaptive key so an immediate repeat is a HIT, even though the request
/// carries no validator (the normal path would bypass). The key is
/// `generation|adaptive_id|host|method|request_uri` (full, unfiltered
/// `request_uri`), where `adaptive_id` isolates tenants
/// ([`ovcc_bursty::adaptive_id`]) and `method` keeps GET and HEAD on separate
/// entries.
///
/// The caller runs the shared pre-cache access gate [`evaluate_access`] before
/// routing here, so expiry validation, cache-bypass policy, and the presigned
/// authorization witness are already established and a fresh HIT is served
/// directly. `forced_miss` carries that decision: a credential that may neither
/// read nor share an existing entry skips the lookup and fills from the origin,
/// which re-authorizes it.
///
/// A HEAD caches only the response head (zero-byte body); a list GET caches the
/// body and serves it via `sendfile` on a HIT. `ttl_secs` bounds freshness, so a
/// hit past its deadline is treated as a MISS and refilled. A MISS or expired
/// hit reuses [`serve_miss`] to fetch, admit, and relay.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "ovcc.serve_bursty", skip_all)]
async fn serve_bursty(
    stream: &mut ClientStream,
    cache: &Arc<Cache<DiskStorage>>,
    upstream: &str,
    head: &RequestHead,
    host: &str,
    generation: i64,
    method: &str,
    ttl_secs: u64,
    max_idle_secs: Option<u64>,
    forced_miss: bool,
) -> std::result::Result<Response, BoxError> {
    let adaptive = ovcc_bursty::adaptive_id(
        header_value(&head.headers, "nvcf-function-id"),
        header_value(&head.headers, "nvcf-function-version"),
        header_value(&head.headers, "nvcf-nca-id"),
        header_value(&head.headers, "authorization"),
    );
    let request_uri = request_uri(head);
    let generation_str = generation.to_string();
    let key = BurstyKeyParts {
        generation: &generation_str,
        adaptive_id: &adaptive,
        host,
        method,
        request_uri: &request_uri,
    }
    .render();

    // A HEAD caches/serves only the response head (no body).
    let head_only = method == "HEAD";
    // Freshness deadline for a new fill (`None` when TTL is disabled).
    let fill_expires_at = ovcc_bursty::expires_at(ttl_secs, now_unix());
    let fill = MissFill {
        snapshot_response: true,
        head_only,
        expires_at: fill_expires_at,
        admit_not_found: true,
        honor_origin_cacheability: false,
        add_lft_cors: false,
        cache_status: CacheStatus::Miss,
        normal_validity: false,
        max_idle_secs,
        rewrite: &[],
        force_identity: false,
        force_buffered: false,
        recheck_auth: false,
        recheck_mutation: false,
        share_existing: !forced_miss,
        key_authorizes_followers: false,
        recheck_upstream: upstream,
    };

    // A forced MISS deliberately skips both the fresh lookup and stale
    // revalidation, so an expired or unauthorized credential can neither be
    // served an existing snapshot nor join another request's fill.
    let lookup = if forced_miss {
        Lookup::Miss
    } else {
        cache_lookup_blocking(cache, key.as_str())
            .instrument(tracing::info_span!("ovcc.cache.lookup"))
            .await?
    };
    let served = match lookup {
        Lookup::Hit(lease) if !ovcc_bursty::is_expired(lease.meta().expires_at, now_unix()) => {
            // The router established this request's access decision before
            // routing here, so a fresh HIT serves directly. A HEAD entry is
            // zero-byte, so the driver sends only the response head.
            serve_bursty_hit(lease, head)
        }
        Lookup::Hit(lease)
            if has_revalidation_validator(lease.meta()) && fill_expires_at.is_some() =>
        {
            drop(lease);
            revalidate_entry(
                stream,
                cache,
                &key,
                upstream,
                head,
                fill,
                is_azure_blob_request(head),
                RevalidationExpiry::Explicit(fill_expires_at.unwrap()),
            )
            .await?
        }
        Lookup::Hit(lease) => {
            drop(lease);
            serve_miss(stream, cache, &key, upstream, head, fill).await?
        }
        Lookup::SoftMiss(_) | Lookup::Miss => {
            serve_miss(stream, cache, &key, upstream, head, fill).await?
        }
    };
    Ok(served)
}

/// On the BYPASS path, fetches fresh from the origin and relays it without
/// touching the cache (`X-Cache-Status: BYPASS`). Used for `Cache-Control: no-cache` and for
/// a versioning backend served without an `If-Match` validator.
///
/// Streams the origin body straight through to the client (constant memory,
/// never cached), so an arbitrarily large object served on a cache-disabled
/// path — a `Cache-Control: no-cache` GET or a `requiresVersioning=true` GET
/// without `If-Match` — flows without buffering the whole body or touching the
/// cache disk. Generic over the client writer for unit testing.
#[tracing::instrument(name = "ovcc.serve_bypass", skip_all)]
async fn serve_bypass<S>(
    stream: &mut S,
    upstream: &str,
    head: &RequestHead,
) -> std::result::Result<Response, BoxError>
where
    S: AsyncWrite + Unpin,
{
    if head.method.eq_ignore_ascii_case("HEAD") {
        return serve_head(upstream, head).await;
    }
    let url = origin_url(upstream, head);
    // An upstream transport error (DNS/connect failure, e.g. an unreachable
    // origin) becomes a `502` rather than a dropped connection (drop-in
    // fidelity, since every serve path answers with a status).
    let mut origin = match ovcc_upstream::open_fetch_with_headers(&url, &head.headers).await {
        Ok(origin) => origin,
        Err(error) => {
            let failure = upstream_failure_response(&error);
            write_response_with(
                stream,
                failure.status,
                failure.reason,
                failure.headers,
                Some("BYPASS"),
                failure.body,
            )
            .await?;
            let response =
                Response::streamed(failure.status, Some("BYPASS"), failure.body.len() as u64);
            return Ok(match failure.timeout_stage {
                Some(stage) => response.with_timeout_stage(stage),
                None => response,
            });
        }
    };

    let upstream_status = origin.status;
    let mut upstream_timing = origin.timing;
    let relayed = relay_headers(&origin.headers);
    let reason = if origin.status == 200 { "OK" } else { "" };
    // Relays a known `Content-Length`, otherwise re-frames it as chunked for the client.
    let content_length = parse_content_length(&origin.headers);
    let use_chunked = status_has_body(origin.status) && content_length.is_none();

    let header_refs: Vec<(&str, &str)> = relayed
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    write_streaming_head(
        stream,
        origin.status,
        reason,
        Some("BYPASS"),
        &header_refs,
        content_length,
        use_chunked,
        false,
    )
    .await?;

    if !status_has_body(origin.status) {
        let body_started = Instant::now();
        timed_flush(stream).await?;
        upstream_timing.response = Some(body_started.elapsed().as_secs_f64());
        record_upstream_fetch_timing(&upstream_timing);
        return Ok(
            Response::streamed(origin.status, Some("BYPASS"), 0).with_upstream(
                upstream_status,
                upstream_timing,
                &origin.headers,
            ),
        );
    }

    let body_started = Instant::now();
    let mut total = 0u64;
    let mut origin_body_failed = false;
    let mut origin_body_timeout = false;
    while let Some(frame) = origin.body.frame().await {
        let chunk = match frame {
            Ok(frame) => match frame.into_data() {
                Ok(data) => data,
                Err(_) => break,
            },
            Err(error) => {
                origin_body_failed = true;
                origin_body_timeout = origin_body_timed_out(&error);
                break;
            }
        };
        if chunk.is_empty() {
            continue;
        }
        if use_chunked {
            write_http_chunk(stream, &chunk).await?;
        } else {
            write_body_counted(stream, &chunk)
                .await
                .map_err(|failure| failure.error)?;
        }
        total += chunk.len() as u64;
    }
    // A failed body omits the terminator so its truncation stays visible.
    if use_chunked && !origin_body_failed {
        timed_write_all(stream, b"0\r\n\r\n").await?;
    }
    timed_flush(stream).await?;

    upstream_timing.response = Some(body_started.elapsed().as_secs_f64());
    record_upstream_fetch_timing(&upstream_timing);

    let response = Response::streamed(origin.status, Some("BYPASS"), total).with_upstream(
        upstream_status,
        upstream_timing,
        &origin.headers,
    );
    Ok(if origin_body_timeout {
        response.with_timeout_stage(TimeoutStage::OriginResponseBody)
    } else {
        response
    })
}

/// On the HEAD path, proxies a `HEAD` to the origin and relays its status and
/// representation metadata with `X-Cache-Status: BYPASS` (HEAD responses carry
/// no body and are never cached).
#[tracing::instrument(name = "ovcc.serve_head", skip_all)]
async fn serve_head(upstream: &str, head: &RequestHead) -> std::result::Result<Response, BoxError> {
    let url = origin_url(upstream, head);
    // An unreachable origin becomes a `502`, not a dropped connection.
    let origin = match ovcc_upstream::fetch_head(&url, &head.headers).await {
        Ok(origin) => origin,
        Err(error) => {
            let failure = upstream_failure_response(&error);
            let response = Response::head(
                failure.status,
                failure.reason,
                Some("BYPASS"),
                owned_upstream_failure_headers(failure),
                None,
            );
            return Ok(match failure.timeout_stage {
                Some(stage) => response.with_timeout_stage(stage),
                None => response,
            });
        }
    };
    let upstream_status = origin.status;
    let upstream_timing = origin.timing;
    let content_length = parse_content_length(&origin.headers);
    // Relays the origin response headers (ETag, Content-Type, and any custom
    // backend headers) so a HEAD reflects the live object metadata.
    let relayed = relay_headers(&origin.headers);
    let reason = if origin.status == 200 { "OK" } else { "" };
    Ok(Response::head(
        origin.status,
        reason,
        Some("BYPASS"),
        relayed,
        content_length,
    )
    .with_upstream(upstream_status, upstream_timing, &origin.headers))
}

// --- Nucleus LFT -----------------------------------------------------------

/// Serves a request that arrived on the dedicated Nucleus listener.
///
/// The listener selects the backend independently of `Host`; the origin comes
/// from configuration or the request host with the listener scheme. The
/// [`ovcc_nucleus`] policy classifies LFT and generic requests and defines
/// headers, CORS, auth bypass, and cache keys.
///
/// - Non-GET/HEAD: a transparent proxy (never cached), with the location's
///   header transform applied so the generic proxy still strips the tenant JWT.
/// - LFT GET/HEAD: keyed by tenant + content UID + range; all client headers
///   (incl. the tenant JWT) are forwarded; the permissive CORS triplet is added
///   to a successful response; an `Authorization` header does NOT bypass.
/// - Generic GET/HEAD: `Authorization` bypasses the cache; otherwise keyed
///   additionally by host + request URI, with the tenant JWT stripped before the
///   origin and no CORS added.
///
/// Cache keys are namespaced (`nucleus|<rendered>`) and stored in the Nucleus
/// backend's selected cache root.
async fn serve_nucleus(
    stream: &mut ClientStream,
    head: &RequestHead,
    host: &str,
    body_prefix: Vec<u8>,
    ctx: &ConnCtx,
    flags: &mut RequestFlags,
    scheme: &str,
) -> std::result::Result<Response, BoxError> {
    let runtime = ctx
        .backends
        .iter()
        .find(|runtime| runtime.config.name == "nucleus")
        .ok_or_else(|| std::io::Error::other("Nucleus listener has no configured backend"))?;
    let upstream = nucleus_upstream_url(&runtime.config, host, scheme);
    let cache = &runtime.store.cache;
    let policy_head = upstream_policy_head(head, &runtime.config.route, None);
    let head = policy_head.as_ref().unwrap_or(head);
    flags.backend = "nucleus".to_string();
    flags.upstream = upstream.clone();

    let location = ovcc_nucleus::classify(&head.path);

    // Any non-GET/HEAD method is a transparent proxy (never cached), with the
    // location's request-header transform applied (generic strips the JWT).
    if !head.method.eq_ignore_ascii_case("GET") && !head.method.eq_ignore_ascii_case("HEAD") {
        let origin_head = nucleus_transformed_head(head, location);
        return serve_transparent(stream, &upstream, &origin_head, body_prefix, None).await;
    }

    let method = canonical_method(&head.method);
    let generation = ctx.generation.load(Ordering::SeqCst).to_string();

    match location {
        ovcc_nucleus::Location::Lft => {
            // LFT forwards every client header (the transform is identity), so
            // the request head is used as-is for the origin fetch.
            let key = CacheKey::from_raw(format!(
                "nucleus|{}",
                ovcc_nucleus::lft_cache_key(&generation, method, &head.headers).into_string()
            ));
            let mut response = nucleus_hit_or_miss(
                stream,
                cache,
                &key,
                &upstream,
                head,
                location,
                runtime.config.cache_policy.backend_max_idle_secs,
            )
            .await?;
            // CORS applies to 2xx/3xx responses on both a MISS and a HIT.
            nucleus_add_cors(&mut response, location);
            Ok(response)
        }
        ovcc_nucleus::Location::Generic => {
            // An authenticated generic request bypasses the cache (multi-tenant
            // safety); the tenant JWT is still stripped before the origin.
            if ovcc_nucleus::auth_bypasses_cache(location, &head.headers) {
                flags.disable_cache = true;
                let origin_head = nucleus_transformed_head(head, location);
                return serve_bypass(stream, &upstream, &origin_head).await;
            }
            let key = CacheKey::from_raw(format!(
                "nucleus|{}",
                ovcc_nucleus::generic_cache_key(
                    &generation,
                    method,
                    host,
                    &request_uri(head),
                    &head.headers,
                )
                .into_string()
            ));
            // The generic origin fetch strips the tenant JWT; the cache key was
            // already computed from the original headers above (identical, since
            // the key does not depend on the token).
            let origin_head = nucleus_transformed_head(head, location);
            nucleus_hit_or_miss(
                stream,
                cache,
                &key,
                &upstream,
                &origin_head,
                location,
                runtime.config.cache_policy.backend_max_idle_secs,
            )
            .await
        }
    }
}

/// Resolves the Nucleus origin from its configured route or the request host,
/// preserving the listener transport scheme.
fn nucleus_upstream_url(backend: &Backend, host: &str, scheme: &str) -> String {
    backend.resolve_nucleus_upstream(host, scheme)
}

/// A shallow clone of `head` whose headers have been run through the Nucleus
/// request-header transform for `location` (generic strips `Omni-Tenant-Token`;
/// LFT is identity). The original `head` is never mutated, so telemetry and the
/// access log still observe the request as received.
fn nucleus_transformed_head(head: &RequestHead, location: ovcc_nucleus::Location) -> RequestHead {
    let mut transformed = head.clone();
    transformed.headers = ovcc_nucleus::transform_request_headers(location, &head.headers);
    transformed
}

/// Returns the head sent to the origin, or `None` when the client head is sent
/// unchanged. `upstream_host` replaces every client `Host` line, and a disabled
/// keepalive forces `Connection: close`; `head.host` is never changed.
fn upstream_policy_head(
    head: &RequestHead,
    route: &ovcc_config::BackendRoute,
    upstream_host: Option<&str>,
) -> Option<RequestHead> {
    if route.enable_upstream_keepalive && upstream_host.is_none() {
        return None;
    }
    let mut head = head.clone();
    if let Some(upstream_host) = upstream_host {
        head.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("host"));
        head.headers
            .push(("Host".to_owned(), upstream_host.to_owned()));
    }
    if !route.enable_upstream_keepalive {
        head.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("connection"));
        head.headers
            .push(("Connection".to_owned(), "close".to_owned()));
    }
    Some(head)
}

fn cache_entry_is_fresh(meta: &StoredMeta, now: i64) -> bool {
    meta.expires_at.is_some_and(|expires_at| now < expires_at)
}

fn has_revalidation_validator(meta: &StoredMeta) -> bool {
    meta.etag.is_some() || meta.last_modified.is_some()
}

fn revalidation_headers(
    request_headers: &[(String, String)],
    meta: &StoredMeta,
    force_identity: bool,
) -> Vec<(String, String)> {
    let request_headers = if force_identity {
        identity_encoding_headers(request_headers)
    } else {
        request_headers.to_vec()
    };
    let mut headers: Vec<_> = request_headers
        .iter()
        .filter(|(name, _)| {
            !name.eq_ignore_ascii_case("if-none-match")
                && !name.eq_ignore_ascii_case("if-modified-since")
        })
        .cloned()
        .collect();
    if let Some(etag) = &meta.etag {
        headers.push(("If-None-Match".to_owned(), etag.clone()));
    }
    if let Some(last_modified) = &meta.last_modified {
        headers.push(("If-Modified-Since".to_owned(), last_modified.clone()));
    }
    headers
}

fn serve_revalidated(
    lease: Lease,
    head: &RequestHead,
    is_azure: bool,
    snapshot_response: bool,
) -> Response {
    let mut response = if snapshot_response {
        serve_bursty_hit(lease, head)
    } else {
        serve_hit_conditional(lease, head, is_azure)
    };
    response.head.cache_status = Some(ovcc_core::CacheStatus::Revalidated.as_str());
    response
}

#[allow(clippy::too_many_arguments)]
async fn revalidate_entry(
    stream: &mut ClientStream,
    cache: &Arc<Cache<DiskStorage>>,
    key: &CacheKey,
    upstream: &str,
    head: &RequestHead,
    fill: MissFill<'_>,
    is_azure: bool,
    expiry: RevalidationExpiry,
) -> std::result::Result<Response, BoxError> {
    let fill = MissFill {
        cache_status: CacheStatus::Expired,
        ..fill
    };
    let permit = match cache.acquire_refill(key.as_str()).await? {
        FillDecision::Owner(permit) => permit,
        FillDecision::Shared(lease) => {
            return serve_shared_fill(stream, cache, key, upstream, head, fill, lease).await;
        }
        FillDecision::Bypass => return serve_bypass(stream, upstream, head).await,
    };

    // A waiter must recheck after acquiring the single-flight permit. The
    // preceding owner may already have refreshed or replaced the generation.
    let current = cache_lookup_blocking(cache, key.as_str()).await?;
    let stale_meta = match current {
        Lookup::Hit(lease) if cache_entry_is_fresh(lease.meta(), now_unix()) => {
            drop(permit);
            return serve_shared_fill(stream, cache, key, upstream, head, fill, lease).await;
        }
        Lookup::Hit(lease) if has_revalidation_validator(lease.meta()) => {
            let meta = lease.meta().clone();
            drop(lease);
            meta
        }
        Lookup::Hit(lease) => {
            drop(lease);
            drop(permit);
            return serve_miss(stream, cache, key, upstream, head, fill).await;
        }
        Lookup::SoftMiss(_) | Lookup::Miss => {
            drop(permit);
            return serve_miss(stream, cache, key, upstream, head, fill).await;
        }
    };

    let url = origin_url(upstream, head);
    let headers = revalidation_headers(&head.headers, &stale_meta, fill.force_identity);
    let cached_etag = stale_meta.etag.clone();
    if fill.head_only {
        let origin = match ovcc_upstream::fetch_head(&url, &headers).await {
            Ok(origin) => origin,
            Err(error) => {
                return Ok(abandon_fill_after_origin_error(
                    Some(permit),
                    &error,
                    fill.cache_status.as_str(),
                    fill.head_only,
                )
                .with_cached_etag(cached_etag));
            }
        };
        if origin.status == 304 {
            return match cache_refresh_blocking(cache, key.as_str(), stale_meta, permit, expiry)
                .await?
            {
                Some(lease) => {
                    Ok(
                        serve_revalidated(lease, head, is_azure, fill.snapshot_response)
                            .with_upstream(304, origin.timing, &origin.headers),
                    )
                }
                None => serve_miss(stream, cache, key, upstream, head, fill).await,
            }
            .map(|response| response.with_cached_etag(cached_etag));
        }
        return finish_buffered_origin_response(
            cache.as_ref(),
            key,
            head,
            fill,
            Some(permit),
            origin.status,
            origin.etag,
            origin.headers,
            origin.timing,
            origin.body,
        )
        .map(|response| response.with_cached_etag(cached_etag));
    }
    let origin = match ovcc_upstream::open_fetch_with_headers(&url, &headers).await {
        Ok(origin) => origin,
        Err(error) => {
            return Ok(abandon_fill_after_origin_error(
                Some(permit),
                &error,
                fill.cache_status.as_str(),
                fill.head_only,
            )
            .with_cached_etag(cached_etag));
        }
    };

    if origin.status == 304 {
        let upstream_timing = origin.timing;
        let upstream_headers = origin.headers;
        // A 304 has no representation body. Dropping Hyper's empty body is
        // sufficient; freshness publication remains guarded by exact metadata.
        return match cache_refresh_blocking(cache, key.as_str(), stale_meta, permit, expiry).await?
        {
            Some(lease) => Ok(
                serve_revalidated(lease, head, is_azure, fill.snapshot_response).with_upstream(
                    304,
                    upstream_timing,
                    &upstream_headers,
                ),
            ),
            None => serve_miss(stream, cache, key, upstream, head, fill).await,
        }.map(|response| response.with_cached_etag(cached_etag));
    }

    if !fill.rewrite.is_empty() || fill.force_buffered {
        let ovcc_upstream::OpenOriginResponse {
            status,
            etag,
            headers,
            timing,
            body,
        } = origin;
        let mut permit = Some(permit);
        return match collect_buffered_origin_body(
            stream,
            &mut permit,
            fill.cache_status,
            status,
            &headers,
            timing,
            body,
        )
        .await?
        {
            BufferedOriginBody::Complete(body, timing) => finish_buffered_origin_response(
                cache.as_ref(),
                key,
                head,
                fill,
                permit,
                status,
                etag,
                headers,
                timing,
                body,
            ),
            BufferedOriginBody::Failed(response) => Ok(*response),
        }
        .map(|response| response.with_cached_etag(cached_etag));
    }

    let outcome = fill_from_open_response(
        stream,
        cache.as_ref(),
        key,
        head,
        fill,
        Some(permit),
        origin.status,
        origin.etag,
        origin.headers,
        Some(origin.timing),
        origin.body,
        FillSource::Origin,
    )
    .await?;
    match outcome {
        FillOutcome::Served(response) => Ok(response.with_cached_etag(cached_etag)),
        FillOutcome::RetryOrigin(_) => {
            Err(Error::internal("revalidation requested peer fallback").into())
        }
    }
}

/// Nucleus cache lookup then fill: a HIT is served via `sendfile`; a MISS
/// fetches from the origin and admits under `key`. Reuses [`serve_miss`] for the
/// fill so a HEAD admits a zero-byte entry and an unreachable origin becomes a
/// `502` (never a dropped connection). No bursty TTL, rewrite, or identity
/// forcing applies to Nucleus.
/// Fill policy for an ordinary object `GET`. A versioned backend skips mutation
/// rechecks because `If-Match` pins the content in the cache key.
fn object_miss_fill<'a>(
    head: &RequestHead,
    requires_versioning: bool,
    forced_miss: bool,
    is_registry: bool,
    rewrite: &'a [ovcc_rewrite::RewriteRule],
    max_idle_secs: Option<u64>,
    recheck_upstream: &'a str,
) -> MissFill<'a> {
    MissFill {
        snapshot_response: false,
        head_only: false,
        expires_at: None,
        admit_not_found: false,
        honor_origin_cacheability: false,
        add_lft_cors: false,
        cache_status: CacheStatus::Miss,
        normal_validity: true,
        max_idle_secs,
        rewrite,
        force_identity: is_registry,
        force_buffered: false,
        recheck_auth: needs_auth_recheck(head),
        recheck_mutation: !requires_versioning,
        share_existing: !forced_miss,
        key_authorizes_followers: false,
        recheck_upstream,
    }
}

fn nucleus_miss_fill(
    head_only: bool,
    location: ovcc_nucleus::Location,
    max_idle_secs: Option<u64>,
    upstream: &str,
) -> MissFill<'_> {
    MissFill {
        snapshot_response: false,
        head_only,
        expires_at: None,
        admit_not_found: false,
        honor_origin_cacheability: true,
        add_lft_cors: matches!(location, ovcc_nucleus::Location::Lft),
        cache_status: CacheStatus::Miss,
        normal_validity: true,
        max_idle_secs,
        rewrite: &[],
        force_identity: false,
        force_buffered: false,
        recheck_auth: false,
        recheck_mutation: false,
        share_existing: true,
        key_authorizes_followers: matches!(location, ovcc_nucleus::Location::Lft),
        recheck_upstream: upstream,
    }
}

#[allow(clippy::too_many_arguments)]
async fn nucleus_hit_or_miss(
    stream: &mut ClientStream,
    cache: &Arc<Cache<DiskStorage>>,
    key: &CacheKey,
    upstream: &str,
    head: &RequestHead,
    location: ovcc_nucleus::Location,
    max_idle_secs: Option<u64>,
) -> std::result::Result<Response, BoxError> {
    let head_only = head.method.eq_ignore_ascii_case("HEAD");
    let fill = nucleus_miss_fill(head_only, location, max_idle_secs, upstream);
    let lookup = {
        cache_lookup_blocking(cache, key.as_str())
            .instrument(tracing::info_span!("ovcc.cache.lookup"))
            .await?
    };
    if let Lookup::Hit(lease) = lookup {
        if cache_entry_is_fresh(lease.meta(), now_unix()) {
            return Ok(serve_hit_conditional(lease, head, false));
        }
        let can_revalidate = has_revalidation_validator(lease.meta());
        drop(lease);
        if can_revalidate {
            return revalidate_entry(
                stream,
                cache,
                key,
                upstream,
                head,
                fill,
                false,
                RevalidationExpiry::CachePolicy,
            )
            .await;
        }
    }
    serve_miss(stream, cache, key, upstream, head, fill).await
}

/// Adds the LFT CORS triplet to a successful 2xx/3xx response. Generic and
/// error responses carry no CORS headers.
fn nucleus_add_cors(response: &mut Response, location: ovcc_nucleus::Location) {
    for (name, value) in ovcc_nucleus::cors_headers(location, response.head.status) {
        response
            .head
            .headers
            .push(((*name).to_string(), (*value).to_string()));
    }
}

/// Builds client-visible MISS headers before a streaming response commits its
/// head. Cache metadata retains only the relayed origin headers.
fn miss_client_headers(
    relayed: &[(String, String)],
    status: u16,
    add_lft_cors: bool,
) -> Vec<(String, String)> {
    let mut client = relayed.to_vec();
    if add_lft_cors {
        client.extend(
            ovcc_nucleus::cors_headers(ovcc_nucleus::Location::Lft, status)
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        );
    }
    client
}

/// Forwards the request method and streamed body, then relays the origin response.
/// `cache_status` marks cache-policy bypasses; other transparent routes omit it.
///
/// Used for any non-GET/HEAD method on a configured backend and for the
/// passthrough default route (an unmatched `Host`). The request body is piped
/// from the client socket straight into the upstream connection through a
/// bounded channel, so a multi-GB upload never buffers in RAM (AGENTS.md). A
/// failure to reach the origin (e.g. DNS NXDOMAIN) becomes a `502 Bad Gateway`.
#[tracing::instrument(
    name = "ovcc.serve_transparent",
    skip_all,
    fields(
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn serve_transparent(
    k: &mut ClientStream,
    upstream: &str,
    head: &RequestHead,
    body_prefix: Vec<u8>,
    cache_status: Option<&'static str>,
) -> std::result::Result<Response, BoxError> {
    let url = origin_url(upstream, head);

    // Classify the client's body framing before opening the upstream connection,
    // mirroring the write-through arm. A `Transfer-Encoding: chunked` upload that
    // also carries AWS chunk signatures is rejected here so the HTTP
    // `ChunkedDecoder` never runs over signature bytes.
    let framing = match transparent_body_framing(&head.headers, head.content_length) {
        Ok(framing) => framing,
        Err((code, msg)) => {
            write_method_response_with(
                k,
                &head.method,
                code,
                reason_phrase(code),
                &[],
                cache_status,
                msg.as_bytes(),
            )
            .await?;
            let bytes = if head.method.eq_ignore_ascii_case("HEAD") {
                0
            } else {
                msg.len() as u64
            };
            return Ok(Response::streamed(code, cache_status, bytes));
        }
    };

    // A bounded channel applies back-pressure, so the client read stalls when the
    // upstream is slow and at most a few chunks are ever in flight.
    let (tx, rx) = mpsc::channel::<UploadItem>(8);
    // A fixed body has a known length, including zero. Only a chunked upload
    // passes an unknown length, so a bodyless GET/HEAD stays unchunked. The
    // chunked feed strips the client's HTTP framing before forwarding bytes.
    let body_len = match framing {
        TransparentFraming::Fixed(content_length) => Some(content_length as u64),
        TransparentFraming::Chunked => None,
    };
    let proxy_fut =
        ovcc_upstream::open_proxy_request(&head.method, &url, &head.headers, rx, body_len);
    let feed_fut = feed_request_body_transparent(k, body_prefix, framing, tx);
    let (proxy_res, feed_res) = tokio::join!(proxy_fut, feed_fut);
    let feed_fault = feed_res?;

    // Answered ahead of `proxy_res` on purpose, because OVCC deliberately killed
    // the upstream body and the origin never saw a valid end of message, so any
    // status it managed to emit must not be relayed as a success. The trace is
    // left green, since a malformed client body is not a gateway fault.
    if let Some(FeedFault::Client(abort)) = feed_fault {
        let (code, body) = client_abort_response(abort);
        tracing::warn!(event = "client_upload_aborted", upstream, abort = %abort);
        write_method_response_with(
            k,
            &head.method,
            code,
            reason_phrase(code),
            &[],
            cache_status,
            body,
        )
        .await?;
        let bytes = if head.method.eq_ignore_ascii_case("HEAD") {
            0
        } else {
            body.len() as u64
        };
        return Ok(Response::streamed(code, cache_status, bytes));
    }

    // This arm interleaves reading the client upload with writing the response,
    // so it writes inline and reports `BodySource::Streamed`; the driver leaves
    // the socket untouched and only the metadata flows on to the access log.
    let served = match proxy_res {
        // The origin stopped reading before the whole client body crossed, so a
        // success it reports covers bytes it never received.
        Ok(origin)
            if feed_fault == Some(FeedFault::OriginStopped)
                && (200..300).contains(&origin.status) =>
        {
            let span = tracing::Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_message", "origin success withheld");
            tracing::warn!(
                event = "incomplete_upload_success_withheld",
                upstream,
                origin_status = origin.status,
            );
            write_method_response_with(
                k,
                &head.method,
                502,
                reason_phrase(502),
                &[],
                cache_status,
                INCOMPLETE_UPLOAD_BODY,
            )
            .await?;
            let bytes = if head.method.eq_ignore_ascii_case("HEAD") {
                0
            } else {
                INCOMPLETE_UPLOAD_BODY.len() as u64
            };
            Response::streamed(502, cache_status, bytes)
        }
        Ok(origin) => {
            relay_transparent_response_with_status(
                k,
                origin.status,
                origin.headers,
                origin.timing,
                origin.body,
                head.method.eq_ignore_ascii_case("HEAD"),
                cache_status,
            )
            .await?
        }
        Err(error) => {
            // The streaming arm bypasses `send_bodiless`, so flag ERROR here so a
            // gateway failure from an unreachable or timed-out origin shows red.
            let span = tracing::Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_message", "upstream request failed");
            let failure = upstream_failure_response(&error);
            write_method_response_with(
                k,
                &head.method,
                failure.status,
                failure.reason,
                failure.headers,
                cache_status,
                failure.body,
            )
            .await?;
            let bytes = if head.method.eq_ignore_ascii_case("HEAD") {
                0
            } else {
                failure.body.len() as u64
            };
            let response = Response::streamed(failure.status, cache_status, bytes);
            match failure.timeout_stage {
                Some(stage) => response.with_timeout_stage(stage),
                None => response,
            }
        }
    };
    Ok(served)
}

#[cfg(test)]
async fn relay_transparent_response<S, B>(
    stream: &mut S,
    status: u16,
    headers: Vec<(String, String)>,
    timing: UpstreamTiming,
    body: B,
    head_only: bool,
) -> std::result::Result<Response, BoxError>
where
    S: AsyncWrite + Unpin,
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    relay_transparent_response_with_status(stream, status, headers, timing, body, head_only, None)
        .await
}

async fn relay_transparent_response_with_status<S, B>(
    stream: &mut S,
    status: u16,
    headers: Vec<(String, String)>,
    mut timing: UpstreamTiming,
    mut body: B,
    head_only: bool,
    cache_status: Option<&'static str>,
) -> std::result::Result<Response, BoxError>
where
    S: AsyncWrite + Unpin,
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let relayed = relay_headers(&headers);
    let header_refs: Vec<(&str, &str)> = relayed
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let content_length = parse_content_length(&headers);
    let use_chunked = !head_only && status_has_body(status) && content_length.is_none();
    let reason = hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("");
    write_streaming_head(
        stream,
        status,
        reason,
        cache_status,
        &header_refs,
        content_length,
        use_chunked,
        false,
    )
    .await?;

    let body_started = Instant::now();
    if head_only || !status_has_body(status) {
        timed_flush(stream).await?;
        timing.response = Some(body_started.elapsed().as_secs_f64());
        record_upstream_fetch_timing(&timing);
        return Ok(
            Response::streamed(status, cache_status, 0).with_upstream(status, timing, &headers)
        );
    }
    let mut total = 0u64;
    while let Some(frame) = body.frame().await {
        let chunk = match frame {
            Ok(frame) => match frame.into_data() {
                Ok(data) => data,
                Err(_) => continue,
            },
            Err(error) => {
                let span = tracing::Span::current();
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_message", "upstream response body failed");
                if origin_body_timed_out(&error) {
                    timed_flush(stream).await?;
                    timing.response = Some(body_started.elapsed().as_secs_f64());
                    record_upstream_fetch_timing(&timing);
                    return Ok(Response::streamed(status, cache_status, total)
                        .with_upstream(status, timing, &headers)
                        .with_timeout_stage(TimeoutStage::OriginResponseBody));
                }
                return Err(Error::upstream(format!(
                    "transparent upstream response body failed: {error}"
                ))
                .into());
            }
        };
        if chunk.is_empty() {
            continue;
        }
        if content_length.is_some_and(|expected| total + chunk.len() as u64 > expected) {
            return Err(Error::upstream(
                "transparent upstream response exceeded its Content-Length",
            )
            .into());
        }
        if use_chunked {
            write_http_chunk(stream, &chunk).await?;
        } else {
            write_body_counted(stream, &chunk)
                .await
                .map_err(|failure| failure.error)?;
        }
        total += chunk.len() as u64;
    }
    if content_length.is_some_and(|expected| total != expected) {
        return Err(Error::upstream(
            "transparent upstream response ended before its Content-Length",
        )
        .into());
    }
    if use_chunked {
        timed_write_all(stream, b"0\r\n\r\n").await?;
    }
    timed_flush(stream).await?;

    timing.response = Some(body_started.elapsed().as_secs_f64());
    record_upstream_fetch_timing(&timing);
    Ok(Response::streamed(status, cache_status, total).with_upstream(status, timing, &headers))
}

/// How the transparent path frames the incoming request body. Distinct from
/// [`PutFraming`] because the transparent relay never caches, so a fixed body is
/// forwarded verbatim (including a verbatim `aws-chunked` body under its own
/// `Content-Length`); only a genuine `Transfer-Encoding: chunked` body needs
/// de-chunking before it is re-framed for the origin.
///
/// Public for the `transparent_relay` bench.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransparentFraming {
    /// Forward `content_length` raw body bytes (0 means no body).
    Fixed(usize),
    /// Strip HTTP chunk framing and forward the decoded object bytes.
    Chunked,
}

/// Decides how the transparent relay frames the request body. A
/// `Transfer-Encoding: chunked` request is de-chunked, unless it also carries
/// AWS chunk-signature framing (`Content-Encoding: aws-chunked` or a streaming
/// `x-amz-content-sha256`), which is rejected with `400` so the HTTP decoder
/// never misreads signature bytes. Everything else is forwarded verbatim under
/// its `Content-Length` (a plain `aws-chunked` upload keeps its own length and
/// streams through unchanged).
///
/// Public for the `transparent_relay` bench.
#[doc(hidden)]
pub fn transparent_body_framing(
    headers: &[(String, String)],
    content_length: usize,
) -> std::result::Result<TransparentFraming, (u16, &'static str)> {
    if let Some(te) = header_value(headers, "transfer-encoding")
        && te.to_ascii_lowercase().contains("chunked")
    {
        if let Some(ce) = header_value(headers, "content-encoding")
            && ce.to_ascii_lowercase().contains("aws-chunked")
        {
            return Err((400, "Content-Encoding: aws-chunked is not supported\n"));
        }
        if let Some(sha) = header_value(headers, "x-amz-content-sha256")
            && sha.to_ascii_lowercase().contains("streaming")
        {
            return Err((400, "AWS streaming-signature payloads are not supported\n"));
        }
        return Ok(TransparentFraming::Chunked);
    }
    Ok(TransparentFraming::Fixed(content_length))
}

/// Why a client request body did not reach the origin whole. Either value means
/// the upstream body could not have ended cleanly, so an origin success would
/// cover bytes the origin never received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedFault {
    /// The client's own input was invalid or truncated, so OVCC ended the
    /// upstream body with a terminal [`UploadAbort`] and answers the client
    /// itself.
    Client(UploadAbort),
    /// The origin stopped reading first. That is not the client's fault, so the
    /// origin's own answer still crosses unless it claims success.
    OriginStopped,
}

/// Outcome of feeding one client request body upstream, where `Ok(None)` means
/// the complete body crossed and the upstream body ended cleanly.
type FeedOutcome = std::io::Result<Option<FeedFault>>;

/// Ends a streamed upload with a body error rather than a clean EOF, so hyper
/// omits the terminating chunk. A closed channel means the upstream request has
/// already failed, which carries the same guarantee, so the send result is
/// deliberately ignored.
async fn abort_upload(tx: &mpsc::Sender<UploadItem>, abort: UploadAbort) -> Option<FeedFault> {
    let _ = tx.send(Err(abort)).await;
    Some(FeedFault::Client(abort))
}

/// The status and body OVCC answers with after aborting a client upload. The
/// origin never saw a valid end of message, so this is a client error, and in
/// particular must not borrow the `Proxy-Status: ovcc; error=connection_terminated`
/// contract that means an upstream connection broke.
fn client_abort_response(abort: UploadAbort) -> (u16, &'static [u8]) {
    match abort {
        UploadAbort::InvalidFraming => (400, b"Malformed chunked request body\n".as_slice()),
        _ => (
            400,
            b"Request body ended before it was complete\n".as_slice(),
        ),
    }
}

/// Answered in place of an origin success for an upload OVCC stopped forwarding
/// partway, since the origin judged bytes it never fully received.
const INCOMPLETE_UPLOAD_BODY: &[u8] = b"Origin reported success for an incomplete upload\n";

/// Dispatches the transparent request-body feed by framing, mirroring
/// [`tee_request_body`] on the write-through path: a fixed length streams raw
/// bytes, a chunked body is de-chunked to object bytes first.
async fn feed_request_body_transparent(
    k: &mut ClientStream,
    prefix: Vec<u8>,
    framing: TransparentFraming,
    tx: mpsc::Sender<UploadItem>,
) -> FeedOutcome {
    match framing {
        TransparentFraming::Fixed(content_length) => {
            feed_request_body(k, prefix, content_length, tx).await
        }
        TransparentFraming::Chunked => feed_request_body_chunked(k, prefix, tx).await,
    }
}

/// Streams a fixed-length request body into `tx`: first the bytes already read
/// past the request head (`prefix`), then `content_length` worth of further
/// reads. Dropping `tx` (on return) marks end-of-body, so it is reached only
/// once the declared length has crossed; a client that closes short sends
/// [`UploadAbort::Truncated`] first. Generic over the reader so a unit test can
/// drive it from an in-memory stream.
async fn feed_request_body<S>(
    reader: &mut S,
    prefix: Vec<u8>,
    content_length: usize,
    tx: mpsc::Sender<UploadItem>,
) -> FeedOutcome
where
    S: AsyncRead + Unpin,
{
    let mut sent = 0usize;
    if content_length > 0 && !prefix.is_empty() {
        let take = prefix.len().min(content_length);
        if tx
            .send(Ok(Bytes::copy_from_slice(&prefix[..take])))
            .await
            .is_err()
        {
            return Ok(Some(FeedFault::OriginStopped));
        }
        sent += take;
    }

    let mut buf = vec![0u8; 64 * 1024];
    while sent < content_length {
        let want = (content_length - sent).min(buf.len());
        let n = match reader.read(&mut buf[..want]).await {
            Ok(n) => n,
            Err(error) => {
                abort_upload(&tx, UploadAbort::Truncated).await;
                return Err(error);
            }
        };
        if n == 0 {
            // The client closed short of its declared `Content-Length`, so the
            // origin must not be able to accept the prefix as a whole object.
            return Ok(abort_upload(&tx, UploadAbort::Truncated).await);
        }
        if tx
            .send(Ok(Bytes::copy_from_slice(&buf[..n])))
            .await
            .is_err()
        {
            return Ok(Some(FeedFault::OriginStopped));
        }
        sent += n;
    }
    Ok(None)
}

/// Streams a chunked request body into `tx`, stripping HTTP chunk framing via
/// [`ChunkedDecoder`] so the origin receives the decoded object bytes. Feeds the
/// already-read `prefix` first, then reads until the terminating zero-size
/// chunk. Stays constant-memory with a fixed read buffer and never collects the
/// whole body, so a large or hostile chunked upload cannot exhaust memory.
/// Malformed framing and an end of stream before the terminator both abort the
/// upstream body rather than closing it cleanly. Generic over the reader so a
/// unit test can drive it from an in-memory stream.
async fn feed_request_body_chunked<S>(
    reader: &mut S,
    prefix: Vec<u8>,
    tx: mpsc::Sender<UploadItem>,
) -> FeedOutcome
where
    S: AsyncRead + Unpin,
{
    let mut decoder = ChunkedDecoder::new();
    let mut out = Vec::new();

    // Feed the prefix first; a fully-contained body may terminate here.
    match decoder.push(&prefix, &mut out) {
        Ok(done) => {
            if !out.is_empty() && tx.send(Ok(Bytes::copy_from_slice(&out))).await.is_err() {
                return Ok(Some(FeedFault::OriginStopped));
            }
            if done {
                return Ok(None);
            }
        }
        // The decoded bytes of a failing push are dropped on purpose, since the
        // message is being abandoned.
        Err(_) => return Ok(abort_upload(&tx, UploadAbort::InvalidFraming).await),
    }

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(n) => n,
            Err(error) => {
                abort_upload(&tx, UploadAbort::Truncated).await;
                return Err(error);
            }
        };
        if n == 0 {
            // End of stream before the terminating zero-size chunk and its
            // trailer, so the body is incomplete and must not end cleanly
            // upstream.
            return Ok(abort_upload(&tx, UploadAbort::Truncated).await);
        }
        out.clear();
        match decoder.push(&buf[..n], &mut out) {
            Ok(done) => {
                if !out.is_empty() && tx.send(Ok(Bytes::copy_from_slice(&out))).await.is_err() {
                    return Ok(Some(FeedFault::OriginStopped));
                }
                if done {
                    return Ok(None);
                }
            }
            Err(_) => return Ok(abort_upload(&tx, UploadAbort::InvalidFraming).await),
        }
    }
}

// --- Write-through PUT -----------------------------------------------------

/// Monotonic counter for unique staging file names within this process.
static WT_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How the incoming write-through PUT body is framed on the wire. The handler
/// must stage **object** bytes, not HTTP framing bytes.
///
/// Public for the `write_through` bench.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutFraming {
    /// `Content-Length` is present, so the raw body bytes are object bytes and
    /// completeness is `staged == content_length`.
    Fixed(usize),
    /// `Transfer-Encoding: chunked` (only when chunked uploads are enabled), so
    /// the HTTP chunk framing is stripped to recover object bytes.
    Chunked,
}

/// Decides how the request body is framed. Returns the framing, or
/// `(status, message)` for a body that must be rejected before any I/O.
/// - `Content-Encoding: aws-chunked` / SigV4 streaming payloads -> `400` (the
///   wire bytes carry AWS chunk signatures, not the object stream we cache).
/// - `Transfer-Encoding: chunked` -> `411` unless chunked uploads are enabled.
/// - `Content-Length` -> fixed framing.
/// - anything else (no length, not chunked) -> `411`.
///
/// Public for the `write_through` bench.
#[doc(hidden)]
pub fn classify_put_body(
    headers: &[(String, String)],
    enable_http_chunked: bool,
) -> std::result::Result<PutFraming, (u16, &'static str)> {
    if let Some(ce) = header_value(headers, "content-encoding")
        && ce.to_ascii_lowercase().contains("aws-chunked")
    {
        return Err((
            400,
            "Content-Encoding: aws-chunked is not supported for write-through\n",
        ));
    }
    if let Some(sha) = header_value(headers, "x-amz-content-sha256")
        && sha.to_ascii_lowercase().contains("streaming")
    {
        return Err((
            400,
            "AWS streaming-signature payloads are not supported for write-through\n",
        ));
    }
    if let Some(te) = header_value(headers, "transfer-encoding")
        && te.to_ascii_lowercase().contains("chunked")
    {
        if !enable_http_chunked {
            return Err((
                411,
                "Transfer-Encoding: chunked is not enabled for write-through\n",
            ));
        }
        return Ok(PutFraming::Chunked);
    }
    if let Some(cl) = header_value(headers, "content-length") {
        return match cl.trim().parse::<usize>() {
            Ok(n) => Ok(PutFraming::Fixed(n)),
            Err(_) => Err((400, "Invalid Content-Length\n")),
        };
    }
    Err((411, "Content-Length required for write-through\n"))
}

/// Outcome of teeing the client body to staging + origin.
struct TeeOutcome {
    /// Object bytes staged. Stops advancing once a staging write fails, while
    /// the origin keeps receiving the rest of the object.
    staged: u64,
    /// Whether the full body arrived intact and reached staging (so the entry
    /// may be seeded). False on a short or aborted upload, a staging write
    /// error, or an upstream that stopped consuming.
    complete: bool,
    /// Set when the client body did not reach the origin whole, so the upstream
    /// body ended as a [`UploadAbort`] rather than a valid end of message. A
    /// staging-volume failure alone leaves this `None`, since the origin still
    /// received every byte.
    feed_fault: Option<FeedFault>,
}

/// Handles a write-through PUT by streaming the client body to the origin AND
/// teeing it to a staging file on the cache volume; on a complete body + 2xx origin response,
/// seeds the cache under the consumer-GET key so the next GET is a HIT. The
/// origin PUT response (status + `ETag`/`Content-Type`) is relayed to the
/// publisher. Distinct from [`serve_transparent`], which never caches.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "ovcc.serve_write_through",
    skip_all,
    fields(
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn serve_write_through(
    k: &mut ClientStream,
    upstream: &str,
    head: &RequestHead,
    body_prefix: Vec<u8>,
    cache: &Cache<DiskStorage>,
    backend: &Backend,
    generation: i64,
    staging: &WriteThroughState,
    enable_http_chunked: bool,
) -> std::result::Result<Response, BoxError> {
    let host = head.host.as_deref().unwrap_or_default();
    // Reject unsupported body framing before any I/O (no upstream connection,
    // no staging file). Like the transparent arm, this path writes inline and
    // reports `BodySource::Streamed`.
    let framing = match classify_put_body(&head.headers, enable_http_chunked) {
        Ok(framing) => framing,
        Err((code, msg)) => {
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::Rejected,
            );
            write_response(k, code, reason_phrase(code), "BYPASS", msg.as_bytes()).await?;
            return Ok(Response::streamed(code, Some("BYPASS"), msg.len() as u64));
        }
    };
    let _active = WriteThroughActiveGuard::start();
    let upload_started = Instant::now();

    // Stage to a process-unique file on the cache volume (never in RAM).
    let token = format!(
        "{}-{}-{}",
        std::process::id(),
        now_unix(),
        WT_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let staging_path = staging.staging_dir.join(&token);
    let mut staging_file = match File::create(&staging_path) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!("write-through: cannot open staging file: {err}");
            ovcc_telemetry::metrics::record_write_through_duration(
                host,
                WriteThroughPhase::Upload,
                upload_started.elapsed().as_secs_f64(),
            );
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::UpstreamError,
            );
            write_response(
                k,
                500,
                "Internal Server Error",
                "BYPASS",
                b"Staging unavailable\n",
            )
            .await?;
            return Ok(Response::streamed(
                500,
                Some("BYPASS"),
                b"Staging unavailable\n".len() as u64,
            ));
        }
    };

    // Tee the client body to staging + origin concurrently. A bounded channel
    // back-pressures the client read when the origin is slow, so at most a few
    // chunks are in flight (constant memory, multi-GB safe).
    let url = origin_url(upstream, head);
    let (tx, rx) = mpsc::channel::<UploadItem>(8);
    // A fixed-length PUT forwards its `Content-Length` so a real S3 origin
    // accepts it (S3 rejects a chunked PUT with `501 Not Implemented`); a
    // chunked upload (when enabled) stays chunked upstream.
    let upstream_len = match framing {
        PutFraming::Fixed(n) => Some(n as u64),
        PutFraming::Chunked => None,
    };
    let proxy_fut = ovcc_upstream::proxy_request("PUT", &url, &head.headers, rx, upstream_len);
    let feed_fut = tee_request_body(
        k,
        body_prefix,
        framing,
        &mut staging_file,
        tx,
        staging.chunk_size,
    );
    let (proxy_res, outcome) = tokio::join!(proxy_fut, feed_fut);
    let _ = staging_file.flush();
    drop(staging_file);
    ovcc_telemetry::metrics::record_write_through_duration(
        host,
        WriteThroughPhase::Upload,
        upload_started.elapsed().as_secs_f64(),
    );

    // A client-caused abort belongs to the client rather than the origin, since
    // OVCC ended the upstream body itself, so nothing was committed and nothing
    // may be seeded. Checked before `proxy_res` so an early origin 2xx is never
    // relayed.
    if let Some(FeedFault::Client(abort)) = outcome.feed_fault {
        let _ = std::fs::remove_file(&staging_path);
        ovcc_telemetry::metrics::record_write_through_result(host, WriteThroughResult::Rejected);
        let (code, body) = client_abort_response(abort);
        tracing::warn!(
            event = "write_through_client_upload_aborted",
            upstream,
            abort = %abort,
        );
        write_response(k, code, reason_phrase(code), "BYPASS", body).await?;
        return Ok(Response::streamed(code, Some("BYPASS"), body.len() as u64));
    }

    let origin = match proxy_res {
        Ok(origin) => origin,
        Err(error) => {
            // The streaming PUT bypasses `send_bodiless`, so flag ERROR here so an
            // unreachable origin shows red in a trace.
            let span = tracing::Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_message", "upstream request failed");
            let _ = std::fs::remove_file(&staging_path);
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::UpstreamError,
            );
            let failure = upstream_failure_response(&error);
            write_response_with(
                k,
                failure.status,
                failure.reason,
                failure.headers,
                Some("BYPASS"),
                failure.body,
            )
            .await?;
            return Ok(Response::streamed(
                failure.status,
                Some("BYPASS"),
                failure.body.len() as u64,
            ));
        }
    };

    let upstream_ok = (200..300).contains(&origin.status);
    // An origin that stopped reading mid-upload judged bytes it never fully
    // received, so its success is withheld rather than relayed as an accepted
    // write.
    if upstream_ok && outcome.feed_fault == Some(FeedFault::OriginStopped) {
        let span = tracing::Span::current();
        span.record("otel.status_code", "ERROR");
        span.record("otel.status_message", "origin success withheld");
        let _ = std::fs::remove_file(&staging_path);
        ovcc_telemetry::metrics::record_write_through_result(host, WriteThroughResult::Cancelled);
        write_response(k, 502, reason_phrase(502), "BYPASS", INCOMPLETE_UPLOAD_BODY).await?;
        return Ok(Response::streamed(
            502,
            Some("BYPASS"),
            INCOMPLETE_UPLOAD_BODY.len() as u64,
        ));
    }

    // Seed only when the upload completed AND the origin accepted it. The seed
    // key must be byte-identical to the consumer GET key, using the same
    // per-backend strip set, `GET` method, generation, and `if_match` derived from the origin
    // ETag (the publisher echoes it as the GET `If-Match`).
    let mut result = if !outcome.complete {
        WriteThroughResult::Cancelled
    } else if !upstream_ok {
        WriteThroughResult::UpstreamError
    } else {
        WriteThroughResult::Success
    };
    let customer_provided_key_seed_bypass =
        backend.name == "azure" && azure_has_customer_provided_key(&head.headers);
    // Azure requires the same customer-provided key headers on every read.
    // Seeding plaintext from such a PUT would bypass origin authorization.
    // https://learn.microsoft.com/en-us/rest/api/storageservices/get-blob#request-headers-customer-provided-encryption-keys
    if outcome.complete && upstream_ok && !customer_provided_key_seed_bypass {
        let strip = if backend.name == "azure" {
            ovcc_cachekey::STRIPPED_PARAMS_AZURE
        } else {
            ovcc_cachekey::STRIPPED_PARAMS
        };
        let filtered = ovcc_cachekey::filter_query_args_with(&head.query, strip);
        let etag = origin.etag.clone().unwrap_or_default();
        let if_match = if_match_for_seed(&etag, backend.requires_versioning);
        let generation_str = generation.to_string();
        let seed_variant = if backend.name == "azure" {
            azure_seed_variant(&head.headers)
        } else {
            String::new()
        };
        let key = KeyParts {
            generation: &generation_str,
            host,
            method: "GET",
            uri: &head.path,
            filtered_args: &filtered,
            if_match,
            range: &seed_variant,
        }
        .render();
        let mut seed_headers = vec![("Accept-Ranges".to_owned(), "bytes".to_owned())];
        if backend.name == "azure" {
            seed_headers.extend(azure_seed_response_headers(&head.headers, &origin.headers));
        } else if let Some(content_type) = header_value(&head.headers, "content-type") {
            seed_headers.push(("Content-Type".to_owned(), content_type.to_owned()));
        }
        if let Some(etag) = &origin.etag {
            seed_headers.push(("ETag".to_owned(), etag.clone()));
        }
        let meta = StoredMeta {
            logical_key: None,
            size: outcome.staged,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: backend.cache_policy.backend_max_idle_secs,
            status: 200,
            etag: origin.etag.clone(),
            last_modified: header_value(&origin.headers, "last-modified").map(str::to_owned),
            content_type: None,
            headers: seed_headers,
            representation_length: Some(outcome.staged),
            // A write-through seed is a durable consumer-GET entry, not bursty.
            expires_at: None,
            versionable: false,
            // A seed is a full object (`PUT` body), never a partial range.
            content_range: None,
        };
        let seed_started = Instant::now();
        if let Err(err) = seed_cache(cache, &key, meta, &staging_path, staging.chunk_size) {
            // A seed failure does not fail the PUT (the object is on the origin);
            // it only leaves the cache cold.
            tracing::warn!("write-through: cache seed failed: {err}");
            result = WriteThroughResult::SeedError;
        }
        ovcc_telemetry::metrics::record_write_through_duration(
            host,
            WriteThroughPhase::Seed,
            seed_started.elapsed().as_secs_f64(),
        );
        ovcc_telemetry::metrics::record_write_through_bytes(host, outcome.staged);
    }
    let _ = std::fs::remove_file(&staging_path);
    ovcc_telemetry::metrics::record_write_through_result(host, result);

    // Relay the origin PUT response (status + safe headers + body). No
    // `X-Cache-Status`: a PUT is not a cache lookup.
    let relayed = relay_headers(&origin.headers);
    let header_refs: Vec<(&str, &str)> = relayed
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let reason = if origin.status == 200 { "OK" } else { "" };
    write_response_with(k, origin.status, reason, &header_refs, None, &origin.body).await?;
    Ok(
        Response::streamed(origin.status, None, origin.body.len() as u64).with_upstream(
            origin.status,
            origin.timing,
            &origin.headers,
        ),
    )
}

// --- Azure block-blob assembly ---------------------------------------------

/// Upper bound on a buffered block-list commit body, so a hostile client cannot
/// grow it without limit. A commit for even a maximal blob is a few MB of ids.
const MAX_BLOCK_LIST_BYTES: usize = 64 * 1024 * 1024;

fn block_origin_accepted(status: u16) -> bool {
    (200..300).contains(&status)
}

fn cache_reset_enabled(ctx: &ConnCtx, listener_kind: ListenerKind, host: &str) -> bool {
    match listener_kind {
        ListenerKind::Cache => ctx.backends.iter().any(|runtime| {
            runtime.config.name != "nucleus"
                && runtime.config.allow_cache_reset
                && runtime.config.resolve_upstream(host).is_some()
        }),
        ListenerKind::Nucleus => ctx
            .backends
            .iter()
            .any(|runtime| runtime.config.name == "nucleus" && runtime.config.allow_cache_reset),
    }
}

/// Applies the canonical monotonic generation contract.
///
/// Missing/empty `gen` is a read. Malformed values report
/// `invalid_generation`; lower/equal values report `generation_not_updated`.
/// A compare-exchange loop serializes concurrent writers without persistence
/// beyond this process.
fn apply_generation(
    generation: &AtomicI64,
    requested: Option<&str>,
) -> (i64, Option<i64>, Option<&'static str>) {
    let Some(requested) = requested.filter(|value| !value.is_empty()) else {
        return (generation.load(Ordering::SeqCst), None, None);
    };
    let Ok(requested) = requested.parse::<i64>() else {
        return (
            generation.load(Ordering::SeqCst),
            None,
            Some("invalid_generation"),
        );
    };

    let mut current = generation.load(Ordering::SeqCst);
    loop {
        if requested <= current {
            return (current, None, Some("generation_not_updated"));
        }
        match generation.compare_exchange(current, requested, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(old) => return (requested, Some(old), None),
            Err(observed) => current = observed,
        }
    }
}

fn generation_json(
    instance: &str,
    generation: i64,
    old_generation: Option<i64>,
    error: Option<&str>,
) -> String {
    let instance = instance.replace('\\', "\\\\").replace('"', "\\\"");
    let mut fields = vec![
        format!("\"instance\":\"{instance}\""),
        format!("\"generation\":\"{generation}\""),
    ];
    if let Some(old_generation) = old_generation {
        fields.push(format!("\"old_generation\":\"{old_generation}\""));
    }
    if let Some(error) = error {
        fields.push(format!("\"error\":\"{error}\""));
    }
    format!("{{{}}}\n", fields.join(","))
}

/// The percent-decoded value of query parameter `name`, if present. Needed for
/// `blockid`, which arrives percent-encoded and must decode to the raw base64
/// id the commit XML carries.
fn query_param_decoded(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// The `comp` operation of an Azure blob request, if any. The value (`block`,
/// `blocklist`, ...) is never percent-encoded, so the raw form is compared.
fn comp_param(query: &str) -> Option<&str> {
    query_param(query, "comp")
}

/// The staged blocks of one blob, each fragment an immutable file. A block-list
/// commit plans metadata for these files and transfers ownership to storage by
/// same-volume rename; fragment bytes are never assembled or copied.
#[derive(Clone)]
struct BlockStaging {
    dir: PathBuf,
    token_ttl: Duration,
}

impl BlockStaging {
    /// The per-blob staging directory under `staging_dir`, keyed by blob
    /// identity so one blob's fragments never mix with another's.
    fn for_blob(
        staging_dir: &std::path::Path,
        host: &str,
        path: &str,
        token_ttl: Duration,
    ) -> Self {
        let mut hasher = DefaultHasher::new();
        hasher.write(host.as_bytes());
        hasher.write(b"|");
        hasher.write(path.as_bytes());
        let blob = hasher.finish();
        Self {
            dir: staging_dir.join(".blocks").join(format!("{blob:016x}")),
            token_ttl,
        }
    }

    /// The staging file path for one block id.
    fn block_path(&self, id: &BlockId) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        hasher.write(id.as_str().as_bytes());
        self.dir.join(format!("{:016x}", hasher.finish()))
    }

    /// Partial files stay outside the metadata source so concurrent commits cannot
    /// assemble a prefix before origin acceptance.
    fn partial_path(&self, id: &BlockId) -> PathBuf {
        self.block_path(id).with_extension("partial")
    }

    fn begin(&self, id: &BlockId) -> std::io::Result<File> {
        Self::remove_expired(&self.dir, self.token_ttl, SystemTime::now())?;
        std::fs::create_dir_all(&self.dir)?;
        File::create(self.partial_path(id))
    }

    /// Keeps only intact, origin-accepted bodies eligible by removing every
    /// other partial file.
    fn finish(&self, id: &BlockId, body_complete: bool, origin_ok: bool) -> std::io::Result<()> {
        let partial = self.partial_path(id);
        if body_complete && origin_ok {
            std::fs::rename(partial, self.block_path(id))
        } else {
            match std::fs::remove_file(partial) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            }
        }
    }

    fn cleanup_staged_path(path: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    fn cleanup_staged_block(&self, id: &BlockId) -> std::io::Result<()> {
        Self::cleanup_staged_path(&self.block_path(id))
    }

    #[cfg(test)]
    fn contains(&self, id: &BlockId) -> bool {
        self.is_fresh(&self.block_path(id), SystemTime::now())
    }

    fn is_fresh(&self, path: &Path, now: SystemTime) -> bool {
        path.metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age < self.token_ttl)
    }

    /// Removes staging files whose filesystem age reached the configured token
    /// lifetime. Directories are traversed without following symlinks.
    fn remove_expired(root: &Path, ttl: Duration, now: SystemTime) -> std::io::Result<()> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                Self::remove_expired(&path, ttl, now)?;
                let _ = std::fs::remove_dir(&path);
                continue;
            }
            let expired = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= ttl);
            if expired {
                Self::cleanup_staged_path(&path)?;
            }
        }
        Ok(())
    }

    /// Discards block fragments after a process restart because an interrupted
    /// publisher cannot prove that its complete staging sequence was retained.
    fn discard_after_restart(staging_dir: &Path) -> std::io::Result<()> {
        let blocks = staging_dir.join(".blocks");
        match std::fs::remove_dir_all(blocks) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl BlockMetadataSource for BlockStaging {
    fn metadata(&self, id: &BlockId) -> Option<BlockMetadata> {
        let path = self.block_path(id);
        if !self.is_fresh(&path, SystemTime::now()) {
            return None;
        }
        let metadata = std::fs::symlink_metadata(path).ok()?;
        metadata
            .file_type()
            .is_file()
            .then(|| BlockMetadata::new(metadata.len()))
    }
}

fn cleanup_adopted_block_staging(staging: &BlockStaging) -> std::io::Result<()> {
    match std::fs::remove_dir(&staging.dir) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn adopt_blocklist_cache<S: ovcc_storage::Storage>(
    cache: &Cache<S>,
    key: &str,
    meta: StoredMeta,
    staging: &BlockStaging,
    ids: &[BlockId],
) -> Result<u64> {
    let adoption = (|| {
        let plan =
            plan_segments(ids, staging).map_err(|error| Error::storage(error.to_string()))?;
        let segments = plan
            .segments()
            .iter()
            .map(|segment| {
                StagedSegment::new(
                    segment.block_id().as_str(),
                    staging.block_path(segment.block_id()),
                    segment.length(),
                )
            })
            .collect::<Vec<_>>();
        let lease = cache.adopt_segmented(key, meta, &segments)?;
        drop(lease);
        cleanup_adopted_block_staging(staging)?;
        Ok(plan.total_length())
    })();
    if adoption.is_err() {
        // The origin already accepted a new generation. Any local planning or
        // publication failure must leave the key cold, not serve older bytes.
        cache.remove(key).map_err(|purge| {
            Error::storage(format!(
                "segmented adoption failed and stale-key purge failed: {purge}"
            ))
        })?;
    }
    adoption
}

/// Stages one Azure block upload (`PUT ...&comp=block&blockid=<id>`) by teeing
/// the block body to the origin and to a per-blob staging file named by the
/// block id. The block is never cached on its own; the blob is seeded later, on
/// the block-list commit. A short or aborted upload, or a non-2xx origin, drops
/// the staging file so the block can never be assembled.
#[tracing::instrument(
    name = "ovcc.serve_write_through_block",
    skip_all,
    fields(
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn serve_write_through_block(
    k: &mut ClientStream,
    upstream: &str,
    head: &RequestHead,
    body_prefix: Vec<u8>,
    write_through: &WriteThroughState,
    enable_http_chunked: bool,
) -> std::result::Result<Response, BoxError> {
    let host = head.host.as_deref().unwrap_or_default();
    let framing = match classify_put_body(&head.headers, enable_http_chunked) {
        Ok(framing) => framing,
        Err((code, msg)) => {
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::Rejected,
            );
            write_response(k, code, reason_phrase(code), "BYPASS", msg.as_bytes()).await?;
            return Ok(Response::streamed(code, Some("BYPASS"), msg.len() as u64));
        }
    };

    let blockid = match query_param_decoded(&head.query, "blockid").filter(|id| !id.is_empty()) {
        Some(id) => BlockId::new(id),
        None => {
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::Rejected,
            );
            write_response(k, 400, reason_phrase(400), "BYPASS", b"Missing blockid\n").await?;
            return Ok(Response::streamed(
                400,
                Some("BYPASS"),
                b"Missing blockid\n".len() as u64,
            ));
        }
    };
    let _active = WriteThroughActiveGuard::start();
    let block_started = Instant::now();

    // Stage the block under its blob's directory, named by the block id, so the
    // commit can find and order it.
    let staging = BlockStaging::for_blob(
        &write_through.staging_dir,
        host,
        &head.path,
        write_through.token_ttl,
    );
    let mut staging_file = match staging.begin(&blockid) {
        Ok(file) => file,
        Err(err) => {
            tracing::warn!("write-through block: cannot open staging file: {err}");
            ovcc_telemetry::metrics::record_write_through_duration(
                host,
                WriteThroughPhase::Block,
                block_started.elapsed().as_secs_f64(),
            );
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::Rejected,
            );
            write_response(
                k,
                500,
                reason_phrase(500),
                "BYPASS",
                b"Staging unavailable\n",
            )
            .await?;
            return Ok(Response::streamed(
                500,
                Some("BYPASS"),
                b"Staging unavailable\n".len() as u64,
            ));
        }
    };

    // Tee the block body to staging and the origin, like the single-shot path.
    let url = origin_url(upstream, head);
    let (tx, rx) = mpsc::channel::<UploadItem>(8);
    let upstream_len = match framing {
        PutFraming::Fixed(n) => Some(n as u64),
        PutFraming::Chunked => None,
    };
    let proxy_fut = ovcc_upstream::proxy_request("PUT", &url, &head.headers, rx, upstream_len);
    let feed_fut = tee_request_body(
        k,
        body_prefix,
        framing,
        &mut staging_file,
        tx,
        write_through.chunk_size,
    );
    let (proxy_res, outcome) = tokio::join!(proxy_fut, feed_fut);
    let _ = staging_file.flush();
    drop(staging_file);

    // Mirrors the single-shot path, answering a client-caused abort before
    // `proxy_res` and dropping the staged block so no commit can assemble it.
    if let Some(FeedFault::Client(abort)) = outcome.feed_fault {
        let _ = staging.finish(&blockid, false, false);
        ovcc_telemetry::metrics::record_write_through_duration(
            host,
            WriteThroughPhase::Block,
            block_started.elapsed().as_secs_f64(),
        );
        ovcc_telemetry::metrics::record_write_through_result(host, WriteThroughResult::Rejected);
        let (code, body) = client_abort_response(abort);
        tracing::warn!(
            event = "write_through_block_client_upload_aborted",
            upstream,
            abort = %abort,
        );
        write_response(k, code, reason_phrase(code), "BYPASS", body).await?;
        return Ok(Response::streamed(code, Some("BYPASS"), body.len() as u64));
    }

    let origin = match proxy_res {
        Ok(origin) => origin,
        Err(error) => {
            let span = tracing::Span::current();
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_message", "upstream request failed");
            tracing::warn!(
                event = "write_through_block_upstream_failed",
                upstream,
                error = %error,
            );
            let _ = staging.finish(&blockid, false, false);
            ovcc_telemetry::metrics::record_write_through_duration(
                host,
                WriteThroughPhase::Block,
                block_started.elapsed().as_secs_f64(),
            );
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::UpstreamError,
            );
            let failure = upstream_failure_response(&error);
            write_response_with(
                k,
                failure.status,
                failure.reason,
                failure.headers,
                Some("BYPASS"),
                failure.body,
            )
            .await?;
            return Ok(Response::streamed(
                failure.status,
                Some("BYPASS"),
                failure.body.len() as u64,
            ));
        }
    };

    let upstream_ok = block_origin_accepted(origin.status);
    // An origin that stopped reading mid-upload judged bytes it never fully
    // received, so its success is withheld rather than relayed as an accepted
    // block.
    if upstream_ok && outcome.feed_fault == Some(FeedFault::OriginStopped) {
        let span = tracing::Span::current();
        span.record("otel.status_code", "ERROR");
        span.record("otel.status_message", "origin success withheld");
        let _ = staging.finish(&blockid, false, false);
        ovcc_telemetry::metrics::record_write_through_duration(
            host,
            WriteThroughPhase::Block,
            block_started.elapsed().as_secs_f64(),
        );
        ovcc_telemetry::metrics::record_write_through_result(host, WriteThroughResult::BlockError);
        write_response(k, 502, reason_phrase(502), "BYPASS", INCOMPLETE_UPLOAD_BODY).await?;
        return Ok(Response::streamed(
            502,
            Some("BYPASS"),
            INCOMPLETE_UPLOAD_BODY.len() as u64,
        ));
    }

    // Keep the staged block only when it arrived intact and the origin accepted
    // it; otherwise drop it so a later commit cannot assemble a broken fragment.
    if let Err(error) = staging.finish(&blockid, outcome.complete, upstream_ok) {
        tracing::warn!("write-through block: cannot finalize staging file: {error}");
    }
    ovcc_telemetry::metrics::record_write_through_duration(
        host,
        WriteThroughPhase::Block,
        block_started.elapsed().as_secs_f64(),
    );
    ovcc_telemetry::metrics::record_write_through_result(
        host,
        if outcome.complete && upstream_ok {
            WriteThroughResult::Block
        } else {
            WriteThroughResult::BlockError
        },
    );

    // Relay the origin block response; a block PUT never seeds the cache.
    let relayed = relay_headers(&origin.headers);
    let header_refs: Vec<(&str, &str)> = relayed
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let reason = if origin.status == 200 { "OK" } else { "" };
    write_response_with(k, origin.status, reason, &header_refs, None, &origin.body).await?;
    Ok(
        Response::streamed(origin.status, None, origin.body.len() as u64).with_upstream(
            origin.status,
            origin.timing,
            &origin.headers,
        ),
    )
}

/// Commits an Azure block blob (`PUT ...&comp=blocklist`) by forwarding the
/// block-list XML to the origin and, on a 2xx commit, atomically adopting the
/// staged fragments and ordered manifest under the
/// consumer-GET key so a later download HITs. A missing block or a non-2xx
/// commit relays without seeding, since the object is still durable on the
/// origin.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "ovcc.serve_write_through_blocklist",
    skip_all,
    fields(
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
    )
)]
async fn serve_write_through_blocklist(
    k: &mut ClientStream,
    upstream: &str,
    head: &RequestHead,
    body_prefix: Vec<u8>,
    cache: &Arc<Cache<DiskStorage>>,
    backend: &Backend,
    generation: i64,
    write_through: &WriteThroughState,
) -> std::result::Result<Response, BoxError> {
    let host = head.host.as_deref().unwrap_or_default();
    let _active = WriteThroughActiveGuard::start();
    let blocklist_started = Instant::now();
    // The commit XML is small; buffer it whole so it can be both forwarded to
    // the origin and parsed for the commit order.
    let body = match read_commit_body(k, head, body_prefix).await {
        Ok(body) => body,
        Err((code, msg)) => {
            ovcc_telemetry::metrics::record_write_through_duration(
                host,
                WriteThroughPhase::BlockList,
                blocklist_started.elapsed().as_secs_f64(),
            );
            ovcc_telemetry::metrics::record_write_through_result(
                host,
                WriteThroughResult::Rejected,
            );
            write_response(k, code, reason_phrase(code), "BYPASS", msg.as_bytes()).await?;
            return Ok(Response::streamed(code, Some("BYPASS"), msg.len() as u64));
        }
    };

    // Forward the commit to the origin with the buffered body as one chunk.
    let url = origin_url(upstream, head);
    // The commit XML is bounded and already buffered, so this path never
    // de-chunks and has no abort case.
    let (tx, rx) = mpsc::channel::<UploadItem>(1);
    if !body.is_empty() {
        let _ = tx.send(Ok(Bytes::copy_from_slice(&body))).await;
    }
    drop(tx);
    let origin =
        match ovcc_upstream::proxy_request("PUT", &url, &head.headers, rx, Some(body.len() as u64))
            .await
        {
            Ok(origin) => origin,
            Err(error) => {
                let span = tracing::Span::current();
                span.record("otel.status_code", "ERROR");
                span.record("otel.status_message", "upstream request failed");
                ovcc_telemetry::metrics::record_write_through_duration(
                    host,
                    WriteThroughPhase::BlockList,
                    blocklist_started.elapsed().as_secs_f64(),
                );
                ovcc_telemetry::metrics::record_write_through_result(
                    host,
                    WriteThroughResult::UpstreamError,
                );
                let failure = upstream_failure_response(&error);
                write_response_with(
                    k,
                    failure.status,
                    failure.reason,
                    failure.headers,
                    Some("BYPASS"),
                    failure.body,
                )
                .await?;
                return Ok(Response::streamed(
                    failure.status,
                    Some("BYPASS"),
                    failure.body.len() as u64,
                ));
            }
        };

    // Publish only when the origin accepted the commit. Planning and adoption
    // run on the blocking pool because they stat/fsync/rename and commit redb.
    let mut result = WriteThroughResult::UpstreamError;
    if block_origin_accepted(origin.status) && azure_has_customer_provided_key(&head.headers) {
        result = WriteThroughResult::Success;
        let staging = BlockStaging::for_blob(
            &write_through.staging_dir,
            host,
            &head.path,
            write_through.token_ttl,
        );
        if let Err(error) = std::fs::remove_dir_all(&staging.dir)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("write-through blocklist: CPK staging cleanup failed: {error}");
        }
    } else if block_origin_accepted(origin.status) {
        result = WriteThroughResult::SeedError;
        let staging = BlockStaging::for_blob(
            &write_through.staging_dir,
            host,
            &head.path,
            write_through.token_ttl,
        );
        let ids = parse_block_list(&body);
        let segment_count = ids.len();
        let adopt_started = Instant::now();
        // The consumer download is a plain GET whose only query is SAS noise,
        // stripped to empty; the seed keys on empty args to match.
        let etag = origin.etag.clone().unwrap_or_default();
        let if_match = if_match_for_seed(&etag, backend.requires_versioning);
        let generation_str = generation.to_string();
        let seed_variant = azure_seed_variant(&head.headers);
        let key = KeyParts {
            generation: &generation_str,
            host,
            method: "GET",
            uri: &head.path,
            filtered_args: "",
            if_match,
            range: &seed_variant,
        }
        .render();
        let mut seed_headers = vec![("Accept-Ranges".to_owned(), "bytes".to_owned())];
        seed_headers.extend(azure_seed_response_headers(&head.headers, &origin.headers));
        if let Some(etag) = &origin.etag {
            seed_headers.push(("ETag".to_owned(), etag.clone()));
        }
        let meta = StoredMeta {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: backend.cache_policy.backend_max_idle_secs,
            status: 200,
            etag: origin.etag.clone(),
            last_modified: header_value(&origin.headers, "last-modified").map(str::to_owned),
            content_type: None,
            headers: seed_headers,
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: None,
        };
        let cache = Arc::clone(cache);
        let staging_for_adopt = staging.clone();
        let adopted = tokio::task::spawn_blocking(move || {
            adopt_blocklist_cache(&cache, key.as_str(), meta, &staging_for_adopt, &ids)
        })
        .await
        .map_err(|error| Error::internal(format!("segmented adoption task failed: {error}")))?;
        ovcc_telemetry::metrics::record_write_through_duration(
            host,
            WriteThroughPhase::Adopt,
            adopt_started.elapsed().as_secs_f64(),
        );
        match adopted {
            Ok(size) => {
                ovcc_telemetry::metrics::record_write_through_bytes(host, size);
                ovcc_telemetry::metrics::record_write_through_segments(host, segment_count);
                result = WriteThroughResult::Success;
            }
            Err(err) => {
                tracing::warn!("write-through blocklist: segmented adoption failed: {err}");
            }
        }
    }
    ovcc_telemetry::metrics::record_write_through_duration(
        host,
        WriteThroughPhase::BlockList,
        blocklist_started.elapsed().as_secs_f64(),
    );
    ovcc_telemetry::metrics::record_write_through_result(host, result);

    // Relay the origin commit response; the commit itself is not a cache lookup.
    let relayed = relay_headers(&origin.headers);
    let header_refs: Vec<(&str, &str)> = relayed
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let reason = if origin.status == 200 { "OK" } else { "" };
    write_response_with(k, origin.status, reason, &header_refs, None, &origin.body).await?;
    Ok(
        Response::streamed(origin.status, None, origin.body.len() as u64).with_upstream(
            origin.status,
            origin.timing,
            &origin.headers,
        ),
    )
}

/// Reads a fixed-length commit body (the `body_prefix` already off the socket,
/// then further reads) into memory, bounded by [`MAX_BLOCK_LIST_BYTES`]. Returns
/// `(status, message)` for a body that must be rejected before use, mirroring
/// the framing guards of the tee path.
async fn read_commit_body(
    k: &mut ClientStream,
    head: &RequestHead,
    prefix: Vec<u8>,
) -> std::result::Result<Vec<u8>, (u16, &'static str)> {
    if let Some(te) = header_value(&head.headers, "transfer-encoding")
        && te.to_ascii_lowercase().contains("chunked")
    {
        return Err((411, "Chunked block-list commit is not supported\n"));
    }
    let len = head.content_length;
    if len == 0 {
        return Err((411, "Content-Length required for block-list commit\n"));
    }
    if len > MAX_BLOCK_LIST_BYTES {
        return Err((413, "Block-list commit too large\n"));
    }

    let mut body = Vec::with_capacity(len.min(1024 * 1024));
    let take = prefix.len().min(len);
    body.extend_from_slice(&prefix[..take]);
    let mut buf = vec![0u8; 64 * 1024];
    while body.len() < len {
        let want = (len - body.len()).min(buf.len());
        match k.read(&mut buf[..want]).await {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    if body.len() != len {
        return Err((400, "Short block-list commit body\n"));
    }
    Ok(body)
}

/// Copies the committed staging file into the cache under `key` with the
/// configured bounded transfer buffer, committing the entry on success.
fn seed_cache(
    cache: &Cache<DiskStorage>,
    key: &ovcc_cachekey::CacheKey,
    meta: StoredMeta,
    staging_path: &std::path::Path,
    chunk_size: usize,
) -> Result<()> {
    let mut writer = cache.begin_fill(key.as_str(), meta)?;
    let mut file = File::open(staging_path)?;
    let mut buf = vec![0u8; chunk_size];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_chunk(&buf[..n])?;
    }
    writer.commit()?;
    Ok(())
}

/// Reads the client body and tees each object-byte chunk to the staging file
/// (sync) and the origin channel (async). Returns the staged byte count and
/// whether the body arrived intact.
async fn tee_request_body(
    k: &mut ClientStream,
    prefix: Vec<u8>,
    framing: PutFraming,
    staging: &mut File,
    tx: mpsc::Sender<UploadItem>,
    chunk_size: usize,
) -> TeeOutcome {
    match framing {
        PutFraming::Fixed(content_length) => {
            tee_fixed(k, prefix, content_length, staging, tx, chunk_size).await
        }
        PutFraming::Chunked => tee_chunked(k, prefix, staging, tx, chunk_size).await,
    }
}

/// Writes `chunk` to staging, best effort, and forwards it upstream. Returns
/// false only when the upstream stopped consuming.
///
/// A staging write failure clears `staging_ok` and keeps forwarding, so a fault
/// on the local cache volume never truncates the object the origin is
/// committing. The caller then reports `complete = false` and skips the seed,
/// leaving nothing worse than a cold cache.
async fn push_chunk(
    chunk: &[u8],
    staging: &mut File,
    staging_ok: &mut bool,
    tx: &mpsc::Sender<UploadItem>,
    staged: &mut u64,
) -> bool {
    if chunk.is_empty() {
        return true;
    }
    if *staging_ok {
        if staging.write_all(chunk).is_err() {
            *staging_ok = false;
        } else {
            *staged += chunk.len() as u64;
        }
    }
    tx.send(Ok(Bytes::copy_from_slice(chunk))).await.is_ok()
}

/// Tees a fixed-length body, forwarding exactly `content_length` object bytes (the
/// already-read `prefix` first, then further socket reads). A short read
/// (client abort/EOF) or any I/O error aborts the upstream body, so the origin
/// cannot accept the prefix as a whole object.
async fn tee_fixed<S>(
    k: &mut S,
    prefix: Vec<u8>,
    content_length: usize,
    staging: &mut File,
    tx: mpsc::Sender<UploadItem>,
    chunk_size: usize,
) -> TeeOutcome
where
    S: AsyncRead + Unpin,
{
    let mut staged: u64 = 0;
    // Tracked apart from `staged`, which stops advancing once staging breaks
    // while the loop must still follow how much of the client body was consumed.
    let mut forwarded: usize = 0;
    let mut staging_ok = true;

    if !prefix.is_empty() {
        let take = prefix.len().min(content_length);
        if take > 0 {
            forwarded += take;
            if !push_chunk(&prefix[..take], staging, &mut staging_ok, &tx, &mut staged).await {
                return TeeOutcome {
                    staged,
                    complete: false,
                    feed_fault: Some(FeedFault::OriginStopped),
                };
            }
        }
    }

    let mut buf = vec![0u8; chunk_size];
    while forwarded < content_length {
        let want = (content_length - forwarded).min(buf.len());
        match k.read(&mut buf[..want]).await {
            Ok(0) | Err(_) => {
                return TeeOutcome {
                    staged,
                    complete: false,
                    feed_fault: abort_upload(&tx, UploadAbort::Truncated).await,
                };
            }
            Ok(n) => {
                forwarded += n;
                if !push_chunk(&buf[..n], staging, &mut staging_ok, &tx, &mut staged).await {
                    return TeeOutcome {
                        staged,
                        complete: false,
                        feed_fault: Some(FeedFault::OriginStopped),
                    };
                }
            }
        }
    }

    TeeOutcome {
        staged,
        complete: staging_ok && staged as usize == content_length,
        feed_fault: None,
    }
}

/// Tees a chunked body, stripping HTTP chunk framing (via [`ChunkedDecoder`]) to
/// recover object bytes. Malformed framing and an end of stream before the
/// terminating zero-size chunk both abort the upstream body, so only a validated
/// terminator plus trailer lets it end cleanly.
async fn tee_chunked<S>(
    k: &mut S,
    prefix: Vec<u8>,
    staging: &mut File,
    tx: mpsc::Sender<UploadItem>,
    chunk_size: usize,
) -> TeeOutcome
where
    S: AsyncRead + Unpin,
{
    let mut decoder = ChunkedDecoder::new();
    let mut staged: u64 = 0;
    let mut staging_ok = true;
    let mut out = Vec::new();

    // Feed the prefix first, then read from the socket until the terminator.
    match decoder.push(&prefix, &mut out) {
        Ok(done) => {
            if !push_chunk(&out, staging, &mut staging_ok, &tx, &mut staged).await {
                return TeeOutcome {
                    staged,
                    complete: false,
                    feed_fault: Some(FeedFault::OriginStopped),
                };
            }
            if done {
                return TeeOutcome {
                    staged,
                    complete: staging_ok,
                    feed_fault: None,
                };
            }
        }
        Err(_) => {
            return TeeOutcome {
                staged,
                complete: false,
                feed_fault: abort_upload(&tx, UploadAbort::InvalidFraming).await,
            };
        }
    }

    let mut buf = vec![0u8; chunk_size];
    loop {
        let n = match k.read(&mut buf).await {
            Ok(0) | Err(_) => {
                return TeeOutcome {
                    staged,
                    complete: false,
                    feed_fault: abort_upload(&tx, UploadAbort::Truncated).await,
                };
            }
            Ok(n) => n,
        };
        out.clear();
        match decoder.push(&buf[..n], &mut out) {
            Ok(done) => {
                if !push_chunk(&out, staging, &mut staging_ok, &tx, &mut staged).await {
                    return TeeOutcome {
                        staged,
                        complete: false,
                        feed_fault: Some(FeedFault::OriginStopped),
                    };
                }
                if done {
                    return TeeOutcome {
                        staged,
                        complete: staging_ok,
                        feed_fault: None,
                    };
                }
            }
            Err(_) => {
                return TeeOutcome {
                    staged,
                    complete: false,
                    feed_fault: abort_upload(&tx, UploadAbort::InvalidFraming).await,
                };
            }
        }
    }
}

/// Why an HTTP/1.1 chunked request body failed to decode. Every variant is a
/// client framing fault, so the upload is aborted rather than silently truncated
/// into a clean origin write.
///
/// Public for the `transparent_relay` bench.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkedError {
    /// The chunk-size line is not a bare hex size with optional extensions.
    BadSizeLine,
    /// A chunk extension does not follow the RFC token or quoted-string grammar.
    BadChunkExtension,
    /// A trailer is not a syntactically valid HTTP field line.
    BadTrailerLine,
    /// The two bytes following a chunk's data were not CRLF.
    MissingChunkCrlf,
    /// A framing line exceeded `MAX_CHUNK_LINE_BYTES` without terminating.
    LineTooLong,
    /// The trailer section exceeded `MAX_CHUNK_TRAILER_BYTES`.
    TrailerTooLong,
}

impl std::fmt::Display for ChunkedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BadSizeLine => "malformed chunk-size line",
            Self::BadChunkExtension => "malformed chunk extension",
            Self::BadTrailerLine => "malformed chunked trailer field",
            Self::MissingChunkCrlf => "chunk data is not followed by CRLF",
            Self::LineTooLong => "chunked framing line exceeds its bound",
            Self::TrailerTooLong => "chunked trailer section exceeds its bound",
        })
    }
}

impl std::error::Error for ChunkedError {}

/// Parses a chunk-size token as one or more ASCII hex digits. No sign,
/// whitespace, or empty token is accepted.
fn parse_chunk_size(hex: &[u8]) -> std::result::Result<usize, ChunkedError> {
    if hex.is_empty() || !hex.iter().all(u8::is_ascii_hexdigit) {
        return Err(ChunkedError::BadSizeLine);
    }
    let text = std::str::from_utf8(hex).map_err(|_| ChunkedError::BadSizeLine)?;
    usize::from_str_radix(text, 16).map_err(|_| ChunkedError::BadSizeLine)
}

fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn skip_bws(input: &[u8], position: &mut usize) {
    while input
        .get(*position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        *position += 1;
    }
}

fn consume_token(input: &[u8], position: &mut usize) -> bool {
    let start = *position;
    while input.get(*position).is_some_and(|byte| is_tchar(*byte)) {
        *position += 1;
    }
    *position > start
}

fn valid_quoted_extension(input: &[u8], position: &mut usize) -> bool {
    if input.get(*position) != Some(&b'"') {
        return false;
    }
    *position += 1;
    while let Some(&byte) = input.get(*position) {
        match byte {
            b'"' => {
                *position += 1;
                return true;
            }
            b'\\' => {
                *position += 1;
                let Some(&escaped) = input.get(*position) else {
                    return false;
                };
                if !matches!(escaped, b'\t' | b' '..=b'~' | 0x80..=0xff) {
                    return false;
                }
                *position += 1;
            }
            b'\t' | b' ' | b'!' | b'#'..=b'[' | b']'..=b'~' | 0x80..=0xff => {
                *position += 1;
            }
            _ => return false,
        }
    }
    false
}

fn validate_chunk_extensions(input: &[u8]) -> bool {
    let mut position = 0;
    while position < input.len() {
        if input[position] != b';' {
            return false;
        }
        position += 1;
        skip_bws(input, &mut position);
        if !consume_token(input, &mut position) {
            return false;
        }
        skip_bws(input, &mut position);
        if input.get(position) == Some(&b'=') {
            position += 1;
            skip_bws(input, &mut position);
            if input.get(position) == Some(&b'"') {
                if !valid_quoted_extension(input, &mut position) {
                    return false;
                }
            } else if !consume_token(input, &mut position) {
                return false;
            }
            skip_bws(input, &mut position);
        }
    }
    true
}

fn valid_trailer_field(line: &[u8]) -> bool {
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        return false;
    };
    colon > 0
        && line[..colon].iter().all(|byte| is_tchar(*byte))
        && line[colon + 1..]
            .iter()
            .all(|byte| matches!(byte, b'\t' | b' '..=b'~' | 0x80..=0xff))
}

/// Incremental HTTP/1.1 chunked-transfer decoder. Feed wire bytes via [`push`];
/// it emits decoded object bytes and reports when the complete message has been
/// consumed. Chunk extensions after `;` are ignored and trailer field lines are
/// consumed without being surfaced.
///
/// The grammar is strict and bounded. A chunk's data must be followed by an
/// exact CRLF, the message ends only at the terminating zero-size chunk *plus*
/// the empty line closing its trailer section, and both framing lines and the
/// trailer section are byte-capped so hostile framing cannot grow the carry
/// buffer.
///
/// [`push`]: ChunkedDecoder::push
///
/// Public for the `transparent_relay` bench.
#[doc(hidden)]
pub struct ChunkedDecoder {
    /// Unconsumed wire bytes carried across `push` calls.
    buf: Vec<u8>,
    state: ChunkState,
    /// Bytes consumed by the trailer section so far, bounded so a hostile
    /// trailer cannot stream forever after the terminating chunk.
    trailer_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    /// Reading the hex chunk-size line.
    Size,
    /// Copying `n` more data bytes for the current chunk.
    Data(usize),
    /// Consuming the CRLF that must follow a chunk's data.
    AfterData,
    /// Consuming trailer field lines after the terminating zero-size chunk, up
    /// to and including the empty line that ends the message.
    Trailer,
    /// The message ended: terminating zero-size chunk plus a complete trailer
    /// section.
    Done,
}

impl ChunkedDecoder {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            state: ChunkState::Size,
            trailer_bytes: 0,
        }
    }

    #[doc(hidden)]
    pub fn finished(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// Appends `input` and parses as much as possible, pushing decoded object
    /// bytes into `out`. Returns whether the **complete** message has been
    /// consumed, meaning the terminating zero-size chunk and its trailer
    /// section.
    ///
    /// Every framing fault is an error, and so is an end of the client stream
    /// before this returns `true`. Either way the body may never be completed
    /// upstream, which is what the caller must act on.
    #[doc(hidden)]
    pub fn push(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
    ) -> std::result::Result<bool, ChunkedError> {
        if self.state == ChunkState::Done {
            // Bytes after a complete message belong to the next request on the
            // connection, not to this body.
            return Ok(true);
        }
        self.buf.extend_from_slice(input);
        let mut pos = 0;
        loop {
            match self.state {
                ChunkState::Done => break,
                ChunkState::Size => {
                    let Some(rel) = find_subslice(&self.buf[pos..], b"\r\n") else {
                        if self.buf.len() - pos > MAX_CHUNK_LINE_BYTES {
                            return Err(ChunkedError::LineTooLong);
                        }
                        break;
                    };
                    if rel > MAX_CHUNK_LINE_BYTES {
                        return Err(ChunkedError::LineTooLong);
                    }
                    let line = &self.buf[pos..pos + rel];
                    let size = match line.iter().position(|byte| *byte == b';') {
                        Some(extension_start) => {
                            let mut size_end = extension_start;
                            while size_end > 0 && matches!(line[size_end - 1], b' ' | b'\t') {
                                size_end -= 1;
                            }
                            if !validate_chunk_extensions(&line[extension_start..]) {
                                return Err(ChunkedError::BadChunkExtension);
                            }
                            parse_chunk_size(&line[..size_end])?
                        }
                        None => parse_chunk_size(line)?,
                    };
                    pos += rel + 2;
                    self.state = if size == 0 {
                        ChunkState::Trailer
                    } else {
                        ChunkState::Data(size)
                    };
                }
                ChunkState::Data(remaining) => {
                    let avail = self.buf.len() - pos;
                    if avail == 0 {
                        break;
                    }
                    let take = remaining.min(avail);
                    out.extend_from_slice(&self.buf[pos..pos + take]);
                    pos += take;
                    self.state = if remaining == take {
                        ChunkState::AfterData
                    } else {
                        ChunkState::Data(remaining - take)
                    };
                }
                ChunkState::AfterData => {
                    if self.buf.len() - pos < 2 {
                        break;
                    }
                    // The delimiter is exactly CRLF. Skipping two unchecked
                    // bytes here let invalid framing decode as a complete body.
                    if &self.buf[pos..pos + 2] != b"\r\n" {
                        return Err(ChunkedError::MissingChunkCrlf);
                    }
                    pos += 2;
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => {
                    let Some(rel) = find_subslice(&self.buf[pos..], b"\r\n") else {
                        if self.buf.len() - pos > MAX_CHUNK_LINE_BYTES {
                            return Err(ChunkedError::LineTooLong);
                        }
                        break;
                    };
                    if rel > MAX_CHUNK_LINE_BYTES {
                        return Err(ChunkedError::LineTooLong);
                    }
                    self.trailer_bytes = self.trailer_bytes.saturating_add(rel + 2);
                    if self.trailer_bytes > MAX_CHUNK_TRAILER_BYTES {
                        return Err(ChunkedError::TrailerTooLong);
                    }
                    pos += rel + 2;
                    if rel == 0 {
                        // The empty line closes the trailer section.
                        self.state = ChunkState::Done;
                    } else if !valid_trailer_field(&self.buf[pos - rel - 2..pos - 2]) {
                        return Err(ChunkedError::BadTrailerLine);
                    }
                }
            }
        }
        self.buf.drain(..pos);
        Ok(self.state == ChunkState::Done)
    }
}

/// Minimal reason phrase for the statuses the write-through path emits.
fn reason_phrase(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

/// Filters origin response headers for relay to the client. Drops hop-by-hop
/// headers and the framing/marker headers the proxy sets itself
/// (`Content-Length`, `Connection`, `Transfer-Encoding`, cache markers).
///
/// Public for the `hit_serve` bench.
#[doc(hidden)]
pub fn relay_headers(origin: &[(String, String)]) -> Vec<(String, String)> {
    origin
        .iter()
        .filter(|(name, _)| is_relayable(name))
        .cloned()
        .collect()
}

fn cache_snapshot_headers(relayed: &[(String, String)]) -> Vec<(String, String)> {
    relayed
        .iter()
        .filter(|(name, _)| !is_client_scoped_response_header(name))
        .cloned()
        .collect()
}

fn is_client_scoped_response_header(name: &str) -> bool {
    // Azure's x-ms-request-id identifies the single origin operation that
    // produced this shared bursty snapshot. It is intentionally replayable;
    // x-ms-client-request-id identifies the caller and must be regenerated.
    name.eq_ignore_ascii_case("x-ms-client-request-id") || name.eq_ignore_ascii_case("set-cookie")
}

/// Returns `headers` with any `Accept-Encoding` replaced by `identity` so the
/// origin sends an uncompressed body. See [`MissFill::force_identity`].
fn identity_encoding_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("accept-encoding"))
        .cloned()
        .collect();
    out.push(("Accept-Encoding".to_string(), "identity".to_string()));
    out
}

/// Whether an origin response header should be forwarded to the client.
fn is_relayable(name: &str) -> bool {
    !matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "content-length"
            | "trailer"
            | "te"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "x-cache-status"
            | "x-cache-tier-hit"
    )
}

/// Whether a cache HIT for this request must be re-authorized against the
/// origin before serving (invariant I18). True for an authenticated request
/// (carries an `Authorization` header) that is not a presigned URL — a
/// presigned URL embeds its own signature and is validated on its own path.
fn needs_auth_recheck(head: &RequestHead) -> bool {
    header_value(&head.headers, "authorization").is_some()
        && ovcc_presigned::classify(&head.query) == PresignKind::None
}

/// Produces the process-local authorization-cache identity for one presigned
/// request. The object cache deliberately strips signatures, while this digest
/// retains the request method, the complete raw credential URI, and any S3
/// headers explicitly named by `X-Amz-SignedHeaders`. The method is hashed
/// because the probe is method-faithful and an Azure SAS signature does not
/// cover the verb, so `GET` and `HEAD` must not share a witness. Raw
/// credentials are never stored or logged.
fn presigned_auth_key(head: &RequestHead, host: &str, kind: PresignKind) -> CredentialDigest {
    fn field(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    field(&mut hasher, b"ovcc-presigned-auth-v2");
    field(&mut hasher, canonical_method(&head.method).as_bytes());
    field(
        &mut hasher,
        match kind {
            PresignKind::S3 => b"s3",
            PresignKind::Azure => b"azure",
            PresignKind::None => b"none",
        },
    );
    field(&mut hasher, host.to_ascii_lowercase().as_bytes());
    field(&mut hasher, head.path.as_bytes());
    field(&mut hasher, head.query.as_bytes());

    if kind == PresignKind::S3
        && let Some(signed_headers) = query_param_decoded(&head.query, "X-Amz-SignedHeaders")
    {
        let mut names = signed_headers
            .split(';')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        for name in names {
            field(&mut hasher, name.as_bytes());
            let value = if name == "host" {
                Some(host)
            } else {
                header_value(&head.headers, &name)
            };
            field(&mut hasher, value.unwrap_or_default().as_bytes());
        }
    }

    hasher.finalize().into()
}

/// Reuses positive presigned authorization: one request probes the origin,
/// concurrent identical credentials wait for that result, and a successful
/// witness expires before the credential itself.
async fn authorize_presigned_request(
    cache: &Arc<AuthorizationCache>,
    presigned: &Presigned,
    auth_upstream: &str,
    head: &RequestHead,
    host: &str,
) -> PresignedAuthorization {
    let Some(expires_at) = presigned.expires_at_unix else {
        return PresignedAuthorization::Failed;
    };
    let key = presigned_auth_key(head, host, presigned.kind);
    match cache.acquire(key, now_unix()) {
        AuthorizationAcquire::Authorized => PresignedAuthorization::Cached,
        AuthorizationAcquire::Wait(waiter) => match waiter.wait().await {
            AuthorizationProbeOutcome::Authorized => PresignedAuthorization::Authorized,
            AuthorizationProbeOutcome::AuthorizedUncached => {
                PresignedAuthorization::AuthorizedUncached
            }
            AuthorizationProbeOutcome::Rejected => PresignedAuthorization::Rejected,
            AuthorizationProbeOutcome::Unavailable => PresignedAuthorization::Failed,
        },
        AuthorizationAcquire::Probe(permit) => {
            let url = origin_url(auth_upstream, head);
            let started = Instant::now();
            // The signature covers the HTTP verb, so the probe uses the
            // request's own method. A `GET` probe against a HEAD-presigned URL
            // fails signature verification and would be read as a rejection. A
            // HEAD probe carries the caller's headers verbatim, exactly as the
            // bursty HEAD fill already does.
            let result = if head.method.eq_ignore_ascii_case("HEAD") {
                ovcc_upstream::fetch_head(&url, &head.headers)
                    .instrument(tracing::info_span!("ovcc.access.probe"))
                    .await
            } else {
                ovcc_upstream::probe_access(&url, &head.headers)
                    .instrument(tracing::info_span!("ovcc.access.probe"))
                    .await
            };
            ovcc_telemetry::metrics::record_access_probe(&AccessProbe {
                host,
                method: canonical_method(&head.method),
                status: result.as_ref().ok().map(|origin| origin.status),
                duration_seconds: started.elapsed().as_secs_f64(),
            });
            let (outcome, response) = match result {
                Ok(origin) if matches!(origin.status, 200 | 206) => (
                    AuthorizationProbeOutcome::Authorized,
                    PresignedAuthorization::Authorized,
                ),
                Ok(origin)
                    if origin.status == 416
                        && header_value(&origin.headers, "content-range").is_some_and(
                            |value| value.trim().eq_ignore_ascii_case("bytes */0"),
                        ) =>
                {
                    (
                        AuthorizationProbeOutcome::AuthorizedUncached,
                        PresignedAuthorization::AuthorizedUncached,
                    )
                }
                Ok(origin) if origin.status == 403 => (
                    AuthorizationProbeOutcome::Rejected,
                    PresignedAuthorization::Rejected,
                ),
                // An authenticated `404` accepts the credential, since a bad
                // signature answers `403` instead, so it releases current
                // waiters without retaining a witness. Treating it as a failure
                // would force a MISS on every presigned bursty request for a
                // missing object and defeat short-TTL `404` admission.
                Ok(origin) if origin.status == 404 => (
                    AuthorizationProbeOutcome::AuthorizedUncached,
                    PresignedAuthorization::AuthorizedUncached,
                ),
                Ok(_) => (
                    AuthorizationProbeOutcome::Unavailable,
                    PresignedAuthorization::Failed,
                ),
                Err(_) => (
                    AuthorizationProbeOutcome::Unavailable,
                    PresignedAuthorization::Failed,
                ),
            };
            permit.complete(outcome, expires_at, now_unix());
            response
        }
    }
}

/// The shared pre-cache access gate, running presigned expiry validation,
/// cache-bypass policy, and independent presigned authorization in that order.
///
/// Every cache-listener serve arm that may read or share a cached entry runs
/// this before its cache lookup, so no cached byte or metadata is exposed to a
/// malformed, expired, or unauthorized S3 or Azure credential, and no entry is
/// reused across a control that is absent from its cache identity. See the
/// `Presigned` invariant in `AGENTS.md`. The ordinary GET path and the bursty
/// list and HEAD path share it and differ only in `bypass`.
async fn evaluate_access(
    presigned_auth: &Arc<AuthorizationCache>,
    head: &RequestHead,
    host: &str,
    auth_upstream: &str,
    flags: &mut RequestFlags,
    bypass: BypassPolicy,
) -> AccessDecision {
    // Handles presigned URLs by kind.
    //    - malformed (missing/invalid time params) -> 400 Bad Request.
    //    - expired -> force a MISS/refetch so a stale entry is never served as a
    //      fresh HIT (upstream re-authorizes and typically 403s).
    //    - valid -> normal cacheable path.
    let mut forced_miss = false;
    let mut presigned = None;
    if ovcc_presigned::classify(&head.query) != PresignKind::None {
        let parsed = if bypass.local_mode {
            ovcc_presigned::parse_for_private_reuse(&head.query)
        } else {
            ovcc_presigned::validate(&head.query, now_unix())
        };
        match parsed {
            Ok(validated) => presigned = Some(validated),
            // `validate` reports an already-expired signature with this exact
            // message; any other error means the presigned URL is malformed.
            Err(err) if err.message() == "expired" => forced_miss = !bypass.local_mode,
            Err(_) => {
                return AccessDecision::Reject(Box::new(Response::buffered(
                    400,
                    "Bad Request",
                    Some("BYPASS"),
                    Vec::new(),
                    b"Invalid presigned URL\n".to_vec(),
                )));
            }
        }
    }

    // Bypasses the cache entirely when `Cache-Control: no-cache` is specified,
    //    fetching fresh from the origin and serving it without reading or writing
    //    the cache (existing entries are left intact, so a later normal request
    //    still HITs). Azure customer-provided-key, tag-condition, and
    //    lease-condition requests bypass for the same reason: the control is not
    //    part of any cache identity, so no stored entry may answer them.
    let no_cache = requests_no_cache(&head.headers);
    let customer_provided_key_bypass =
        bypass.is_azure && azure_has_customer_provided_key(&head.headers);
    let origin_validation_bypass =
        bypass.is_azure && azure_requires_origin_validation(&head.headers);
    if no_cache
        || bypass.versioning
        || bypass.uri_excluded
        || customer_provided_key_bypass
        || origin_validation_bypass
    {
        flags.disable_cache = true;
        return AccessDecision::Bypass;
    }

    // Independently signed callers may share object bytes only after the exact
    // credential digest has a positive authorization witness. A rejection
    // never reads or evicts object data.
    if !bypass.local_mode
        && let Some(presigned) = presigned.as_ref()
    {
        let authorization =
            authorize_presigned_request(presigned_auth, presigned, auth_upstream, head, host).await;
        flags.presigned_auth_outcome = authorization.as_str();
        match authorization {
            PresignedAuthorization::Cached
            | PresignedAuthorization::Authorized
            | PresignedAuthorization::AuthorizedUncached => {}
            PresignedAuthorization::Rejected => {
                return AccessDecision::Reject(Box::new(Response::buffered(
                    403,
                    "Forbidden",
                    Some("BYPASS"),
                    Vec::new(),
                    b"Forbidden\n".to_vec(),
                )));
            }
            PresignedAuthorization::Failed => forced_miss = true,
        }
    }

    AccessDecision::Proceed { forced_miss }
}

/// Whether a cached object has mutated relative to a fresh origin re-check.
/// ETag takes precedence when both sides provide it; otherwise Last-Modified is
/// used when available on both sides. Missing validators are inconclusive.
fn is_mutated(cached: &StoredMeta, fresh_headers: &[(String, String)]) -> bool {
    if let (Some(cached_etag), Some(fresh_etag)) =
        (cached.etag.as_deref(), header_value(fresh_headers, "etag"))
    {
        return cached_etag != fresh_etag;
    }

    match (
        cached.last_modified.as_deref(),
        header_value(fresh_headers, "last-modified"),
    ) {
        (Some(cached_lm), Some(fresh_lm)) => cached_lm != fresh_lm,
        _ => false,
    }
}

/// Whether `first_byte` looks like the start of a TLS handshake record (the
/// `handshake` content type, `0x16`). Used to distinguish a real TLS client
/// from a plaintext one that mis-dialed the TLS port, which receives `400`.
fn looks_like_tls_client_hello(first_byte: u8) -> bool {
    first_byte == 0x16
}

/// Renders a negotiated TLS version in nginx `$ssl_protocol` form (`TLSv1.3`).
fn tls_version_str(version: rustls::ProtocolVersion) -> String {
    match version {
        rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3".to_owned(),
        rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2".to_owned(),
        other => format!("{other:?}"),
    }
}

/// The canonical (uppercase) HTTP method used as a bursty cache-key segment.
/// Only `GET`/`HEAD` reach the bursty arm (other methods are proxied earlier),
/// and the key embeds the method so GET and HEAD never share an entry.
fn canonical_method(method: &str) -> &'static str {
    if method.eq_ignore_ascii_case("HEAD") {
        "HEAD"
    } else {
        "GET"
    }
}

/// Reconstructs the original request target (`path` + `?query`) — the
/// `$request_uri` used for bursty rule matching and bursty cache keying.
fn request_uri(head: &RequestHead) -> String {
    if head.query_present {
        format!("{}?{}", head.path, head.query)
    } else {
        head.path.clone()
    }
}

/// Builds the origin URL for a backend fetch from the request path + query.
fn origin_url(upstream: &str, head: &RequestHead) -> String {
    if head.query_present {
        format!("{upstream}{}?{}", head.path, head.query)
    } else {
        format!("{upstream}{}", head.path)
    }
}

/// Returns true if `host` (with any `:port` stripped) names this proxy itself.
fn is_self_host(host: &str, self_names: &[String]) -> bool {
    let bare = host.split(':').next().unwrap_or(host);
    self_names.iter().any(|name| name == bare)
}

/// Returns the HTTP reason phrase for a cacheable MISS status.
fn miss_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        404 => "Not Found",
        _ => "",
    }
}

/// Whether an origin response is cache-admissible for this fill.
fn should_admit(status: u16, admit_not_found: bool) -> bool {
    status == 200 || status == 206 || (admit_not_found && status == 404)
}

/// Whether Nucleus origin response metadata permits storing the response.
///
/// Multiple cache-control header lines and comma-separated directives are
/// treated as one directive list. Invalid `max-age` and `Expires` values are
/// ignored, matching a conservative interpretation of otherwise cacheable
/// origin metadata without turning malformed dates into proxy failures.
fn origin_response_is_cacheable(headers: &[(String, String)], now: SystemTime) -> bool {
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
    {
        return false;
    }

    for value in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
        .map(|(_, value)| value.as_str())
    {
        for directive in value.split(',') {
            let (name, value) = directive
                .split_once('=')
                .map_or((directive.trim(), None), |(name, value)| {
                    (name.trim(), Some(value.trim().trim_matches('"')))
                });
            if name.eq_ignore_ascii_case("no-store")
                || name.eq_ignore_ascii_case("no-cache")
                || name.eq_ignore_ascii_case("private")
            {
                return false;
            }
            if name.eq_ignore_ascii_case("max-age")
                && value.and_then(|value| value.parse::<u64>().ok()) == Some(0)
            {
                return false;
            }
        }
    }

    !headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("expires"))
        .filter_map(|(_, value)| httpdate::parse_http_date(value).ok())
        .any(|expires| expires < now)
}

/// Parses `Content-Length` from an origin header list, when present and valid.
fn parse_content_length(headers: &[(String, String)]) -> Option<u64> {
    header_value(headers, "content-length").and_then(|v| v.trim().parse::<u64>().ok())
}

/// Records upstream timing fields on the active `ovcc.upstream.fetch` span.
fn record_upstream_fetch_timing(timing: &UpstreamTiming) {
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

/// Async writer whose inactivity clock arms only while the inner writer is
/// pending and resets after every positive write or completed flush.
struct ProgressTimeoutWriter<'a, S> {
    inner: &'a mut S,
    timeout: Duration,
    stall: Option<Pin<Box<tokio::time::Sleep>>>,
    armed: bool,
}

impl<'a, S> ProgressTimeoutWriter<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            timeout: DOWNSTREAM_WRITE_STALL_TIMEOUT,
            stall: None,
            armed: false,
        }
    }

    #[cfg(test)]
    fn with_timeout(inner: &'a mut S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            stall: None,
            armed: false,
        }
    }

    fn poll_stall(&mut self, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.armed {
            let deadline = tokio::time::Instant::now() + self.timeout;
            if let Some(stall) = &mut self.stall {
                stall.as_mut().reset(deadline);
            } else {
                self.stall = Some(Box::pin(tokio::time::sleep_until(deadline)));
            }
            self.armed = true;
        }
        let stall = self.stall.as_mut().expect("armed stall timer");
        if stall.as_mut().poll(context).is_ready() {
            mark_downstream_timeout();
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "downstream write inactivity timeout",
            )))
        } else {
            Poll::Pending
        }
    }

    fn progressed(&mut self) {
        self.armed = false;
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ProgressTimeoutWriter<'_, S> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut *this.inner).poll_write(context, bytes) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    this.progressed();
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => match this.poll_stall(context) {
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) | Poll::Pending => Poll::Pending,
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut *this.inner).poll_flush(context) {
            Poll::Ready(result) => {
                if result.is_ok() {
                    this.progressed();
                }
                Poll::Ready(result)
            }
            Poll::Pending => this.poll_stall(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_shutdown(context)
    }
}

async fn timed_write_all<S: AsyncWrite + Unpin>(
    stream: &mut S,
    bytes: &[u8],
) -> std::io::Result<()> {
    ProgressTimeoutWriter::new(stream).write_all(bytes).await
}

async fn timed_flush<S: AsyncWrite + Unpin>(stream: &mut S) -> std::io::Result<()> {
    ProgressTimeoutWriter::new(stream).flush().await
}

fn is_downstream_timeout(error: &BoxError) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
}

fn origin_timeout_stage(stage: ovcc_upstream::OriginTimeoutStage) -> TimeoutStage {
    match stage {
        ovcc_upstream::OriginTimeoutStage::Connect => TimeoutStage::OriginConnect,
        ovcc_upstream::OriginTimeoutStage::ResponseHeader => TimeoutStage::OriginResponseHeader,
        ovcc_upstream::OriginTimeoutStage::ResponseBody => TimeoutStage::OriginResponseBody,
    }
}

#[derive(Clone, Copy)]
struct UpstreamFailureResponse {
    status: u16,
    reason: &'static str,
    body: &'static [u8],
    headers: &'static [(&'static str, &'static str)],
    timeout_stage: Option<TimeoutStage>,
}

const CONNECTION_TERMINATED_PROXY_STATUS: &[(&str, &str)] =
    &[("Proxy-Status", "ovcc; error=connection_terminated")];

fn upstream_failure_response(error: &ovcc_core::Error) -> UpstreamFailureResponse {
    if let Some(stage) = ovcc_upstream::timeout_stage(error).map(origin_timeout_stage) {
        return UpstreamFailureResponse {
            status: 504,
            reason: "Gateway Timeout",
            body: GATEWAY_TIMEOUT_BODY,
            headers: &[],
            timeout_stage: Some(stage),
        };
    }
    if error.is_upstream_connection_failure() {
        return UpstreamFailureResponse {
            status: 503,
            reason: "Service Unavailable",
            body: b"",
            headers: CONNECTION_TERMINATED_PROXY_STATUS,
            timeout_stage: None,
        };
    }
    UpstreamFailureResponse {
        status: 502,
        reason: "Bad Gateway",
        body: BAD_GATEWAY_BODY,
        headers: &[],
        timeout_stage: None,
    }
}

fn owned_upstream_failure_headers(failure: UpstreamFailureResponse) -> Vec<(String, String)> {
    failure
        .headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn abandon_fill_after_origin_error(
    permit: Option<FillPermit>,
    error: &ovcc_core::Error,
    cache_status: &'static str,
    head_only: bool,
) -> Response {
    drop(permit);
    let failure = upstream_failure_response(error);
    let response = if head_only {
        Response::head(
            failure.status,
            failure.reason,
            Some(cache_status),
            owned_upstream_failure_headers(failure),
            None,
        )
    } else {
        Response::buffered(
            failure.status,
            failure.reason,
            Some(cache_status),
            owned_upstream_failure_headers(failure),
            failure.body.to_vec(),
        )
    };
    match failure.timeout_stage {
        Some(stage) => response.with_timeout_stage(stage),
        None => response,
    }
}

fn origin_body_timed_out<E: std::error::Error + 'static>(error: &E) -> bool {
    (error as &dyn std::error::Error)
        .downcast_ref::<ovcc_upstream::OriginBodyError>()
        .and_then(ovcc_upstream::OriginBodyError::timeout_stage)
        == Some(ovcc_upstream::OriginTimeoutStage::ResponseBody)
}

/// Whether a response with `status` carries a body its framing can delimit.
/// `1xx`, `204`, and `304` responses end at the header block, so a relayed
/// chunk terminator after one would desynchronize a reused connection.
fn status_has_body(status: u16) -> bool {
    !(100..200).contains(&status) && status != 204 && status != 304
}

/// Writes a streaming response head (shared by the MISS-fill and bypass arms).
/// Emits `Content-Length` when known, otherwise `Transfer-Encoding: chunked` for
/// the body that follows. `keep_alive` is the arm's promise to finish that
/// framing and its precondition for reusing the connection.
#[allow(clippy::too_many_arguments)]
async fn write_streaming_head<S>(
    stream: &mut S,
    status: u16,
    reason: &str,
    cache_status: Option<&str>,
    extra_headers: &[(&str, &str)],
    content_length: Option<u64>,
    chunked: bool,
    keep_alive: bool,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    mark_downstream_response_started(status);
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    if let Some(cache_status) = cache_status {
        head.push_str(&format!("X-Cache-Status: {cache_status}\r\n"));
    }
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if status_has_body(status) || status == 304 {
        if let Some(content_length) = content_length {
            head.push_str(&format!("Content-Length: {content_length}\r\n"));
        } else if chunked && status_has_body(status) {
            head.push_str("Transfer-Encoding: chunked\r\n");
        }
    }
    let connection = if keep_alive { "keep-alive" } else { "close" };
    head.push_str(&format!("Connection: {connection}\r\n\r\n"));
    timed_write_all(stream, head.as_bytes()).await
}

/// Writes one HTTP/1.1 chunked body fragment (`<hex>\r\n<data>\r\n`).
async fn write_http_chunk<S>(stream: &mut S, chunk: &[u8]) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_http_chunk_counted(stream, chunk)
        .await
        .map(|_| ())
        .map_err(|failure| failure.error)
}

/// Writes a complete buffered HTTP/1.1 response with `X-Cache-Status` set and a
/// `Connection: close` (one request per connection).
async fn write_response<S>(
    stream: &mut S,
    status: u16,
    reason: &str,
    cache_status: &str,
    body: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response_with(stream, status, reason, &[], Some(cache_status), body).await
}

// Keeps inline/operator replies subject to the same HEAD rule as routed replies.
async fn write_method_response_with<S>(
    stream: &mut S,
    method: &str,
    code: u16,
    reason: &'static str,
    headers: &[(&str, &str)],
    cache_status: Option<&'static str>,
    body: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    if method.eq_ignore_ascii_case("HEAD") {
        mark_downstream_response_started(code);
        let response = Response::head(
            code,
            reason,
            cache_status,
            headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            None,
        );
        timed_write_all(stream, render_head(&response.head, false).as_bytes()).await?;
        timed_flush(stream).await
    } else {
        write_response_with(stream, code, reason, headers, cache_status, body).await
    }
}

/// Writes a buffered HTTP/1.1 response with optional extra headers and an
/// optional `X-Cache-Status` (omitted when `None`, e.g. on the passthrough
/// path). Always sets `Content-Length` and `Connection: close`.
async fn write_response_with<S>(
    stream: &mut S,
    status: u16,
    reason: &str,
    extra_headers: &[(&str, &str)],
    cache_status: Option<&str>,
    mut body: &[u8],
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    write_response_buf_with(
        stream,
        status,
        reason,
        extra_headers,
        cache_status,
        &mut body,
        false,
    )
    .await
}

/// Writes a complete `Content-Length` response. `keep_alive` announces the
/// reuse that a successful return makes safe.
async fn write_response_buf_with<S, B>(
    stream: &mut S,
    status: u16,
    reason: &str,
    extra_headers: &[(&str, &str)],
    cache_status: Option<&str>,
    body: &mut B,
    keep_alive: bool,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
    B: Buf,
{
    mark_downstream_response_started(status);
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    if let Some(cache_status) = cache_status {
        head.push_str(&format!("X-Cache-Status: {cache_status}\r\n"));
    }
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let connection = if keep_alive { "keep-alive" } else { "close" };
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: {connection}\r\n\r\n",
        body.remaining()
    ));
    timed_write_all(stream, head.as_bytes()).await?;
    write_buf_counted(stream, body)
        .await
        .map_err(|failure| failure.error)?;
    timed_flush(stream).await?;
    Ok(())
}

/// A request head read off a connection.
struct ReadHead {
    head: RequestHead,
    /// Bytes read past the head's terminator, which the streaming-proxy path
    /// consumes first.
    body_prefix: Vec<u8>,
    /// Whether the next request would start exactly at `body_prefix`. That
    /// takes a `\r\n\r\n` terminator within [`MAX_HEAD_BYTES`] and strictly
    /// delimited lines, since an oversized or truncated head leaves its own
    /// tail unread, and a loose line can hide a field another parser honors.
    delimited: bool,
}

/// Reads until the end of the request head (`\r\n\r\n`) and parses it, or
/// returns `None` on a malformed or empty request.
async fn read_request_head<S>(stream: &mut S) -> std::io::Result<Option<ReadHead>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let mut boundary = None;
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        let prev = buf.len();
        buf.extend_from_slice(&chunk[..n]);
        // Scan from just before the newly appended bytes so a `\r\n\r\n` split
        // across reads is still found, without rescanning the whole buffer.
        let scan_from = prev.saturating_sub(3);
        if let Some(rel) = find_subslice(&buf[scan_from..], b"\r\n\r\n") {
            boundary = Some(scan_from + rel);
            break;
        }
        if buf.len() > MAX_HEAD_BYTES {
            break;
        }
    }

    let (head_bytes, body_prefix) = match boundary {
        Some(pos) => (&buf[..pos], buf[pos + 4..].to_vec()),
        None => (&buf[..], Vec::new()),
    };
    let delimited = boundary.is_some() && head_lines_are_strict(head_bytes);
    Ok(parse_request_head(head_bytes).map(|head| ReadHead {
        head,
        body_prefix,
        delimited,
    }))
}

/// Whether every head line ends with CRLF and every field line is
/// `name: value` with no whitespace before the colon or at the line start. A
/// bare CR or LF, a folded line, or a line without a colon is one this parser
/// reads differently from others, so a front proxy could see a body framing
/// field that this parser drops.
fn head_lines_are_strict(head: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    let mut lines = text.split("\r\n");
    let request_line_strict = lines
        .next()
        .is_some_and(|line| !line.contains(['\r', '\n']));
    request_line_strict
        && lines.all(|line| {
            !line.contains(['\r', '\n'])
                && !line.starts_with([' ', '\t'])
                && line
                    .split_once(':')
                    .is_some_and(|(name, _)| !name.is_empty() && !name.ends_with([' ', '\t']))
        })
}

/// Returns the index of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parses the request line and the `Host` header. Path and query are split on
/// the first `?`. Header names are matched case-insensitively.
fn parse_request_head(buf: &[u8]) -> Option<RequestHead> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");

    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    // The request-line version token (e.g. `HTTP/1.1`), for the access log.
    let http_version = parts.next().unwrap_or_default().to_string();

    let (path, query, query_present) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string(), true),
        None => (target.to_string(), String::new(), false),
    };

    let mut host = None;
    let mut content_length = 0usize;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("host") {
                host = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name.to_string(), value.to_string()));
        }
    }

    Some(RequestHead {
        method,
        path,
        query,
        query_present,
        http_version,
        host,
        headers,
        content_length,
        head_length: buf.len().saturating_add(4),
    })
}

/// Returns the value of the first `name=value` pair in a raw query string.
fn query_param<'a>(raw_query: &'a str, name: &str) -> Option<&'a str> {
    raw_query.split('&').find_map(|pair| {
        pair.split_once('=')
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value)
    })
}

/// Case-insensitive lookup of the first value for header `name`.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Returns a declared payload size only when request framing makes it known.
fn http_request_body_size(head: &RequestHead) -> Option<u64> {
    if header_value(&head.headers, "transfer-encoding").is_some() {
        return None;
    }
    match header_value(&head.headers, "content-length") {
        Some(value) => value.parse().ok(),
        None => Some(0),
    }
}

/// Returns the range that selects the response representation and cache key.
/// Azure gives a non-empty `x-ms-range` precedence over the standard header;
/// all other backends use `Range` only.
fn effective_range(headers: &[(String, String)], is_azure: bool) -> &str {
    if is_azure
        && let Some(range) = header_value(headers, "x-ms-range").filter(|range| !range.is_empty())
    {
        return range;
    }
    header_value(headers, "range").unwrap_or_default()
}

/// Reconstructs Azure logical descriptors without authorizing their use for a backend.
/// The configured storage policy independently validates backend ownership.
pub fn azure_optional_key(key: &str) -> Option<ovcc_cachekey::LogicalKey> {
    azure_optional_keys::AzurePolicy::from_key(key)
}

/// Extends Azure's effective range with fields that select a distinct Get Blob
/// representation or checksum response. Plain ranges retain their legacy key.
fn azure_get_variant(headers: &[(String, String)], effective_range: &str) -> String {
    let fields = [
        (
            "structured",
            header_value(headers, "x-ms-structured-body").unwrap_or_default(),
        ),
        (
            "md5",
            header_value(headers, "x-ms-range-get-content-md5").unwrap_or_default(),
        ),
        (
            "crc64",
            header_value(headers, "x-ms-range-get-content-crc64").unwrap_or_default(),
        ),
        (
            "version",
            header_value(headers, "x-ms-version").unwrap_or_default(),
        ),
        ("upn", header_value(headers, "x-ms-upn").unwrap_or_default()),
        (
            "origin",
            header_value(headers, "origin").unwrap_or_default(),
        ),
    ];
    if fields.iter().all(|(_, value)| value.is_empty()) {
        return effective_range.to_owned();
    }

    let mut variant = format!("range={}:{}", effective_range.len(), effective_range);
    for (name, value) in fields {
        variant.push(';');
        variant.push_str(name);
        variant.push('=');
        variant.push_str(&value.len().to_string());
        variant.push(':');
        variant.push_str(value);
    }
    variant
}

fn azure_seed_variant(headers: &[(String, String)]) -> String {
    let selected: Vec<(String, String)> = ["x-ms-version", "origin"]
        .into_iter()
        .filter_map(|name| {
            header_value(headers, name).map(|value| (name.to_owned(), value.to_owned()))
        })
        .collect();
    azure_get_variant(&selected, "")
}

fn azure_seed_response_headers(
    request_headers: &[(String, String)],
    origin_headers: &[(String, String)],
) -> Vec<(String, String)> {
    let mut seeded = Vec::new();
    for (request_name, response_name) in [
        ("x-ms-blob-cache-control", "Cache-Control"),
        ("x-ms-blob-content-disposition", "Content-Disposition"),
        ("x-ms-blob-content-encoding", "Content-Encoding"),
        ("x-ms-blob-content-language", "Content-Language"),
        ("x-ms-blob-content-md5", "Content-MD5"),
        ("x-ms-encryption-scope", "x-ms-encryption-scope"),
    ] {
        if let Some(value) = header_value(request_headers, request_name) {
            seeded.push((response_name.to_owned(), value.to_owned()));
        }
    }
    let content_type = header_value(request_headers, "x-ms-blob-content-type")
        .or_else(|| header_value(request_headers, "content-type"))
        .unwrap_or("application/octet-stream");
    seeded.push(("Content-Type".to_owned(), content_type.to_owned()));
    let blob_type = header_value(request_headers, "x-ms-blob-type").unwrap_or("BlockBlob");
    seeded.push(("x-ms-blob-type".to_owned(), blob_type.to_owned()));
    let (lease_state, lease_status) = if header_value(request_headers, "x-ms-lease-id").is_some() {
        ("leased", "locked")
    } else {
        ("available", "unlocked")
    };
    seeded.push(("x-ms-lease-state".to_owned(), lease_state.to_owned()));
    seeded.push(("x-ms-lease-status".to_owned(), lease_status.to_owned()));
    if let Some(value) = header_value(origin_headers, "x-ms-request-server-encrypted") {
        seeded.push(("x-ms-server-encrypted".to_owned(), value.to_owned()));
    }
    for name in ["x-ms-version-id", "x-ms-encryption-scope"] {
        if let Some(value) = header_value(origin_headers, name) {
            seeded.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
            seeded.push((name.to_owned(), value.to_owned()));
        }
    }
    seeded.extend(
        request_headers
            .iter()
            .filter(|(name, _)| name.to_ascii_lowercase().starts_with("x-ms-meta-"))
            .cloned(),
    );
    seeded
}

/// Whether any `Cache-Control` line carries `no-cache`. RFC 9110 §5.3 joins
/// repeated lines into one directive list, so [`header_value`], which returns
/// only the first line, would miss a directive on a later one. The
/// case-insensitive substring search errs toward a bypass, which is always safe.
fn requests_no_cache(headers: &[(String, String)]) -> bool {
    const NO_CACHE: &[u8] = b"no-cache";
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("cache-control")
            && value
                .as_bytes()
                .windows(NO_CACHE.len())
                .any(|window| window.eq_ignore_ascii_case(NO_CACHE))
    })
}

fn azure_has_customer_provided_key(headers: &[(String, String)]) -> bool {
    [
        "x-ms-encryption-key",
        "x-ms-encryption-key-sha256",
        "x-ms-encryption-algorithm",
    ]
    .into_iter()
    .any(|name| header_value(headers, name).is_some_and(|value| !value.is_empty()))
}

fn azure_requires_origin_validation(headers: &[(String, String)]) -> bool {
    ["x-ms-if-tags", "x-ms-lease-id"]
        .into_iter()
        .any(|name| header_value(headers, name).is_some_and(|value| !value.is_empty()))
}

/// Seconds since the Unix epoch, for presigned-URL expiry checks.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod blockassembly_state_machine_tests;

#[cfg(test)]
mod bursty_cache_state_machine_tests;

#[cfg(test)]
mod bursty_routing_state_machine_tests;

#[cfg(test)]
mod connection_close_state_machine_tests;

#[cfg(test)]
mod serve_protocol_state_machine_tests;

#[cfg(test)]
mod origin_fill_follower_state_machine_tests;

#[cfg(test)]
mod origin_follower_eligibility_tests;

#[cfg(test)]
mod storage_shutdown_state_machine_tests;

#[cfg(test)]
mod timeout_lifecycle_state_machine_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use ovcc_storage::Lookup;
    use tracing::field::{Field, Visit};
    use tracing::instrument::WithSubscriber as _;
    use tracing::{Event, Subscriber};
    use tracing_subscriber::filter::LevelFilter;
    use tracing_subscriber::layer::{Context as LayerContext, Layer, SubscriberExt as _};

    #[tokio::test]
    async fn proxy_scope_downstream_state_isolated_across_polls() {
        with_downstream_request_state(async {
            mark_downstream_timeout();
            mark_downstream_response_started(206);
            add_downstream_body_bytes(42);
            with_downstream_request_state(async {
                tokio::task::yield_now().await;
                assert!(!downstream_timed_out());
                assert!(!downstream_response_started());
                assert_eq!(downstream_response_status(), 500);
                assert_eq!(downstream_body_bytes(), 0);
            })
            .await;
            assert!(downstream_timed_out());
            assert!(downstream_response_started());
            assert_eq!(downstream_response_status(), 206);
            assert_eq!(downstream_body_bytes(), 42);
        })
        .await;
        assert!(!downstream_timed_out());
        assert!(!downstream_response_started());
        assert_eq!(downstream_body_bytes(), 0);
    }

    #[test]
    fn proxy_get_request_futures_stay_within_budget() {
        fn fut_size<A, F: Future>(_: fn(A) -> F) -> usize {
            std::mem::size_of::<F>()
        }
        fn connection_size<A, B, C, D, E, F: Future>(_: fn(A, B, C, D, E) -> F) -> usize {
            std::mem::size_of::<F>()
        }
        let route_bytes = fut_size(route);
        let connection_bytes = connection_size(handle_connection);
        println!("route={route_bytes} handle_connection={connection_bytes}");
        // Pinned rustc 1.96.0, opt-level=1; headroom accommodates platform layout.
        assert!(
            route_bytes <= 14_000,
            "route future grew to {route_bytes} bytes"
        );
        assert!(
            connection_bytes <= 8_192,
            "connection future grew to {connection_bytes} bytes"
        );
    }

    #[test]
    fn s3_get_presigned_authorization_identity_keeps_credentials_separate() {
        // Arrange.
        let mut first = get_head("bucket.s3.example", "/asset.usd");
        first.query = "X-Amz-Signature=first&X-Amz-Date=20300101T000000Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host%3Bx-amz-checksum-mode".to_owned();
        first
            .headers
            .push(("x-amz-checksum-mode".to_owned(), "ENABLED".to_owned()));
        let mut other_signature = first.clone();
        other_signature.query = other_signature.query.replace("first", "second");
        let mut other_signed_header = first.clone();
        other_signed_header.headers[0].1 = "DISABLED".to_owned();
        let mut head_request = first.clone();
        head_request.method = "HEAD".to_owned();

        // Act.
        let first_key = presigned_auth_key(&first, "bucket.s3.example", PresignKind::S3);
        let repeated_key = presigned_auth_key(&first, "bucket.s3.example", PresignKind::S3);
        let other_signature_key =
            presigned_auth_key(&other_signature, "bucket.s3.example", PresignKind::S3);
        let other_signed_header_key =
            presigned_auth_key(&other_signed_header, "bucket.s3.example", PresignKind::S3);
        let head_key = presigned_auth_key(&head_request, "bucket.s3.example", PresignKind::S3);

        // Assert.
        assert_eq!(first_key, repeated_key);
        assert_ne!(first_key, other_signature_key);
        assert_ne!(first_key, other_signed_header_key);
        assert_ne!(first_key, head_key);
    }

    #[test]
    fn azure_effective_range_prefers_nonempty_sdk_header() {
        let both = vec![
            ("Range".to_owned(), "bytes=8-11".to_owned()),
            ("x-ms-range".to_owned(), "bytes=4-7".to_owned()),
        ];
        let empty_sdk = vec![
            ("Range".to_owned(), "bytes=8-11".to_owned()),
            ("x-ms-range".to_owned(), String::new()),
        ];

        assert_eq!(effective_range(&both, true), "bytes=4-7");
        assert_eq!(effective_range(&empty_sdk, true), "bytes=8-11");
        assert_eq!(effective_range(&both, false), "bytes=8-11");
    }

    #[test]
    fn azure_get_variant_isolates_structured_and_checksum_responses() {
        let range = vec![("x-ms-range".to_owned(), "bytes=0-1023".to_owned())];
        let mut md5 = range.clone();
        md5.push(("x-ms-range-get-content-md5".to_owned(), "true".to_owned()));
        let mut crc64 = range.clone();
        crc64.push(("x-ms-range-get-content-crc64".to_owned(), "true".to_owned()));
        let structured = vec![(
            "x-ms-structured-body".to_owned(),
            "XSM/1.0; properties=crc64".to_owned(),
        )];

        assert_eq!(azure_get_variant(&range, "bytes=0-1023"), "bytes=0-1023");
        assert_ne!(
            azure_get_variant(&md5, "bytes=0-1023"),
            azure_get_variant(&crc64, "bytes=0-1023")
        );
        assert_ne!(azure_get_variant(&structured, ""), "");
    }

    #[test]
    fn azure_get_variant_keys_version_and_origin_like_write_through_seed() {
        let headers = vec![
            ("x-ms-version".to_owned(), "2026-06-06".to_owned()),
            ("Origin".to_owned(), "https://client.example".to_owned()),
        ];
        let other_version = vec![
            ("x-ms-version".to_owned(), "2025-11-05".to_owned()),
            ("Origin".to_owned(), "https://client.example".to_owned()),
        ];

        assert_eq!(
            azure_get_variant(&headers, ""),
            azure_seed_variant(&headers)
        );
        assert_ne!(
            azure_get_variant(&headers, ""),
            azure_get_variant(&other_version, "")
        );
        assert_ne!(
            azure_get_variant(&[("x-ms-upn".to_owned(), "true".to_owned())], ""),
            azure_get_variant(&[("x-ms-upn".to_owned(), "false".to_owned())], "")
        );
    }

    #[test]
    fn azure_put_write_through_seed_preserves_blob_properties_and_metadata() {
        // Microsoft: each x-ms-blob-content-* property is stored with the blob
        // and returned by a later read request.
        // https://learn.microsoft.com/en-us/rest/api/storageservices/put-blob#request-headers-all-blob-types
        let headers = vec![
            ("x-ms-blob-content-type".to_owned(), "image/png".to_owned()),
            (
                "x-ms-blob-cache-control".to_owned(),
                "max-age=60".to_owned(),
            ),
            ("X-MS-META-Owner".to_owned(), "kit".to_owned()),
            ("x-ms-blob-type".to_owned(), "BlockBlob".to_owned()),
        ];
        let origin_headers = vec![
            (
                "x-ms-request-server-encrypted".to_owned(),
                "true".to_owned(),
            ),
            ("x-ms-version-id".to_owned(), "version-one".to_owned()),
        ];

        let seeded = azure_seed_response_headers(&headers, &origin_headers);

        assert!(seeded.contains(&("Content-Type".to_owned(), "image/png".to_owned())));
        assert!(seeded.contains(&("Cache-Control".to_owned(), "max-age=60".to_owned())));
        assert!(seeded.contains(&("X-MS-META-Owner".to_owned(), "kit".to_owned())));
        assert!(seeded.contains(&("x-ms-blob-type".to_owned(), "BlockBlob".to_owned())));
        assert!(seeded.contains(&("x-ms-lease-state".to_owned(), "available".to_owned())));
        assert!(seeded.contains(&("x-ms-lease-status".to_owned(), "unlocked".to_owned())));
        assert!(seeded.contains(&("x-ms-server-encrypted".to_owned(), "true".to_owned())));
        assert!(seeded.contains(&("x-ms-version-id".to_owned(), "version-one".to_owned())));
    }

    #[test]
    fn azure_get_client_request_id_accepts_only_visible_bounded_ascii() {
        let valid = vec![(
            "x-ms-client-request-id".to_owned(),
            "request-two".to_owned(),
        )];
        let non_visible = vec![(
            "x-ms-client-request-id".to_owned(),
            "request\u{7f}".to_owned(),
        )];
        let too_long = vec![("x-ms-client-request-id".to_owned(), "x".repeat(1025))];

        assert_eq!(azure_client_request_id(&valid), Some("request-two"));
        assert_eq!(azure_client_request_id(&non_visible), None);
        assert_eq!(azure_client_request_id(&too_long), None);
        assert_eq!(azure_client_request_id(&[]), None);
    }

    #[test]
    fn azure_get_customer_provided_key_requires_origin_bypass() {
        let ordinary = vec![("x-ms-version".to_owned(), "2026-06-06".to_owned())];
        let encrypted = vec![(
            "x-ms-encryption-key-sha256".to_owned(),
            "base64-sha256".to_owned(),
        )];

        assert!(!azure_has_customer_provided_key(&ordinary));
        assert!(azure_has_customer_provided_key(&encrypted));
    }

    #[test]
    fn general_get_no_cache_matches_every_cache_control_line() {
        // Arrange.
        let header = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let max_age = vec![header("Cache-Control", "max-age=0")];
        let no_store = vec![header("Cache-Control", "no-store")];
        let mixed_case = vec![header("cache-control", "No-Cache")];
        let listed = vec![header("Cache-Control", "max-age=0, no-cache")];
        let later_line = vec![
            header("Cache-Control", "max-age=0"),
            header("Cache-Control", "no-cache"),
        ];
        let earlier_line = vec![
            header("Cache-Control", "no-cache"),
            header("Cache-Control", "max-age=0"),
        ];

        // Act and assert.
        assert!(!requests_no_cache(&[]));
        assert!(!requests_no_cache(&max_age));
        assert!(!requests_no_cache(&no_store));
        assert!(requests_no_cache(&mixed_case));
        assert!(requests_no_cache(&listed));
        assert!(requests_no_cache(&later_line));
        assert!(requests_no_cache(&earlier_line));
    }

    #[test]
    fn azure_get_conditional_headers_require_origin_validation() {
        // Arrange.
        let ordinary = vec![("If-Match".to_owned(), "\"etag\"".to_owned())];
        let modified_since = vec![(
            "If-Modified-Since".to_owned(),
            "Wed, 01 Jan 2100 00:00:00 GMT".to_owned(),
        )];
        let none_match = vec![("If-None-Match".to_owned(), "\"etag\"".to_owned())];
        let unmodified_since = vec![(
            "If-Unmodified-Since".to_owned(),
            "Wed, 01 Jan 1970 00:00:00 GMT".to_owned(),
        )];
        let tag_condition = vec![("x-ms-if-tags".to_owned(), "owner = 'kit'".to_owned())];
        let lease = vec![("x-ms-lease-id".to_owned(), "lease-id".to_owned())];

        // Act and assert.
        assert!(!azure_requires_origin_validation(&ordinary));
        assert!(!azure_requires_origin_validation(&modified_since));
        assert!(!azure_requires_origin_validation(&none_match));
        assert!(!azure_requires_origin_validation(&unmodified_since));
        assert!(azure_requires_origin_validation(&tag_condition));
        assert!(azure_requires_origin_validation(&lease));
    }

    #[test]
    fn cached_http_date_conditions_use_last_modified() {
        // Arrange.
        let last_modified = Some("Mon, 24 Aug 2026 12:00:00 GMT");

        // Act and assert.
        assert!(if_modified_since_not_modified(
            Some("Tue, 25 Aug 2026 12:00:00 GMT"),
            last_modified
        ));
        assert!(!if_modified_since_not_modified(
            Some("Sun, 23 Aug 2026 12:00:00 GMT"),
            last_modified
        ));
        assert!(if_unmodified_since_failed(
            None,
            Some("Sun, 23 Aug 2026 12:00:00 GMT"),
            last_modified
        ));
        assert!(!if_unmodified_since_failed(
            None,
            Some("Tue, 25 Aug 2026 12:00:00 GMT"),
            last_modified
        ));
        assert!(!if_unmodified_since_failed(
            Some("\"immutable-version\""),
            Some("Sun, 23 Aug 2026 12:00:00 GMT"),
            last_modified
        ));
    }

    #[test]
    fn freshness_probe_ignores_client_response_conditions() {
        // Arrange.
        let headers = vec![
            ("Authorization".to_owned(), "Bearer token".to_owned()),
            ("If-Match".to_owned(), "\"version\"".to_owned()),
            (
                "If-Modified-Since".to_owned(),
                "Wed, 01 Jan 2100 00:00:00 GMT".to_owned(),
            ),
            ("If-None-Match".to_owned(), "\"version\"".to_owned()),
            (
                "If-Unmodified-Since".to_owned(),
                "Wed, 01 Jan 1970 00:00:00 GMT".to_owned(),
            ),
        ];

        // Act.
        let probe = freshness_probe_headers(&headers);

        // Assert.
        assert_eq!(
            probe,
            vec![("Authorization".to_owned(), "Bearer token".to_owned())]
        );
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct CapturedEvent {
        level: tracing::Level,
        fields: HashMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct EventCapture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl EventCapture {
        fn info_subscriber(&self) -> impl Subscriber + Send + Sync {
            tracing_subscriber::registry()
                .with(LevelFilter::INFO)
                .with(self.clone())
        }

        fn events(&self) -> Vec<CapturedEvent> {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl<S> Layer<S> for EventCapture
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &Event<'_>, _context: LayerContext<'_, S>) {
            let mut visitor = EventVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields: visitor.fields,
                });
        }
    }

    #[derive(Default)]
    struct EventVisitor {
        fields: HashMap<String, String>,
    }

    impl Visit for EventVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_owned(), value.to_owned());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields
                .insert(field.name().to_owned(), value.to_string());
        }
    }

    fn captured_event<'a>(events: &'a [CapturedEvent], name: &str) -> &'a CapturedEvent {
        events
            .iter()
            .find(|event| event.fields.get("event").is_some_and(|value| value == name))
            .unwrap_or_else(|| panic!("missing captured event {name:?}: {events:?}"))
    }

    struct BlockedWriter;

    impl AsyncWrite for BlockedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct OriginBodyTimeoutAfterChunk {
        chunk: Option<Bytes>,
        timed_out: bool,
    }

    impl hyper::body::Body for OriginBodyTimeoutAfterChunk {
        type Data = Bytes;
        type Error = ovcc_upstream::OriginBodyError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<hyper::body::Frame<Self::Data>, Self::Error>>>
        {
            if let Some(chunk) = self.chunk.take() {
                return Poll::Ready(Some(Ok(hyper::body::Frame::data(chunk))));
            }
            if self.timed_out {
                return Poll::Ready(None);
            }
            self.timed_out = true;
            Poll::Ready(Some(Err(ovcc_upstream::OriginBodyError::Timeout(
                ovcc_upstream::OriginTimeout {
                    stage: ovcc_upstream::OriginTimeoutStage::ResponseBody,
                    duration: Duration::from_secs(60),
                },
            ))))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn general_read_keepalive_connection_closes_on_idle_or_shutdown() {
        for stop in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, accepted) = tokio::join!(
                TcpStream::connect(listener.local_addr().unwrap()),
                listener.accept()
            );
            let mut client = client.unwrap();
            let (server, _) = accepted.unwrap();
            let (sender, shutdown) = watch::channel(false);
            let context = ConnCtx {
                backends: Arc::new(Vec::new()),
                passthrough: Passthrough::default(),
                self_names: Arc::new(Vec::new()),
                generation: Arc::new(AtomicI64::new(0)),
                reset_compat: Arc::new(CompatConfig::default()),
                instance: Arc::new("idle-test".to_owned()),
                presigned_auth: Arc::new(AuthorizationCache::new(1)),
                local_mode: false,
                peer: None,
            };
            let connection =
                handle_connection(server, None, context, ListenerKind::Cache, shutdown);
            tokio::pin!(connection);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(connection.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            tokio::time::advance(KEEPALIVE_IDLE_TIMEOUT / 2).await;
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(connection.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            if stop {
                sender.send(true).unwrap();
            } else {
                tokio::time::advance(KEEPALIVE_IDLE_TIMEOUT).await;
            }
            // The graceful close half-closes and then lingers until the client
            // hangs up, so the client reads the end of stream and closes.
            let client_end = async move {
                let mut byte = [0];
                let read = client.read(&mut byte).await.unwrap();
                drop(client);
                read
            };
            let (connection, read) = tokio::join!(connection, client_end);
            connection.unwrap();
            assert_eq!(read, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn general_read_keepalive_idle_expires_and_rearms() {
        let (_sender, mut shutdown) = watch::channel(false);
        let idle = tokio::time::sleep(KEEPALIVE_IDLE_TIMEOUT);
        tokio::pin!(idle);
        for _ in 0..2 {
            let read = read_before_idle(&mut shutdown, idle.as_mut(), std::future::pending::<()>());
            tokio::pin!(read);
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            tokio::time::advance(KEEPALIVE_IDLE_TIMEOUT - Duration::from_secs(1)).await;
            assert!(
                std::future::poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx)))
                    .await
                    .is_pending()
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(read.await, None);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn general_read_keepalive_shutdown_wins_ready_request() {
        let (sender, mut shutdown) = watch::channel(false);
        let idle = tokio::time::sleep(KEEPALIVE_IDLE_TIMEOUT);
        tokio::pin!(idle);
        assert_eq!(
            read_before_idle(&mut shutdown, idle.as_mut(), std::future::ready(7)).await,
            Some(7)
        );
        sender.send(true).unwrap();
        assert_eq!(
            read_before_idle(&mut shutdown, idle.as_mut(), std::future::ready(9)).await,
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn general_write_downstream_reuses_fired_timer_after_progress() {
        let (mut reader, mut writer) = tokio::io::duplex(1);
        let mut writer = ProgressTimeoutWriter::with_timeout(&mut writer, Duration::from_secs(2));
        writer.write_all(b"a").await.unwrap();
        assert!(writer.stall.is_none(), "ready writers allocate no timer");
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(
            Pin::new(&mut writer)
                .poll_write(&mut context, b"b")
                .is_pending()
        );
        let timer = (&**writer.stall.as_ref().unwrap()) as *const tokio::time::Sleep;
        let mut byte = [0];
        reader.read_exact(&mut byte).await.unwrap();
        writer.write_all(b"b").await.unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(
            Pin::new(&mut writer)
                .poll_write(&mut context, b"c")
                .is_pending(),
            "first Pending after progress resets an expired timer"
        );
        assert_eq!(
            (&**writer.stall.as_ref().unwrap()) as *const tokio::time::Sleep,
            timer
        );
        tokio::time::advance(Duration::from_secs(3)).await;
        let Poll::Ready(Err(error)) = Pin::new(&mut writer).poll_write(&mut context, b"c") else {
            panic!("stalled writer must expire")
        };
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn general_write_downstream_timeout_fires_after_inactivity() {
        // Arrange.
        let (_reader, mut writer) = tokio::io::duplex(1);
        let mut timed_writer =
            ProgressTimeoutWriter::with_timeout(&mut writer, Duration::from_millis(20));
        let write = timed_writer.write_all(b"blocked");

        // Act.
        let result = timeout(Duration::from_millis(200), write)
            .await
            .expect("inner inactivity timeout wins");

        // Assert.
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn general_write_downstream_timeout_resets_after_progress() {
        // Arrange.
        let (mut reader, mut writer) = tokio::io::duplex(1);
        let drain = async {
            let mut byte = [0u8; 1];
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(60)).await;
                reader.read_exact(&mut byte).await.unwrap();
            }
        };
        let mut timed_writer =
            ProgressTimeoutWriter::with_timeout(&mut writer, Duration::from_millis(100));
        let write = timeout(Duration::from_millis(500), timed_writer.write_all(b"abcd"));

        // Act.
        let (write, ()) = tokio::join!(write, drain);

        // Assert.
        write
            .expect("overall test ceiling")
            .expect("progress resets inactivity clock");
    }

    #[tokio::test]
    async fn origin_connect_timeout_releases_fill_permit() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/connect-timeout");
        let permit = owner_permit(&cache, &key).await;

        let error = ovcc_core::Error::upstream(
            "request failed: origin connect inactivity timeout after 60s",
        );

        // Act.
        let response = abandon_fill_after_origin_error(permit, &error, "MISS", false);
        let next = cache.acquire_fill(key.as_str()).await?;

        // Assert.
        assert_eq!(response.head.status, 504);
        assert_eq!(response.timeout_stage, Some(TimeoutStage::OriginConnect));
        assert!(cache.get(key.as_str())?.is_none());
        assert!(matches!(next, FillDecision::Owner(_)));
        Ok(())
    }

    #[test]
    fn upstream_connection_failure_is_an_empty_attributed_503() {
        // Arrange.
        let error = ovcc_core::Error::upstream_connection("connection terminated");

        // Act.
        let failure = upstream_failure_response(&error);

        // Assert.
        assert_eq!(failure.status, 503);
        assert_eq!(failure.reason, "Service Unavailable");
        assert!(failure.body.is_empty());
        assert_eq!(
            failure.headers,
            &[("Proxy-Status", "ovcc; error=connection_terminated")]
        );
        assert!(failure.timeout_stage.is_none());
    }

    #[test]
    fn general_head_fill_failures_preserve_status_without_body() {
        // Arrange.
        for (error, status) in [
            (Error::upstream("connection refused"), 502),
            (Error::upstream_connection("connection terminated"), 503),
            (
                Error::upstream("origin connect inactivity timeout after 60s"),
                504,
            ),
        ] {
            // Act.
            let response = abandon_fill_after_origin_error(None, &error, "MISS", true);
            let get = abandon_fill_after_origin_error(None, &error, "MISS", false);
            // Assert.
            assert_eq!(response.head.status, status);
            assert_eq!(response.bytes_sent, 0);
            assert_eq!(response.head.content_length, None);
            assert!(matches!(response.body, BodySource::Buffered(ref body) if body.is_empty()));
            assert_eq!(response.head.headers, get.head.headers);
            assert_eq!(response.timeout_stage, get.timeout_stage);
            assert_eq!(
                get.bytes_sent,
                upstream_failure_response(&error).body.len() as u64
            );
        }
    }

    #[tokio::test]
    async fn general_head_bypass_preserves_origin_method_and_omits_body()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let upstream = format!("http://{}", listener.local_addr()?);
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let request = read_request_head(&mut socket)
                .await?
                .ok_or("missing request")?;
            socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 19\r\nConnection: close\r\n\r\n").await?;
            Ok::<_, BoxError>(request.head.method)
        });
        let mut head = get_head("origin.example", "/head-bypass");
        head.method = "HEAD".into();
        let mut wire = Vec::new();

        // Act.
        let response = serve_bypass(&mut wire, &upstream, &head).await?;
        let method = origin.await??;

        // Assert.
        assert_eq!(method, "HEAD");
        assert_eq!(response.head.status, 500);
        assert_eq!(response.head.content_length, Some(19));
        assert_eq!(response.bytes_sent, 0);
        assert!(wire.is_empty());
        assert!(matches!(response.body, BodySource::Buffered(ref body) if body.is_empty()));
        Ok(())
    }

    #[tokio::test]
    async fn nucleus_head_revalidation_failure_has_no_body() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_directory, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = plain_key("nucleus", "/head-revalidation");
        let writer = cache.begin_fill_preserving_expiry(
            key.as_str(),
            StoredMeta {
                etag: Some("\"head-version\"".into()),
                expires_at: Some(0),
                representation_length: Some(19),
                ..StoredMeta::default()
            },
        )?;
        writer.commit()?;
        let refused = tokio::net::TcpSocket::new_v4()?;
        refused.bind("127.0.0.1:0".parse()?)?;
        let upstream = format!("http://{}", refused.local_addr()?);
        let mut head = get_head("nucleus", "/head-revalidation");
        head.method = "HEAD".into();
        let (mut stream, _client) = plain_stream_pair().await;
        let fill = nucleus_miss_fill(true, ovcc_nucleus::Location::Generic, None, &upstream);

        // Act.
        let response = revalidate_entry(
            &mut stream,
            &cache,
            &key,
            &upstream,
            &head,
            fill,
            false,
            RevalidationExpiry::CachePolicy,
        )
        .await?;

        // Assert.
        assert_eq!(response.head.status, 502);
        assert_eq!(response.bytes_sent, 0);
        assert_eq!(response.head.content_length, None);
        assert!(matches!(response.body, BodySource::Buffered(ref body) if body.is_empty()));
        Ok(())
    }

    #[derive(Clone)]
    struct ProbeService {
        calls: Arc<std::sync::atomic::AtomicU64>,
        delay: Duration,
        committed: bool,
    }

    #[tonic::async_trait]
    impl peer_proto::peer_control_server::PeerControl for ProbeService {
        async fn who_has(
            &self,
            request: GrpcRequest<peer_proto::WhoHasRequest>,
        ) -> std::result::Result<GrpcResponse<peer_proto::WhoHasResponse>, GrpcStatus> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            Ok(GrpcResponse::new(peer_proto::WhoHasResponse {
                omitted_parts: Vec::new(),
                optional_keys_ack: String::new(),
                object_status: if self.committed {
                    peer_proto::ObjectStatus::Committed as i32
                } else {
                    peer_proto::ObjectStatus::Absent as i32
                },
                http_status: u32::from(self.committed) * 200,
                object_size: u64::from(self.committed) * 4096,
                validator: request.into_inner().validator,
                max_idle_seconds: self.committed.then_some(600),
            }))
        }
    }

    async fn spawn_probe_service(
        service: ProbeService,
    ) -> std::result::Result<SocketAddr, BoxError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(peer_proto::peer_control_server::PeerControlServer::new(
                    service,
                ))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await;
        });
        Ok(address)
    }

    #[tokio::test]
    async fn peer_who_has_cancels_remaining_losers() -> std::result::Result<(), BoxError> {
        // Arrange.
        let miss_calls = Arc::new(AtomicU64::new(0));
        let winner_calls = Arc::new(AtomicU64::new(0));
        let slow_calls = Arc::new(AtomicU64::new(0));
        let miss = spawn_probe_service(ProbeService {
            calls: Arc::clone(&miss_calls),
            delay: Duration::from_millis(10),
            committed: false,
        })
        .await?;
        let winner = spawn_probe_service(ProbeService {
            calls: Arc::clone(&winner_calls),
            delay: Duration::from_millis(50),
            committed: true,
        })
        .await?;
        let slow = spawn_probe_service(ProbeService {
            calls: Arc::clone(&slow_calls),
            delay: Duration::from_secs(5),
            committed: true,
        })
        .await?;
        let directory = PeerDirectory::new(
            PeerDiscovery::Explicit(vec![slow, winner, miss]),
            Duration::from_secs(30),
            Duration::from_secs(30),
            ovcc_peer::SelfIdentity::default(),
        );
        directory.refresh().await?;
        directory.mark_failed(slow);
        let runtime = PeerRuntime {
            directory,
            client: PeerClient::new(PeerClientOptions {
                probe_timeout: Duration::from_secs(1),
                connect_timeout: Duration::from_secs(1),
                first_byte_timeout: Duration::from_secs(1),
                stall_timeout: Duration::from_secs(1),
                ..PeerClientOptions::default()
            })?,
            random: RunRandom::from_seed(Some(7)),
            budget: Duration::from_secs(1),
            replicate: true,
            cache_port: AtomicU16::new(14_128),
            min_object_bytes: 0,
        };
        let identity = ObjectIdentity {
            host: "bucket.example.com".to_owned(),
            method: http::Method::GET,
            uri: "/object".to_owned(),
            args: String::new(),
            if_match: String::new(),
            range: String::new(),
        };
        let validator = ExactValidator::Etag("\"v1\"".to_owned());

        // Act.
        let started = Instant::now();
        let selected = first_peer_probe(&runtime, &identity, &validator, None)
            .await
            .ok_or_else(|| std::io::Error::other("no peer winner selected"))?;
        let elapsed = started.elapsed();

        // Assert.
        assert_eq!(selected.probe.peer, winner);
        assert!(selected.cancelled_losers >= 1);
        assert_eq!(miss_calls.load(Ordering::SeqCst), 1);
        assert_eq!(winner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(slow_calls.load(Ordering::SeqCst), 1);
        assert!(
            elapsed < Duration::from_secs(1),
            "the five-second losing RPC was not cancelled: {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn run_seed_repeats_shuffle_sequence() {
        // Arrange.
        let first_run = RunRandom::from_seed(Some(20_260_830));
        let second_run = RunRandom::from_seed(Some(20_260_830));
        let mut first_lookup: Vec<_> = (0..20).collect();
        let mut second_lookup = first_lookup.clone();
        let mut first_next_lookup = first_lookup.clone();
        let mut second_next_lookup = first_lookup.clone();

        // Act.
        first_run.shuffle(&mut first_lookup);
        second_run.shuffle(&mut second_lookup);
        first_run.shuffle(&mut first_next_lookup);
        second_run.shuffle(&mut second_next_lookup);

        // Assert.
        assert_eq!(first_lookup, second_lookup);
        assert_eq!(first_next_lookup, second_next_lookup);
        assert_ne!(first_lookup, (0..20).collect::<Vec<_>>());
        assert_ne!(first_lookup, first_next_lookup);
    }

    #[test]
    fn invalid_run_seed_falls_back_to_zero() {
        // Arrange.
        let invalid_seed = RunRandom::from_configured_seed("not-a-seed");
        let zero_seed = RunRandom::from_seed(Some(0));
        let mut invalid_order: Vec<_> = (0..20).collect();
        let mut zero_order = invalid_order.clone();

        // Act.
        invalid_seed.shuffle(&mut invalid_order);
        zero_seed.shuffle(&mut zero_order);

        // Assert.
        assert_eq!(invalid_order, zero_order);
    }

    #[tokio::test]
    async fn abort_join_set_counts_only_tasks_cancelled_by_abort() {
        // Arrange.
        let mut tasks = JoinSet::new();
        tasks.spawn(async {});
        tasks.spawn(std::future::pending::<()>());
        let completed = tasks.join_next().await;

        // Act.
        let cancelled = abort_join_set(&mut tasks).await;

        // Assert.
        assert!(matches!(completed, Some(Ok(()))));
        assert_eq!(cancelled, 1);
    }

    #[test]
    fn peer_who_has_requires_exact_validator_kind_and_value() {
        // Arrange.
        let meta = StoredMeta {
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Sat, 15 Aug 2026 00:00:00 GMT".to_owned()),
            ..StoredMeta::default()
        };

        // Act.
        let etag_match =
            stored_validator_matches(&meta, &ExactValidator::Etag("\"v1\"".to_owned()));
        let etag_mismatch =
            stored_validator_matches(&meta, &ExactValidator::Etag("\"v2\"".to_owned()));
        let kind_mismatch =
            stored_validator_matches(&meta, &ExactValidator::LastModified("\"v1\"".to_owned()));

        // Assert.
        assert!(etag_match);
        assert!(!etag_mismatch);
        assert!(!kind_mismatch);
    }

    #[test]
    fn peer_who_has_response_advertises_persisted_idle_ceiling() {
        // Arrange.
        let meta = StoredMeta {
            size: 4_096,
            max_idle_secs: Some(600),
            ..StoredMeta::default()
        };
        let validator = ExactValidator::Etag("\"v1\"".to_owned());

        // Act.
        let response = committed_who_has(&meta, &validator);

        // Assert.
        assert_eq!(response.object_size, 4_096);
        assert_eq!(response.max_idle_seconds, Some(600));
    }

    #[test]
    fn peer_fill_uses_the_shortest_local_and_advertised_backend_ceiling() {
        // Arrange.
        let source = FillSource::Peer {
            replicate: true,
            max_idle_secs: Some(600),
        };

        // Act.
        let effective = source.max_idle_secs(Some(1_200));

        // Assert.
        assert_eq!(effective, Some(600));
    }

    #[test]
    fn mutation_detection_prefers_etag_and_falls_back_to_last_modified() {
        // Arrange.
        let cached = StoredMeta {
            etag: Some("\"v1\"".to_owned()),
            last_modified: Some("Sat, 15 Aug 2026 00:00:00 GMT".to_owned()),
            ..StoredMeta::default()
        };
        let changed_etag = vec![("ETag".to_owned(), "\"v2\"".to_owned())];
        let matching_etag_with_changed_lm = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            (
                "Last-Modified".to_owned(),
                "Sun, 16 Aug 2026 00:00:00 GMT".to_owned(),
            ),
        ];
        let cached_without_etag = StoredMeta {
            last_modified: cached.last_modified.clone(),
            ..StoredMeta::default()
        };
        let changed_lm = vec![(
            "Last-Modified".to_owned(),
            "Sun, 16 Aug 2026 00:00:00 GMT".to_owned(),
        )];

        // Act and assert.
        assert!(is_mutated(&cached, &changed_etag));
        assert!(!is_mutated(&cached, &matching_etag_with_changed_lm));
        assert!(is_mutated(&cached_without_etag, &changed_lm));
        assert!(!is_mutated(&cached_without_etag, &[]));
    }

    #[test]
    fn generation_read_returns_current_without_optional_fields() {
        // Arrange.
        let generation = AtomicI64::new(7);

        // Act.
        let result = apply_generation(&generation, None);

        // Assert.
        assert_eq!(result, (7, None, None));
    }

    #[test]
    fn generation_update_accepts_only_strictly_higher_value() {
        // Arrange.
        let generation = AtomicI64::new(7);

        // Act.
        let higher = apply_generation(&generation, Some("9"));
        let equal = apply_generation(&generation, Some("9"));
        let lower = apply_generation(&generation, Some("8"));

        // Assert.
        assert_eq!(higher, (9, Some(7), None));
        assert_eq!(equal, (9, None, Some("generation_not_updated")));
        assert_eq!(lower, (9, None, Some("generation_not_updated")));
        assert_eq!(generation.load(Ordering::SeqCst), 9);
    }

    #[test]
    fn generation_update_rejects_malformed_value_without_change() {
        // Arrange.
        let generation = AtomicI64::new(7);

        // Act.
        let result = apply_generation(&generation, Some("not-a-number"));

        // Assert.
        assert_eq!(result, (7, None, Some("invalid_generation")));
        assert_eq!(generation.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn backend_stores_deduplicate_canonical_roots_and_select_fallback() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let fallback = temp.path().join("fallback");
        let backend = |name: &str, cache_root: Option<String>| Backend {
            name: name.to_owned(),
            cache_root,
            ..Backend::default()
        };

        // Act.
        let runtimes = open_backend_stores(
            vec![
                backend("first", Some(shared.to_string_lossy().into_owned())),
                backend(
                    "alias",
                    Some(shared.join(".").to_string_lossy().into_owned()),
                ),
                backend("missing", None),
                backend("empty", Some("   ".to_owned())),
            ],
            &fallback,
            &WriteThroughConfig::default(),
        )
        .unwrap();
        let manager_stores = unique_backend_stores(&runtimes);

        // Assert.
        assert!(Arc::ptr_eq(&runtimes[0].store, &runtimes[1].store));
        assert!(Arc::ptr_eq(&runtimes[2].store, &runtimes[3].store));
        assert!(!Arc::ptr_eq(&runtimes[0].store, &runtimes[2].store));
        assert_eq!(manager_stores.len(), 2);
    }

    #[test]
    fn write_through_overrides_keep_staging_below_each_backend_root() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let config = WriteThroughConfig {
            request_body_chunk_size: 4_096,
            staging_subdir: "custom/staging".to_owned(),
            ..WriteThroughConfig::default()
        };
        let backend = |name: &str, root: &Path| Backend {
            name: name.to_owned(),
            cache_root: Some(root.to_string_lossy().into_owned()),
            ..Backend::default()
        };

        // Act.
        let runtimes = open_backend_stores(
            vec![backend("first", &first), backend("second", &second)],
            temp.path(),
            &config,
        )
        .unwrap();

        // Assert.
        assert_eq!(
            runtimes[0].store.write_through.staging_dir,
            std::fs::canonicalize(first.join("custom/staging")).unwrap()
        );
        assert_eq!(
            runtimes[1].store.write_through.staging_dir,
            std::fs::canonicalize(second.join("custom/staging")).unwrap()
        );
        assert_eq!(runtimes[0].store.write_through.chunk_size, 4_096);
        assert!(!Arc::ptr_eq(&runtimes[0].store, &runtimes[1].store));
    }

    #[cfg(unix)]
    #[test]
    fn write_through_staging_symlink_cannot_escape_backend_root() {
        use std::os::unix::fs::symlink;

        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("staging")).unwrap();
        let backend = Backend {
            name: "test".to_owned(),
            cache_root: Some(root.to_string_lossy().into_owned()),
            ..Backend::default()
        };
        let config = WriteThroughConfig {
            staging_subdir: "staging".to_owned(),
            ..WriteThroughConfig::default()
        };

        // Act.
        let error = match open_backend_stores(vec![backend], temp.path(), &config) {
            Ok(_) => panic!("staging escape must fail"),
            Err(error) => error,
        };

        // Assert.
        assert!(error.to_string().contains("outside backend cache root"));
    }

    #[test]
    fn write_through_block_tokens_expire_at_the_configured_limit() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let staging = BlockStaging::for_blob(
            temp.path(),
            "container.blob.core.windows.net",
            "/object",
            Duration::from_secs(10),
        );
        let expired = BlockId::new("expired");
        let fresh = BlockId::new("fresh");
        std::fs::create_dir_all(&staging.dir).unwrap();
        let expired_file = File::create(staging.block_path(&expired)).unwrap();
        expired_file
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(now.checked_sub(Duration::from_secs(11)).unwrap()),
            )
            .unwrap();
        File::create(staging.block_path(&fresh)).unwrap();

        // Act.
        BlockStaging::remove_expired(temp.path(), Duration::from_secs(10), now).unwrap();

        // Assert.
        assert!(!staging.block_path(&expired).exists());
        assert!(staging.block_path(&fresh).exists());
        assert!(!staging.contains(&expired));
        assert!(staging.contains(&fresh));
    }

    fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn azure_get_response_version_selects_request_header_and_sas_fallbacks() {
        // Arrange. SAS authorization and REST protocol versions can differ.
        let mut head = RequestHead {
            method: "GET".into(),
            path: "/blob".into(),
            query: "sv=2022-11-02&api-version=2024-11-04".into(),
            query_present: true,
            http_version: "HTTP/1.1".into(),
            host: Some("account.blob.core.windows.net".into()),
            headers: hdr(&[("x-ms-version", "2025-01-05")]),
            content_length: 0,
            head_length: 0,
        };

        // Act and assert. Explicit versions win; unknown defaults stay unset.
        assert_eq!(azure_response_version(&head).as_deref(), Some("2025-01-05"));
        head.headers.clear();
        assert_eq!(azure_response_version(&head).as_deref(), Some("2024-11-04"));
        head.query = "sv=2022-11-02".into();
        assert_eq!(azure_response_version(&head).as_deref(), Some("2022-11-02"));
        head.query.clear();
        assert_eq!(azure_response_version(&head), None);
        head.query = "sv=2022-11-02%0D%0AX-Injected%3Ayes".into();
        assert_eq!(azure_response_version(&head), None);
    }

    #[test]
    fn azure_response_overrides_decode_supported_headers() {
        // Arrange.
        let query = "rscc=public%2C+max-age%3D60\
            &rscd=attachment%3B+filename%3D%22payload.zip%22\
            &rsce=identity&rscl=en-US&rsct=application%2Fzip";

        // Act.
        let headers = azure_response_overrides(query);

        // Assert.
        assert_eq!(
            headers,
            vec![
                ("Cache-Control", "public, max-age=60".to_owned()),
                (
                    "Content-Disposition",
                    "attachment; filename=\"payload.zip\"".to_owned()
                ),
                ("Content-Encoding", "identity".to_owned()),
                ("Content-Language", "en-US".to_owned()),
                ("Content-Type", "application/zip".to_owned()),
            ]
        );
    }

    #[test]
    fn azure_response_overrides_reject_header_injection() {
        // Arrange.
        let query = "rscc=no-cache%0D%0AX-Injected%3A+true&rsct=application%2Fzip";

        // Act.
        let headers = azure_response_overrides(query);

        // Assert.
        assert_eq!(
            headers,
            vec![("Content-Type", "application/zip".to_owned())]
        );
    }

    #[test]
    fn access_log_uses_client_transport_and_http_protocol() {
        let head = RequestHead {
            method: "GET".to_string(),
            path: "/object".to_string(),
            query: String::new(),
            query_present: false,
            http_version: "HTTP/1.1".to_string(),
            host: Some("bucket.example.com".to_string()),
            headers: Vec::new(),
            content_length: 0,
            head_length: 0,
        };
        let response = Response::buffered(200, "OK", Some("HIT"), Vec::new(), b"body".to_vec());

        let plaintext = build_access_log(
            &head,
            "bucket.example.com",
            AccessConnection {
                remote_addr: "127.0.0.1",
                scheme: "http",
                ssl_protocol: "",
                request: RequestConnection { id: 1, requests: 1 },
            },
            &RequestFlags::default(),
            &response,
        );
        let tls = build_access_log(
            &head,
            "bucket.example.com",
            AccessConnection {
                remote_addr: "127.0.0.1",
                scheme: "https",
                ssl_protocol: "TLSv1.3",
                request: RequestConnection { id: 2, requests: 1 },
            },
            &RequestFlags::default(),
            &response,
        );

        assert_eq!(plaintext.scheme, "http");
        assert_eq!(plaintext.http_protocol, "HTTP/1.1");
        assert_eq!(plaintext.ssl_protocol, "");
        assert_eq!(tls.scheme, "https");
        assert_eq!(tls.http_protocol, "HTTP/1.1");
        assert_eq!(tls.ssl_protocol, "TLSv1.3");
    }

    #[test]
    fn classify_fixed_length_is_accepted() {
        let headers = hdr(&[("Content-Type", "text/plain"), ("Content-Length", "11")]);
        assert_eq!(
            classify_put_body(&headers, false),
            Ok(PutFraming::Fixed(11))
        );
    }

    #[test]
    fn classify_rejects_aws_chunked_with_400() {
        // Arrange.
        let headers = hdr(&[
            ("Content-Encoding", "aws-chunked"),
            ("Content-Length", "5"),
            ("x-amz-decoded-content-length", "5"),
        ]);

        // Act.
        let error = classify_put_body(&headers, false).unwrap_err();

        // Assert.
        assert_eq!(
            error,
            (
                400,
                "Content-Encoding: aws-chunked is not supported for write-through\n"
            )
        );
    }

    #[test]
    fn classify_rejects_streaming_signature_with_400() {
        // Arrange.
        let headers = hdr(&[("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")]);

        // Act.
        let error = classify_put_body(&headers, false).unwrap_err();

        // Assert.
        assert_eq!(
            error,
            (
                400,
                "AWS streaming-signature payloads are not supported for write-through\n"
            )
        );
    }

    #[test]
    fn classify_rejects_chunked_when_disabled_with_411() {
        // Arrange.
        let headers = hdr(&[("Transfer-Encoding", "chunked")]);

        // Act.
        let error = classify_put_body(&headers, false).unwrap_err();

        // Assert.
        assert_eq!(
            error,
            (
                411,
                "Transfer-Encoding: chunked is not enabled for write-through\n"
            )
        );
        // Enabling chunked uploads turns it into a streamable framing.
        assert_eq!(classify_put_body(&headers, true), Ok(PutFraming::Chunked));
    }

    #[test]
    fn classify_requires_a_body_length_otherwise_411() {
        // Arrange.
        let headers = hdr(&[]);

        // Act.
        let error = classify_put_body(&headers, false).unwrap_err();

        // Assert.
        assert_eq!(error, (411, "Content-Length required for write-through\n"));
    }

    #[tokio::test]
    async fn fixed_write_through_reads_and_forwards_configured_chunks() {
        // Arrange.
        let mut body = &b"abcdefghij"[..];
        let staging_dir = tempfile::tempdir().unwrap();
        let mut staging = File::create(staging_dir.path().join("upload")).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        // Act.
        let outcome = tee_fixed(&mut body, Vec::new(), 10, &mut staging, tx, 3).await;
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk.expect("no abort on a complete body").len());
        }

        // Assert.
        assert!(outcome.complete);
        assert_eq!(outcome.staged, 10);
        assert_eq!(chunks, vec![3, 3, 3, 1]);
    }

    #[test]
    fn chunked_decoder_decodes_single_push() {
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let done = decoder
            .push(b"5\r\nhello\r\n0\r\n\r\n", &mut out)
            .expect("valid chunked");
        assert!(done);
        assert!(decoder.finished());
        assert_eq!(out, b"hello");
    }

    #[test]
    fn chunked_decoder_handles_split_pushes() {
        // Same body split mid-size-line, mid-data, and around the terminator.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        for piece in [
            &b"5\r"[..],
            &b"\nhel"[..],
            &b"lo\r\n"[..],
            &b"0\r\n"[..],
            &b"\r\n"[..],
        ] {
            decoder.push(piece, &mut out).expect("valid chunked");
        }
        assert!(decoder.finished());
        assert_eq!(out, b"hello");
    }

    #[test]
    fn chunked_decoder_concatenates_multiple_chunks_and_ignores_extensions() {
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let done = decoder
            .push(
                b"3;ext=\"quoted value\"\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
                &mut out,
            )
            .expect("valid chunked");
        assert!(done);
        assert_eq!(out, b"abcde");
    }

    #[test]
    fn chunked_decoder_rejects_bad_size_line() {
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        assert!(decoder.push(b"zz\r\n", &mut out).is_err());
    }

    #[test]
    fn general_decode_chunked_rejects_malformed_extensions() {
        // Arrange.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let outcome = decoder.push(b"5;=missing-name\r\nhello\r\n0\r\n\r\n", &mut out);

        // Assert.
        assert_eq!(outcome, Err(ChunkedError::BadChunkExtension));
    }

    #[test]
    fn general_decode_chunked_rejects_unextended_size_whitespace() {
        // Arrange.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let outcome = decoder.push(b"5 \r\nhello\r\n0\r\n\r\n", &mut out);

        // Assert.
        assert_eq!(outcome, Err(ChunkedError::BadSizeLine));
    }

    #[test]
    fn chunked_decoder_incomplete_stream_is_not_finished() {
        // A truncated body (no terminating zero-size chunk) must not report done,
        // so a client abort mid-upload is never mistaken for a complete transfer.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let done = decoder.push(b"5\r\nhel", &mut out).expect("valid so far");
        assert!(!done);
        assert!(!decoder.finished());
        assert_eq!(out, b"hel");
    }

    #[test]
    fn general_decode_chunked_rejects_non_crlf_delimiter() {
        // Arrange. The two bytes standing where the delimiter belongs are the
        // shape an unchecked two-byte skip decodes as a complete body.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let outcome = decoder.push(b"5\r\nhelloXX0\r\n\r\n", &mut out);

        // Assert. The fault surfaces as a framing error rather than a message
        // the upload path could hand to an origin.
        assert_eq!(outcome, Err(ChunkedError::MissingChunkCrlf));
        assert!(!decoder.finished());
    }

    #[test]
    fn general_decode_chunked_requires_trailer_terminator() {
        // Arrange. The terminating zero-size chunk closes the chunk sequence,
        // while the message ends only at the empty trailer line.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let without_terminator = decoder.push(b"5\r\nhello\r\n0\r\n", &mut out);

        // Assert. A client that stops there has not completed the body.
        assert_eq!(without_terminator, Ok(false));
        assert!(!decoder.finished());

        // Arrange. Trailer state is carried across pushes, so the empty line is
        // all that the message still owes.
        let terminator = &b"\r\n"[..];

        // Act.
        let with_terminator = decoder.push(terminator, &mut out);

        // Assert.
        assert_eq!(with_terminator, Ok(true));
        assert!(decoder.finished());
        assert_eq!(out, b"hello");
    }

    #[test]
    fn general_decode_chunked_accepts_valid_trailer_field() {
        // Arrange. A trailer field line sits between the terminating chunk and
        // the empty line, which the strict grammar still has to accept.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let outcome = decoder.push(b"5\r\nhello\r\n0\r\nX-Checksum: 1\r\n\r\n", &mut out);

        // Assert. The message completes and the trailer contributes no object
        // bytes.
        assert_eq!(outcome, Ok(true));
        assert_eq!(out, b"hello");
    }

    #[test]
    fn general_decode_chunked_rejects_malformed_trailer() {
        // Arrange.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let outcome = decoder.push(b"5\r\nhello\r\n0\r\nnot-a-field\r\n\r\n", &mut out);

        // Assert.
        assert_eq!(outcome, Err(ChunkedError::BadTrailerLine));
        assert!(!decoder.finished());
    }

    #[test]
    fn general_decode_chunked_bounds_unterminated_size_line() {
        // Arrange. A size line that never terminates is what grows the carry
        // buffer for as long as a client keeps sending.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();
        let flood = vec![b'0'; MAX_CHUNK_LINE_BYTES + 1];

        // Act.
        let outcome = decoder.push(&flood, &mut out);

        // Assert. Framing syntax is byte-bounded rather than client-controlled.
        assert_eq!(outcome, Err(ChunkedError::LineTooLong));
    }

    #[test]
    fn general_decode_chunked_bounds_trailer_section() {
        // Arrange.
        let mut decoder = ChunkedDecoder::new();
        let mut out = Vec::new();

        // Act.
        let after_terminating_chunk = decoder.push(b"0\r\n", &mut out);

        // Assert. The decoder is now in trailer state, where field lines
        // accumulate until the empty line ends the message.
        assert_eq!(after_terminating_chunk, Ok(false));

        // Arrange. Each field line stays within the per-line bound, so only the
        // section total can stop a trailer that never ends.
        let field = b"X: y\r\n";

        // Act.
        let mut error = None;
        for _ in 0..(MAX_CHUNK_TRAILER_BYTES / field.len() + 2) {
            if let Err(err) = decoder.push(field, &mut out) {
                error = Some(err);
                break;
            }
        }

        // Assert.
        assert_eq!(error, Some(ChunkedError::TrailerTooLong));
    }

    // A randomized companion to the example-based decoder cases above. It runs
    // against the same in-process decoder, so no sockets, files, or origins are
    // involved and the only external inputs are the two environment knobs.

    // Ceiling on fuzz iterations, so the loop terminates even if the clock the
    // budget is read from ever stops advancing.
    const FUZZ_MAX_ITERATIONS: u64 = 1_000_000;

    // SplitMix64, written out rather than pulled from a crate, so a seed
    // reproduces a run byte for byte without adding a dependency.
    struct FuzzRng(u64);

    impl FuzzRng {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut mixed = self.0;
            mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            mixed ^ (mixed >> 31)
        }

        // Draws from the inclusive range `low..=high`.
        fn range(&mut self, low: usize, high: usize) -> usize {
            low + (self.next_u64() % (high - low + 1) as u64) as usize
        }

        fn bytes(&mut self, count: usize) -> Vec<u8> {
            (0..count).map(|_| self.next_u64() as u8).collect()
        }
    }

    // Encodes `object` with random chunk sizes, occasional extensions, and an
    // occasional trailer field, so the round-trip property covers the grammar
    // rather than one canonical shape.
    fn fuzz_encode_chunked(rng: &mut FuzzRng, object: &[u8]) -> Vec<u8> {
        let mut wire = Vec::new();
        let mut offset = 0;
        while offset < object.len() {
            let size = rng.range(1, (object.len() - offset).min(64));
            wire.extend_from_slice(format!("{size:x}").as_bytes());
            if rng.range(0, 3) == 0 {
                wire.extend_from_slice(b";ext=1");
            }
            wire.extend_from_slice(b"\r\n");
            wire.extend_from_slice(&object[offset..offset + size]);
            wire.extend_from_slice(b"\r\n");
            offset += size;
        }
        wire.extend_from_slice(b"0\r\n");
        if rng.range(0, 1) == 1 {
            wire.extend_from_slice(b"X-Trailer: v\r\n");
        }
        wire.extend_from_slice(b"\r\n");
        wire
    }

    // Feeds `wire` in random pieces, mirroring the arbitrary read boundaries a
    // socket produces, and checks after every accepted push that the carry
    // buffer and trailer accounting stay inside their caps. Each piece is at
    // least one byte, so the loop is bounded by the input regardless of what
    // the decoder does with it.
    fn fuzz_push_in_pieces(
        rng: &mut FuzzRng,
        wire: &[u8],
        max_piece: usize,
        decoder: &mut ChunkedDecoder,
        out: &mut Vec<u8>,
    ) -> std::result::Result<bool, ChunkedError> {
        let mut offset = 0;
        let mut done = false;
        while offset < wire.len() {
            let take = rng.range(1, (wire.len() - offset).min(max_piece));
            done = decoder.push(&wire[offset..offset + take], out)?;
            assert!(
                decoder.buf.len() <= MAX_CHUNK_LINE_BYTES,
                "carry buffer grew past the framing-line bound"
            );
            assert!(
                decoder.trailer_bytes <= MAX_CHUNK_TRAILER_BYTES,
                "trailer accounting grew past its bound"
            );
            offset += take;
        }
        Ok(done)
    }

    #[test]
    fn general_decode_chunked_fuzzes_round_trip_and_garbage_bounds() {
        // Arrange. The budget defaults low enough to sit in the ordinary unit
        // suite and is raised by `FUZZ_DURATION_SECONDS`; the seed is printed so
        // a failing run replays exactly through `FUZZ_SEED`.
        let budget = Duration::from_secs(
            std::env::var("FUZZ_DURATION_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(2),
        );
        let seed = std::env::var("FUZZ_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0x5EED_5EED, |since| since.as_nanos() as u64)
            });
        println!(
            "chunked decoder fuzz: seed={seed} budget={}s (replay with FUZZ_SEED={seed}, \
             extend with FUZZ_DURATION_SECONDS)",
            budget.as_secs()
        );
        let mut rng = FuzzRng(seed);
        let started = Instant::now();

        // Act. Both properties are checked per iteration against a fresh
        // decoder, and every inner loop is bounded by its input, so the budget
        // and the iteration ceiling are the only things that end the run.
        let mut iterations = 0_u64;
        while started.elapsed() < budget && iterations < FUZZ_MAX_ITERATIONS {
            iterations += 1;

            // Valid framing round-trips to exactly the object bytes, whatever
            // chunk sizes and read boundaries it arrives on.
            let object_length = rng.range(0, 1024);
            let object = rng.bytes(object_length);
            let wire = fuzz_encode_chunked(&mut rng, &object);
            let mut decoder = ChunkedDecoder::new();
            let mut decoded = Vec::new();
            let done = fuzz_push_in_pieces(&mut rng, &wire, 64, &mut decoder, &mut decoded)
                .unwrap_or_else(|err| panic!("valid chunked input failed at seed={seed}: {err}"));
            assert!(done, "valid chunked input did not complete at seed={seed}");
            assert!(decoder.finished(), "decoder not finished at seed={seed}");
            assert_eq!(decoded, object, "round-trip mismatch at seed={seed}");

            // Garbage yields a value or an error and nothing else. A panic fails
            // the test where it happens, the bounds are asserted on every
            // accepted push, and neither loop can outlast its input.
            let (garbage, max_piece) = match rng.range(0, 7) {
                // Wholly arbitrary bytes.
                0..=3 => {
                    let length = rng.range(0, 300);
                    (rng.bytes(length), 64)
                }
                // A single corrupted byte in an otherwise valid encoding, which
                // keeps the input near the grammar where framing faults live.
                4..=6 => {
                    let length = rng.range(0, 64);
                    let object = rng.bytes(length);
                    let mut wire = fuzz_encode_chunked(&mut rng, &object);
                    let at = rng.range(0, wire.len() - 1);
                    let flip = rng.range(1, 255) as u8;
                    wire[at] ^= flip;
                    (wire, 64)
                }
                // A framing line that never terminates, the case the byte caps
                // exist to stop.
                _ => (vec![b'0'; MAX_CHUNK_LINE_BYTES + 64], 512),
            };
            let mut decoder = ChunkedDecoder::new();
            let mut decoded = Vec::new();
            let outcome =
                fuzz_push_in_pieces(&mut rng, &garbage, max_piece, &mut decoder, &mut decoded);
            if let Ok(done) = outcome {
                assert_eq!(
                    done,
                    decoder.finished(),
                    "completion disagreed with decoder state at seed={seed}"
                );
            }
        }
        println!("chunked decoder fuzz: completed {iterations} iterations");

        // Assert.
        assert!(iterations > 0, "the fuzz loop did not run");
    }

    #[tokio::test]
    async fn general_upload_chunked_bad_delimiter_aborts_origin() {
        // Arrange.
        let mut reader: &[u8] = b"5\r\nhelloXX0\r\n\r\n";
        let (tx, mut rx) = mpsc::channel::<UploadItem>(8);

        // Act.
        let feed = feed_request_body_chunked(&mut reader, Vec::new(), tx);
        let drain = async {
            let mut items = Vec::new();
            while let Some(item) = rx.recv().await {
                items.push(item);
            }
            items
        };
        let (feed_res, items) = tokio::join!(feed, drain);

        // Assert. The channel's last item is a terminal error, so `ChannelBody`
        // cannot end the upstream body cleanly and hyper cannot write the
        // terminating chunk.
        assert_eq!(
            feed_res.expect("feed returns"),
            Some(FeedFault::Client(UploadAbort::InvalidFraming))
        );
        assert_eq!(items.last(), Some(&Err(UploadAbort::InvalidFraming)));
    }

    #[tokio::test]
    async fn general_upload_chunked_truncation_aborts_origin() {
        // Arrange. The wire carries a valid data prefix and no terminating
        // zero-size chunk, which is what a client abort mid-upload looks like.
        let mut reader: &[u8] = b"5\r\nhello\r\n";
        let (tx, mut rx) = mpsc::channel::<UploadItem>(8);

        // Act.
        let feed = feed_request_body_chunked(&mut reader, Vec::new(), tx);
        let drain = async {
            let mut items = Vec::new();
            while let Some(item) = rx.recv().await {
                items.push(item);
            }
            items
        };
        let (feed_res, items) = tokio::join!(feed, drain);

        // Assert. The decoded prefix crosses first, then the abort, so an
        // origin that already holds a partial write is told the body failed.
        assert_eq!(
            feed_res.expect("feed returns"),
            Some(FeedFault::Client(UploadAbort::Truncated))
        );
        assert_eq!(items.first(), Some(&Ok(Bytes::from_static(b"hello"))));
        assert_eq!(items.last(), Some(&Err(UploadAbort::Truncated)));
    }

    #[tokio::test]
    async fn general_put_chunked_truncation_aborts_origin() {
        // Arrange.
        let mut body = &b"5\r\nhello\r\n"[..];
        let staging_dir = tempfile::tempdir().unwrap();
        let mut staging = File::create(staging_dir.path().join("upload")).unwrap();
        let (tx, mut rx) = mpsc::channel::<UploadItem>(8);

        // Act.
        let tee = tee_chunked(&mut body, Vec::new(), &mut staging, tx, 4);
        let drain = async {
            let mut items = Vec::new();
            while let Some(item) = rx.recv().await {
                items.push(item);
            }
            items
        };
        let (outcome, items) = tokio::join!(tee, drain);

        // Assert.
        assert!(!outcome.complete);
        assert_eq!(
            outcome.feed_fault,
            Some(FeedFault::Client(UploadAbort::Truncated))
        );
        assert_eq!(items.last(), Some(&Err(UploadAbort::Truncated)));
    }

    #[test]
    fn transparent_framing_detects_chunked() {
        let headers = hdr(&[("Transfer-Encoding", "chunked")]);
        assert_eq!(
            transparent_body_framing(&headers, 0),
            Ok(TransparentFraming::Chunked)
        );
    }

    #[test]
    fn transparent_framing_forwards_fixed_length_verbatim() {
        // A plain Content-Length forwards unchanged; a verbatim aws-chunked body
        // keeps its own length and streams through without de-chunking.
        assert_eq!(
            transparent_body_framing(&hdr(&[("Content-Length", "11")]), 11),
            Ok(TransparentFraming::Fixed(11))
        );
        let aws = hdr(&[
            ("Content-Encoding", "aws-chunked"),
            ("Content-Length", "42"),
        ]);
        assert_eq!(
            transparent_body_framing(&aws, 42),
            Ok(TransparentFraming::Fixed(42))
        );
    }

    #[test]
    fn transparent_framing_rejects_chunked_aws_signature_with_400() {
        // A chunked upload carrying AWS chunk signatures must not reach the HTTP
        // decoder, so it is rejected rather than de-chunked into a corrupt body.
        let aws = hdr(&[
            ("Transfer-Encoding", "chunked"),
            ("Content-Encoding", "aws-chunked"),
        ]);
        assert_eq!(transparent_body_framing(&aws, 0).unwrap_err().0, 400);
        let streaming = hdr(&[
            ("Transfer-Encoding", "chunked"),
            ("x-amz-content-sha256", "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"),
        ]);
        assert_eq!(transparent_body_framing(&streaming, 0).unwrap_err().0, 400);
    }

    #[tokio::test]
    async fn transparent_chunked_body_forwards_full_object() {
        // Arrange: a chunked upload carries no Content-Length, so the transparent
        // relay must de-chunk the client stream to recover the object bytes.
        let wire = b"5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n";
        let (tx, mut rx) = mpsc::channel::<UploadItem>(8);

        // Act: feed the wire bytes through the chunked body feed while draining
        // the channel concurrently, then reassemble what reached the origin.
        let mut reader: &[u8] = wire;
        let feed = feed_request_body_chunked(&mut reader, Vec::new(), tx);
        let drain = async {
            let mut collected = Vec::new();
            while let Some(chunk) = rx.recv().await {
                collected.extend_from_slice(&chunk.expect("no abort on a complete body"));
            }
            collected
        };
        let (feed_res, collected) = tokio::join!(feed, drain);

        // Assert: the full object crosses to the origin, not a silent empty body.
        assert_eq!(feed_res.expect("feed succeeds"), None);
        assert_eq!(collected, b"helloworld");
    }

    #[tokio::test]
    async fn passthrough_get_response_relays_before_origin_body_completes()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (body_tx, body_rx) = mpsc::channel(2);
        let (mut client, mut server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            relay_transparent_response(
                &mut server,
                206,
                hdr(&[("Content-Length", "10"), ("Content-Range", "bytes 0-9/10")]),
                UpstreamTiming::default(),
                TestBody { rx: body_rx },
                false,
            )
            .await
        });

        // Act.
        body_tx.send(Ok(Bytes::from_static(b"first"))).await?;
        let prefix = tokio::time::timeout(
            Duration::from_secs(1),
            read_through_body_prefix(&mut client, 5),
        )
        .await?;
        body_tx.send(Ok(Bytes::from_static(b"later"))).await?;
        drop(body_tx);
        let response = task.await??;
        let mut suffix = Vec::new();
        client.read_to_end(&mut suffix).await?;

        // Assert.
        let (wire_head, first_body) = split_wire(&prefix);
        assert!(wire_head.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(wire_head.contains("Content-Range: bytes 0-9/10"));
        assert!(!wire_head.to_ascii_lowercase().contains("x-cache-status"));
        assert_eq!(first_body, b"first");
        assert_eq!(suffix, b"later");
        assert_eq!(response.bytes_sent, 10);
        Ok(())
    }

    #[tokio::test]
    async fn passthrough_get_response_body_failure_is_not_completed()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (body_tx, body_rx) = mpsc::channel(2);
        body_tx.send(Ok(Bytes::from_static(b"first"))).await?;
        body_tx.send(Err(TestBodyError)).await?;
        drop(body_tx);

        // Act.
        let result = relay_transparent_response(
            &mut tokio::io::sink(),
            200,
            hdr(&[("Content-Length", "10")]),
            UpstreamTiming::default(),
            TestBody { rx: body_rx },
            false,
        )
        .await;

        // Assert.
        let error = result.err().ok_or_else(|| {
            Error::internal("an origin body failure completed the passthrough response")
        })?;
        assert!(error.to_string().contains("response body failed"));
        Ok(())
    }

    #[tokio::test]
    async fn passthrough_get_response_truncation_is_not_completed()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = http_body_util::Full::new(Bytes::from_static(b"short"));

        // Act.
        let result = relay_transparent_response(
            &mut tokio::io::sink(),
            200,
            hdr(&[("Content-Length", "10")]),
            UpstreamTiming::default(),
            body,
            false,
        )
        .await;

        // Assert.
        let error = result
            .err()
            .ok_or_else(|| Error::internal("a truncated passthrough response completed"))?;
        assert!(error.to_string().contains("before its Content-Length"));
        Ok(())
    }

    #[tokio::test]
    async fn passthrough_head_response_preserves_object_length_without_body()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = http_body_util::Full::new(Bytes::from_static(b"must-not-relay"));
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Act.
        let response = relay_transparent_response(
            &mut server,
            200,
            hdr(&[("Content-Length", "4096")]),
            UpstreamTiming::default(),
            body,
            true,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.contains("Content-Length: 4096"));
        assert!(wire_body.is_empty());
        assert_eq!(response.bytes_sent, 0);
        Ok(())
    }

    #[tokio::test]
    async fn default_head_content_length_preserves_origin_representation()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let addr = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nContent-Length: 1329006803\r\nETag: \"wheel\"\r\nConnection: close\r\n\r\n"
                .to_vec(),
        )
        .await;
        let mut request = get_head("artifactory.example", "/large-wheel.whl");
        request.method = "HEAD".to_owned();

        // Act.
        let response = serve_head(&format!("http://{addr}"), &request).await?;
        let wire_head = render_head(&response.head, false);

        // Assert.
        assert_eq!(response.head.content_length, Some(1_329_006_803));
        assert!(wire_head.contains("Content-Length: 1329006803\r\n"));
        assert_eq!(response.bytes_sent, 0);
        let BodySource::Buffered(body) = response.body else {
            return Err(Error::internal("HEAD response was not buffered").into());
        };
        assert!(body.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn default_head_content_length_omits_missing_origin_length()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let addr = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nETag: \"unknown-length\"\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let mut request = get_head("artifactory.example", "/unknown-length.whl");
        request.method = "HEAD".to_owned();

        // Act.
        let response = serve_head(&format!("http://{addr}"), &request).await?;
        let wire_head = render_head(&response.head, false);

        // Assert.
        assert_eq!(response.head.content_length, None);
        assert!(!wire_head.contains("Content-Length:"));
        assert_eq!(response.bytes_sent, 0);
        Ok(())
    }

    #[derive(Debug, PartialEq, Eq)]
    struct BurstyHeadObservation {
        status: u16,
        cache_status: Option<&'static str>,
        headers: Vec<(String, String)>,
        content_length: Option<u64>,
        body_bytes: Option<u64>,
        bytes_sent: u64,
    }

    fn observe_bursty_head(response: Response) -> BurstyHeadObservation {
        let body_bytes = match response.body {
            BodySource::Buffered(body) => Some(body.len() as u64),
            BodySource::Sendfile(lease) => Some(lease.meta().size),
            BodySource::Streamed | BodySource::LocalPath(_) => None,
        };
        BurstyHeadObservation {
            status: response.head.status,
            cache_status: response.head.cache_status,
            headers: response.head.headers,
            content_length: response.head.content_length,
            body_bytes,
            bytes_sent: response.bytes_sent,
        }
    }

    async fn observe_bursty_head_requests(
        origin_response: Vec<u8>,
        miss_request: &RequestHead,
        hit_request: &RequestHead,
        host: &str,
    ) -> std::result::Result<([BurstyHeadObservation; 2], usize), BoxError> {
        let origin_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(origin_response, Arc::clone(&origin_requests)).await;
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::new(DiskStorage::open(dir.path())?));
        let upstream = format!("http://{origin}");
        let (mut miss_stream, miss_client) = plain_stream_pair().await;

        let miss = serve_bursty(
            &mut miss_stream,
            &cache,
            &upstream,
            miss_request,
            host,
            0,
            "HEAD",
            300,
            None,
            false,
        )
        .await?;
        drop(miss_client);
        let (mut hit_stream, hit_client) = plain_stream_pair().await;
        let hit = serve_bursty(
            &mut hit_stream,
            &cache,
            &upstream,
            hit_request,
            host,
            0,
            "HEAD",
            300,
            None,
            false,
        )
        .await?;
        drop(hit_client);

        Ok((
            [observe_bursty_head(miss), observe_bursty_head(hit)],
            origin_requests.load(std::sync::atomic::Ordering::SeqCst),
        ))
    }

    async fn observe_bursty_head_miss_then_hit(
        origin_response: Vec<u8>,
        path: &str,
    ) -> std::result::Result<([BurstyHeadObservation; 2], usize), BoxError> {
        let mut request = get_head("bucket.example", path);
        request.method = "HEAD".to_owned();
        observe_bursty_head_requests(origin_response, &request, &request, "bucket.example").await
    }

    fn shared_bursty_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
        let mut shared = headers
            .iter()
            .filter(|(name, _)| {
                !name.eq_ignore_ascii_case("x-ms-client-request-id")
                    && !name.eq_ignore_ascii_case("x-cache-status")
                    && !name.eq_ignore_ascii_case("x-cache-tier-hit")
            })
            .cloned()
            .collect::<Vec<_>>();
        shared.sort();
        shared
    }

    #[tokio::test]
    async fn s3_head_bursty_cache_preserves_large_representation_length_on_miss_and_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let origin_response = b"HTTP/1.1 200 OK\r\nContent-Length: 1329006803\r\nETag: \"wheel\"\r\nConnection: close\r\n\r\n"
            .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_miss_then_hit(origin_response, "/large-wheel.whl").await?;

        // Assert.
        assert_eq!(
            observed.each_ref().map(|response| response.cache_status),
            [Some("MISS"), Some("HIT")]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.content_length),
            [Some(1_329_006_803), Some(1_329_006_803)]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.body_bytes),
            [Some(0), Some(0)]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.bytes_sent),
            [0, 0]
        );
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn s3_head_bursty_cache_omits_unknown_representation_length_on_miss_and_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let origin_response =
            b"HTTP/1.1 200 OK\r\nETag: \"unknown-length\"\r\nConnection: close\r\n\r\n".to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_miss_then_hit(origin_response, "/unknown-length.whl").await?;

        // Assert.
        assert_eq!(
            observed.each_ref().map(|response| response.cache_status),
            [Some("MISS"), Some("HIT")]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.content_length),
            [None, None]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.body_bytes),
            [Some(0), Some(0)]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.bytes_sent),
            [0, 0]
        );
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn s3_head_bursty_cache_preserves_zero_representation_length_on_miss_and_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let origin_response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"empty\"\r\nConnection: close\r\n\r\n"
                .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_miss_then_hit(origin_response, "/empty-object").await?;

        // Assert.
        assert_eq!(
            observed.each_ref().map(|response| response.cache_status),
            [Some("MISS"), Some("HIT")]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.content_length),
            [Some(0), Some(0)]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.body_bytes),
            [Some(0), Some(0)]
        );
        assert_eq!(
            observed.each_ref().map(|response| response.bytes_sent),
            [0, 0]
        );
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn s3_head_bursty_cache_preserves_shared_header_snapshot_on_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let origin_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"snapshot-v1\"\r\nLast-Modified: Sat, 15 Aug 2026 00:00:00 GMT\r\nContent-Type: application/octet-stream\r\nContent-Encoding: identity\r\nCache-Control: public, max-age=60\r\nContent-Disposition: attachment; filename=scene.usd\r\nX-Origin-Snapshot: first\r\nConnection: close\r\n\r\n"
            .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_miss_then_hit(origin_response, "/scene.usd").await?;
        let [miss, hit] = observed;
        let miss_headers = shared_bursty_headers(&miss.headers);
        let hit_headers = shared_bursty_headers(&hit.headers);

        // Assert.
        assert_eq!(miss.status, hit.status);
        assert_eq!(miss_headers, hit_headers);
        assert_eq!(
            header_value(&hit.headers, "x-origin-snapshot"),
            Some("first")
        );
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn azure_head_bursty_cache_derives_client_scoped_header_for_hit_request()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let host = "container.blob.core.windows.net";
        let mut client_a = get_head(host, "/scene.usd");
        client_a.method = "HEAD".to_owned();
        client_a
            .headers
            .push(("x-ms-client-request-id".to_owned(), "client-a".to_owned()));
        let mut client_b = client_a.clone();
        client_b.headers.clear();
        client_b
            .headers
            .push(("x-ms-client-request-id".to_owned(), "client-b".to_owned()));
        let origin_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"snapshot-v1\"\r\nX-Origin-Snapshot: first\r\nX-Ms-Request-Id: origin-request-1\r\nX-Ms-Client-Request-Id: client-a\r\nConnection: close\r\n\r\n"
            .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_requests(origin_response, &client_a, &client_b, host).await?;
        let [miss, hit] = observed;

        // Assert.
        assert_eq!(
            header_value(&miss.headers, "x-ms-client-request-id"),
            Some("client-a")
        );
        assert_eq!(
            header_value(&hit.headers, "x-ms-client-request-id"),
            Some("client-b")
        );
        assert_eq!(
            header_value(&hit.headers, "x-origin-snapshot"),
            Some("first")
        );
        assert_eq!(
            header_value(&miss.headers, "x-ms-request-id"),
            Some("origin-request-1")
        );
        assert_eq!(
            header_value(&hit.headers, "x-ms-request-id"),
            Some("origin-request-1")
        );
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn azure_head_bursty_cache_does_not_invent_absent_client_scoped_header()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let host = "container.blob.core.windows.net";
        let mut client_a = get_head(host, "/scene-without-client-id.usd");
        client_a.method = "HEAD".to_owned();
        let client_b = client_a.clone();
        let origin_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"snapshot-v1\"\r\nX-Origin-Snapshot: first\r\nConnection: close\r\n\r\n"
            .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_requests(origin_response, &client_a, &client_b, host).await?;
        let [miss, hit] = observed;

        // Assert.
        assert_eq!(header_value(&miss.headers, "x-ms-client-request-id"), None);
        assert_eq!(header_value(&hit.headers, "x-ms-client-request-id"), None);
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn s3_head_bursty_cache_does_not_replay_set_cookie_to_second_client()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let origin_response = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"snapshot-v1\"\r\nSet-Cookie: client=a; Secure\r\nConnection: close\r\n\r\n"
            .to_vec();

        // Act.
        let (observed, origin_requests) =
            observe_bursty_head_miss_then_hit(origin_response, "/client-cookie.usd").await?;
        let [miss, hit] = observed;

        // Assert.
        assert_eq!(
            header_value(&miss.headers, "set-cookie"),
            Some("client=a; Secure")
        );
        assert_eq!(header_value(&hit.headers, "set-cookie"), None);
        assert_eq!(origin_requests, 1);
        Ok(())
    }

    #[derive(Debug)]
    struct BurstyGetObservation {
        miss_wire: Vec<u8>,
        miss_cache_status: Option<&'static str>,
        miss_bytes_sent: u64,
        hit_content_length: Option<u64>,
        hit_cache_status: Option<&'static str>,
        hit_bytes_sent: u64,
        hit_uses_sendfile: bool,
        origin_requests: usize,
    }

    async fn observe_bursty_get_miss_then_hit(
        origin_response: Vec<u8>,
        path: &str,
    ) -> std::result::Result<BurstyGetObservation, BoxError> {
        let origin_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(origin_response, Arc::clone(&origin_requests)).await;
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::new(DiskStorage::open(dir.path())?));
        let request = get_head("bucket.example", path);
        let upstream = format!("http://{origin}");
        let (mut miss_stream, mut miss_client) = plain_stream_pair().await;

        let miss = serve_bursty(
            &mut miss_stream,
            &cache,
            &upstream,
            &request,
            "bucket.example",
            0,
            "GET",
            300,
            None,
            false,
        )
        .await?;
        drop(miss_stream);
        let mut miss_wire = Vec::new();
        miss_client.read_to_end(&mut miss_wire).await?;
        let (mut hit_stream, hit_client) = plain_stream_pair().await;
        let hit = serve_bursty(
            &mut hit_stream,
            &cache,
            &upstream,
            &request,
            "bucket.example",
            0,
            "GET",
            300,
            None,
            false,
        )
        .await?;
        drop(hit_client);

        Ok(BurstyGetObservation {
            miss_wire,
            miss_cache_status: miss.head.cache_status,
            miss_bytes_sent: miss.bytes_sent,
            hit_content_length: hit.head.content_length,
            hit_cache_status: hit.head.cache_status,
            hit_bytes_sent: hit.bytes_sent,
            hit_uses_sendfile: matches!(hit.body, BodySource::Sendfile(_)),
            origin_requests: origin_requests.load(std::sync::atomic::Ordering::SeqCst),
        })
    }

    #[tokio::test]
    async fn s3_get_bursty_cache_preserves_fixed_length_framing_on_miss_and_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"list response";
        let origin_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"list\"\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();

        // Act.
        let observed = observe_bursty_get_miss_then_hit(origin_response, "/?list-type=2").await?;
        let (miss_head, miss_body) = split_wire(&observed.miss_wire);

        // Assert.
        assert_eq!(observed.miss_cache_status, Some("MISS"));
        assert_eq!(observed.miss_bytes_sent, body.len() as u64);
        assert!(miss_head.contains(&format!("Content-Length: {}", body.len())));
        assert!(!miss_head.to_ascii_lowercase().contains("transfer-encoding"));
        assert_eq!(miss_body, body);
        assert_eq!(observed.hit_cache_status, Some("HIT"));
        assert_eq!(observed.hit_content_length, Some(body.len() as u64));
        assert_eq!(observed.hit_bytes_sent, body.len() as u64);
        assert!(observed.hit_uses_sendfile);
        assert_eq!(observed.origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn s3_get_bursty_forced_miss_refetches_admitted_entry()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        // A forced miss stands in for a request the access gate would not admit,
        // arriving on a key that is already warm, so the fresh entry has to stay
        // unreachable.
        let body = b"list response";
        let origin_response: Vec<u8> = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"list\"\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect();
        let origin_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(origin_response, Arc::clone(&origin_requests)).await;
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::new(DiskStorage::open(dir.path())?));
        let request = get_head("bucket.example", "/?list-type=2");
        let upstream = format!("http://{origin}");

        // Act.
        let (mut admit_stream, admit_client) = plain_stream_pair().await;
        let admitted = serve_bursty(
            &mut admit_stream,
            &cache,
            &upstream,
            &request,
            "bucket.example",
            0,
            "GET",
            300,
            None,
            false,
        )
        .await?;
        drop(admit_client);
        let (mut forced_stream, forced_client) = plain_stream_pair().await;
        let forced = serve_bursty(
            &mut forced_stream,
            &cache,
            &upstream,
            &request,
            "bucket.example",
            0,
            "GET",
            300,
            None,
            true,
        )
        .await?;
        drop(forced_client);

        // Assert.
        assert_eq!(admitted.head.cache_status, Some("MISS"));
        assert_eq!(forced.head.cache_status, Some("MISS"));
        assert_eq!(
            origin_requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a forced miss must refetch rather than serve the fresh bursty entry"
        );
        Ok(())
    }

    #[tokio::test]
    async fn s3_get_bursty_cache_reframes_unknown_length_miss_and_sizes_hit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"unknown list response";
        let origin_response = b"HTTP/1.1 200 OK\r\nETag: \"list\"\r\nConnection: close\r\n\r\n"
            .iter()
            .copied()
            .chain(body.iter().copied())
            .collect();

        // Act.
        let observed = observe_bursty_get_miss_then_hit(origin_response, "/?list-type=2").await?;
        let (miss_head, miss_body) = split_wire(&observed.miss_wire);

        // Assert.
        assert_eq!(observed.miss_cache_status, Some("MISS"));
        assert_eq!(observed.miss_bytes_sent, body.len() as u64);
        assert!(!miss_head.contains("Content-Length:"));
        assert!(
            miss_head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
        );
        assert!(find_subslice(&miss_body, body).is_some());
        assert!(miss_body.ends_with(b"0\r\n\r\n"));
        assert_eq!(observed.hit_cache_status, Some("HIT"));
        assert_eq!(observed.hit_content_length, Some(body.len() as u64));
        assert_eq!(observed.hit_bytes_sent, body.len() as u64);
        assert!(observed.hit_uses_sendfile);
        assert_eq!(observed.origin_requests, 1);
        Ok(())
    }

    #[tokio::test]
    async fn passthrough_get_response_memory_is_bounded_by_chunk_size()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        const CHUNK_BYTES: usize = 1024 * 1024;
        const CHUNKS: usize = 2 * 1024;
        let total_bytes = (CHUNK_BYTES * CHUNKS) as u64;
        let live_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = TrackedBody {
            remaining_chunks: CHUNKS,
            live_chunks: Arc::clone(&live_chunks),
            peak_chunks: Arc::clone(&peak_chunks),
        };

        // Act.
        let response = relay_transparent_response(
            &mut tokio::io::sink(),
            200,
            hdr(&[("Content-Length", &total_bytes.to_string())]),
            UpstreamTiming::default(),
            body,
            false,
        )
        .await?;

        // Assert.
        assert_eq!(response.bytes_sent, total_bytes);
        assert!(
            peak_chunks.load(Ordering::SeqCst) <= 2,
            "a 2 GiB logical response retained more than two 1 MiB chunks"
        );
        assert_eq!(live_chunks.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn if_none_match_conditional_logic() {
        // Exact match (quotes included) -> 304.
        assert!(if_none_match_matches(Some("\"abc\""), Some("\"abc\"")));
        // Wildcard matches any present validator.
        assert!(if_none_match_matches(Some("*"), Some("\"abc\"")));
        // One of several comma-separated candidates matches.
        assert!(if_none_match_matches(
            Some("\"x\", \"abc\""),
            Some("\"abc\"")
        ));
        // No match -> serve the full HIT.
        assert!(!if_none_match_matches(Some("\"other\""), Some("\"abc\"")));
        // Absent header or absent cached ETag never matches.
        assert!(!if_none_match_matches(None, Some("\"abc\"")));
        assert!(!if_none_match_matches(Some("\"abc\""), None));
    }

    #[test]
    fn tls_client_hello_detection() {
        // A TLS handshake record starts with 0x16 (handshake content type).
        assert!(looks_like_tls_client_hello(0x16));
        // Plaintext HTTP methods (`C`ONNECT, `G`ET, `P`UT, `H`EAD) do not, so
        // they are answered with a 400 instead of failing the TLS handshake.
        for b in [b'C', b'G', b'P', b'H', 0x00, 0xff] {
            assert!(!looks_like_tls_client_hello(b));
        }
    }

    /// Drives `serve_miss_buffered` directly against a plain-HTTP mock origin: a registry
    /// fill with content rewrite rules must rewrite the matching index body
    /// before admit, so the MISS response, the served `Content-Length`, and the
    /// stored entry all carry the rewritten bytes (which keeps a later HIT a
    /// correct `sendfile`). The `https://<host>` registry routing itself is unit
    /// tested in `ovcc-registry` and validated end to end by the `registry`
    /// flavor, like the other real-upstream backends.
    #[tokio::test]
    async fn serve_miss_rewrites_index_before_admit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = "<a href=\"https://files.pythonhosted.org/p/idna.whl\">idna</a>";
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(DiskStorage::open(dir.path()).unwrap());

        let rules = vec![ovcc_rewrite::RewriteRule {
            uri_starts_with: "/simple/".to_string(),
            types: vec!["text/html".to_string()],
            substitute: ovcc_rewrite::Substitute {
                match_: "https://files.pythonhosted.org".to_string(),
                replace: "http://files.pythonhosted.org.cache.svc.cluster.local:14128".to_string(),
            },
        }];
        let head = RequestHead {
            method: "GET".to_string(),
            path: "/simple/idna/".to_string(),
            query: String::new(),
            query_present: false,
            http_version: "HTTP/1.1".to_string(),
            host: Some("pypi.org.cache.svc.cluster.local".to_string()),
            headers: Vec::new(),
            content_length: 0,
            head_length: 0,
        };
        let key = KeyParts {
            generation: "0",
            host: "pypi.org.cache.svc.cluster.local",
            method: "GET",
            uri: "/simple/idna/",
            filtered_args: "",
            if_match: "",
            range: "",
        }
        .render();
        let upstream = format!("http://{addr}");
        let permit = owner_permit(&cache, &key).await;
        let mut client = tokio::io::sink();

        let resp = serve_miss_buffered(
            &mut client,
            &cache,
            &key,
            &upstream,
            &head,
            MissFill {
                snapshot_response: false,
                head_only: false,
                expires_at: None,
                admit_not_found: false,
                honor_origin_cacheability: false,
                add_lft_cors: false,
                cache_status: CacheStatus::Miss,
                normal_validity: true,
                max_idle_secs: None,
                rewrite: &rules,
                force_identity: false,
                force_buffered: true,
                recheck_auth: false,
                recheck_mutation: false,
                share_existing: true,
                key_authorizes_followers: false,
                recheck_upstream: &upstream,
            },
            permit,
        )
        .await
        .expect("serve_miss_buffered");
        let rewritten = "http://files.pythonhosted.org.cache.svc.cluster.local:14128/p/idna.whl";
        let BodySource::Buffered(served) = &resp.body else {
            panic!("expected a buffered MISS body");
        };
        let text = String::from_utf8(served.clone()).unwrap();
        assert!(text.contains(rewritten), "the MISS body is rewritten");
        assert!(
            !text.contains("https://files.pythonhosted.org/"),
            "the original cross-host link is gone"
        );
        // The served Content-Length reflects the rewritten (longer) body, not
        // the origin's.
        assert_eq!(resp.head.content_length, Some(served.len() as u64));

        // The admitted entry stores the rewritten bytes, so a HIT sendfiles the
        // correct length without a serve-time rewrite.
        let (lease, _status) = cache
            .get(key.as_str())
            .unwrap()
            .expect("the rewritten index was admitted");
        assert_eq!(lease.meta().size, served.len() as u64);
    }

    /// A fill whose request URI does not match any rewrite rule admits and serves
    /// the origin bytes unchanged, even when rules are configured.
    #[tokio::test]
    async fn serve_miss_leaves_non_matching_uri_unchanged() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = "https://files.pythonhosted.org/p/idna.whl raw wheel bytes";
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(DiskStorage::open(dir.path()).unwrap());
        let rules = vec![ovcc_rewrite::RewriteRule {
            uri_starts_with: "/simple/".to_string(),
            types: vec!["text/html".to_string()],
            substitute: ovcc_rewrite::Substitute {
                match_: "https://files.pythonhosted.org".to_string(),
                replace: "http://cache.local".to_string(),
            },
        }];
        // An artifact path, not the index, with a binary content type: neither
        // the URI prefix nor the type matches, so nothing is rewritten.
        let head = RequestHead {
            method: "GET".to_string(),
            path: "/packages/idna.whl".to_string(),
            query: String::new(),
            query_present: false,
            http_version: "HTTP/1.1".to_string(),
            host: Some("files.pythonhosted.org.cache.svc.cluster.local".to_string()),
            headers: Vec::new(),
            content_length: 0,
            head_length: 0,
        };
        let key = KeyParts {
            generation: "0",
            host: "files.pythonhosted.org.cache.svc.cluster.local",
            method: "GET",
            uri: "/packages/idna.whl",
            filtered_args: "",
            if_match: "",
            range: "",
        }
        .render();
        let upstream = format!("http://{addr}");
        let permit = owner_permit(&cache, &key).await;
        let mut client = tokio::io::sink();
        let resp = serve_miss_buffered(
            &mut client,
            &cache,
            &key,
            &upstream,
            &head,
            MissFill {
                snapshot_response: false,
                head_only: false,
                expires_at: None,
                admit_not_found: false,
                honor_origin_cacheability: false,
                add_lft_cors: false,
                cache_status: CacheStatus::Miss,
                normal_validity: true,
                max_idle_secs: None,
                rewrite: &rules,
                force_identity: false,
                force_buffered: true,
                recheck_auth: false,
                recheck_mutation: false,
                share_existing: true,
                key_authorizes_followers: false,
                recheck_upstream: &upstream,
            },
            permit,
        )
        .await
        .expect("serve_miss_buffered");
        let BodySource::Buffered(served) = &resp.body else {
            panic!("expected a buffered MISS body");
        };
        assert_eq!(served, body.as_bytes(), "non-matching body is untouched");
    }

    #[test]
    fn passthrough_get_origin_connection_header_is_not_relayed() {
        // Arrange.
        let origin = hdr(&[("Connection", "close"), ("X-Origin-Marker", "passthrough")]);

        // Act.
        let relayed = relay_headers(&origin);

        // Assert.
        assert_eq!(
            relayed,
            vec![("X-Origin-Marker".to_owned(), "passthrough".to_owned())]
        );
    }

    #[test]
    fn identity_encoding_headers_overrides_accept_encoding() {
        // A client's compressing Accept-Encoding is replaced with identity so the
        // origin returns an uncompressed body (registry fetch path).
        let headers = hdr(&[
            ("Accept-Encoding", "gzip, br"),
            ("Authorization", "Bearer x"),
        ]);
        let out = identity_encoding_headers(&headers);
        let encodings: Vec<_> = out
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(encodings, vec!["identity"], "exactly one identity encoding");
        // Other headers are preserved.
        assert!(
            out.iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer x")
        );
    }

    #[test]
    fn peer_depth_accepts_exactly_one_marked_get() {
        // Arrange.
        let marked = RequestHead {
            method: "GET".to_owned(),
            path: "/objects/a.usd".to_owned(),
            query: "signature=raw".to_owned(),
            query_present: true,
            http_version: "HTTP/1.1".to_owned(),
            host: Some("bucket.example.com".to_owned()),
            headers: hdr(&[
                ("Host", "bucket.example.com"),
                ("Authorization", "Bearer delegated"),
                ("X-OVCC-Peer-Depth", "1"),
            ]),
            content_length: 0,
            head_length: 128,
        };
        let mut recursive = marked.clone();
        recursive
            .headers
            .push(("X-OVCC-Peer-Depth".to_owned(), "2".to_owned()));
        let mut unmarked = marked.clone();
        unmarked
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("x-ovcc-peer-depth"));
        let mut marked_head = marked.clone();
        marked_head.method = "HEAD".to_owned();

        // Act.
        let accepted = peer_forwarded_request(&marked);
        let rejected_recursive = peer_forwarded_request(&recursive);
        let ordinary = peer_forwarded_request(&unmarked);
        let rejected_method = peer_forwarded_request(&marked_head);

        // Assert.
        assert_eq!(accepted, Ok(true));
        assert_eq!(ordinary, Ok(false));
        assert_eq!(rejected_recursive, Err(()));
        assert_eq!(rejected_method, Err(()));
    }

    #[test]
    fn canonical_method_normalizes_to_get_or_head() {
        // HEAD (any case) maps to the canonical uppercase key segment.
        assert_eq!(canonical_method("HEAD"), "HEAD");
        assert_eq!(canonical_method("head"), "HEAD");
        // Everything that reaches the bursty arm otherwise is GET.
        assert_eq!(canonical_method("GET"), "GET");
        assert_eq!(canonical_method("get"), "GET");
    }

    #[test]
    fn bursty_head_and_get_render_distinct_keys() {
        // GET and HEAD for the same tenant/uri must never share a bursty entry
        // (the method segment differs), so caching a HEAD cannot serve a GET.
        let get = BurstyKeyParts {
            generation: "0",
            adaptive_id: "fn-a|-",
            host: "bucket.s3.amazonaws.com",
            method: "GET",
            request_uri: "/obj",
        }
        .render();
        let head = BurstyKeyParts {
            generation: "0",
            adaptive_id: "fn-a|-",
            host: "bucket.s3.amazonaws.com",
            method: "HEAD",
            request_uri: "/obj",
        }
        .render();
        assert_ne!(get.as_str(), head.as_str());
    }

    #[test]
    fn bursty_not_found_is_the_only_extra_admitted_status() {
        // Arrange.
        let ordinary_statuses = [200, 206, 404, 410, 500];

        // Act.
        let normal: Vec<_> = ordinary_statuses
            .iter()
            .map(|status| should_admit(*status, false))
            .collect();
        let bursty: Vec<_> = ordinary_statuses
            .iter()
            .map(|status| should_admit(*status, true))
            .collect();

        // Assert.
        assert_eq!(normal, vec![true, true, false, false, false]);
        assert_eq!(bursty, vec![true, true, true, false, false]);
    }

    #[test]
    fn nucleus_get_and_head_cache_never_use_bursty_snapshot_policy() {
        // Arrange.
        let get = nucleus_miss_fill(
            false,
            ovcc_nucleus::Location::Generic,
            Some(86_400),
            "https://nucleus.example",
        );
        let head = nucleus_miss_fill(
            true,
            ovcc_nucleus::Location::Lft,
            Some(86_400),
            "https://nucleus.example",
        );

        // Act.
        let policies = [get, head];

        // Assert.
        assert!(policies.iter().all(|fill| !fill.snapshot_response));
        assert!(policies.iter().all(|fill| !fill.admit_not_found));
        assert!(policies.iter().all(|fill| fill.expires_at.is_none()));
    }

    #[test]
    fn nucleus_origin_response_rejects_explicitly_uncacheable_metadata() {
        // Arrange.
        let now = httpdate::parse_http_date("Mon, 24 Aug 2026 12:00:00 GMT").unwrap();
        let cases = [
            (vec![("Cache-Control", "public, no-store")], false),
            (vec![("Cache-Control", "public, no-cache")], false),
            (vec![("cache-control", "private=\"Set-Cookie\"")], false),
            (vec![("Cache-Control", "public, max-age=\"0\"")], false),
            (vec![("Set-Cookie", "session=secret")], false),
            (vec![("Expires", "Mon, 24 Aug 2026 11:59:59 GMT")], false),
            (vec![("Cache-Control", "public, max-age=60")], true),
            (vec![("Expires", "Mon, 24 Aug 2026 12:00:01 GMT")], true),
            (vec![("Expires", "not-an-http-date")], true),
        ];

        // Act.
        let actual: Vec<_> = cases
            .iter()
            .map(|(headers, _)| {
                let headers = headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect::<Vec<_>>();
                origin_response_is_cacheable(&headers, now)
            })
            .collect();

        // Assert.
        assert_eq!(
            actual,
            cases.map(|(_, expected)| expected),
            "Nucleus must not store origin responses carrying explicit no-store metadata"
        );
    }

    #[test]
    fn general_get_health_path_uses_the_route_prefix_contract() {
        // Arrange.
        let paths = [
            ("/healthz", true),
            ("/healthz/", true),
            ("/healthz/deep", true),
            ("/health", false),
            ("/healthz-extra", true),
        ];

        // Act.
        let matches: Vec<bool> = paths
            .iter()
            .map(|(path, _expected)| is_health_path(path))
            .collect();

        // Assert.
        assert_eq!(matches, paths.map(|(_path, expected)| expected));
    }

    #[cfg(feature = "test-fault-injection")]
    #[test]
    fn general_post_test_fault_arms_and_reports_one_generation() {
        // Arrange.
        let controller = Arc::new(ovcc_testing::FaultController::default());
        let request = b"POST /__ovcc/testing/faults/upstream-request-body?skip_matches=2&after_bytes=4194304 HTTP/1.1\r\nHost: localhost\r\n\r\n";

        // Act.
        let response = test_control_response(request, &controller).expect("control route");
        let body = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| &response[index + 4..])
            .expect("response body");
        let snapshot: ovcc_testing::Snapshot = serde_json::from_slice(body).expect("snapshot JSON");

        // Assert.
        assert!(response.starts_with(b"HTTP/1.1 201 Created\r\n"));
        assert_eq!(
            snapshot.controller_state,
            ovcc_testing::ControllerState::Armed
        );
        assert_eq!(snapshot.generation, 1);
        assert_eq!(snapshot.skips_remaining, 2);
        assert_eq!(snapshot.after_bytes, Some(4_194_304));
    }

    #[cfg(feature = "test-fault-injection")]
    #[test]
    fn general_post_test_fault_rejects_overwrite_and_bad_query() {
        // Arrange.
        let controller = Arc::new(ovcc_testing::FaultController::default());
        let valid =
            b"POST /__ovcc/testing/faults/upstream-request-body?after_bytes=1 HTTP/1.1\r\n\r\n";
        let duplicate = b"POST /__ovcc/testing/faults/upstream-request-body?after_bytes=1&after_bytes=2 HTTP/1.1\r\n\r\n";
        let missing =
            b"POST /__ovcc/testing/faults/upstream-request-body?skip_matches=1 HTTP/1.1\r\n\r\n";

        // Act.
        let first = test_control_response(valid, &controller).expect("first arm");
        let overwrite = test_control_response(valid, &controller).expect("overwrite response");
        controller.clear().expect("clear first generation");
        let bad_duplicate =
            test_control_response(duplicate, &controller).expect("duplicate response");
        let bad_missing = test_control_response(missing, &controller).expect("missing response");

        // Assert.
        assert!(first.starts_with(b"HTTP/1.1 201 Created\r\n"));
        assert!(overwrite.starts_with(b"HTTP/1.1 409 Conflict\r\n"));
        assert!(bad_duplicate.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
        assert!(bad_missing.starts_with(b"HTTP/1.1 400 Bad Request\r\n"));
    }

    // --- Streaming MISS fill / bypass -------------------------------------

    /// Spawns a one-shot mock origin that replies with the given raw HTTP/1.1
    /// response bytes, then closes the socket. Closing is what ends a
    /// close-delimited (no `Content-Length`) body, and lets a short body under a
    /// declared `Content-Length` surface as a truncated read.
    async fn spawn_raw_origin(response: Vec<u8>) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(&response).await;
                    let _ = sock.flush().await;
                    // Socket drops here, closing the connection.
                });
            }
        });
        addr
    }

    /// Spawns the same raw origin while counting accepted fetches. The count is
    /// incremented before writing so an aborted upstream connection is visible.
    async fn spawn_counted_raw_origin(
        response: Vec<u8>,
        requests: Arc<std::sync::atomic::AtomicUsize>,
    ) -> std::net::SocketAddr {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                requests.fetch_add(1, Ordering::SeqCst);
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(&response).await;
                    let _ = sock.flush().await;
                });
            }
        });
        addr
    }

    /// Downstream writer that accepts exactly `remaining` bytes, then behaves
    /// like a client whose socket disconnected mid-response.
    struct DisconnectAfter {
        remaining: usize,
    }

    impl AsyncWrite for DisconnectAfter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "client disconnected",
                )));
            }
            let accepted = self.remaining.min(buf.len());
            self.remaining -= accepted;
            Poll::Ready(Ok(accepted))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct FailingStageWriter {
        fail_write: bool,
        fail_flush: bool,
    }

    impl AsyncWrite for FailingStageWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.fail_write {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected response-head write failure",
                )))
            } else {
                Poll::Ready(Ok(buf.len()))
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.fail_flush {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected response-head flush failure",
                )))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct DisconnectDuringBody {
        head_written: bool,
        body_remaining: usize,
        body_written: Arc<std::sync::atomic::AtomicU64>,
    }

    impl AsyncWrite for DisconnectDuringBody {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if !self.head_written {
                self.head_written = true;
                return Poll::Ready(Ok(buf.len()));
            }
            if self.body_remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected partial body disconnect",
                )));
            }
            let accepted = self.body_remaining.min(buf.len());
            self.body_remaining -= accepted;
            self.body_written
                .fetch_add(accepted as u64, Ordering::SeqCst);
            Poll::Ready(Ok(accepted))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn counted_body_write_reports_partial_progress() -> std::result::Result<(), BoxError> {
        // Arrange.
        let mut downstream = DisconnectAfter { remaining: 3 };

        // Act.
        let result = write_body_counted(&mut downstream, b"abcdefgh").await;

        // Assert.
        let Err(failure) = result else {
            return Err(Error::internal("partial body write unexpectedly completed").into());
        };
        assert_eq!(failure.body_bytes, 3);
        assert_eq!(failure.error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(failure.error.to_string(), "client disconnected");
        Ok(())
    }

    #[tokio::test]
    async fn counted_chunk_write_reports_body_progress_at_each_boundary()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"abcdef";
        let header_len = format!("{:x}\r\n", body.len()).len();
        let mut header_failure = DisconnectAfter { remaining: 1 };

        // Act.
        let header_result = write_http_chunk_counted(&mut header_failure, body).await;

        // Assert.
        let Err(header_error) = header_result else {
            return Err(Error::internal("partial chunk header unexpectedly completed").into());
        };
        assert_eq!(header_error.body_bytes, 0);

        // Arrange.
        let mut body_failure = DisconnectAfter {
            remaining: header_len + 2,
        };

        // Act.
        let body_result = write_http_chunk_counted(&mut body_failure, body).await;

        // Assert.
        let Err(body_error) = body_result else {
            return Err(Error::internal("partial chunk body unexpectedly completed").into());
        };
        assert_eq!(body_error.body_bytes, 2);

        // Arrange.
        let mut trailer_failure = DisconnectAfter {
            remaining: header_len + body.len(),
        };

        // Act.
        let trailer_result = write_http_chunk_counted(&mut trailer_failure, body).await;

        // Assert.
        let Err(trailer_error) = trailer_result else {
            return Err(Error::internal("partial chunk trailer unexpectedly completed").into());
        };
        assert_eq!(trailer_error.body_bytes, body.len() as u64);
        Ok(())
    }

    #[tokio::test]
    async fn general_get_buffered_miss_counts_disconnect_across_chunks()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let mut body = Bytes::from_static(b"abc").chain(Bytes::from_static(b"defgh"));
        let mut downstream = DisconnectAfter { remaining: 5 };
        // Act.
        let result = write_buf_counted(&mut downstream, &mut body).await;
        // Assert.
        let Err(failure) = result else {
            return Err(Error::internal("disconnected body unexpectedly completed").into());
        };
        assert_eq!(failure.body_bytes, 5);
        assert_eq!(failure.error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(body.remaining(), 3);
        assert_eq!(body.chunk(), b"fgh");
        Ok(())
    }

    #[tokio::test]
    async fn follower_response_head_reports_write_and_flush_stages()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let meta = peer_meta(8);
        let mut write_failure = FailingStageWriter {
            fail_write: true,
            fail_flush: false,
        };

        // Act.
        let write_result =
            write_follower_response_head(&mut write_failure, &meta, 8, false, false).await;

        // Assert.
        let Err((write_stage, write_error)) = write_result else {
            return Err(Error::internal("response-head write failure was swallowed").into());
        };
        assert_eq!(write_stage, FollowerClientFailureStage::ResponseHeadWrite);
        assert_eq!(
            write_error.to_string(),
            "injected response-head write failure"
        );

        // Arrange.
        let mut flush_failure = FailingStageWriter {
            fail_write: false,
            fail_flush: true,
        };

        // Act.
        let flush_result =
            write_follower_response_head(&mut flush_failure, &meta, 8, false, false).await;

        // Assert.
        let Err((flush_stage, flush_error)) = flush_result else {
            return Err(Error::internal("response-head flush failure was swallowed").into());
        };
        assert_eq!(flush_stage, FollowerClientFailureStage::ResponseHeadFlush);
        assert_eq!(
            flush_error.to_string(),
            "injected response-head flush failure"
        );
        Ok(())
    }

    #[test]
    fn follower_client_abort_context_is_visible_at_info() {
        // Arrange.
        let capture = EventCapture::default();
        let subscriber = capture.info_subscriber();

        // Act.
        tracing::subscriber::with_default(subscriber, || {
            log_peer_fill_follower_client_abort(
                FollowerClientFailureStage::ResponseHeadFlush,
                17,
                &std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected visible client abort",
                ),
            );
        });

        // Assert.
        let events = capture.events();
        let event = captured_event(&events, "peer_fill_follower_client_abort");
        assert_eq!(event.level, tracing::Level::INFO);
        assert_eq!(
            event.fields.get("stage").map(String::as_str),
            Some("response_head_flush")
        );
        assert_eq!(
            event.fields.get("bytes_sent").map(String::as_str),
            Some("17")
        );
        assert!(
            event
                .fields
                .get("error")
                .is_some_and(|error| error.contains("injected visible client abort"))
        );
    }

    fn large_streaming_response() -> (Vec<u8>, usize) {
        let body_len = MISS_BUFFER_MAX_BYTES as usize + 1;
        let head = format!(
            "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {body_len}\r\nConnection: close\r\n\r\n"
        );
        let mut response = Vec::with_capacity(head.len() + body_len);
        response.extend_from_slice(head.as_bytes());
        response.resize(head.len() + body_len, b'x');
        (response, body_len)
    }

    #[derive(Debug)]
    struct TestBodyError;

    impl std::fmt::Display for TestBodyError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("injected body failure")
        }
    }

    impl std::error::Error for TestBodyError {}

    struct TestBody {
        rx: mpsc::Receiver<std::result::Result<Bytes, TestBodyError>>,
    }

    impl hyper::body::Body for TestBody {
        type Data = Bytes;
        type Error = TestBodyError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            Pin::new(&mut self.rx)
                .poll_recv(context)
                .map(|frame| frame.map(|frame| frame.map(hyper::body::Frame::data)))
        }
    }

    struct TimeoutBody {
        state: u8,
    }

    impl hyper::body::Body for TimeoutBody {
        type Data = Bytes;
        type Error = ovcc_upstream::OriginBodyError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            self.state += 1;
            match self.state {
                1 => Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from_static(
                    b"partial",
                ))))),
                2 => Poll::Ready(Some(Err(ovcc_upstream::OriginBodyError::Timeout(
                    ovcc_upstream::OriginTimeout {
                        stage: ovcc_upstream::OriginTimeoutStage::ResponseBody,
                        duration: Duration::from_secs(60),
                    },
                )))),
                _ => Poll::Ready(None),
            }
        }
    }

    #[tokio::test]
    async fn origin_body_timeout_closes_chunked_response_without_terminator()
    -> std::result::Result<(), BoxError> {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let response = relay_transparent_response(
            &mut server,
            200,
            Vec::new(),
            UpstreamTiming::default(),
            TimeoutBody { state: 0 },
            false,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        assert_eq!(
            response.timeout_stage,
            Some(TimeoutStage::OriginResponseBody)
        );
        assert!(
            wire.windows(b"7\r\npartial\r\n".len())
                .any(|part| part == b"7\r\npartial\r\n")
        );
        assert!(!wire.ends_with(b"0\r\n\r\n"));
        Ok(())
    }

    struct TrackedChunk {
        bytes: &'static [u8],
        live_chunks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl AsRef<[u8]> for TrackedChunk {
        fn as_ref(&self) -> &[u8] {
            self.bytes
        }
    }

    impl Drop for TrackedChunk {
        fn drop(&mut self) {
            self.live_chunks.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct TrackedBody {
        remaining_chunks: usize,
        live_chunks: Arc<std::sync::atomic::AtomicUsize>,
        peak_chunks: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl hyper::body::Body for TrackedBody {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
            static CHUNK: [u8; 1024 * 1024] = [b'x'; 1024 * 1024];
            if self.remaining_chunks == 0 {
                return Poll::Ready(None);
            }
            self.remaining_chunks -= 1;
            let live = self.live_chunks.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak_chunks.fetch_max(live, Ordering::SeqCst);
            let bytes = Bytes::from_owner(TrackedChunk {
                bytes: &CHUNK,
                live_chunks: Arc::clone(&self.live_chunks),
            });
            Poll::Ready(Some(Ok(hyper::body::Frame::data(bytes))))
        }
    }

    #[tokio::test]
    async fn nucleus_get_buffered_miss_releases_frames_after_serving()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let key = plain_key("nucleus.example", "/small.usd");
        let head = get_head("nucleus.example", "/small.usd");
        let permit = owner_permit(&cache, &key).await;
        let live_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = TrackedBody {
            remaining_chunks: 16,
            live_chunks: Arc::clone(&live_chunks),
            peak_chunks: Arc::clone(&peak_chunks),
        };
        let outcome = fill_from_open_response(
            &mut tokio::io::sink(),
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            permit,
            200,
            None,
            hdr(&[("Content-Length", "16777216")]),
            None,
            body,
            FillSource::Origin,
        )
        .await?;
        let FillOutcome::Served(response) = outcome else {
            panic!("origin fill must be served");
        };
        assert_eq!(response.cache_store_result, Some(CacheStoreResult::Stored));
        assert_eq!(response.bytes_sent, 16 * 1024 * 1024);
        assert!(cache.get(key.as_str())?.is_some());
        assert_eq!(live_chunks.load(Ordering::SeqCst), 0);
        assert_eq!(
            peak_chunks.load(Ordering::SeqCst),
            16,
            "small MISS must retain the original chunks through admission"
        );
        Ok(())
    }

    #[tokio::test]
    async fn general_get_buffered_miss_keeps_single_frame_allocation() {
        use bytes::Buf;
        // Arrange.
        let data = Bytes::from(vec![42; 16384]);
        let pointer = data.as_ptr();
        // Act.
        let collected = collect_miss_body(http_body_util::Full::new(data), 16384)
            .await
            .unwrap();
        // Assert.
        assert_eq!(collected.remaining(), 16384);
        assert_eq!(collected.chunk().as_ptr(), pointer);
    }

    #[tokio::test]
    async fn general_get_buffered_miss_rejects_overlength_before_admission() {
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/overlength");
        let head = get_head("origin.example", "/overlength");
        let outcome = fill_from_open_response(
            &mut tokio::io::sink(),
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            owner_permit(&cache, &key).await,
            200,
            None,
            hdr(&[("Content-Length", "3")]),
            None,
            http_body_util::Full::new(Bytes::from_static(b"long")),
            FillSource::Origin,
        )
        .await
        .unwrap();
        let FillOutcome::Served(response) = outcome else {
            panic!("origin response");
        };
        assert_eq!(response.bytes_sent, 0);
        assert!(cache.get(key.as_str()).unwrap().is_none());
    }

    #[tokio::test]
    async fn general_get_buffered_miss_preserves_frames_trailers_and_short_body() {
        use hyper::body::Frame;
        let frames = [
            Ok::<_, std::convert::Infallible>(Frame::data(Bytes::from_static(b"ab"))),
            Ok(Frame::data(Bytes::new())),
            Ok(Frame::data(Bytes::from_static(b"c"))),
            Ok(Frame::trailers(http::HeaderMap::new())),
        ];
        let body = http_body_util::StreamBody::new(tokio_stream::iter(frames));
        let mut collected = collect_miss_body(body, 4).await.unwrap();
        assert_eq!(collected.copy_to_bytes(collected.remaining()), b"abc"[..]);
        let empty = http_body_util::Empty::<Bytes>::new();
        assert!(!collect_miss_body(empty, 0).await.unwrap().has_remaining());
        assert!(matches!(
            collect_miss_body(http_body_util::Full::new(Bytes::from_static(b"x")), 0).await,
            Err(BufferedMissError::TooLong)
        ));
    }

    struct DiscardStorage {
        written: Arc<AtomicU64>,
    }

    impl Storage for DiscardStorage {
        fn lookup(&self, _key: &str) -> Result<ovcc_storage::Lookup> {
            Ok(ovcc_storage::Lookup::Miss)
        }

        fn peek(&self, _key: &str) -> Result<Option<StoredMeta>> {
            Ok(None)
        }

        fn begin_fill(&self, _key: &str, _meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
            Ok(Box::new(DiscardWriter {
                written: Arc::clone(&self.written),
            }))
        }

        fn evict_data(&self, _key: &str, _revision: u64) -> Result<bool> {
            Ok(false)
        }

        fn remove(&self, _key: &str) -> Result<()> {
            Ok(())
        }

        fn recover_on_open(&self) -> Result<ovcc_storage::RecoveryReport> {
            Ok(ovcc_storage::RecoveryReport::default())
        }
    }

    struct DiscardWriter {
        written: Arc<AtomicU64>,
    }

    impl StoreWrite for DiscardWriter {
        fn write_chunk(&mut self, bytes: &[u8]) -> Result<()> {
            self.written.fetch_add(bytes.len() as u64, Ordering::SeqCst);
            Ok(())
        }

        fn commit(self: Box<Self>) -> Result<Lease> {
            Err(Error::storage("discard writer has no committed lease"))
        }

        fn abort(self: Box<Self>) {}
    }

    struct PrefixSink {
        prefix: Vec<u8>,
        capacity: usize,
    }

    impl PrefixSink {
        fn new(capacity: usize) -> Self {
            Self {
                prefix: Vec::with_capacity(capacity),
                capacity,
            }
        }
    }

    impl AsyncWrite for PrefixSink {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let retained = (self.capacity - self.prefix.len()).min(bytes.len());
            self.prefix.extend_from_slice(&bytes[..retained]);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A `RequestHead` for a plain GET to `host`/`path` (no validators).
    fn get_head(host: &str, path: &str) -> RequestHead {
        RequestHead {
            method: "GET".to_string(),
            path: path.to_string(),
            query: String::new(),
            query_present: false,
            http_version: "HTTP/1.1".to_string(),
            host: Some(host.to_string()),
            headers: Vec::new(),
            content_length: 0,
            head_length: 0,
        }
    }

    #[test]
    fn registry_get_policy_head_sends_only_the_upstream_host() {
        // Arrange.
        let head = parse_request_head(
            b"GET /simple/idna/ HTTP/1.1\r\nHost: decoy.example\r\nHost: PyPI.org.cache.svc.cluster.local:14128\r\nAuthorization: Bearer test-credential\r\n",
        )
        .unwrap();

        for keepalive in [true, false] {
            let mut route = ovcc_config::BackendRoute::default();
            route.enable_upstream_keepalive = keepalive;

            // Act.
            let origin = upstream_policy_head(&head, &route, Some("pypi.org")).unwrap();

            // Assert.
            let hosts: Vec<_> = origin
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(hosts, ["pypi.org"], "keepalive={keepalive}");
            assert_eq!(origin.host, head.host, "keepalive={keepalive}");
            assert!(header_value(&origin.headers, "authorization").is_some());
            assert_eq!(
                header_value(&origin.headers, "connection").is_some(),
                !keepalive,
                "keepalive={keepalive}"
            );
        }
    }

    #[test]
    fn general_get_policy_head_keeps_the_client_host_outside_registry() {
        // Arrange.
        let head =
            parse_request_head(b"GET /object HTTP/1.1\r\nHost: objects.test:9443\r\n").unwrap();
        let keepalive = ovcc_config::BackendRoute::default();
        let mut close = ovcc_config::BackendRoute::default();
        close.enable_upstream_keepalive = false;

        // Act.
        let unchanged = upstream_policy_head(&head, &keepalive, None);
        let closed = upstream_policy_head(&head, &close, None).unwrap();

        // Assert.
        assert!(unchanged.is_none());
        assert_eq!(
            header_value(&closed.headers, "host"),
            Some("objects.test:9443")
        );
        assert_eq!(header_value(&closed.headers, "connection"), Some("close"));
    }

    #[test]
    fn registry_get_cacheable_uri_trailing_empty_query_preserved() {
        // Arrange.
        let bare =
            parse_request_head(b"GET /v2/token HTTP/1.1\r\nHost: registry.example\r\n").unwrap();
        let empty_query =
            parse_request_head(b"GET /v2/token? HTTP/1.1\r\nHost: registry.example\r\n").unwrap();

        // Act and assert.
        assert_eq!(request_uri(&bare), "/v2/token");
        assert_eq!(request_uri(&empty_query), "/v2/token?");
        assert_eq!(
            origin_url("https://registry.example", &empty_query),
            "https://registry.example/v2/token?"
        );
    }

    #[test]
    fn http_request_body_size_requires_known_framing() {
        let empty = get_head("bucket.example.com", "/object");
        assert_eq!(http_request_body_size(&empty), Some(0));

        let mut fixed = empty;
        fixed
            .headers
            .push(("Content-Length".to_owned(), "42".to_owned()));
        assert_eq!(http_request_body_size(&fixed), Some(42));

        let mut invalid = get_head("bucket.example.com", "/object");
        invalid
            .headers
            .push(("Content-Length".to_owned(), "invalid".to_owned()));
        assert_eq!(http_request_body_size(&invalid), None);

        let mut chunked = fixed;
        chunked
            .headers
            .push(("Transfer-Encoding".to_owned(), "chunked".to_owned()));
        assert_eq!(http_request_body_size(&chunked), None);
    }

    /// Runs `f` with an in-memory client socket and returns `(response, wire)` —
    /// the arm's [`Response`] plus every byte it wrote to the client.
    async fn drive_streamed<F, Fut>(f: F) -> (Response, Vec<u8>)
    where
        F: FnOnce(tokio::io::DuplexStream) -> Fut,
        Fut: std::future::Future<Output = (Response, tokio::io::DuplexStream)>,
    {
        use tokio::io::AsyncReadExt;
        // 8MiB buffer holds the small test bodies without a concurrent reader.
        let (mut client_rx, client_tx) = tokio::io::duplex(8 * 1024 * 1024);
        let (response, client_tx) = f(client_tx).await;
        // Drop the write half so `read_to_end` on the peer terminates.
        drop(client_tx);
        let mut wire = Vec::new();
        client_rx.read_to_end(&mut wire).await.unwrap();
        (response, wire)
    }

    /// Splits an HTTP/1.1 wire capture into `(head, body)` at the blank line.
    fn split_wire(wire: &[u8]) -> (String, Vec<u8>) {
        let sep = find_subslice(wire, b"\r\n\r\n").expect("head/body boundary");
        let head = String::from_utf8_lossy(&wire[..sep]).into_owned();
        (head, wire[sep + 4..].to_vec())
    }

    fn fresh_cache() -> (tempfile::TempDir, Cache<DiskStorage>) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(DiskStorage::open(dir.path()).unwrap());
        (dir, cache)
    }

    #[test]
    fn revalidation_uses_stored_validators_and_exact_expiry_boundary() {
        let meta = StoredMeta {
            etag: Some("\"stored-etag\"".to_owned()),
            last_modified: Some("Mon, 24 Aug 2026 12:00:00 GMT".to_owned()),
            expires_at: Some(100),
            ..StoredMeta::default()
        };
        let headers = revalidation_headers(
            &[
                ("If-None-Match".to_owned(), "\"client-etag\"".to_owned()),
                (
                    "If-Modified-Since".to_owned(),
                    "Sun, 23 Aug 2026 12:00:00 GMT".to_owned(),
                ),
                ("Range".to_owned(), "bytes=0-3".to_owned()),
            ],
            &meta,
            false,
        );

        assert!(cache_entry_is_fresh(&meta, 99));
        assert!(!cache_entry_is_fresh(&meta, 100));
        assert_eq!(
            header_value(&headers, "if-none-match"),
            Some("\"stored-etag\"")
        );
        assert_eq!(
            header_value(&headers, "if-modified-since"),
            Some("Mon, 24 Aug 2026 12:00:00 GMT")
        );
        assert_eq!(header_value(&headers, "range"), Some("bytes=0-3"));
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
                .count(),
            1
        );
    }

    #[test]
    fn registry_revalidation_keeps_identity_representation_policy() {
        let meta = StoredMeta {
            etag: Some("\"identity-etag\"".to_owned()),
            ..StoredMeta::default()
        };
        let headers = revalidation_headers(
            &[
                ("Accept-Encoding".to_owned(), "gzip, br".to_owned()),
                ("Authorization".to_owned(), "Bearer token".to_owned()),
            ],
            &meta,
            true,
        );

        assert_eq!(header_value(&headers, "accept-encoding"), Some("identity"));
        assert_eq!(
            header_value(&headers, "if-none-match"),
            Some("\"identity-etag\"")
        );
        assert_eq!(
            header_value(&headers, "authorization"),
            Some("Bearer token")
        );
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
                .count(),
            1
        );
    }

    #[test]
    fn proxy_get_validator_logs_preserve_fallback_cache_observation() {
        // Arrange.
        for inner_etag in [None, Some(""), Some("\"replacement\"")] {
            let inner = Response::buffered(200, "OK", Some("HIT"), Vec::new(), Vec::new())
                .with_cached_etag(inner_etag.map(str::to_owned));

            // Act.
            let outer = inner.with_probe_etags(
                Some("\"superseded\"".to_owned()),
                Some("\"origin\"".to_owned()),
            );

            // Assert.
            assert_eq!(outer.cached_etag.as_deref(), inner_etag);
            assert_eq!(outer.upstream_etag.as_deref(), Some("\"origin\""));
        }
    }

    #[test]
    fn successful_revalidation_reports_revalidated_on_body_and_304() {
        let (_dir, cache) = fresh_cache();
        let mut writer = cache
            .begin_fill(
                "revalidated",
                StoredMeta {
                    etag: Some("\"etag-1\"".to_owned()),
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();

        let lease = match cache.lookup("revalidated").unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected committed generation, got {other:?}"),
        };
        let body_response = serve_revalidated(lease, &get_head("nucleus", "/asset"), false, false);
        assert_eq!(body_response.head.status, 200);
        assert_eq!(body_response.head.cache_status, Some("REVALIDATED"));
        assert_eq!(body_response.cached_etag.as_deref(), Some("\"etag-1\""));

        let lease = match cache.lookup("revalidated").unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected committed generation, got {other:?}"),
        };
        let mut conditional = get_head("nucleus", "/asset");
        conditional
            .headers
            .push(("If-None-Match".to_owned(), "\"etag-1\"".to_owned()));
        let not_modified = serve_revalidated(lease, &conditional, false, false);
        assert_eq!(not_modified.head.status, 304);
        assert_eq!(not_modified.head.cache_status, Some("REVALIDATED"));
        assert_eq!(not_modified.head.content_length, None);
        assert_eq!(not_modified.cached_etag.as_deref(), Some("\"etag-1\""));
    }

    #[tokio::test]
    async fn global_get_expired_entry_origin_not_modified_preserves_body()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                validity_200_secs: Some(60),
                ..EvictionPolicy::default()
            },
        ));
        let key = CacheKey::from_raw("s3|expired-revalidation".to_owned());
        let mut writer = cache.begin_fill(
            key.as_str(),
            StoredMeta {
                etag: Some("\"stored-etag\"".to_owned()),
                expires_at: Some(0),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"unchanged body")?;
        writer.commit()?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let (request_tx, request_rx) = oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = request_tx.send(request);
            socket
                .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
                .await?;
            socket.flush().await?;
            Ok::<(), std::io::Error>(())
        });
        let (mut stream, client) = plain_stream_pair().await;
        let head = get_head("bucket.s3.example", "/asset.usd");

        // Act.
        let response = revalidate_entry(
            &mut stream,
            &cache,
            &key,
            &format!("http://{origin_address}"),
            &head,
            miss_fill_plain(),
            false,
            RevalidationExpiry::CachePolicy,
        )
        .await?;
        let request = String::from_utf8(request_rx.await?)?;
        origin.await??;
        drop(client);

        // Assert.
        assert!(request.contains("if-none-match: \"stored-etag\"\r\n"));
        assert_eq!(response.head.status, 200);
        assert_eq!(response.head.cache_status, Some("REVALIDATED"));
        assert_eq!(response.cached_etag.as_deref(), Some("\"stored-etag\""));
        assert_eq!(response.upstream_etag, None);
        let BodySource::Sendfile(mut lease) = response.body else {
            return Err(Error::internal("revalidated body was not served from cache").into());
        };
        std::io::Seek::seek(&mut lease.file, std::io::SeekFrom::Start(lease.body_offset))?;
        let mut body = Vec::new();
        lease.file.read_to_end(&mut body)?;
        assert_eq!(body, b"unchanged body");
        assert!(
            lease
                .meta
                .expires_at
                .is_some_and(|expiry| expiry > now_unix())
        );
        Ok(())
    }

    #[tokio::test]
    async fn global_get_revalidation_waiter_fresh_entry_rechecks_requester_authorization()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::new(DiskStorage::open(dir.path())?));
        let key = CacheKey::from_raw("registry|authorized-revalidation-waiter".to_owned());
        let mut writer = cache.begin_fill_preserving_expiry(
            key.as_str(),
            StoredMeta {
                etag: Some("\"stored-etag\"".to_owned()),
                expires_at: Some(now_unix() + 3_600),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"authorized body")?;
        writer.commit()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let (request_tx, request_rx) = oneshot::channel();
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = request_tx.send(request);
            socket
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            socket.flush().await?;
            Ok::<(), std::io::Error>(())
        });
        let upstream = format!("http://{origin_address}");
        let mut fill = miss_fill_plain();
        fill.recheck_auth = true;
        fill.recheck_upstream = &upstream;
        let mut head = get_head("registry.example", "/v2/image/manifests/latest");
        head.headers
            .push(("Authorization".to_owned(), "Bearer requester".to_owned()));
        let (mut stream, client) = plain_stream_pair().await;

        // Act.
        let response = revalidate_entry(
            &mut stream,
            &cache,
            &key,
            &upstream,
            &head,
            fill,
            false,
            RevalidationExpiry::CachePolicy,
        )
        .await?;
        let request = String::from_utf8(request_rx.await?)?;
        origin.await??;
        drop(client);

        // Assert.
        assert!(request.contains("authorization: Bearer requester\r\n"));
        assert_eq!(response.head.status, 403);
        assert_eq!(response.head.cache_status, Some("HIT"));
        let BodySource::Buffered(body) = response.body else {
            return Err(Error::internal("unauthorized waiter received cached bytes").into());
        };
        assert_eq!(body, b"Forbidden\n");
        assert!(cache.peek(key.as_str())?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn registry_revalidation_changed_body_still_applies_rewrite()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                validity_200_secs: Some(60),
                ..EvictionPolicy::default()
            },
        ));
        let key = CacheKey::from_raw("registry|expired-index".to_owned());
        let mut writer = cache.begin_fill(
            key.as_str(),
            StoredMeta {
                etag: Some("\"old-index\"".to_owned()),
                expires_at: Some(0),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"old index")?;
        writer.commit()?;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let (request_tx, request_rx) = oneshot::channel();
        let origin_body = b"https://files.example/new.whl";
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = request_tx.send(request);
            let response_head = format!(
                "HTTP/1.1 200 OK\r\nETag: \"new-index\"\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                origin_body.len()
            );
            socket.write_all(response_head.as_bytes()).await?;
            socket.write_all(origin_body).await?;
            socket.flush().await?;
            Ok::<(), std::io::Error>(())
        });
        let rules = vec![ovcc_rewrite::RewriteRule {
            uri_starts_with: "/simple/".to_owned(),
            types: vec!["text/html".to_owned()],
            substitute: ovcc_rewrite::Substitute {
                match_: "https://files.example".to_owned(),
                replace: "https://files.example.cache.svc".to_owned(),
            },
        }];
        let mut fill = miss_fill_plain();
        fill.rewrite = &rules;
        fill.force_identity = true;
        let (mut stream, client) = plain_stream_pair().await;
        let mut head = get_head("registry.example", "/simple/project/");
        head.headers
            .push(("Accept-Encoding".to_owned(), "gzip".to_owned()));

        // Act.
        let response = revalidate_entry(
            &mut stream,
            &cache,
            &key,
            &format!("http://{origin_address}"),
            &head,
            fill,
            false,
            RevalidationExpiry::CachePolicy,
        )
        .await?;
        let request = String::from_utf8(request_rx.await?)?;
        origin.await??;
        drop(client);

        // Assert.
        assert!(request.contains("if-none-match: \"old-index\"\r\n"));
        assert!(request.contains("accept-encoding: identity\r\n"));
        assert_eq!(response.head.cache_status, Some("EXPIRED"));
        assert_eq!(response.cached_etag.as_deref(), Some("\"old-index\""));
        assert_eq!(response.upstream_etag.as_deref(), Some("\"new-index\""));
        let expected = b"https://files.example.cache.svc/new.whl";
        let BodySource::Buffered(body) = response.body else {
            return Err(Error::internal("rewritten revalidation response was not buffered").into());
        };
        assert_eq!(body, expected);
        let mut cached = match cache.lookup(key.as_str())? {
            Lookup::Hit(lease) => lease,
            other => {
                return Err(
                    Error::internal(format!("rewritten body not cached: {other:?}")).into(),
                );
            }
        };
        let mut cached_body = Vec::new();
        std::io::Seek::seek(
            &mut cached.file,
            std::io::SeekFrom::Start(cached.body_offset),
        )?;
        cached.file.read_to_end(&mut cached_body)?;
        assert_eq!(cached_body, expected);
        Ok(())
    }

    #[tokio::test]
    async fn bursty_revalidation_refreshes_with_bursty_ttl() -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                validity_200_secs: Some(86_400),
                ..EvictionPolicy::default()
            },
        ));
        let head = get_head("bucket.example", "/?list-type=2");
        let request_uri = request_uri(&head);
        let key = BurstyKeyParts {
            generation: "0",
            adaptive_id: "-",
            host: "bucket.example",
            method: "GET",
            request_uri: &request_uri,
        }
        .render();
        let mut writer = cache.begin_fill_preserving_expiry(
            key.as_str(),
            StoredMeta {
                etag: Some("\"list-v1\"".to_owned()),
                expires_at: Some(0),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"cached list")?;
        writer.commit()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            assert!(String::from_utf8_lossy(&request).contains("if-none-match: \"list-v1\"\r\n"));
            socket
                .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
                .await?;
            socket.flush().await?;
            Ok::<(), std::io::Error>(())
        });
        let (mut stream, client) = plain_stream_pair().await;
        let before = now_unix();

        // Act.
        let response = serve_bursty(
            &mut stream,
            &cache,
            &format!("http://{origin_address}"),
            &head,
            "bucket.example",
            0,
            "GET",
            30,
            None,
            false,
        )
        .await?;
        origin.await??;
        drop(client);

        // Assert.
        assert_eq!(response.head.cache_status, Some("REVALIDATED"));
        let BodySource::Sendfile(lease) = response.body else {
            return Err(Error::internal("bursty revalidation did not retain cached body").into());
        };
        assert!(lease.meta().expires_at.is_some_and(|expires_at| {
            expires_at >= before + 30 && expires_at <= now_unix() + 30
        }));
        Ok(())
    }

    #[tokio::test]
    async fn azure_head_bursty_cache_revalidation_preserves_snapshot_and_method()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Arc::new(Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                validity_200_secs: Some(86_400),
                ..EvictionPolicy::default()
            },
        ));
        let host = "container.blob.core.windows.net";
        let mut head = get_head(host, "/asset.usd");
        head.method = "HEAD".to_owned();
        head.headers
            .push(("x-ms-client-request-id".to_owned(), "client-b".to_owned()));
        let request_uri = request_uri(&head);
        let key = BurstyKeyParts {
            generation: "0",
            adaptive_id: "-",
            host,
            method: "HEAD",
            request_uri: &request_uri,
        }
        .render();
        let writer = cache.begin_fill_preserving_expiry(
            key.as_str(),
            StoredMeta {
                etag: Some("\"head-v1\"".to_owned()),
                headers: vec![
                    ("x-origin-snapshot".to_owned(), "first".to_owned()),
                    ("x-ms-client-request-id".to_owned(), "client-a".to_owned()),
                ],
                representation_length: Some(4_096),
                expires_at: Some(0),
                ..StoredMeta::default()
            },
        )?;
        writer.commit()?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            assert!(request.starts_with("HEAD /asset.usd HTTP/1.1\r\n"));
            assert!(request.contains("if-none-match: \"head-v1\"\r\n"));
            socket
                .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
                .await?;
            socket.flush().await?;
            Ok::<(), std::io::Error>(())
        });
        let (mut stream, client) = plain_stream_pair().await;

        // Act.
        let response = serve_bursty(
            &mut stream,
            &cache,
            &format!("http://{origin_address}"),
            &head,
            host,
            0,
            "HEAD",
            30,
            None,
            false,
        )
        .await?;
        origin.await??;
        drop(client);

        // Assert.
        assert_eq!(response.head.cache_status, Some("REVALIDATED"));
        assert_eq!(response.head.content_length, Some(4_096));
        assert_eq!(
            header_value(&response.head.headers, "x-origin-snapshot"),
            Some("first")
        );
        assert_eq!(
            header_value(&response.head.headers, "x-ms-client-request-id"),
            Some("client-b")
        );
        assert_eq!(response.bytes_sent, 0);
        let BodySource::Buffered(body) = response.body else {
            return Err(Error::internal("revalidated HEAD response carried a body source").into());
        };
        assert!(body.is_empty());
        Ok(())
    }

    fn plain_key(host: &str, uri: &str) -> ovcc_cachekey::CacheKey {
        KeyParts {
            generation: "0",
            host,
            method: "GET",
            uri,
            filtered_args: "",
            if_match: "",
            range: "",
        }
        .render()
    }

    fn miss_fill_plain() -> MissFill<'static> {
        MissFill {
            snapshot_response: false,
            head_only: false,
            expires_at: None,
            admit_not_found: false,
            honor_origin_cacheability: false,
            add_lft_cors: false,
            cache_status: CacheStatus::Miss,
            normal_validity: true,
            max_idle_secs: None,
            rewrite: &[],
            force_identity: false,
            force_buffered: false,
            recheck_auth: false,
            recheck_mutation: false,
            share_existing: true,
            key_authorizes_followers: false,
            recheck_upstream: "",
        }
    }

    #[tokio::test]
    async fn nucleus_uncacheable_origin_response_is_relayed_without_admission() {
        // Arrange.
        let origin = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nCache-Control: public, no-store\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
                .to_vec(),
        )
        .await;
        let (_dir, cache) = fresh_cache();
        let key = plain_key("nucleus.example", "/omni/object.usd");
        let head = get_head("nucleus.example", "/omni/object.usd");
        let upstream = format!("http://{origin}");
        let permit = owner_permit(&cache, &key).await;
        let mut fill = miss_fill_plain();
        fill.honor_origin_cacheability = true;
        fill.force_buffered = true;
        let mut client = tokio::io::sink();

        // Act.
        let response =
            serve_miss_buffered(&mut client, &cache, &key, &upstream, &head, fill, permit)
                .await
                .expect("uncacheable origin response should still be relayed");

        // Assert.
        let BodySource::Buffered(body) = response.body else {
            panic!("Nucleus buffered MISS should preserve the response body");
        };
        assert_eq!(body, b"body");
        assert_eq!(response.head.reason, "OK");
        assert_eq!(response.head.cache_status, Some("MISS"));
        assert!(cache.get(key.as_str()).unwrap().is_none());
    }

    #[tokio::test]
    async fn nucleus_get_streaming_lft_partial_commits_before_cors_response()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = CacheKey::from_raw("nucleus|0|tenant|content|bytes=0-3".to_owned());
        let mut head = get_head("nucleus.example", "/omni/lft");
        head.headers
            .push(("Range".to_owned(), "bytes=0-3".to_owned()));
        let permit = match cache.acquire_fill(key.as_str()).await? {
            FillDecision::Owner(permit) => Some(permit),
            _ => return Err(Error::internal("cold LFT key did not elect a fill owner").into()),
        };
        let mut fill = miss_fill_plain();
        fill.honor_origin_cacheability = true;
        fill.add_lft_cors = true;
        let headers = hdr(&[
            ("Content-Length", "4"),
            ("Content-Range", "bytes 0-3/10"),
            ("ETag", "\"slice-v1\""),
        ]);
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Act.
        let outcome = fill_from_open_response(
            &mut server,
            &cache,
            &key,
            &head,
            fill,
            permit,
            206,
            Some("\"slice-v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(Bytes::from_static(b"part")),
            FillSource::Origin,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin LFT fill requested peer fallback").into());
        };
        assert_eq!(response.cache_store_result, Some(CacheStoreResult::Stored));
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.contains("HTTP/1.1 206 Partial Content"));
        assert!(wire_head.contains("Content-Range: bytes 0-3/10"));
        assert!(wire_head.contains("Access-Control-Allow-Origin: *"));
        assert!(wire_head.contains("Access-Control-Allow-Headers: *"));
        assert!(wire_head.contains("Access-Control-Allow-Methods: *"));
        assert_eq!(wire_body, b"part");

        let (lease, _) = cache
            .get(key.as_str())?
            .ok_or_else(|| Error::internal("small LFT fill was not committed before return"))?;
        assert_eq!(lease.meta().content_range.as_deref(), Some("bytes 0-3/10"));
        assert!(
            lease
                .meta
                .headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("access-control-allow-origin"))
        );
        let mut hit = serve_hit_conditional(lease, &head, false);
        nucleus_add_cors(&mut hit, ovcc_nucleus::Location::Lft);
        assert_eq!(hit.head.status, 206);
        assert!(
            hit.head
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("content-range")
                    && value == "bytes 0-3/10")
        );
        assert!(hit.head.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("access-control-allow-origin") && value == "*"
        }));

        let (lease, _) = cache
            .get(key.as_str())?
            .ok_or_else(|| Error::internal("committed LFT slice disappeared before recheck"))?;
        let mut conditional_head = head.clone();
        conditional_head
            .headers
            .push(("If-None-Match".to_owned(), "\"slice-v1\"".to_owned()));
        let mut not_modified = serve_hit_conditional(lease, &conditional_head, false);
        nucleus_add_cors(&mut not_modified, ovcc_nucleus::Location::Lft);
        assert_eq!(not_modified.head.status, 304);
        assert_eq!(not_modified.bytes_sent, 0);
        assert!(not_modified.head.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("access-control-allow-origin") && value == "*"
        }));
        Ok(())
    }

    #[tokio::test]
    async fn nucleus_get_streaming_generic_memory_is_bounded_by_chunk_size()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        const CHUNK_BYTES: usize = 1024 * 1024;
        const CHUNKS: usize = 2 * 1024;
        let total_bytes = (CHUNK_BYTES * CHUNKS) as u64;
        let written = Arc::new(AtomicU64::new(0));
        let cache = Cache::new(DiscardStorage {
            written: Arc::clone(&written),
        });
        let key = CacheKey::from_raw("nucleus|generic|large".to_owned());
        let head = get_head("nucleus.example", "/large.usd");
        let permit = match cache.acquire_fill(key.as_str()).await? {
            FillDecision::Owner(permit) => Some(permit),
            _ => {
                return Err(Error::internal("cold generic key did not elect a fill owner").into());
            }
        };
        let live_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak_chunks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let body = TrackedBody {
            remaining_chunks: CHUNKS,
            live_chunks: Arc::clone(&live_chunks),
            peak_chunks: Arc::clone(&peak_chunks),
        };
        let mut fill = miss_fill_plain();
        fill.honor_origin_cacheability = true;
        let mut sink = PrefixSink::new(4096);

        // Act.
        let outcome = fill_from_open_response(
            &mut sink,
            &cache,
            &key,
            &head,
            fill,
            permit,
            200,
            None,
            hdr(&[("Content-Length", &total_bytes.to_string())]),
            None,
            body,
            FillSource::Origin,
        )
        .await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin generic fill requested peer fallback").into());
        };
        assert_eq!(response.bytes_sent, total_bytes);
        assert_eq!(written.load(Ordering::SeqCst), total_bytes);
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::StorageError)
        );
        assert!(
            peak_chunks.load(Ordering::SeqCst) <= 2,
            "a 2 GiB generic Nucleus response retained more than two 1 MiB chunks"
        );
        assert_eq!(live_chunks.load(Ordering::SeqCst), 0);
        assert!(
            !String::from_utf8_lossy(&sink.prefix)
                .to_ascii_lowercase()
                .contains("access-control-allow-origin")
        );
        Ok(())
    }

    #[tokio::test]
    async fn nucleus_get_streaming_uncacheable_response_is_not_admitted()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = CacheKey::from_raw("nucleus|generic|no-store".to_owned());
        let head = get_head("nucleus.example", "/private.usd");
        let permit = match cache.acquire_fill(key.as_str()).await? {
            FillDecision::Owner(permit) => Some(permit),
            _ => {
                return Err(Error::internal("cold no-store key did not elect a fill owner").into());
            }
        };
        let mut fill = miss_fill_plain();
        fill.honor_origin_cacheability = true;
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Act.
        let outcome = fill_from_open_response(
            &mut server,
            &cache,
            &key,
            &head,
            fill,
            permit,
            200,
            None,
            hdr(&[
                ("Content-Length", "4"),
                ("Cache-Control", "public, no-store"),
            ]),
            None,
            http_body_util::Full::new(Bytes::from_static(b"body")),
            FillSource::Origin,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin no-store fill requested peer fallback").into());
        };
        assert_eq!(response.bytes_sent, 4);
        assert_eq!(split_wire(&wire).1, b"body");
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn origin_header_timeout_returns_gateway_timeout() -> std::result::Result<(), BoxError> {
        // Arrange.
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let origin_address = listener.local_addr()?;
        let origin = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await?;
            std::future::pending::<()>().await;
            Ok::<(), std::io::Error>(())
        });
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/header-timeout");
        let head = get_head("origin.example", "/header-timeout");
        let permit = owner_permit(&cache, &key).await;

        // Act.
        let (response, wire) = drive_streamed(|mut stream| async move {
            let response = serve_miss_streaming(
                &mut stream,
                &cache,
                &key,
                &format!("http://{origin_address}"),
                &head,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("header timeout returns a gateway response");
            assert!(cache.get(key.as_str()).unwrap().is_none());
            assert!(matches!(
                cache.acquire_fill(key.as_str()).await.unwrap(),
                FillDecision::Owner(_)
            ));
            (response, stream)
        })
        .await;
        origin.abort();

        // Assert.
        assert_eq!(response.head.status, 504);
        assert_eq!(
            response.timeout_stage,
            Some(TimeoutStage::OriginResponseHeader)
        );
        assert_eq!(split_wire(&wire).1, GATEWAY_TIMEOUT_BODY);
        Ok(())
    }

    #[tokio::test]
    async fn origin_body_stall_timeout_never_commits() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/body-timeout");
        let head = get_head("origin.example", "/body-timeout");
        let permit = owner_permit(&cache, &key).await;
        let mut stream = tokio::io::sink();
        let body = OriginBodyTimeoutAfterChunk {
            chunk: Some(Bytes::from_static(b"partial")),
            timed_out: false,
        };

        // Act.
        let outcome = fill_from_open_response(
            &mut stream,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            permit,
            200,
            Some("\"v1\"".to_owned()),
            hdr(&[("Content-Length", &(MISS_BUFFER_MAX_BYTES + 1).to_string())]),
            None,
            body,
            FillSource::Origin,
        )
        .await?;
        let next = cache.acquire_fill(key.as_str()).await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin body timeout requested peer fallback").into());
        };
        assert_eq!(
            response.timeout_stage,
            Some(TimeoutStage::OriginResponseBody)
        );
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::Incomplete)
        );
        assert!(cache.get(key.as_str())?.is_none());
        assert!(matches!(next, FillDecision::Owner(_)));
        Ok(())
    }

    #[tokio::test]
    async fn origin_body_timeout_after_status_preserves_upstream_status()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/partial-body-timeout");
        let head = get_head("origin.example", "/partial-body-timeout");
        let permit = owner_permit(&cache, &key).await;
        let body = OriginBodyTimeoutAfterChunk {
            chunk: Some(Bytes::from_static(b"partial")),
            timed_out: false,
        };
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Act.
        let outcome = fill_from_open_response(
            &mut server,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            permit,
            206,
            Some("\"v1\"".to_owned()),
            hdr(&[("Content-Length", "7"), ("Content-Range", "bytes 0-6/7")]),
            None,
            body,
            FillSource::Origin,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin body timeout requested peer fallback").into());
        };
        let (wire_head, wire_body) = split_wire(&wire);

        // Assert.
        assert_eq!(response.head.status, 206);
        assert_eq!(
            response.timeout_stage,
            Some(TimeoutStage::OriginResponseBody)
        );
        assert!(wire_head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(wire_body.len() < 7, "timed-out body must remain incomplete");
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn buffered_origin_body_failure_without_length_closes_abortively()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let failing_body = || OriginBodyTimeoutAfterChunk {
            chunk: Some(Bytes::from_static(b"partial")),
            timed_out: false,
        };
        let (mut unframed_client, mut unframed_server) = tokio::io::duplex(4096);
        let (mut framed_client, mut framed_server) = tokio::io::duplex(4096);

        // Act.
        let unframed = collect_buffered_origin_body(
            &mut unframed_server,
            &mut None,
            CacheStatus::Miss,
            200,
            &hdr(&[("Transfer-Encoding", "chunked")]),
            UpstreamTiming::default(),
            failing_body(),
        )
        .await?;
        let framed = collect_buffered_origin_body(
            &mut framed_server,
            &mut None,
            CacheStatus::Miss,
            200,
            &hdr(&[("Content-Length", "7")]),
            UpstreamTiming::default(),
            failing_body(),
        )
        .await?;
        drop((unframed_server, framed_server));
        let mut unframed_wire = Vec::new();
        unframed_client.read_to_end(&mut unframed_wire).await?;
        let mut framed_wire = Vec::new();
        framed_client.read_to_end(&mut framed_wire).await?;

        // Assert. The unframed head ends its empty body at the close, which a
        // reset keeps from reading as complete. The declared length already
        // shows the framed body short, so it closes gracefully.
        let (BufferedOriginBody::Failed(unframed), BufferedOriginBody::Failed(framed)) =
            (unframed, framed)
        else {
            return Err(Error::internal("a failed buffered body completed").into());
        };
        let (unframed_head, unframed_body) = split_wire(&unframed_wire);
        let unframed_head = unframed_head.to_ascii_lowercase();
        assert!(!unframed_head.contains("content-length"), "{unframed_head}");
        assert!(
            !unframed_head.contains("transfer-encoding"),
            "{unframed_head}"
        );
        assert!(unframed_body.is_empty());
        assert_eq!(
            connection_next(false, true, &unframed),
            ConnectionNext::Close(CloseMode::Reset)
        );
        assert!(split_wire(&framed_wire).0.contains("Content-Length: 7"));
        assert_eq!(
            connection_next(false, true, &framed),
            ConnectionNext::Close(CloseMode::Graceful)
        );
        Ok(())
    }

    #[tokio::test]
    async fn buffered_origin_body_timeout_preserves_upstream_status()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/buffered-body-timeout");
        let mut permit = owner_permit(&cache, &key).await;
        let body = OriginBodyTimeoutAfterChunk {
            chunk: Some(Bytes::from_static(b"partial")),
            timed_out: false,
        };
        let (mut client, mut server) = tokio::io::duplex(4096);

        // Act.
        let outcome = collect_buffered_origin_body(
            &mut server,
            &mut permit,
            CacheStatus::Miss,
            206,
            &hdr(&[("Content-Length", "7"), ("Content-Range", "bytes 0-6/7")]),
            UpstreamTiming::default(),
            body,
        )
        .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;
        let BufferedOriginBody::Failed(response) = outcome else {
            return Err(Error::internal("timed-out buffered body completed").into());
        };
        let (wire_head, wire_body) = split_wire(&wire);
        let next = cache.acquire_fill(key.as_str()).await?;

        // Assert.
        assert_eq!(response.head.status, 206);
        assert_eq!(
            response.timeout_stage,
            Some(TimeoutStage::OriginResponseBody)
        );
        assert!(wire_head.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(wire_body.len() < 7, "timed-out body must remain incomplete");
        assert!(cache.get(key.as_str())?.is_none());
        assert!(matches!(next, FillDecision::Owner(_)));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn client_write_timeout_retains_streaming_fill() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/retained-fill");
        let head = get_head("origin.example", "/retained-fill");
        let permit = owner_permit(&cache, &key).await;
        let fill = miss_fill_plain();
        let mut blocked = BlockedWriter;

        // Act.
        let outcome = fill_from_open_response(
            &mut blocked,
            &cache,
            &key,
            &head,
            fill,
            permit,
            200,
            Some("\"v1\"".to_owned()),
            Vec::new(),
            None,
            http_body_util::Full::new(Bytes::from_static(b"complete body")),
            FillSource::Origin,
        )
        .await?;
        let stored = cache.get(key.as_str())?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("retained fill requested peer fallback").into());
        };
        assert_eq!(response.bytes_sent, 0);
        assert_eq!(response.cache_store_result, Some(CacheStoreResult::Stored));
        assert_eq!(stored.map(|(lease, _)| lease.meta().size), Some(13));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn client_write_timeout_aborts_unretained_fill() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/abortable-fill");
        let head = get_head("origin.example", "/abortable-fill");
        let permit = owner_permit(&cache, &key).await;
        let mut blocked = BlockedWriter;

        // Act.
        let result = fill_from_open_response(
            &mut blocked,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            permit,
            404,
            Some("\"v1\"".to_owned()),
            Vec::new(),
            None,
            http_body_util::Full::new(Bytes::from_static(b"unread body")),
            FillSource::Origin,
        )
        .await;
        let next = cache.acquire_fill(key.as_str()).await?;

        // Assert.
        let error = match result {
            Err(error) => error,
            Ok(_) => return Err(Error::internal("blocked abortable fill did not stop").into()),
        };
        assert!(is_downstream_timeout(&error));
        assert!(cache.get(key.as_str())?.is_none());
        assert!(matches!(next, FillDecision::Owner(_)));
        Ok(())
    }

    #[tokio::test]
    async fn nucleus_get_streaming_client_abort_still_commits_complete_fill()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = Bytes::from(vec![b'x'; MISS_BUFFER_MAX_BYTES as usize + 1]);
        let (_dir, cache) = fresh_cache();
        let key = CacheKey::from_raw("nucleus|generic|client-abort".to_owned());
        let head = get_head("nucleus.example", "/large-after-abort.usd");
        let permit = match cache.acquire_fill(key.as_str()).await? {
            FillDecision::Owner(permit) => Some(permit),
            _ => {
                return Err(
                    Error::internal("cold client-abort key did not elect a fill owner").into(),
                );
            }
        };
        let mut fill = miss_fill_plain();
        fill.honor_origin_cacheability = true;
        let mut disconnected = DisconnectAfter { remaining: 0 };

        // Act.
        let outcome = fill_from_open_response(
            &mut disconnected,
            &cache,
            &key,
            &head,
            fill,
            permit,
            200,
            None,
            hdr(&[("Content-Length", &body.len().to_string())]),
            None,
            http_body_util::Full::new(body.clone()),
            FillSource::Origin,
        )
        .await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin client-abort fill requested peer fallback").into());
        };
        assert_eq!(response.bytes_sent, 0);
        assert_eq!(response.cache_store_result, Some(CacheStoreResult::Stored));
        let (lease, _) = cache
            .get(key.as_str())?
            .ok_or_else(|| Error::internal("complete body was not committed after client abort"))?;
        assert_eq!(lease.meta().size, body.len() as u64);
        Ok(())
    }

    #[test]
    fn stored_partial_content_hit_does_not_invent_accept_ranges() {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = "range-without-accept-ranges";
        let mut writer = cache
            .begin_fill(
                key,
                StoredMeta {
                    size: 4,
                    status: 206,
                    content_range: Some("bytes 0-3/10".to_owned()),
                    headers: vec![("Content-Range".to_owned(), "bytes 0-3/10".to_owned())],
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"part").unwrap();
        writer.commit().unwrap();
        let lease = cache.get(key).unwrap().expect("stored range must HIT").0;

        // Act.
        let response = serve_hit(lease);

        // Assert.
        assert_eq!(response.head.status, 206);
        assert!(
            response
                .head
                .headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("accept-ranges"))
        );
        assert_eq!(
            response.head.headers,
            vec![("Content-Range".to_owned(), "bytes 0-3/10".to_owned())]
        );
    }

    #[test]
    fn stored_complete_content_hit_advertises_byte_ranges_once() {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = "complete-without-accept-ranges";
        let mut writer = cache
            .begin_fill(
                key,
                StoredMeta {
                    size: 4,
                    status: 200,
                    headers: vec![("ETag".to_owned(), "\"v1\"".to_owned())],
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();
        let lease = cache.get(key).unwrap().expect("stored object must HIT").0;

        // Act.
        let response = serve_hit(lease);

        // Assert.
        assert_eq!(response.head.status, 200);
        assert_eq!(
            response
                .head
                .headers
                .iter()
                .filter(|(name, value)| {
                    name.eq_ignore_ascii_case("accept-ranges") && value == "bytes"
                })
                .count(),
            1
        );
    }

    async fn owner_permit<S>(cache: &Cache<S>, key: &ovcc_cachekey::CacheKey) -> Option<FillPermit>
    where
        S: Storage,
    {
        match cache.acquire_fill(key.as_str()).await.unwrap() {
            FillDecision::Owner(permit) => Some(permit),
            _ => panic!("cold test key must elect a fill owner"),
        }
    }

    fn peer_meta(size: u64) -> StoredMeta {
        StoredMeta {
            logical_key: None,
            size,
            revision: 0,
            provenance: CacheProvenance::PeerCopy,
            max_idle_secs: None,
            status: 200,
            etag: Some("\"peer-v1\"".to_owned()),
            last_modified: Some("Sat, 15 Aug 2026 00:00:00 GMT".to_owned()),
            content_type: None,
            headers: vec![
                ("ETag".to_owned(), "\"peer-v1\"".to_owned()),
                (
                    "Last-Modified".to_owned(),
                    "Sat, 15 Aug 2026 00:00:00 GMT".to_owned(),
                ),
            ],
            representation_length: Some(size),
            expires_at: None,
            versionable: false,
            content_range: None,
        }
    }

    async fn plain_stream_pair() -> (ClientStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let client = client.unwrap();
        let (server, _) = accepted.unwrap();
        (ClientStream::Plain(server), client)
    }

    fn limit_socket_send_buffer(stream: &ClientStream) {
        socket2::SockRef::from(stream.tcp().expect("test TCP stream"))
            .set_send_buffer_size(4096)
            .expect("set a bounded test send buffer");
    }

    /// Winsock accepts a whole non-blocking send while fewer than `SO_SNDBUF`
    /// bytes are queued, however large the send. Filling the queue first makes
    /// the next write wait on the stalled reader, as it would on Linux.
    #[cfg(windows)]
    const SATURATE_FILLER: u8 = 0xa5;

    #[cfg(windows)]
    fn saturate_send_queue(stream: &ClientStream) {
        // Raw sends: tokio's try_write reports WouldBlock until readiness has
        // been polled once, which would leave the queue empty.
        let socket = socket2::SockRef::from(stream.tcp().expect("test TCP stream"));
        let filler = vec![SATURATE_FILLER; 64 * 1024];
        while socket.send(&filler).is_ok() {}
    }

    async fn start_follower<F>(
        follower: F,
    ) -> (
        tokio::task::JoinHandle<std::result::Result<FollowerFillOutcome, BoxError>>,
        TcpStream,
    )
    where
        F: Into<ServingFollower> + Send + 'static,
    {
        let (mut server, client) = plain_stream_pair().await;
        let task = tokio::spawn(async move {
            let result = serve_in_progress_fill(&mut server, follower, false).await;
            drop(server);
            result
        });
        (task, client)
    }

    async fn read_through_body_prefix<S>(stream: &mut S, body_bytes: usize) -> Vec<u8>
    where
        S: AsyncRead + Unpin,
    {
        let mut wire = Vec::new();
        loop {
            if let Some(separator) = find_subslice(&wire, b"\r\n\r\n")
                && wire.len() >= separator + 4 + body_bytes
            {
                return wire;
            }
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "response closed before the requested body prefix");
            wire.extend_from_slice(&chunk[..read]);
        }
    }

    struct FaultStorage {
        path: PathBuf,
        fail_begin: bool,
        fail_after_writes: Option<usize>,
        fail_commit: bool,
    }

    impl Storage for FaultStorage {
        fn lookup(&self, _key: &str) -> Result<ovcc_storage::Lookup> {
            Ok(ovcc_storage::Lookup::Miss)
        }

        fn peek(&self, _key: &str) -> Result<Option<StoredMeta>> {
            Ok(None)
        }

        fn begin_fill(&self, _key: &str, _meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
            if self.fail_begin {
                return Err(Error::storage("injected staging begin failure"));
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .open(&self.path)?;
            Ok(Box::new(FaultWriter {
                file: Some(file),
                path: self.path.clone(),
                writes: 0,
                fail_after_writes: self.fail_after_writes,
                fail_commit: self.fail_commit,
            }))
        }

        fn evict_data(&self, _key: &str, _revision: u64) -> Result<bool> {
            Ok(false)
        }

        fn remove(&self, _key: &str) -> Result<()> {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        }

        fn recover_on_open(&self) -> Result<ovcc_storage::RecoveryReport> {
            Ok(ovcc_storage::RecoveryReport::default())
        }
    }

    struct FaultWriter {
        file: Option<File>,
        path: PathBuf,
        writes: usize,
        fail_after_writes: Option<usize>,
        fail_commit: bool,
    }

    impl StoreWrite for FaultWriter {
        fn write_chunk(&mut self, buf: &[u8]) -> Result<()> {
            if self
                .fail_after_writes
                .is_some_and(|limit| self.writes >= limit)
            {
                return Err(Error::storage("injected staging write failure"));
            }
            self.file
                .as_mut()
                .ok_or_else(|| Error::storage("write after completion"))?
                .write_all(buf)?;
            self.writes += 1;
            Ok(())
        }

        fn in_progress_read(&self) -> Result<ovcc_storage::InProgressRead> {
            Ok(ovcc_storage::InProgressRead {
                file: Arc::new(File::open(&self.path)?),
                body_offset: 0,
            })
        }

        fn commit(mut self: Box<Self>) -> Result<Lease> {
            self.file.take();
            if self.fail_commit {
                Err(Error::storage("injected metadata commit failure"))
            } else {
                Err(Error::internal(
                    "successful FaultWriter commit is not used by these tests",
                ))
            }
        }

        fn abort(mut self: Box<Self>) {
            self.file.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct FailingStoreWrite {
        fail_write: bool,
        aborted: Arc<std::sync::atomic::AtomicBool>,
    }

    impl StoreWrite for FailingStoreWrite {
        fn write_chunk(&mut self, _buf: &[u8]) -> Result<()> {
            if self.fail_write {
                return Err(Error::storage("injected cache write failure"));
            }
            Ok(())
        }

        fn commit(self: Box<Self>) -> Result<Lease> {
            Err(Error::storage("injected cache commit failure"))
        }

        fn abort(self: Box<Self>) {
            self.aborted.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn buffered_cache_io_failure_is_non_fatal() {
        // Arrange.
        let write_aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let write_start = CacheFillStart::Writer(Box::new(FailingStoreWrite {
            fail_write: true,
            aborted: Arc::clone(&write_aborted),
        }));

        // Act.
        let write_result = store_complete_body(write_start, b"origin body");

        // Assert.
        assert_eq!(write_result, CacheStoreResult::StorageError);
        assert!(write_aborted.load(Ordering::SeqCst));

        // Arrange.
        let commit_aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commit_start = CacheFillStart::Writer(Box::new(FailingStoreWrite {
            fail_write: false,
            aborted: Arc::clone(&commit_aborted),
        }));

        // Act.
        let commit_result = store_complete_body(commit_start, b"origin body");

        // Assert.
        assert_eq!(commit_result, CacheStoreResult::StorageError);
        assert!(
            !commit_aborted.load(Ordering::SeqCst),
            "commit consumes the writer and owns its cleanup"
        );
    }

    #[tokio::test]
    async fn general_get_buffered_miss_relays_after_second_chunk_store_failure()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("partial-staging");
        let cache = Cache::new(FaultStorage {
            path: path.clone(),
            fail_begin: false,
            fail_after_writes: Some(1),
            fail_commit: false,
        });
        let key = plain_key("origin.example", "/two-chunks");
        let head = get_head("origin.example", "/two-chunks");
        let frames = [
            Ok::<_, std::convert::Infallible>(hyper::body::Frame::data(Bytes::from_static(
                b"first",
            ))),
            Ok(hyper::body::Frame::data(Bytes::from_static(b"second"))),
        ];
        let mut wire = Vec::new();

        // Act.
        let outcome = fill_from_open_response(
            &mut wire,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            owner_permit(&cache, &key).await,
            200,
            None,
            hdr(&[("Content-Length", "11")]),
            None,
            http_body_util::StreamBody::new(tokio_stream::iter(frames)),
            FillSource::Origin,
        )
        .await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("origin response was not served").into());
        };
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::StorageError)
        );
        assert_eq!(split_wire(&wire).1, b"firstsecond");
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn begin_cache_fill_reports_lock_bypass() -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/lock-bypass");

        // Act.
        let result = begin_cache_fill(&cache, &key, StoredMeta::default(), true, None, false)?;

        // Assert.
        assert!(matches!(
            result,
            CacheFillStart::Skipped(CacheStoreResult::LockBypass)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn begin_cache_fill_retains_storage_failure() -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let storage = FaultStorage {
            path: dir.path().join("unused-staging"),
            fail_begin: true,
            fail_after_writes: None,
            fail_commit: false,
        };
        let cache = Cache::new(storage);
        let key = plain_key("origin.example", "/staging-begin-failure");
        let permit = owner_permit(&cache, &key).await;

        // Act.
        let result = begin_cache_fill(&cache, &key, peer_meta(8), true, permit, true)?;

        // Assert.
        let CacheFillStart::StorageFailure(error) = result else {
            return Err(Error::internal("storage failure lost its original error").into());
        };
        assert_eq!(error.to_string(), "Storage: injected staging begin failure");
        Ok(())
    }

    #[tokio::test]
    async fn begin_cache_fill_ignores_eviction_pressure() -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                max_bytes: Some(0),
                ..EvictionPolicy::default()
            },
        );
        let key = plain_key("origin.example", "/optimistic-admission");
        let permit = owner_permit(&cache, &key).await;
        let meta = StoredMeta {
            size: 1,
            ..StoredMeta::default()
        };

        // Act.
        let result = begin_cache_fill(&cache, &key, meta, true, permit, false)?;

        // Assert.
        let CacheFillStart::Writer(writer) = result else {
            return Err(Error::internal("eviction pressure rejected a cache writer").into());
        };
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn in_progress_join_requires_exact_preflight_metadata_and_key()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/joinable");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(5), permit)?;
        let exact = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };
        let wrong_validator = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v2\"".to_owned()),
        };
        let wrong_size = ovcc_upstream::PreflightMetadata {
            total_size: 6,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };
        let other_range_key = KeyParts {
            generation: "0",
            host: "origin.example",
            method: "GET",
            uri: "/joinable",
            filtered_args: "",
            if_match: "",
            range: "bytes=0-1",
        }
        .render();

        // Act / assert.
        assert!(matching_in_progress_fill(&cache, &key, &exact).is_some());
        assert!(matching_in_progress_fill(&cache, &key, &wrong_validator).is_none());
        assert!(matching_in_progress_fill(&cache, &key, &wrong_size).is_none());
        assert!(
            matching_in_progress_fill(&cache, &other_range_key, &exact).is_none(),
            "canonical Range identity must select a distinct registry key"
        );
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn in_progress_range_join_checks_total_length_and_response_headers()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = KeyParts {
            generation: "0",
            host: "origin.example",
            method: "GET",
            uri: "/range",
            filtered_args: "",
            if_match: "",
            range: "bytes=1-3",
        }
        .render();
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(3);
        meta.status = 206;
        meta.content_range = Some("bytes 1-3/5".to_owned());
        meta.headers
            .push(("Content-Range".to_owned(), "bytes 1-3/5".to_owned()));
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        let exact = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };
        let changed_object = ovcc_upstream::PreflightMetadata {
            total_size: 6,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };

        // Act / assert.
        assert!(matching_in_progress_fill(&cache, &key, &exact).is_some());
        assert!(matching_in_progress_fill(&cache, &key, &changed_object).is_none());
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn fill_lock_timeout_joins_fill_registered_while_acquire_is_blocked()
    -> std::result::Result<(), BoxError> {
        // Arrange an ordinary owner lock with no registry entry, exactly as it
        // exists between owner election and shareable-fill registration.
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = plain_key("origin.example", "/register-during-fill-lock");
        let permit = owner_permit(cache.as_ref(), &key)
            .await
            .expect("cold key elects owner");
        let preflight = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };
        let (blocked_tx, blocked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *FILL_LOCK_WAIT_TEST_HOOK
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(FillLockWaitTestHook {
            key: key.as_str().to_owned(),
            blocked: blocked_tx,
            release: release_rx,
        });
        let waiter_cache = Arc::clone(&cache);
        let waiter_key = key.clone();
        let waiter = tokio::spawn(async move {
            acquire_replicated_fill(
                waiter_cache.as_ref(),
                &waiter_key,
                &preflight,
                tokio::time::Instant::now(),
            )
            .await
        });
        blocked_rx
            .await
            .expect("waiter must poll acquire_fill to Pending");

        // Act: publish the registry entry while acquire_fill is already
        // blocked, then let its exhausted outer budget take the final check.
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(5), permit)?;
        release_tx
            .send(())
            .expect("blocked waiter must still be active");
        let decision = waiter.await??;

        // Assert: this is a follower, not a bypass/new owner that could open a
        // duplicate peer or origin object body.
        assert!(matches!(decision, ReplicatedFillDecision::Follower(_)));
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn fill_lock_bypass_rechecks_exact_registered_fill() -> std::result::Result<(), BoxError>
    {
        // Arrange a shareable owner whose zero age budget forces acquire_fill
        // itself to return Bypass while the owner lock remains held.
        let dir = tempfile::tempdir()?;
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                lock_age_secs: 0,
                lock_timeout_secs: 120,
                ..EvictionPolicy::default()
            },
        );
        let key = plain_key("origin.example", "/bypass-final-join");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(5), permit)?;
        let preflight = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };

        // Act.
        let decision = acquire_replicated_fill(
            &cache,
            &key,
            &preflight,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await?;

        // Assert.
        assert!(matches!(decision, ReplicatedFillDecision::Follower(_)));
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn follower_waits_for_first_published_byte_before_writing_head()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"hello follower";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/early-follower");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("shareable fill is visible");
        let (task, mut client) = start_follower(follower).await;
        tokio::task::yield_now().await;
        let mut before_publish = [0_u8; 1];

        // Assert the pre-byte fallback boundary before allowing publication.
        assert_eq!(
            client.try_read(&mut before_publish).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the follower exposed a response head before any staging byte"
        );

        // Act.
        writer.write_chunk(body)?;
        writer.commit()?;
        let outcome = task.await??;
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let FollowerFillOutcome::Served(response, terminal) = outcome else {
            return Err(Error::internal("complete follower requested normal fallback").into());
        };
        assert_eq!(terminal, PeerFillFollowerResult::Complete);
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::NotStored)
        );
        assert_eq!(response.bytes_sent, body.len() as u64);
        let (head, actual_body) = split_wire(&wire);
        assert!(head.contains("X-Cache-Status: MISS"));
        assert!(head.contains("X-Cache-Tier-Hit: peer"));
        assert_eq!(actual_body, body);
        Ok(())
    }

    #[tokio::test]
    async fn origin_owner_streams_to_authorized_follower_before_completion()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = Arc::new(plain_key("origin.example", "/origin-tail"));
        let head = Arc::new(get_head("origin.example", "/origin-tail"));
        let size = MISS_BUFFER_MAX_BYTES + 1;
        let permit = owner_permit(cache.as_ref(), key.as_ref()).await;
        let mut registration = cache.subscribe_fill_registration(key.as_str());
        let (body_tx, body_rx) =
            mpsc::channel::<std::result::Result<hyper::body::Frame<Bytes>, std::io::Error>>(2);
        let body = http_body_util::StreamBody::new(ReceiverStream::new(body_rx));
        let owner_cache = Arc::clone(&cache);
        let owner_key = Arc::clone(&key);
        let owner_head = Arc::clone(&head);
        let owner = tokio::spawn(async move {
            fill_from_open_response(
                &mut tokio::io::sink(),
                owner_cache.as_ref(),
                owner_key.as_ref(),
                owner_head.as_ref(),
                miss_fill_plain(),
                permit,
                200,
                Some("\"origin-v1\"".to_owned()),
                vec![
                    ("ETag".to_owned(), "\"origin-v1\"".to_owned()),
                    ("Content-Length".to_owned(), size.to_string()),
                ],
                None,
                body,
                FillSource::Origin,
            )
            .await
        });
        registration.changed().await?;
        let origin = spawn_raw_origin(
            format!(
                "HTTP/1.1 200 OK\r\nETag: \"origin-v1\"\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n"
            )
            .into_bytes(),
        )
        .await;
        let decision = acquire_origin_fill(
            cache.as_ref(),
            key.as_ref(),
            head.as_ref(),
            &format!("http://{origin}"),
            &format!("http://{origin}"),
            false,
        )
        .await?;
        let ReplicatedFillDecision::Follower(follower) = decision else {
            return Err(Error::internal("authorized waiter did not join origin fill").into());
        };
        let (follower_task, mut client) = start_follower(follower).await;
        body_tx
            .send(Ok(hyper::body::Frame::data(Bytes::from_static(b"first-"))))
            .await?;
        let mut wire = tokio::time::timeout(
            Duration::from_secs(5),
            read_through_body_prefix(&mut client, 6),
        )
        .await?;
        assert!(
            !owner.is_finished(),
            "follower must receive bytes during the fill"
        );
        let (response_head, prefix) = split_wire(&wire);
        assert!(response_head.contains("X-Cache-Tier-Hit: origin"));
        assert_eq!(prefix, b"first-");

        body_tx
            .send(Ok(hyper::body::Frame::data(Bytes::from(vec![
                b'x';
                size as usize
                    - 6
            ]))))
            .await?;
        drop(body_tx);
        client.read_to_end(&mut wire).await?;
        let FollowerFillOutcome::Served(response, PeerFillFollowerResult::Complete) =
            follower_task.await??
        else {
            return Err(Error::internal("origin follower did not finish").into());
        };
        assert_eq!(response.bytes_sent, size);
        assert_eq!(split_wire(&wire).1.len() as u64, size);
        assert!(matches!(owner.await??, FillOutcome::Served(_)));
        assert!(cache.get(key.as_str())?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn origin_waiter_does_not_follow_denied_or_changed_preflight()
    -> std::result::Result<(), BoxError> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                lock_age_secs: 0,
                ..EvictionPolicy::default()
            },
        );
        let key = plain_key("origin.example", "/protected-origin");
        let head = get_head("origin.example", "/protected-origin");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        for response in [
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nETag: \"changed\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                .to_vec(),
        ] {
            let origin = spawn_raw_origin(response).await;
            let decision = acquire_origin_fill(
                &cache,
                &key,
                &head,
                &format!("http://{origin}"),
                &format!("http://{origin}"),
                false,
            )
            .await?;
            assert!(matches!(decision, ReplicatedFillDecision::Bypass));
        }
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn origin_waiter_uses_separate_authorization_and_content_preflights()
    -> std::result::Result<(), BoxError> {
        let dir = tempfile::tempdir()?;
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path())?,
            EvictionPolicy {
                lock_age_secs: 0,
                ..EvictionPolicy::default()
            },
        );
        let key = plain_key("origin.example", "/separate-auth");
        let head = get_head("origin.example", "/separate-auth");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        let auth = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let content = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nETag: \"peer-v1\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                .to_vec(),
        )
        .await;
        let decision = acquire_origin_fill(
            &cache,
            &key,
            &head,
            &format!("http://{content}"),
            &format!("http://{auth}"),
            false,
        )
        .await?;
        assert!(matches!(decision, ReplicatedFillDecision::Follower(_)));
        let denied = spawn_raw_origin(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
        )
        .await;
        let content_probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let content = spawn_counted_raw_origin(
            b"HTTP/1.1 200 OK\r\nETag: \"peer-v1\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                .to_vec(),
            Arc::clone(&content_probes),
        )
        .await;
        let decision = acquire_origin_fill(
            &cache,
            &key,
            &head,
            &format!("http://{content}"),
            &format!("http://{denied}"),
            false,
        )
        .await?;
        assert!(matches!(decision, ReplicatedFillDecision::Bypass));
        assert_eq!(content_probes.load(Ordering::SeqCst), 0);
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn origin_waiter_shares_only_a_committed_preflight_match()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/origin-committed-match");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let mut writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        writer.write_chunk(b"hello")?;
        writer.commit()?;
        let exact = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"peer-v1\"".to_owned()),
        };
        let changed = ovcc_upstream::PreflightMetadata {
            total_size: 5,
            validator: ovcc_upstream::PreflightValidator::Etag("\"changed\"".to_owned()),
        };

        for preflight in [None, Some(&changed)] {
            let lease = cache.get(key.as_str())?.expect("committed body").0;
            assert!(matches!(
                origin_decision_after_preflight(
                    &cache,
                    &key,
                    preflight,
                    FillDecision::Shared(lease),
                ),
                ReplicatedFillDecision::Bypass
            ));
        }
        let lease = cache.get(key.as_str())?.expect("committed body").0;
        assert!(matches!(
            origin_decision_after_preflight(
                &cache,
                &key,
                Some(&exact),
                FillDecision::Shared(lease),
            ),
            ReplicatedFillDecision::Shared(_)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn serve_miss_uses_origin_follower_after_independent_preflight()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = Arc::new(plain_key("origin.example", "/serve-origin-follower"));
        let head = Arc::new(get_head("origin.example", "/serve-origin-follower"));
        let permit = owner_permit(cache.as_ref(), key.as_ref())
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let mut writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        let origin = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nETag: \"peer-v1\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                .to_vec(),
        )
        .await;
        let upstream = format!("http://{origin}");
        let (mut server, mut client) = plain_stream_pair().await;
        let waiter_cache = Arc::clone(&cache);
        let waiter_key = Arc::clone(&key);
        let waiter_head = Arc::clone(&head);
        let waiter = tokio::spawn(async move {
            let fill = MissFill {
                recheck_upstream: &upstream,
                add_lft_cors: true,
                ..miss_fill_plain()
            };
            let response = serve_miss(
                &mut server,
                waiter_cache.as_ref(),
                waiter_key.as_ref(),
                &upstream,
                waiter_head.as_ref(),
                fill,
            )
            .await;
            drop(server);
            response
        });
        writer.write_chunk(b"hello")?;
        let wire = tokio::time::timeout(
            Duration::from_secs(5),
            read_through_body_prefix(&mut client, 5),
        )
        .await?;
        assert_eq!(split_wire(&wire).1, b"hello");
        assert!(split_wire(&wire).0.contains("X-Cache-Tier-Hit: origin"));
        for (name, value) in ovcc_nucleus::cors_headers(ovcc_nucleus::Location::Lft, 200) {
            assert!(
                split_wire(&wire)
                    .0
                    .to_ascii_lowercase()
                    .contains(&format!("{name}: {value}").to_ascii_lowercase())
            );
        }
        assert!(!waiter.is_finished());
        writer.commit()?;
        let response = waiter.await??;
        assert_eq!(response.bytes_sent, 5);
        assert_eq!(response.cache_tier, Some("origin"));
        Ok(())
    }

    #[tokio::test]
    async fn nucleus_lft_waiter_follows_fill_without_origin_preflight()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let mut head = get_head("nucleus.example", "/omni/lft");
        head.headers = vec![
            ("nvcf-nca-id".to_owned(), "tenant-a".to_owned()),
            ("omniverse-content-uid".to_owned(), "uid-1".to_owned()),
            ("omni-tenant-token".to_owned(), "tenant-jwt".to_owned()),
        ];
        let head = Arc::new(head);
        let key = Arc::new(ovcc_nucleus::lft_cache_key("0", "GET", &head.headers));
        let permit = owner_permit(cache.as_ref(), key.as_ref())
            .await
            .expect("cold key elects owner");
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let mut writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        // LFT access is isolated by the tenant cache key, so a waiter never
        // needs the bridge. Rejecting every method exposes any origin contact.
        let origin_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(
            b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            Arc::clone(&origin_requests),
        )
        .await;
        let upstream = format!("http://{origin}");
        let (mut server, _client) = plain_stream_pair().await;
        let waiter_cache = Arc::clone(&cache);
        let waiter_key = Arc::clone(&key);
        let waiter_head = Arc::clone(&head);
        let waiter = tokio::spawn(async move {
            let fill = nucleus_miss_fill(false, ovcc_nucleus::Location::Lft, None, &upstream);
            serve_miss(
                &mut server,
                waiter_cache.as_ref(),
                waiter_key.as_ref(),
                &upstream,
                waiter_head.as_ref(),
                fill,
            )
            .await
        });
        // Publishes bytes only after any follower preflight has reached the
        // origin, so a waiter that probes is queued before the fill completes.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while origin_requests.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        writer.write_chunk(b"hello")?;
        writer.commit()?;

        let response = tokio::time::timeout(Duration::from_secs(5), waiter).await???;
        assert_eq!(
            origin_requests.load(Ordering::SeqCst),
            0,
            "an LFT waiter must not contact the bridge"
        );
        assert_eq!(response.head.status, 200);
        assert_eq!(response.bytes_sent, 5);
        assert_eq!(response.cache_tier, Some("origin"));
        Ok(())
    }

    #[tokio::test]
    async fn origin_waiter_shares_buffered_fill_without_follower_preflight()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/buffered-no-preflight");
        let head = get_head("origin.example", "/buffered-no-preflight");
        let permit = owner_permit(&cache, &key).await.expect("cold owner");
        let mut writer = cache.begin_acquired_fill(key.as_str(), peer_meta(5), permit)?;
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_vec(),
            Arc::clone(&probes),
        )
        .await;
        let upstream = format!("http://{origin}");
        let waiter = acquire_origin_fill(&cache, &key, &head, &upstream, &upstream, false);
        tokio::pin!(waiter);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiter.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        writer.write_chunk(b"hello")?;
        writer.commit()?;
        let decision = tokio::time::timeout(Duration::from_secs(5), waiter).await??;
        assert_eq!(
            probes.load(Ordering::SeqCst),
            0,
            "buffered fills need no follower metadata probe"
        );
        assert!(matches!(decision, ReplicatedFillDecision::Shared(_)));
        Ok(())
    }

    #[tokio::test]
    async fn origin_waiter_joins_registration_after_fill_lock_contention()
    -> std::result::Result<(), BoxError> {
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = Arc::new(plain_key("origin.example", "/origin-registration"));
        let head = Arc::new(get_head("origin.example", "/origin-registration"));
        let permit = owner_permit(cache.as_ref(), key.as_ref())
            .await
            .expect("cold key elects owner");
        let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let origin = spawn_counted_raw_origin(
            b"HTTP/1.1 200 OK\r\nETag: \"peer-v1\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\n"
                .to_vec(),
            Arc::clone(&probes),
        )
        .await;
        let upstream = format!("http://{origin}");
        let waiter = acquire_origin_fill(
            cache.as_ref(),
            key.as_ref(),
            head.as_ref(),
            &upstream,
            &upstream,
            false,
        );
        tokio::pin!(waiter);
        assert!(
            std::future::poll_fn(|cx| Poll::Ready(waiter.as_mut().poll(cx)))
                .await
                .is_pending()
        );
        assert_eq!(probes.load(Ordering::SeqCst), 0);
        let mut meta = peer_meta(5);
        meta.provenance = CacheProvenance::Origin;
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), meta, permit)?;
        let decision = tokio::time::timeout(Duration::from_secs(5), waiter).await??;
        assert!(matches!(decision, ReplicatedFillDecision::Follower(_)));
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        writer.abort();
        Ok(())
    }

    #[tokio::test]
    async fn replicated_peer_owner_streams_to_three_late_independent_followers()
    -> std::result::Result<(), BoxError> {
        // Arrange a real production fill with a deterministic body-frame gate.
        let first = Bytes::from_static(b"first-");
        let second = Bytes::from_static(b"second");
        let expected = [first.as_ref(), second.as_ref()].concat();
        let (_dir, cache) = fresh_cache();
        let cache = Arc::new(cache);
        let key = Arc::new(plain_key("origin.example", "/coalesced"));
        let head = Arc::new(get_head("origin.example", "/coalesced"));
        let permit = owner_permit(cache.as_ref(), key.as_ref()).await;
        let headers = vec![
            ("ETag".to_owned(), "\"peer-v1\"".to_owned()),
            ("Content-Length".to_owned(), expected.len().to_string()),
        ];
        let (body_tx, body_rx) =
            mpsc::channel::<std::result::Result<hyper::body::Frame<Bytes>, std::io::Error>>(2);
        let body = http_body_util::StreamBody::new(ReceiverStream::new(body_rx));
        let (mut owner_rx, mut owner_tx) = tokio::io::duplex(4096);
        let owner_cache = Arc::clone(&cache);
        let owner_key = Arc::clone(&key);
        let owner_head = Arc::clone(&head);
        let owner = tokio::spawn(async move {
            let result = fill_from_open_response(
                &mut owner_tx,
                owner_cache.as_ref(),
                owner_key.as_ref(),
                owner_head.as_ref(),
                miss_fill_plain(),
                permit,
                200,
                Some("\"peer-v1\"".to_owned()),
                headers,
                None,
                body,
                FillSource::Peer {
                    replicate: true,
                    max_idle_secs: None,
                },
            )
            .await;
            drop(owner_tx);
            result
        });

        // Publish one owner chunk, then stop the source so all followers join
        // late and must catch up independently from logical offset zero.
        body_tx
            .send(Ok(hyper::body::Frame::data(first.clone())))
            .await?;
        let mut owner_wire = read_through_body_prefix(&mut owner_rx, first.len()).await;
        let progress = cache
            .join_in_progress(key.as_str())
            .expect("replicated peer owner registered a shareable fill")
            .progress();
        assert_eq!(progress.published(), first.len() as u64);
        let mut followers = Vec::new();
        for _ in 0..3 {
            let follower = cache
                .join_in_progress(key.as_str())
                .expect("each follower independently joins");
            assert_eq!(follower.offset(), 0);
            followers.push(start_follower(follower).await);
        }

        // Act.
        body_tx
            .send(Ok(hyper::body::Frame::data(second.clone())))
            .await?;
        drop(body_tx);
        let FillOutcome::Served(owner_response) = owner.await?? else {
            return Err(Error::internal("complete peer owner requested origin fallback").into());
        };
        owner_rx.read_to_end(&mut owner_wire).await?;

        // Assert owner, followers, and the post-commit HIT all see exact bytes.
        assert_eq!(
            owner_response.cache_store_result,
            Some(CacheStoreResult::Stored)
        );
        assert_eq!(split_wire(&owner_wire).1, expected);
        for (task, mut client) in followers {
            let FollowerFillOutcome::Served(response, terminal) = task.await?? else {
                return Err(Error::internal("complete late follower requested fallback").into());
            };
            assert_eq!(terminal, PeerFillFollowerResult::Complete);
            let mut wire = Vec::new();
            client.read_to_end(&mut wire).await?;
            assert_eq!(response.bytes_sent, expected.len() as u64);
            assert_eq!(
                response.cache_store_result,
                Some(CacheStoreResult::NotStored)
            );
            assert_eq!(split_wire(&wire).1, expected);
        }
        let (lease, _) = cache
            .get(key.as_str())?
            .ok_or_else(|| Error::internal("complete owner did not admit a later HIT"))?;
        assert_eq!(lease.meta().size, expected.len() as u64);
        assert_eq!(lease.meta().provenance, CacheProvenance::PeerCopy);
        Ok(())
    }

    #[tokio::test]
    async fn follower_abort_before_head_retries_normal_path_without_bytes()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/abort-before-head");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(5), permit)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower joins before abort");
        writer.abort();
        let (task, mut client) = start_follower(follower).await;

        // Act.
        let outcome = task.await??;
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        assert!(matches!(
            outcome,
            FollowerFillOutcome::RetryNormal(PeerFillFollowerResult::PrebyteFallback)
        ));
        assert!(wire.is_empty());
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    /// Starts a follower for a client that allows connection reuse.
    async fn start_reusable_follower<F>(
        follower: F,
    ) -> (
        tokio::task::JoinHandle<std::result::Result<FollowerFillOutcome, BoxError>>,
        TcpStream,
    )
    where
        F: Into<ServingFollower> + Send + 'static,
    {
        let (mut server, client) = plain_stream_pair().await;
        let task = tokio::spawn(with_downstream_request_state(async move {
            set_downstream_reuse_allowed(true);
            let result = serve_in_progress_fill(&mut server, follower, false).await;
            drop(server);
            result
        }));
        (task, client)
    }

    #[tokio::test]
    async fn follower_keepalive_completes_framing_after_the_whole_body()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"hello follower";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/reusable-follower");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("shareable fill is visible");
        let (task, mut client) = start_reusable_follower(follower).await;

        // Act.
        writer.write_chunk(body)?;
        writer.commit()?;
        let outcome = task.await??;
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let FollowerFillOutcome::Served(response, PeerFillFollowerResult::Complete) = outcome
        else {
            return Err(Error::internal("complete follower did not finish").into());
        };
        assert!(response.framing_complete);
        let (head, actual_body) = split_wire(&wire);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        assert_eq!(actual_body, body);
        Ok(())
    }

    #[tokio::test]
    async fn follower_keepalive_abort_after_head_leaves_framing_incomplete()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let first = b"first";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/reusable-follower-abort");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer =
            cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(10), permit)?;
        writer.write_chunk(first)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower joins after first publication");
        let (task, mut client) = start_reusable_follower(follower).await;
        let wire = read_through_body_prefix(&mut client, first.len()).await;

        // Act.
        writer.abort();
        let outcome = task.await??;

        // Assert. The head promised keep-alive before the abort, so only the
        // close tells the client the body is short.
        let FollowerFillOutcome::Served(response, PeerFillFollowerResult::Partial) = outcome else {
            return Err(Error::internal("post-byte abort was not partial").into());
        };
        assert!(!response.framing_complete);
        let (head, _) = split_wire(&wire);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        Ok(())
    }

    #[tokio::test]
    async fn follower_abort_after_bytes_is_partial_and_never_admitted()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let first = b"first";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/abort-after-head");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer =
            cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(10), permit)?;
        writer.write_chunk(first)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower joins after first publication");
        let (task, mut client) = start_follower(follower).await;
        let wire = read_through_body_prefix(&mut client, first.len()).await;

        // Act.
        writer.abort();
        let outcome = task.await??;

        // Assert.
        let FollowerFillOutcome::Served(response, terminal) = outcome else {
            return Err(Error::internal("post-byte abort requested transparent fallback").into());
        };
        assert_eq!(terminal, PeerFillFollowerResult::Partial);
        assert_eq!(response.bytes_sent, first.len() as u64);
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::Incomplete)
        );
        assert_eq!(split_wire(&wire).1, first);
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn follower_response_head_socket_failure_is_client_abort()
    -> std::result::Result<(), BoxError> {
        // Arrange a published follower window, then deterministically close the
        // server socket's downstream write half before response-head delivery.
        let body = b"published";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/head-write-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        writer.write_chunk(body)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower joins published body");
        let (mut server, client) = plain_stream_pair().await;
        server.shutdown().await?;

        // Act.
        let outcome = serve_in_progress_fill(&mut server, follower, false).await?;
        writer.abort();
        drop(client);

        // Assert.
        let FollowerFillOutcome::ClientAbort {
            bytes_sent,
            error,
            result,
            stage,
        } = outcome
        else {
            return Err(Error::internal("head write failure was not client_abort").into());
        };
        assert_eq!(result, PeerFillFollowerResult::ClientAbort);
        assert_eq!(stage, FollowerClientFailureStage::ResponseHeadWrite);
        assert_eq!(bytes_sent, 0);
        assert!(!error.to_string().is_empty());
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn follower_readable_window_failure_is_terminal_io_failure()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/readable-window-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .ok_or_else(|| Error::internal("cold key did not elect an owner"))?;
        let writer = cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(8), permit)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .ok_or_else(|| Error::internal("shareable fill was not visible"))?;
        let faulted = ServingFollower::with_fault(follower, FollowerFault::ReadableWindow);
        let (mut server, client) = plain_stream_pair().await;

        // Act.
        let outcome = serve_in_progress_fill(&mut server, faulted, false).await?;
        writer.abort();
        drop(client);

        // Assert.
        let FollowerFillOutcome::IoFailure {
            error,
            bytes_sent,
            result,
            stage,
        } = outcome
        else {
            return Err(
                Error::internal("readable-window failure bypassed terminal outcome").into(),
            );
        };
        assert_eq!(result, PeerFillFollowerResult::IoFailure);
        assert_eq!(stage, FollowerIoFailureStage::ReadableWindow);
        assert_eq!(bytes_sent, 0);
        assert_eq!(
            error.to_string(),
            "Internal: injected follower readable-window failure"
        );
        Ok(())
    }

    #[tokio::test]
    async fn follower_advance_failure_after_sendfile_is_terminal_io_failure()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"published body";
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/advance-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .ok_or_else(|| Error::internal("cold key did not elect an owner"))?;
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        writer.write_chunk(body)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .ok_or_else(|| Error::internal("shareable fill was not visible"))?;
        let faulted = ServingFollower::with_fault(follower, FollowerFault::Advance);
        let (task, mut client) = start_follower(faulted).await;

        // Act.
        let outcome = task.await??;
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;
        writer.abort();

        // Assert.
        let FollowerFillOutcome::IoFailure {
            error,
            bytes_sent,
            result,
            stage,
        } = outcome
        else {
            return Err(Error::internal("advance failure bypassed terminal outcome").into());
        };
        assert_eq!(result, PeerFillFollowerResult::IoFailure);
        assert_eq!(stage, FollowerIoFailureStage::AdvanceAfterSendfile);
        assert_eq!(bytes_sent, body.len() as u64);
        assert_eq!(
            error.to_string(),
            "Internal: injected follower advance failure"
        );
        assert_eq!(split_wire(&wire).1, body);
        Ok(())
    }

    #[tokio::test]
    async fn follower_compound_sendfile_and_advance_failure_logs_both_errors()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"trusted peer bytes";
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("compound-failure");
        let storage = FaultStorage {
            path: path.clone(),
            fail_begin: false,
            fail_after_writes: None,
            fail_commit: true,
        };
        let cache = Cache::new(storage);
        let key = plain_key("origin.example", "/compound-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .ok_or_else(|| Error::internal("cold key did not elect an owner"))?;
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        writer.write_chunk(body)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .ok_or_else(|| Error::internal("shareable fill was not visible"))?;
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)?;
        let faulted = ServingFollower::with_fault(follower, FollowerFault::Advance);
        let (mut server, mut client) = plain_stream_pair().await;
        let capture = EventCapture::default();
        let subscriber = capture.info_subscriber();

        // Act.
        let outcome = serve_in_progress_fill(&mut server, faulted, false)
            .with_subscriber(subscriber)
            .await?;
        drop(server);
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;
        writer.abort();

        // Assert.
        let FollowerFillOutcome::IoFailure {
            error,
            bytes_sent,
            result,
            stage,
        } = outcome
        else {
            return Err(Error::internal("compound failure bypassed terminal outcome").into());
        };
        assert_eq!(result, PeerFillFollowerResult::IoFailure);
        assert_eq!(stage, FollowerIoFailureStage::AdvanceAfterSendfileFailure);
        assert_eq!(bytes_sent, 0);
        assert_eq!(
            error.to_string(),
            "Internal: injected follower advance failure"
        );
        let events = capture.events();
        let event = captured_event(&events, "peer_fill_follower_compound_io_failure");
        assert!(
            event
                .fields
                .get("sendfile_error")
                .is_some_and(|error| error.contains("sendfile returned 0"))
        );
        assert!(
            event
                .fields
                .get("advance_error")
                .is_some_and(|error| error.contains("injected follower advance failure"))
        );
        assert!(split_wire(&wire).1.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn slow_disconnected_follower_does_not_backpressure_owner_commit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let chunk = vec![b'x'; 4 * 1024 * 1024];
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/slow-follower");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta((chunk.len() * 2) as u64),
            permit,
        )?;
        writer.write_chunk(&chunk)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("slow follower joins");
        let (task, client) = start_follower(follower).await;
        tokio::task::yield_now().await;

        // Act: the follower's socket is deliberately unread while the owner
        // writes and commits independently.
        writer.write_chunk(&chunk)?;
        writer.commit()?;
        let hit = cache.get(key.as_str())?;
        drop(client);
        let _ = task.await;

        // Assert.
        assert!(hit.is_some(), "slow follower prevented owner admission");
        Ok(())
    }

    #[tokio::test]
    async fn follower_sendfile_disconnect_is_conservative_io_failure()
    -> std::result::Result<(), BoxError> {
        // Arrange enough published data that sendfile cannot finish into the
        // loopback socket before the client closes.
        let body = vec![b'x'; 64 * 1024 * 1024];
        let (_dir, cache) = fresh_cache();
        let key = plain_key("origin.example", "/disconnecting-follower");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        writer.write_chunk(&body)?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("disconnecting follower joins");
        let (mut server, mut client) = plain_stream_pair().await;
        let task = tokio::spawn(with_downstream_request_state(async move {
            let outcome = serve_in_progress_fill(&mut server, follower, false).await;
            let downstream_bytes = downstream_body_bytes();
            drop(server);
            outcome.map(|outcome| (outcome, downstream_bytes))
        }));
        let _wire_prefix = read_through_body_prefix(&mut client, 1).await;

        // Act.
        drop(client);
        let (outcome, downstream_bytes) = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("follower send must observe the disconnected client")??;
        writer.commit()?;

        // Assert.
        let FollowerFillOutcome::IoFailure {
            bytes_sent,
            error,
            result,
            stage,
        } = outcome
        else {
            return Err(
                Error::internal("sendfile failure was not classified as io_failure").into(),
            );
        };
        assert_eq!(result, PeerFillFollowerResult::IoFailure);
        assert_eq!(stage, FollowerIoFailureStage::Sendfile);
        assert!(bytes_sent > 0, "the observed body byte must be accounted");
        assert!(bytes_sent < body.len() as u64);
        assert_eq!(downstream_bytes, bytes_sent);
        assert!(!error.to_string().is_empty());
        assert!(
            cache.get(key.as_str())?.is_some(),
            "follower disconnect must not cancel owner admission"
        );
        Ok(())
    }

    #[tokio::test]
    async fn truncated_staging_sendfile_is_io_failure_and_isolates_other_follower()
    -> std::result::Result<(), BoxError> {
        // Arrange two independent production followers over one published
        // staging inode, then truncate the inode behind the monotonic frontier.
        let body = b"trusted peer bytes";
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("truncated-staging");
        let storage = FaultStorage {
            path: path.clone(),
            fail_begin: false,
            fail_after_writes: None,
            fail_commit: true,
        };
        let cache = Cache::new(storage);
        let key = plain_key("origin.example", "/truncated-staging");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        writer.write_chunk(body)?;
        let failed_follower = cache
            .join_in_progress(key.as_str())
            .expect("first follower joins published body");
        let isolated_follower = cache
            .join_in_progress(key.as_str())
            .expect("other follower independently joins");
        std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)?;
        let (failed_task, mut failed_client) = start_follower(failed_follower).await;

        // Act: the first sendfile reaches EOF before its published range. Its
        // failure must not mutate owner progress or the other follower.
        let failed_outcome = failed_task.await??;
        let mut failed_wire = Vec::new();
        failed_client.read_to_end(&mut failed_wire).await?;
        assert_eq!(
            isolated_follower.progress().state(),
            FillProgressState::Receiving
        );
        assert_eq!(isolated_follower.progress().published(), body.len() as u64);

        // Restore the same inode for the already-joined independent follower.
        // The owner then reaches exact body completion but injected metadata
        // commit failure keeps the cache cold.
        std::fs::write(&path, body)?;
        let (isolated_task, mut isolated_client) = start_follower(isolated_follower).await;
        let isolated_wire = read_through_body_prefix(&mut isolated_client, body.len()).await;
        assert!(writer.commit().is_err());
        let isolated_outcome = isolated_task.await??;

        // Assert the first client saw only a response head (partial HTTP body),
        // while the other follower completed and no object was admitted.
        let FollowerFillOutcome::IoFailure {
            bytes_sent,
            error,
            result,
            stage,
        } = failed_outcome
        else {
            return Err(Error::internal("truncated staging inode was not io_failure").into());
        };
        assert_eq!(result, PeerFillFollowerResult::IoFailure);
        assert_eq!(stage, FollowerIoFailureStage::Sendfile);
        assert_eq!(bytes_sent, 0);
        assert!(!error.to_string().is_empty());
        let (failed_head, failed_body) = split_wire(&failed_wire);
        assert!(failed_head.contains("Content-Length"));
        assert!(failed_body.is_empty());

        let FollowerFillOutcome::Served(response, terminal) = isolated_outcome else {
            return Err(Error::internal("independent follower did not complete").into());
        };
        assert_eq!(terminal, PeerFillFollowerResult::Complete);
        assert_eq!(response.bytes_sent, body.len() as u64);
        assert_eq!(split_wire(&isolated_wire).1, body);
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn staging_write_failure_stops_follower_after_published_frontier()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let dir = tempfile::tempdir()?;
        let storage = FaultStorage {
            path: dir.path().join("write-fault"),
            fail_begin: false,
            fail_after_writes: Some(1),
            fail_commit: false,
        };
        let cache = Cache::new(storage);
        let key = plain_key("origin.example", "/write-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer =
            cache.begin_shareable_acquired_fill(key.as_str(), peer_meta(10), permit)?;
        writer.write_chunk(b"first")?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower sees successful first write");
        let (task, mut client) = start_follower(follower).await;
        let wire = read_through_body_prefix(&mut client, 5).await;

        // Act.
        assert!(writer.write_chunk(b"later").is_err());
        writer.abort();
        let outcome = task.await??;

        // Assert.
        let FollowerFillOutcome::Served(response, terminal) = outcome else {
            return Err(Error::internal("post-byte storage failure requested fallback").into());
        };
        assert_eq!(terminal, PeerFillFollowerResult::Partial);
        assert_eq!(response.bytes_sent, 5);
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::Incomplete)
        );
        assert_eq!(split_wire(&wire).1, b"first");
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn commit_failed_after_exact_body_still_completes_follower()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"trusted peer bytes";
        let dir = tempfile::tempdir()?;
        let storage = FaultStorage {
            path: dir.path().join("commit-fault"),
            fail_begin: false,
            fail_after_writes: None,
            fail_commit: true,
        };
        let cache = Cache::new(storage);
        let key = plain_key("origin.example", "/commit-failure");
        let permit = owner_permit(&cache, &key)
            .await
            .expect("cold key elects owner");
        let mut writer = cache.begin_shareable_acquired_fill(
            key.as_str(),
            peer_meta(body.len() as u64),
            permit,
        )?;
        let follower = cache
            .join_in_progress(key.as_str())
            .expect("follower joins exact body");
        let (task, mut client) = start_follower(follower).await;

        // Act.
        writer.write_chunk(body)?;
        assert!(writer.commit().is_err());
        let outcome = task.await??;
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await?;

        // Assert.
        let FollowerFillOutcome::Served(response, terminal) = outcome else {
            return Err(Error::internal("commit failure invalidated exact peer bytes").into());
        };
        assert_eq!(terminal, PeerFillFollowerResult::Complete);
        assert_eq!(response.bytes_sent, body.len() as u64);
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::NotStored)
        );
        assert_eq!(split_wire(&wire).1, body);
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn serve_miss_streaming_fixed_length_admits_and_hits() {
        // Arrange.
        let body = b"hello streaming world";
        let response = format!(
            "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect::<Vec<u8>>();
        let addr = spawn_raw_origin(response).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/obj");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/obj");

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;

        // Act.
        let (resp, wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let r = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("serve_miss_streaming");
            (r, tx)
        })
        .await;

        // Assert.
        assert_eq!(resp.head.status, 200);
        assert_eq!(
            resp.cache_store_result,
            Some(CacheStoreResult::Stored),
            "the access record reports the committed fill"
        );
        assert!(matches!(resp.body, BodySource::Streamed));
        assert_eq!(resp.bytes_sent, body.len() as u64);

        let (wire_head, wire_body) = split_wire(&wire);
        assert!(
            wire_head.contains("X-Cache-Status: MISS"),
            "head: {wire_head}"
        );
        assert!(
            wire_head.contains("X-Cache-Tier-Hit: origin"),
            "origin fills identify their tier: {wire_head}"
        );
        assert!(
            wire_head.contains(&format!("Content-Length: {}", body.len())),
            "fixed-length head must carry Content-Length: {wire_head}"
        );
        assert!(
            !wire_head.to_ascii_lowercase().contains("transfer-encoding"),
            "a fixed-length body must not be chunked: {wire_head}"
        );
        assert_eq!(wire_body, body, "client body matches origin");

        // Re-open a fresh cache handle on the same dir to prove the entry was
        // committed, and its stored bytes match.
        let (lease, _status) = cache
            .get(key.as_str())
            .unwrap()
            .expect("streamed MISS admitted the object");
        assert_eq!(lease.meta().size, body.len() as u64);
        assert_eq!(lease.meta().provenance, CacheProvenance::Origin);
        let file_bytes = std::fs::read(&lease.path).unwrap();
        assert_eq!(&file_bytes[lease.body_offset as usize..], body);
    }

    #[tokio::test]
    async fn peer_get_body_failure_before_response_requests_origin_fallback()
    -> std::result::Result<(), BoxError> {
        for replicate in [false, true] {
            // Arrange.
            let (_dir, cache) = fresh_cache();
            let host = "bucket.s3.amazonaws.com";
            let path = if replicate {
                "/peer-first-read-replicated"
            } else {
                "/peer-first-read-relayed"
            };
            let key = plain_key(host, path);
            let head = get_head(host, path);
            let headers = vec![
                ("ETag".to_owned(), "\"v1\"".to_owned()),
                ("Content-Length".to_owned(), "10".to_owned()),
            ];
            let body = http_body_util::StreamBody::new(tokio_stream::iter(vec![Err::<
                hyper::body::Frame<Bytes>,
                _,
            >(
                std::io::Error::other("injected peer body failure"),
            )]));
            let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
            let permit = if replicate {
                owner_permit(&cache, &key).await
            } else {
                None
            };

            // Act.
            let outcome = fill_from_open_response(
                &mut client_tx,
                &cache,
                &key,
                &head,
                miss_fill_plain(),
                permit,
                200,
                Some("\"v1\"".to_owned()),
                headers,
                None,
                body,
                FillSource::Peer {
                    replicate,
                    max_idle_secs: None,
                },
            )
            .await?;
            drop(client_tx);
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;

            // Assert.
            assert!(matches!(
                outcome,
                FillOutcome::RetryOrigin(PeerFallbackReason::BodyReadFailed)
            ));
            assert!(
                wire.is_empty(),
                "peer failure must precede every response byte"
            );
            assert!(cache.get(key.as_str())?.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_body_eof_before_response_requests_origin_fallback()
    -> std::result::Result<(), BoxError> {
        for replicate in [false, true] {
            // Arrange.
            let (_dir, cache) = fresh_cache();
            let host = "bucket.s3.amazonaws.com";
            let path = if replicate {
                "/peer-first-eof-replicated"
            } else {
                "/peer-first-eof-relayed"
            };
            let key = plain_key(host, path);
            let head = get_head(host, path);
            let headers = vec![
                ("ETag".to_owned(), "\"v1\"".to_owned()),
                ("Content-Length".to_owned(), "10".to_owned()),
            ];
            let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
            let permit = if replicate {
                owner_permit(&cache, &key).await
            } else {
                None
            };

            // Act.
            let outcome = fill_from_open_response(
                &mut client_tx,
                &cache,
                &key,
                &head,
                miss_fill_plain(),
                permit,
                200,
                Some("\"v1\"".to_owned()),
                headers,
                None,
                http_body_util::Empty::<Bytes>::new(),
                FillSource::Peer {
                    replicate,
                    max_idle_secs: None,
                },
            )
            .await?;
            drop(client_tx);
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;

            // Assert.
            assert!(matches!(
                outcome,
                FillOutcome::RetryOrigin(PeerFallbackReason::BodyIncomplete)
            ));
            assert!(
                wire.is_empty(),
                "premature EOF must precede every response byte"
            );
            assert!(cache.get(key.as_str())?.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_oversized_first_frame_before_response_requests_origin_fallback()
    -> std::result::Result<(), BoxError> {
        for replicate in [false, true] {
            // Arrange.
            let body = Bytes::from_static(b"oversized");
            let (_dir, cache) = fresh_cache();
            let host = "bucket.s3.amazonaws.com";
            let path = if replicate {
                "/peer-oversized-replicated"
            } else {
                "/peer-oversized-relayed"
            };
            let key = plain_key(host, path);
            let head = get_head(host, path);
            let headers = vec![
                ("ETag".to_owned(), "\"v1\"".to_owned()),
                ("Content-Length".to_owned(), "3".to_owned()),
            ];
            let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
            let permit = if replicate {
                owner_permit(&cache, &key).await
            } else {
                None
            };

            // Act.
            let outcome = fill_from_open_response(
                &mut client_tx,
                &cache,
                &key,
                &head,
                miss_fill_plain(),
                permit,
                200,
                Some("\"v1\"".to_owned()),
                headers,
                None,
                http_body_util::Full::new(body),
                FillSource::Peer {
                    replicate,
                    max_idle_secs: None,
                },
            )
            .await?;
            drop(client_tx);
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;

            // Assert.
            assert!(matches!(
                outcome,
                FillOutcome::RetryOrigin(PeerFallbackReason::BodyIncomplete)
            ));
            assert!(
                wire.is_empty(),
                "an oversized first frame must not commit the peer response"
            );
            assert!(cache.get(key.as_str())?.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_body_failure_after_response_does_not_request_fallback()
    -> std::result::Result<(), BoxError> {
        for replicate in [false, true] {
            // Arrange.
            let first = Bytes::from_static(b"first peer bytes");
            let declared_length = if replicate {
                MISS_BUFFER_MAX_BYTES + 1
            } else {
                first.len() as u64 + 100
            };
            let (_dir, cache) = fresh_cache();
            let host = "bucket.s3.amazonaws.com";
            let path = if replicate {
                "/peer-late-failure-replicated"
            } else {
                "/peer-late-failure-relayed"
            };
            let key = plain_key(host, path);
            let head = get_head(host, path);
            let headers = vec![
                ("ETag".to_owned(), "\"v1\"".to_owned()),
                ("Content-Length".to_owned(), declared_length.to_string()),
            ];
            let body = http_body_util::StreamBody::new(tokio_stream::iter(vec![
                Ok::<_, std::io::Error>(hyper::body::Frame::data(first.clone())),
                Err(std::io::Error::other("injected late peer body failure")),
            ]));
            let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
            let permit = if replicate {
                owner_permit(&cache, &key).await
            } else {
                None
            };

            // Act.
            let outcome = fill_from_open_response(
                &mut client_tx,
                &cache,
                &key,
                &head,
                miss_fill_plain(),
                permit,
                200,
                Some("\"v1\"".to_owned()),
                headers,
                None,
                body,
                FillSource::Peer {
                    replicate,
                    max_idle_secs: None,
                },
            )
            .await?;
            drop(client_tx);
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;

            // Assert.
            let FillOutcome::Served(response) = outcome else {
                return Err(Error::internal("late peer failure requested origin fallback").into());
            };
            assert_eq!(
                response.cache_store_result,
                Some(CacheStoreResult::Incomplete)
            );
            assert_eq!(response.bytes_sent, first.len() as u64);
            let (wire_head, wire_body) = split_wire(&wire);
            assert!(wire_head.contains("X-Cache-Tier-Hit: peer"));
            assert_eq!(wire_body, first);
            assert!(cache.get(key.as_str())?.is_none());
        }
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_staging_begin_failure_logs_original_error_and_relays()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = Bytes::from_static(b"peer body remains authoritative");
        let dir = tempfile::tempdir()?;
        let storage = FaultStorage {
            path: dir.path().join("unused-staging"),
            fail_begin: true,
            fail_after_writes: None,
            fail_commit: false,
        };
        let cache = Cache::new(storage);
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/peer-staging-begin-failure");
        let head = get_head(host, "/peer-staging-begin-failure");
        let headers = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Length".to_owned(), body.len().to_string()),
        ];
        let permit = owner_permit(&cache, &key).await;
        let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
        let reader = tokio::spawn(async move {
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;
            Ok::<_, std::io::Error>(wire)
        });
        let capture = EventCapture::default();
        let subscriber = capture.info_subscriber();

        // Act.
        let outcome = fill_from_open_response(
            &mut client_tx,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            permit,
            200,
            Some("\"v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(body.clone()),
            FillSource::Peer {
                replicate: true,
                max_idle_secs: None,
            },
        )
        .with_subscriber(subscriber)
        .await?;
        drop(client_tx);
        let wire = reader.await??;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("staging failure requested origin fallback").into());
        };
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::StorageError)
        );
        assert_eq!(response.bytes_sent, body.len() as u64);
        assert_eq!(split_wire(&wire).1, body);
        let events = capture.events();
        let event = captured_event(&events, "peer_fill_data_path_failure");
        assert_eq!(
            event.fields.get("stage").map(String::as_str),
            Some("staging_begin")
        );
        assert!(
            event
                .fields
                .get("error")
                .is_some_and(|error| error.contains("injected staging begin failure"))
        );
        assert_eq!(
            event
                .fields
                .get("bytes_received")
                .and_then(|value| value.parse::<u64>().ok()),
            Some(body.len() as u64)
        );
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_downstream_failure_records_partial_body_progress()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = Bytes::from(vec![b'x'; 8192]);
        let partial_bytes = 1234_u64;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/peer-partial-downstream");
        let head = get_head(host, "/peer-partial-downstream");
        let headers = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Length".to_owned(), body.len().to_string()),
        ];
        let permit = owner_permit(&cache, &key).await;
        let body_written = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut downstream = DisconnectDuringBody {
            head_written: false,
            body_remaining: partial_bytes as usize,
            body_written: Arc::clone(&body_written),
        };
        let fill = miss_fill_plain();
        let capture = EventCapture::default();
        let subscriber = capture.info_subscriber();

        // Act.
        let outcome = fill_from_open_response(
            &mut downstream,
            &cache,
            &key,
            &head,
            fill,
            permit,
            200,
            Some("\"v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(body.clone()),
            FillSource::Peer {
                replicate: true,
                max_idle_secs: None,
            },
        )
        .with_subscriber(subscriber)
        .await?;

        // Assert.
        let FillOutcome::Served(response) = outcome else {
            return Err(Error::internal("client abort requested origin fallback").into());
        };
        assert_eq!(body_written.load(Ordering::SeqCst), partial_bytes);
        assert_eq!(response.bytes_sent, partial_bytes);
        assert_eq!(response.cache_store_result, Some(CacheStoreResult::Stored));
        assert!(cache.get(key.as_str())?.is_some());
        let events = capture.events();
        let event = captured_event(&events, "peer_fill_data_path_failure");
        assert_eq!(
            event.fields.get("stage").map(String::as_str),
            Some("downstream_body_write")
        );
        assert_eq!(
            event
                .fields
                .get("bytes_sent")
                .and_then(|value| value.parse::<u64>().ok()),
            Some(partial_bytes)
        );
        assert!(
            event
                .fields
                .get("error")
                .is_some_and(|error| error.contains("injected partial body disconnect"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_replicate_disabled_streams_without_admit() -> std::result::Result<(), BoxError>
    {
        // Arrange.
        let body = Bytes::from_static(b"remote peer body");
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/peer-relay");
        let head = get_head(host, "/peer-relay");
        let headers = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Length".to_owned(), body.len().to_string()),
        ];
        let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
        let reader = tokio::spawn(async move {
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;
            Ok::<_, std::io::Error>(wire)
        });

        // Act.
        let response = fill_from_open_response(
            &mut client_tx,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            None,
            200,
            Some("\"v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(body.clone()),
            FillSource::Peer {
                replicate: false,
                max_idle_secs: None,
            },
        )
        .await?;
        let FillOutcome::Served(response) = response else {
            return Err(Error::internal("complete peer body requested fallback").into());
        };
        drop(client_tx);
        let wire = reader.await??;

        // Assert.
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::NotStored)
        );
        assert_eq!(response.bytes_sent, body.len() as u64);
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.contains("X-Cache-Status: MISS"));
        assert!(wire_head.contains("X-Cache-Tier-Hit: peer"));
        assert_eq!(wire_body, body);
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_replicate_disabled_marks_truncated_without_admit()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = Bytes::from_static(b"short");
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/peer-truncated-relay");
        let head = get_head(host, "/peer-truncated-relay");
        let headers = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Length".to_owned(), "100".to_owned()),
        ];
        let (mut client_rx, mut client_tx) = tokio::io::duplex(4096);
        let reader = tokio::spawn(async move {
            let mut wire = Vec::new();
            client_rx.read_to_end(&mut wire).await?;
            Ok::<_, std::io::Error>(wire)
        });

        // Act.
        let response = fill_from_open_response(
            &mut client_tx,
            &cache,
            &key,
            &head,
            miss_fill_plain(),
            None,
            200,
            Some("\"v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(body),
            FillSource::Peer {
                replicate: false,
                max_idle_secs: None,
            },
        )
        .await?;
        let FillOutcome::Served(response) = response else {
            return Err(Error::internal("partial peer body requested fallback after bytes").into());
        };
        drop(client_tx);
        let _wire = reader.await??;

        // Assert.
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::Incomplete)
        );
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn peer_get_replicate_disabled_stops_after_client_abort()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = Bytes::from(vec![b'x'; 8192]);
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/peer-aborted-relay");
        let head = get_head(host, "/peer-aborted-relay");
        let headers = vec![
            ("ETag".to_owned(), "\"v1\"".to_owned()),
            ("Content-Length".to_owned(), body.len().to_string()),
        ];
        let mut client = DisconnectAfter { remaining: 200 };
        let fill = miss_fill_plain();

        // Act.
        let response = fill_from_open_response(
            &mut client,
            &cache,
            &key,
            &head,
            fill,
            None,
            200,
            Some("\"v1\"".to_owned()),
            headers,
            None,
            http_body_util::Full::new(body),
            FillSource::Peer {
                replicate: false,
                max_idle_secs: None,
            },
        )
        .await;

        // Assert.
        assert!(
            response.is_err(),
            "relay-only transfers stop on client disconnect"
        );
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cache_begin_io_failure_still_relays_complete_origin_body()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let body = b"origin remains authoritative";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect::<Vec<u8>>();
        let addr = spawn_raw_origin(response).await;
        let dir = tempfile::tempdir()?;
        let storage = DiskStorage::open(dir.path())?;
        std::fs::remove_dir_all(dir.path().join("staging"))?;
        File::create(dir.path().join("staging"))?;
        let cache = Cache::new(storage);
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/storage-error");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/storage-error");

        // Act.
        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;
        let (response, wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let response = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("cache failure remains non-fatal");
            (response, tx)
        })
        .await;

        // Assert.
        assert_eq!(
            response.cache_store_result,
            Some(CacheStoreResult::StorageError)
        );
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.contains("X-Cache-Status: MISS"));
        assert_eq!(wire_body, body);
        assert!(cache.get(key.as_str())?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn streaming_miss_client_disconnect_still_commits_complete_origin_body() {
        // Arrange.
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (origin_response, body_len) = large_streaming_response();
        let addr = spawn_counted_raw_origin(origin_response, Arc::clone(&requests)).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/client-disconnect");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/client-disconnect");
        let mut client = DisconnectAfter { remaining: 4096 };
        let fill = miss_fill_plain();

        // Act.
        let permit = owner_permit(&cache, &key).await;
        let response =
            serve_miss_streaming(&mut client, &cache, &key, &upstream, &head, fill, permit).await;
        let stored = cache.get(key.as_str()).unwrap();

        // Assert.
        assert!(response.is_ok(), "client disconnect is ignored");
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the origin is drained once"
        );
        let (lease, _) = stored.expect("the completed body becomes a later HIT");
        assert_eq!(lease.meta().size, body_len as u64);
        let file_bytes = std::fs::read(&lease.path).unwrap();
        assert_eq!(
            file_bytes.len() - lease.body_offset as usize,
            body_len,
            "the committed cache body is complete"
        );
    }

    #[tokio::test]
    async fn streaming_miss_uncacheable_client_abort_stops_then_refetches() {
        // Arrange.
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (origin_response, _) = large_streaming_response();
        let origin_response = String::from_utf8(origin_response)
            .unwrap()
            .replacen("200 OK", "404 Not Found", 1)
            .into_bytes();
        let addr = spawn_counted_raw_origin(origin_response, Arc::clone(&requests)).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/abort");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/abort");
        let mut disconnected = DisconnectAfter { remaining: 4096 };

        // Act.
        let aborted_permit = owner_permit(&cache, &key).await;
        let aborted = serve_miss_streaming(
            &mut disconnected,
            &cache,
            &key,
            &upstream,
            &head,
            miss_fill_plain(),
            aborted_permit,
        )
        .await;
        let after_abort = cache.get(key.as_str()).unwrap();
        let mut replacement_client = tokio::io::sink();
        let refetch_permit = owner_permit(&cache, &key).await;
        let refetched = serve_miss_streaming(
            &mut replacement_client,
            &cache,
            &key,
            &upstream,
            &head,
            miss_fill_plain(),
            refetch_permit,
        )
        .await;
        let after_refetch = cache.get(key.as_str()).unwrap();

        // Assert.
        assert!(aborted.is_err(), "the downstream failure aborts the fill");
        assert!(
            after_abort.is_none(),
            "an aborted fill leaves no cache entry"
        );
        assert!(refetched.is_ok(), "the next MISS refetches successfully");
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the aborted fill forces a second origin request"
        );
        assert!(
            after_refetch.is_none(),
            "uncacheable responses never commit"
        );
    }

    #[tokio::test]
    async fn serve_miss_streaming_chunked_when_length_unknown() {
        // Arrange.
        // No Content-Length: the origin close-delimits the body, so the engine
        // must re-frame it to the client as Transfer-Encoding: chunked.
        let body = b"length-unknown streamed body";
        let response = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n"
            .iter()
            .copied()
            .chain(body.iter().copied())
            .collect::<Vec<u8>>();
        let addr = spawn_raw_origin(response).await;
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_bytes: Some(0),
                min_free_bytes: Some(u64::MAX),
                ..EvictionPolicy::default()
            },
        );
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/nolen");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/nolen");

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;

        // Act.
        let (resp, wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let r = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("serve_miss_streaming");
            (r, tx)
        })
        .await;

        // Assert.
        assert_eq!(resp.head.status, 200);
        assert_eq!(
            resp.cache_store_result,
            Some(CacheStoreResult::Stored),
            "capacity pressure does not reject an unknown-length fill"
        );
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(
            wire_head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked"),
            "unknown-length body must be chunked: {wire_head}"
        );
        assert!(
            wire_body.ends_with(b"0\r\n\r\n"),
            "chunked body must end with the terminating zero chunk"
        );
        // The object bytes appear inside the chunk framing.
        assert!(
            find_subslice(&wire_body, body).is_some(),
            "client body carries the origin bytes"
        );

        // A close-delimited body is admitted (no declared length to fall short of).
        let (lease, _status) = cache.get(key.as_str()).unwrap().expect("admitted");
        let file_bytes = std::fs::read(&lease.path).unwrap();
        assert_eq!(&file_bytes[lease.body_offset as usize..], body);
    }

    #[tokio::test]
    async fn serve_miss_streaming_truncated_body_does_not_admit() {
        // Declares Content-Length: 100 but sends only 5 bytes then closes. The
        // fill must abort so a later GET re-MISSes rather than serving a
        // truncated HIT (cache is never poisoned).
        let response = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort"
            .to_vec();
        let addr = spawn_raw_origin(response).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/truncated");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/truncated");

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;
        let (_resp, _wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let r = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("serve_miss_streaming");
            (r, tx)
        })
        .await;

        assert!(
            cache.get(key.as_str()).unwrap().is_none(),
            "a truncated fill must not admit a cache entry"
        );
    }

    #[tokio::test]
    async fn serve_miss_streaming_non_2xx_relays_without_admit() {
        let response =
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
                .to_vec();
        let addr = spawn_raw_origin(response).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/missing");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/missing");

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;
        let (resp, wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let r = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("serve_miss_streaming");
            (r, tx)
        })
        .await;

        assert_eq!(resp.head.status, 404);
        let (_head, wire_body) = split_wire(&wire);
        assert_eq!(wire_body, b"nope");
        assert!(
            cache.get(key.as_str()).unwrap().is_none(),
            "a non-2xx response is never admitted"
        );
    }

    /// Runs one MISS fill for a request whose client allows reuse and returns
    /// the arm's response plus the client wire bytes.
    async fn reusable_miss_fill(
        origin_response: &[u8],
        http_version: &str,
        path: &str,
    ) -> (Response, Vec<u8>) {
        let addr = spawn_raw_origin(origin_response.to_vec()).await;
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, path);
        let upstream = format!("http://{addr}");
        let mut head = get_head(host, path);
        head.http_version = http_version.to_owned();

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;
        with_downstream_request_state(async move {
            set_downstream_reuse_allowed(true);
            drive_streamed(|mut tx| async move {
                let permit = owner_permit(cache_ref, key_ref).await;
                let response = serve_miss_streaming(
                    &mut tx,
                    cache_ref,
                    key_ref,
                    upstream_ref,
                    head_ref,
                    miss_fill_plain(),
                    permit,
                )
                .await
                .expect("serve_miss_streaming");
                (response, tx)
            })
            .await
        })
        .await
    }

    #[test]
    fn general_get_keepalive_requires_client_permission_and_no_request_bytes() {
        // (request version, request headers, bytes read past the head, head
        // delimited, reusable)
        type Case = (
            &'static str,
            &'static [(&'static str, &'static str)],
            &'static [u8],
            bool,
            bool,
        );
        let cases: [Case; 13] = [
            ("HTTP/1.1", &[], b"", true, true),
            ("HTTP/1.1", &[("Connection", "close")], b"", true, false),
            (
                "HTTP/1.1",
                &[("Connection", "keep-alive"), ("Connection", "close")],
                b"",
                true,
                false,
            ),
            ("HTTP/1.0", &[], b"", true, false),
            ("HTTP/1.0", &[("Connection", "keep-alive")], b"", true, true),
            (
                "HTTP/1.0",
                &[("Connection", "upgrade"), ("Connection", "Keep-Alive")],
                b"",
                true,
                true,
            ),
            ("HTTP/1.1", &[("Content-Length", "0")], b"", true, true),
            ("HTTP/1.1", &[("Content-Length", "5")], b"", true, false),
            (
                "HTTP/1.1",
                &[("Content-Length", "invalid")],
                b"",
                true,
                false,
            ),
            (
                "HTTP/1.1",
                &[("Transfer-Encoding", "chunked")],
                b"",
                true,
                false,
            ),
            (
                "HTTP/1.1",
                &[("Content-Length", "0"), ("Content-Length", "5")],
                b"",
                true,
                false,
            ),
            ("HTTP/1.1", &[], b"GET /next HTTP/1.1\r\n", true, false),
            ("HTTP/1.1", &[], b"", false, false),
        ];
        for (version, headers, prefix, delimited, expected) in cases {
            let mut head = get_head("bucket.example.com", "/object");
            head.http_version = version.to_owned();
            head.headers = headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();

            assert_eq!(
                client_allows_reuse(&head, prefix, delimited),
                expected,
                "{version} {headers:?} {prefix:?} delimited={delimited}"
            );
        }
    }

    #[tokio::test]
    async fn general_get_keepalive_needs_a_strictly_delimited_head() {
        // (wire bytes, delimited)
        let oversized = format!(
            "GET /first HTTP/1.1\r\nHost: h\r\nX-Pad: {}\r\nGET /smuggled HTTP/1.1\r\nHost: h\r\n\r\n",
            "a".repeat(MAX_HEAD_BYTES)
        );
        let cases: [(&[u8], bool); 7] = [
            (b"GET / HTTP/1.1\r\nHost: h\r\n\r\n", true),
            (b"GET / HTTP/1.1\r\nHost: h\r\n", false),
            (oversized.as_bytes(), false),
            (b"GET / HTTP/1.1\r\nX: a\nContent-Length: 5\r\n\r\n", false),
            (
                b"GET / HTTP/1.1\nContent-Length: 5\r\nHost: h\r\n\r\n",
                false,
            ),
            (
                b"GET / HTTP/1.1\r\nX: a\r\n Content-Length: 5\r\n\r\n",
                false,
            ),
            (
                b"GET / HTTP/1.1\r\nContent-Length 5\r\nHost: h\r\n\r\n",
                false,
            ),
        ];
        for (wire, expected) in cases {
            // Arrange.
            let (mut client, mut server) = tokio::io::duplex(MAX_HEAD_BYTES * 2);
            client.write_all(wire).await.unwrap();
            drop(client);

            // Act.
            let read = read_request_head(&mut server)
                .await
                .unwrap()
                .expect("a parseable request line");

            // Assert.
            assert_eq!(
                read.delimited,
                expected,
                "{:?}",
                String::from_utf8_lossy(&wire[..wire.len().min(80)])
            );
        }
    }

    #[test]
    fn general_get_keepalive_connection_next_resets_timeouts_and_unframed_failures() {
        // Arrange.
        let complete = Response::streamed(200, Some("MISS"), 5).with_framing_complete(true);
        let unframed = Response::streamed(200, Some("MISS"), 0).with_abort_close(true);
        let driver_written =
            Response::buffered(200, "OK", Some("HIT"), Vec::new(), b"hit".to_vec());
        let streamed_close = Response::streamed(200, Some("BYPASS"), 5);

        // Act and assert.
        assert_eq!(
            connection_next(false, true, &complete),
            ConnectionNext::KeepAlive
        );
        assert_eq!(
            connection_next(false, true, &driver_written),
            ConnectionNext::KeepAlive
        );
        assert_eq!(
            connection_next(false, false, &driver_written),
            ConnectionNext::Close(CloseMode::Graceful)
        );
        assert_eq!(
            connection_next(false, true, &streamed_close),
            ConnectionNext::Close(CloseMode::Graceful)
        );
        assert_eq!(
            connection_next(true, true, &complete),
            ConnectionNext::Close(CloseMode::Reset)
        );
        assert_eq!(
            connection_next(false, true, &unframed),
            ConnectionNext::Close(CloseMode::Reset)
        );
    }

    #[tokio::test]
    async fn general_get_keepalive_complete_miss_fills_mark_framing_complete() {
        // Arrange.
        let small = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";
        let unknown_length =
            b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nclose-delimited";

        // Act.
        let (small_response, small_wire) = reusable_miss_fill(small, "HTTP/1.1", "/small").await;
        let (chunked_response, chunked_wire) =
            reusable_miss_fill(unknown_length, "HTTP/1.1", "/chunked").await;

        // Assert.
        let (small_head, small_body) = split_wire(&small_wire);
        assert!(
            small_head.contains("Connection: keep-alive"),
            "{small_head}"
        );
        assert_eq!(small_body, b"hello");
        assert!(small_response.framing_complete);
        let (chunked_head, chunked_body) = split_wire(&chunked_wire);
        assert!(
            chunked_head.contains("Transfer-Encoding: chunked"),
            "{chunked_head}"
        );
        assert!(
            chunked_head.contains("Connection: keep-alive"),
            "{chunked_head}"
        );
        assert!(chunked_body.ends_with(b"0\r\n\r\n"));
        assert!(chunked_response.framing_complete);
    }

    #[tokio::test]
    async fn general_get_keepalive_length_relay_completes_framing_only_when_whole() {
        // Arrange. A 404 is relayed without admission, so it streams through
        // the tail writer under the origin's Content-Length.
        let whole =
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found";
        let short =
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 20\r\nConnection: close\r\n\r\nnot found";

        // Act.
        let (whole_response, whole_wire) =
            reusable_miss_fill(whole, "HTTP/1.1", "/relay-whole").await;
        let (short_response, short_wire) =
            reusable_miss_fill(short, "HTTP/1.1", "/relay-short").await;

        // Assert.
        let (whole_head, whole_body) = split_wire(&whole_wire);
        assert!(whole_head.contains("Content-Length: 9"), "{whole_head}");
        assert!(
            whole_head.contains("Connection: keep-alive"),
            "{whole_head}"
        );
        assert_eq!(whole_body, b"not found");
        assert!(whole_response.framing_complete);
        let (short_head, short_body) = split_wire(&short_wire);
        assert!(short_head.contains("Content-Length: 20"), "{short_head}");
        assert!(
            short_head.contains("Connection: keep-alive"),
            "{short_head}"
        );
        assert_eq!(short_body, b"not found");
        assert!(!short_response.framing_complete);
    }

    #[tokio::test]
    async fn general_get_keepalive_truncated_miss_fill_is_not_reusable() {
        // Arrange.
        let truncated = b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nTransfer-Encoding: chunked\r\n\r\n7\r\npartial\r\n";

        // Act.
        let (response, wire) = reusable_miss_fill(truncated, "HTTP/1.1", "/truncated").await;

        // Assert. The head promised keep-alive before the origin failed, so the
        // missing terminator and the close tell the client the body is short.
        let (head, body) = split_wire(&wire);
        assert!(head.contains("Connection: keep-alive"), "{head}");
        assert!(!body.ends_with(b"0\r\n\r\n"));
        assert!(!response.framing_complete);
    }

    #[tokio::test]
    async fn general_get_keepalive_undelimitable_miss_fills_announce_close() {
        // Arrange.
        let not_modified =
            b"HTTP/1.1 304 Not Modified\r\nETag: \"v1\"\r\nConnection: close\r\n\r\n";
        let unknown_length =
            b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nConnection: close\r\n\r\nclose-delimited";

        // Act.
        let (bodyless, bodyless_wire) =
            reusable_miss_fill(not_modified, "HTTP/1.1", "/not-modified").await;
        let (legacy, legacy_wire) =
            reusable_miss_fill(unknown_length, "HTTP/1.0", "/legacy-chunked").await;

        // Assert. A 304 ends at its head, and HTTP/1.0 cannot parse chunked
        // coding, so neither framing can delimit a reused connection.
        let (bodyless_head, _) = split_wire(&bodyless_wire);
        assert!(
            bodyless_head.contains("Connection: close"),
            "{bodyless_head}"
        );
        assert!(!bodyless.framing_complete);
        let (legacy_head, _) = split_wire(&legacy_wire);
        assert!(legacy_head.contains("Connection: close"), "{legacy_head}");
        assert!(!legacy.framing_complete);
    }

    #[tokio::test]
    async fn serve_miss_streaming_unreachable_origin_is_502() {
        // Bind then drop a listener so the port is (almost certainly) dead.
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let (_dir, cache) = fresh_cache();
        let host = "bucket.s3.amazonaws.com";
        let key = plain_key(host, "/dead");
        let upstream = format!("http://{addr}");
        let head = get_head(host, "/dead");

        let cache_ref = &cache;
        let key_ref = &key;
        let upstream_ref = upstream.as_str();
        let head_ref = &head;
        let (resp, wire) = drive_streamed(|mut tx| async move {
            let permit = owner_permit(cache_ref, key_ref).await;
            let r = serve_miss_streaming(
                &mut tx,
                cache_ref,
                key_ref,
                upstream_ref,
                head_ref,
                miss_fill_plain(),
                permit,
            )
            .await
            .expect("serve_miss_streaming");
            (r, tx)
        })
        .await;

        assert_eq!(resp.head.status, 502);
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert_eq!(wire_body, BAD_GATEWAY_BODY);
        assert!(
            cache.get(key.as_str()).unwrap().is_none(),
            "an unreachable origin admits nothing"
        );
    }

    #[tokio::test]
    async fn serve_bypass_streams_fixed_length() {
        // serve_bypass has no cache handle, so it structurally cannot admit an
        // entry; this asserts it streams the body through with BYPASS framing.
        let body = b"bypass fixed length body";
        let response = format!(
            "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect::<Vec<u8>>();
        let addr = spawn_raw_origin(response).await;
        let upstream = format!("http://{addr}");
        let head = get_head("bucket.s3.amazonaws.com", "/obj");

        let (resp, wire) = drive_streamed(|mut tx| async move {
            let r = serve_bypass(&mut tx, &upstream, &head)
                .await
                .expect("serve_bypass");
            (r, tx)
        })
        .await;

        assert_eq!(resp.head.status, 200);
        assert!(matches!(resp.body, BodySource::Streamed));
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(
            wire_head.contains("X-Cache-Status: BYPASS"),
            "head: {wire_head}"
        );
        assert!(wire_head.contains(&format!("Content-Length: {}", body.len())));
        assert_eq!(wire_body, body);
    }

    #[tokio::test]
    async fn serve_bypass_streams_chunked_when_length_unknown() {
        let body = b"bypass unknown length body";
        let response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n"
            .iter()
            .copied()
            .chain(body.iter().copied())
            .collect::<Vec<u8>>();
        let addr = spawn_raw_origin(response).await;
        let upstream = format!("http://{addr}");
        let head = get_head("bucket.s3.amazonaws.com", "/nolen");

        let (resp, wire) = drive_streamed(|mut tx| async move {
            let r = serve_bypass(&mut tx, &upstream, &head)
                .await
                .expect("serve_bypass");
            (r, tx)
        })
        .await;

        assert_eq!(resp.head.status, 200);
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(
            wire_head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked"),
            "head: {wire_head}"
        );
        assert!(wire_body.ends_with(b"0\r\n\r\n"));
        assert!(find_subslice(&wire_body, body).is_some());
    }

    #[tokio::test]
    async fn serve_bypass_truncated_chunked_origin_body_omits_the_terminator() {
        // Arrange. The origin ends its chunked body without a terminator.
        let addr = spawn_raw_origin(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n7\r\npartial\r\n"
                .to_vec(),
        )
        .await;
        let upstream = format!("http://{addr}");
        let head = get_head("bucket.s3.amazonaws.com", "/truncated-bypass");

        // Act.
        let (response, wire) = drive_streamed(|mut tx| async move {
            let response = serve_bypass(&mut tx, &upstream, &head)
                .await
                .expect("serve_bypass");
            (response, tx)
        })
        .await;

        // Assert. A terminator would make the truncated body look complete to a
        // client that then reads close_notify.
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(
            wire_head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked"),
            "head: {wire_head}"
        );
        assert!(find_subslice(&wire_body, b"partial").is_some());
        assert!(
            !wire_body.ends_with(b"0\r\n\r\n"),
            "{:?}",
            String::from_utf8_lossy(&wire_body)
        );
        assert_eq!(response.timeout_stage, None);
    }

    #[tokio::test]
    async fn serve_bypass_unreachable_origin_is_502() {
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let upstream = format!("http://{addr}");
        let head = get_head("bucket.s3.amazonaws.com", "/dead");

        let (resp, wire) = drive_streamed(|mut tx| async move {
            let r = serve_bypass(&mut tx, &upstream, &head)
                .await
                .expect("serve_bypass");
            (r, tx)
        })
        .await;

        assert_eq!(resp.head.status, 502);
        let (wire_head, wire_body) = split_wire(&wire);
        assert!(wire_head.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert_eq!(wire_body, BAD_GATEWAY_BODY);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_segment_chain_yields_to_other_runtime_work() {
        use ovcc_storage::Storage as _;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("empty");
        std::fs::write(&source, b"").unwrap();
        let segment = StagedSegment::new("empty", source, 0);
        let segments = vec![segment; 50_000];
        let storage = DiskStorage::open(&temp.path().join("cache")).unwrap();
        let lease = storage
            .adopt_segmented("empty-chain", StoredMeta::default(), &segments)
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut stream = ClientStream::Plain(server);

        let started = Arc::new(AtomicBool::new(false));
        let heartbeats = Arc::new(AtomicUsize::new(0));
        let heartbeat_started = Arc::clone(&started);
        let heartbeat_count = Arc::clone(&heartbeats);
        let heartbeat = tokio::spawn(async move {
            while !heartbeat_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            heartbeat_count.fetch_add(1, Ordering::SeqCst);
        });
        tokio::task::yield_now().await;
        started.store(true, Ordering::SeqCst);

        sendfile_lease(&mut stream, &lease).await.unwrap();
        heartbeat.await.unwrap();
        drop(client);
        assert!(heartbeats.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn general_sendfile_chunk_caps_every_call_at_16_mib() {
        // Arrange.
        let short_remaining = (SENDFILE_CHUNK_BYTES - 1) as u64;
        let long_remaining = (SENDFILE_CHUNK_BYTES as u64) * 4;

        // Act.
        let short_chunk = sendfile_chunk_len(short_remaining);
        let long_chunk = sendfile_chunk_len(long_remaining);

        // Assert.
        assert_eq!(short_chunk, SENDFILE_CHUNK_BYTES - 1);
        assert_eq!(long_chunk, SENDFILE_CHUNK_BYTES);
    }

    #[tokio::test]
    async fn general_sendfile_chunk_yields_after_successful_call()
    -> std::result::Result<(), BoxError> {
        use tokio::io::AsyncReadExt as _;

        // Arrange.
        let source = tempfile::tempfile()?;
        let length = 4 * 1024;
        source.set_len(length)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (server, _) = listener.accept().await?;
        let mut stream = ClientStream::Plain(server);
        let mut cooperations = 0usize;

        // Act.
        let report = sendfile_file_with_cooperation(&mut stream, &source, 0, length, || {
            cooperations += 1;
            std::future::ready(())
        })
        .await
        .map_err(|(_, error)| error)?;
        let mut received = vec![0u8; length as usize];
        client.read_exact(&mut received).await?;

        // Assert.
        assert_eq!(report.body_bytes, length);
        assert_eq!(cooperations, 1);
        assert!(received.iter().all(|byte| *byte == 0));
        Ok(())
    }

    #[tokio::test]
    async fn slow_client_exercises_measured_sendfile_backpressure() {
        use tokio::io::AsyncReadExt as _;

        // Arrange. A sparse file stays file-backed while exceeding the loopback
        // socket buffer, so delaying reads must make sendfile await writability.
        let source = tempfile::tempfile().unwrap();
        let length = 64 * 1024 * 1024;
        source.set_len(length).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut stream = ClientStream::Plain(server);
        let sending =
            tokio::spawn(async move { sendfile_file(&mut stream, &source, 0, length).await });

        // Act.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut received = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        while received < length {
            let read = client.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "body ended before Content-Length");
            received += read as u64;
        }
        let report = sending.await.unwrap().unwrap();

        // Assert.
        assert_eq!(report.body_bytes, length);
        assert_eq!(received, length);
        assert!(
            report.socket_writable_wait >= Duration::from_millis(20),
            "measured wait was {:?}",
            report.socket_writable_wait
        );
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_sendfile_progress_resets_inactivity_timeout() {
        use tokio::io::AsyncReadExt as _;

        let source = tempfile::tempfile().unwrap();
        let length = 512 * 1024;
        let body_byte = 0x5a;
        (&source)
            .write_all(&vec![body_byte; length as usize])
            .unwrap();
        let (stream, mut client) = plain_stream_pair().await;
        limit_socket_send_buffer(&stream);
        socket2::SockRef::from(&client)
            .set_recv_buffer_size(32 * 1024)
            .unwrap();
        saturate_send_queue(&stream);
        let source = ovcc_platform::TransmitSource::new(&source).unwrap();
        let reader = tokio::spawn(async move {
            let mut body_received = 0usize;
            let mut body_started = false;
            let mut buffer = [0u8; 32 * 1024];
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let read = client.read(&mut buffer).await.unwrap();
                if read == 0 {
                    return body_received;
                }
                for byte in &buffer[..read] {
                    if !body_started && *byte == body_byte {
                        body_started = true;
                    }
                    if body_started {
                        assert_eq!(*byte, body_byte, "filler appeared inside the file body");
                        body_received += 1;
                    } else {
                        assert_eq!(*byte, SATURATE_FILLER);
                    }
                }
            }
        });

        let started = Instant::now();
        let outcome = send_file_chunk_with_timeout(
            stream.tcp().expect("test TCP stream"),
            &source,
            0,
            length as usize,
            Duration::from_millis(300),
        )
        .await;
        drop(stream);

        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.sent, length as usize);
        assert!(started.elapsed() > Duration::from_millis(300));
        assert_eq!(reader.await.unwrap(), length as usize);
    }

    #[tokio::test(start_paused = true)]
    async fn client_write_timeout_reclaims_buffered_response() {
        // Arrange.
        let (mut stream, client) = plain_stream_pair().await;
        limit_socket_send_buffer(&stream);
        #[cfg(windows)]
        saturate_send_queue(&stream);
        let response =
            Response::buffered(200, "OK", Some("MISS"), Vec::new(), vec![b'x'; 1024 * 1024]);

        // Act.
        let (result, timed_out) = DOWNSTREAM_TIMED_OUT
            .scope(std::cell::Cell::new(false), async {
                let result = drive_response(&mut stream, response, false).await;
                (result, downstream_timed_out())
            })
            .await;
        stream.reset_on_drop();
        drop(stream);
        drop(client);

        // Assert.
        let (_report, error) = result.expect_err("a blocked buffered write must time out");
        assert!(is_downstream_timeout(&error));
        assert!(timed_out);
    }

    /// Answers one request with a prepared response, so a test drives the
    /// telemetry layer's write path without routing.
    struct PreparedRouter(Option<Response>);

    impl Service<ProxyRequest> for PreparedRouter {
        type Response = ServeOutcome;
        type Error = BoxError;
        type Future = std::future::Ready<std::result::Result<ServeOutcome, BoxError>>;

        fn poll_ready(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ProxyRequest) -> Self::Future {
            std::future::ready(Ok(ServeOutcome {
                stream: request.stream,
                response: self.0.take().expect("one prepared response"),
                access: ovcc_telemetry::access::AccessLog::default(),
                next: ConnectionNext::KeepAlive,
            }))
        }
    }

    fn bare_connection_context() -> ConnCtx {
        ConnCtx {
            backends: Arc::new(Vec::new()),
            passthrough: Passthrough::default(),
            self_names: Arc::new(Vec::new()),
            generation: Arc::new(AtomicI64::new(0)),
            reset_compat: Arc::new(CompatConfig::default()),
            instance: Arc::new("close-test".to_owned()),
            presigned_auth: Arc::new(AuthorizationCache::new(1)),
            local_mode: false,
            peer: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn client_write_timeout_in_the_driver_resets_through_the_connection_loop() {
        // Arrange.
        let (stream, client) = plain_stream_pair().await;
        limit_socket_send_buffer(&stream);
        #[cfg(windows)]
        saturate_send_queue(&stream);
        let response =
            Response::buffered(200, "OK", Some("HIT"), Vec::new(), vec![b'x'; 1024 * 1024]);
        let mut service = TelemetryService {
            inner: PreparedRouter(Some(response)),
        };
        let request = ProxyRequest {
            stream,
            head: get_head("bucket.example.com", "/stalled-hit"),
            body_prefix: Vec::new(),
            head_delimited: true,
            host: "bucket.example.com".to_owned(),
            remote_addr: "127.0.0.1".to_owned(),
            scheme: "http",
            ssl_protocol: String::new(),
            connection: RequestConnection { id: 1, requests: 1 },
            ctx: bare_connection_context(),
            listener_kind: ListenerKind::Cache,
            peer_forwarded: false,
        };

        // Act.
        let outcome = service.call(request).await;
        drop(client);

        // Assert. The client allowed reuse, yet the stalled write ends the
        // connection with a reset that the connection loop records.
        let (_stream, next) = outcome.expect("a write timeout is a close outcome");
        assert_eq!(next, ConnectionNext::Close(CloseMode::Reset));
    }

    #[tokio::test(start_paused = true)]
    async fn client_write_timeout_reclaims_sendfile_hit() {
        // Arrange.
        let source = tempfile::tempfile().unwrap();
        let length = 64 * 1024 * 1024;
        source.set_len(length).unwrap();
        let (mut stream, client) = plain_stream_pair().await;
        limit_socket_send_buffer(&stream);

        // Act.
        let (result, timed_out) = DOWNSTREAM_TIMED_OUT
            .scope(std::cell::Cell::new(false), async {
                let result = sendfile_file(&mut stream, &source, 0, length).await;
                (result, downstream_timed_out())
            })
            .await;
        stream.reset_on_drop();
        drop(stream);
        drop(client);

        // Assert.
        let (report, error) = result.expect_err("a blocked sendfile must time out");
        assert!(is_downstream_timeout(&error));
        assert!(timed_out);
        assert!(report.body_bytes < length);
    }

    #[tokio::test]
    async fn sendfile_error_reports_bytes_sent_before_disconnect() {
        use tokio::io::AsyncReadExt as _;

        let source = tempfile::tempfile().unwrap();
        let length = 64 * 1024 * 1024;
        source.set_len(length).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut stream = ClientStream::Plain(server);

        let sending =
            tokio::spawn(async move { sendfile_file(&mut stream, &source, 0, length).await });
        let mut first_byte = [0u8; 1];
        client.read_exact(&mut first_byte).await.unwrap();
        drop(client);

        let (report, _) = tokio::time::timeout(Duration::from_secs(5), sending)
            .await
            .expect("sendfile must observe the disconnected peer")
            .unwrap()
            .expect_err("a disconnected peer cannot receive the complete file");
        assert!(
            report.body_bytes > 0,
            "the byte read by the peer must be accounted"
        );
        assert!(
            report.body_bytes < length,
            "the complete sparse file was not sent"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn segment_open_runs_off_the_tokio_runtime_thread() {
        use ovcc_storage::Storage as _;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("fragment");
        std::fs::write(&source, b"x").unwrap();
        let storage = DiskStorage::open(&temp.path().join("cache")).unwrap();
        let lease = storage
            .adopt_segmented(
                "open-thread",
                StoredMeta::default(),
                &[StagedSegment::new("fragment", source, 1)],
            )
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let mut stream = ClientStream::Plain(server);
        let runtime_thread = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::channel();
        *SEGMENT_OPEN_THREAD_REPORTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(sender);

        sendfile_lease(&mut stream, &lease).await.unwrap();
        *SEGMENT_OPEN_THREAD_REPORTER.get().unwrap().lock().unwrap() = None;
        let open_thread = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let mut body = [0u8; 1];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut body)
            .await
            .unwrap();
        assert_ne!(open_thread, runtime_thread);
        assert_eq!(&body, b"x");
    }

    #[test]
    fn failed_blocklist_adoption_retains_fragments_for_successful_retry() {
        use std::io::{Read as _, Write as _};

        let temp = tempfile::tempdir().unwrap();
        let staging = BlockStaging::for_blob(
            temp.path(),
            "account.blob.core.windows.net",
            "/blob",
            Duration::MAX,
        );
        let cache = Cache::new(DiskStorage::open(&temp.path().join("cache")).unwrap());
        let b1 = BlockId::new("b1");
        let b2 = BlockId::new("b2");

        let mut first = staging.begin(&b1).unwrap();
        first.write_all(b"first").unwrap();
        drop(first);
        staging.finish(&b1, true, true).unwrap();
        assert!(
            adopt_blocklist_cache(
                &cache,
                "model://retry",
                StoredMeta::default(),
                &staging,
                &[b1.clone(), b2.clone()],
            )
            .is_err()
        );
        assert!(staging.contains(&b1), "failed plan must retain b1");

        let mut second = staging.begin(&b2).unwrap();
        second.write_all(b"second").unwrap();
        drop(second);
        staging.finish(&b2, true, true).unwrap();
        adopt_blocklist_cache(
            &cache,
            "model://retry",
            StoredMeta::default(),
            &staging,
            &[b1, b2],
        )
        .unwrap();

        let lease = cache.get("model://retry").unwrap().unwrap().0;
        let mut actual = Vec::new();
        for descriptor in lease.segments().unwrap() {
            lease
                .open_segment(&descriptor)
                .unwrap()
                .read_to_end(&mut actual)
                .unwrap();
        }
        assert_eq!(actual, b"firstsecond");
    }

    #[test]
    fn failed_blocklist_purge_error_still_leaves_prior_key_cold() {
        use ovcc_storage::{Lease, Lookup, RecoveryReport, Storage, StoreWrite};
        use std::io::Write as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct FailClosedStorage {
            stale_path: PathBuf,
            serveable: Arc<AtomicBool>,
        }

        impl Storage for FailClosedStorage {
            fn lookup(&self, _key: &str) -> Result<Lookup> {
                if !self.serveable.load(Ordering::SeqCst) {
                    return Ok(Lookup::Miss);
                }
                Ok(Lookup::Hit(Lease {
                    path: self.stale_path.clone(),
                    meta: StoredMeta {
                        size: 5,
                        ..StoredMeta::default()
                    },
                    file: std::fs::File::open(&self.stale_path)?,
                    body_offset: 0,
                }))
            }

            fn peek(&self, _key: &str) -> Result<Option<StoredMeta>> {
                Ok(None)
            }

            fn begin_fill(&self, _key: &str, _meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
                Err(Error::storage("unused"))
            }

            fn adopt_segmented_with_admission(
                &self,
                _key: &str,
                _meta: StoredMeta,
                _segments: &[StagedSegment],
                admit: &mut dyn FnMut() -> Result<()>,
            ) -> Result<Lease> {
                admit()?;
                Err(Error::storage("injected adoption failure"))
            }

            fn evict_data(&self, _key: &str, _revision: u64) -> Result<bool> {
                Ok(false)
            }

            fn remove(&self, _key: &str) -> Result<()> {
                self.serveable.store(false, Ordering::SeqCst);
                Err(Error::storage("injected physical cleanup failure"))
            }

            fn recover_on_open(&self) -> Result<RecoveryReport> {
                Ok(RecoveryReport::default())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let stale_path = temp.path().join("stale");
        std::fs::write(&stale_path, b"stale").unwrap();
        let serveable = Arc::new(AtomicBool::new(true));
        let cache = Cache::new(FailClosedStorage {
            stale_path,
            serveable: Arc::clone(&serveable),
        });
        assert!(cache.get("model://cold").unwrap().is_some());

        let staging = BlockStaging::for_blob(
            temp.path(),
            "account.blob.core.windows.net",
            "/cold",
            Duration::MAX,
        );
        let block = BlockId::new("block");
        let mut file = staging.begin(&block).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);
        staging.finish(&block, true, true).unwrap();

        let error = adopt_blocklist_cache(
            &cache,
            "model://cold",
            StoredMeta::default(),
            &staging,
            &[block],
        )
        .unwrap_err();
        assert!(error.to_string().contains("stale-key purge failed"));
        assert!(!serveable.load(Ordering::SeqCst));
        assert!(
            cache.get("model://cold").unwrap().is_none(),
            "origin-accepted replacement must remain cold despite cleanup error"
        );
    }
}
