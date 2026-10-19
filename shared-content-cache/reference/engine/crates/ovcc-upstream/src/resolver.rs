// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(all(windows, test))]
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyper_util::client::legacy::connect::dns::Name;
use tokio::net::UdpSocket;
#[cfg(windows)]
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tower_service::Service;

const MAX_CACHE_ENTRIES: usize = 4096;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Cache lifetimes, per-query limits, and nameserver selection for the origin resolver.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolverOptions {
    pub success_ttl: Duration,
    pub failure_ttl: Duration,
    pub query_timeout: Duration,
    pub retries: usize,
    pub random_nameserver_order: bool,
    pub fallback_nameservers: Vec<String>,
    pub nameserver_override: Option<String>,
}

impl Default for ResolverOptions {
    fn default() -> Self {
        Self {
            success_ttl: Duration::from_secs(30),
            failure_ttl: Duration::from_secs(10),
            query_timeout: Duration::from_secs(20),
            retries: 3,
            random_nameserver_order: true,
            fallback_nameservers: Vec::new(),
            nameserver_override: None,
        }
    }
}

#[derive(Clone)]
pub struct CachedResolver {
    inner: Arc<ResolverInner>,
}

struct ResolverInner {
    query: ResolverQuery,
    cache: Mutex<HashMap<String, CacheEntry>>,
    success_ttl: Duration,
    failure_ttl: Duration,
    round_robin: AtomicU64,
}

struct CacheEntry {
    expires_at: Instant,
    result: io::Result<Vec<Ipv4Addr>>,
}

tokio::task_local! {
    static NAMESERVER_OVERRIDE: Option<String>;
}

pub(crate) fn nameserver_override() -> Option<String> {
    NAMESERVER_OVERRIDE.try_with(Clone::clone).ok().flatten()
}

pub fn with_nameserver_override<T>(
    nameserver: Option<String>,
    future: impl Future<Output = T>,
) -> impl Future<Output = T> {
    NAMESERVER_OVERRIDE.scope(nameserver, future)
}

pub(crate) fn validate_options(options: &ResolverOptions) -> io::Result<()> {
    #[cfg(windows)]
    if options.nameserver_override.is_none() {
        return Ok(());
    }
    system_nameservers(options).map(|_| ())
}

impl CachedResolver {
    pub fn new(options: ResolverOptions) -> io::Result<Self> {
        #[cfg(windows)]
        if options.nameserver_override.is_none() {
            return Ok(Self::with_query(
                &options,
                ResolverQuery::System {
                    timeout: options.query_timeout,
                    retries: options.retries,
                    lookup: native_lookup(),
                    #[cfg(test)]
                    observer: None,
                },
            ));
        }
        let nameservers = system_nameservers(&options)?;
        Self::with_exchange(options, nameservers, Arc::new(UdpExchange::default()))
    }

    fn with_query(options: &ResolverOptions, query: ResolverQuery) -> Self {
        Self {
            inner: Arc::new(ResolverInner {
                query,
                cache: Mutex::new(HashMap::new()),
                success_ttl: options.success_ttl,
                failure_ttl: options.failure_ttl,
                round_robin: AtomicU64::new(0),
            }),
        }
    }

    fn with_exchange(
        options: ResolverOptions,
        nameservers: Vec<String>,
        exchange: Arc<dyn DnsExchange>,
    ) -> io::Result<Self> {
        if nameservers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no DNS nameservers configured",
            ));
        }
        Ok(Self::with_query(
            &options,
            ResolverQuery::Explicit(QueryEngine::new(
                nameservers,
                options.query_timeout,
                options.retries,
                options.random_nameserver_order,
                exchange,
            )),
        ))
    }

    async fn resolve_at(&self, host: &str, now: Instant) -> io::Result<Vec<SocketAddr>> {
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            return Ok(vec![SocketAddr::new(IpAddr::V4(ip), 0)]);
        }

        if let Some(result) = self.cached(host, now) {
            return result.map(|addresses| self.rotate(addresses));
        }

        let result = self.inner.query.lookup(host).await;
        let ttl = if result.is_ok() {
            self.inner.success_ttl
        } else {
            self.inner.failure_ttl
        };
        self.store(host, now + ttl, &result);
        result.map(|addresses| self.rotate(addresses))
    }

    fn cached(&self, host: &str, now: Instant) -> Option<io::Result<Vec<Ipv4Addr>>> {
        let mut cache = self.inner.cache.lock().ok()?;
        match cache.get(host) {
            Some(entry) if entry.expires_at > now => Some(clone_result(&entry.result)),
            Some(_) => {
                cache.remove(host);
                None
            }
            None => None,
        }
    }

    fn store(&self, host: &str, expires_at: Instant, result: &io::Result<Vec<Ipv4Addr>>) {
        if let Ok(mut cache) = self.inner.cache.lock() {
            if cache.len() >= MAX_CACHE_ENTRIES
                && !cache.contains_key(host)
                && let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires_at)
                    .map(|(name, _)| name.clone())
            {
                cache.remove(&oldest);
            }
            cache.insert(
                host.to_owned(),
                CacheEntry {
                    expires_at,
                    result: clone_result(result),
                },
            );
        }
    }

    fn rotate(&self, addresses: Vec<Ipv4Addr>) -> Vec<SocketAddr> {
        let len = addresses.len();
        let start = self.inner.round_robin.fetch_add(1, Ordering::Relaxed) as usize % len;
        addresses
            .into_iter()
            .cycle()
            .skip(start)
            .take(len)
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), 0))
            .collect()
    }
}

fn system_nameservers(options: &ResolverOptions) -> io::Result<Vec<String>> {
    #[cfg(target_os = "linux")]
    {
        configured_nameservers(options, ovcc_platform::system_resolver_config)
    }
    #[cfg(windows)]
    {
        configured_nameservers(options, || {
            unreachable!("Windows uses its native DNS client")
        })
    }
}

impl Service<Name> for CachedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = BoxFuture<io::Result<Self::Response>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let resolver = self.clone();
        Box::pin(async move {
            resolver
                .resolve_at(name.as_str(), Instant::now())
                .await
                .map(Vec::into_iter)
        })
    }
}

fn clone_result<T: Clone>(result: &io::Result<T>) -> io::Result<T> {
    result
        .as_ref()
        .cloned()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

fn configured_nameservers(
    options: &ResolverOptions,
    read_resolv_conf: impl FnOnce() -> io::Result<String>,
) -> io::Result<Vec<String>> {
    if let Some(override_nameserver) = options.nameserver_override.as_ref() {
        if override_nameserver.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "DNS nameserver override must not be empty",
            ));
        }
        return Ok(vec![override_nameserver.clone()]);
    }

    match read_resolv_conf() {
        Ok(contents) => {
            let nameservers = contents
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace();
                    (fields.next() == Some("nameserver"))
                        .then(|| fields.next().map(str::to_owned))
                        .flatten()
                })
                .collect::<Vec<_>>();
            if !nameservers.is_empty() {
                return Ok(nameservers);
            }
            explicit_fallback_or_error(
                &options.fallback_nameservers,
                io::ErrorKind::NotFound,
                &format!(
                    "no DNS nameservers found in {}",
                    ovcc_platform::SYSTEM_RESOLVER_SOURCE
                ),
            )
        }
        Err(error) => explicit_fallback_or_error(
            &options.fallback_nameservers,
            error.kind(),
            &format!(
                "failed to read {}: {error}",
                ovcc_platform::SYSTEM_RESOLVER_SOURCE
            ),
        ),
    }
}

fn explicit_fallback_or_error(
    fallback_nameservers: &[String],
    error_kind: io::ErrorKind,
    reason: &str,
) -> io::Result<Vec<String>> {
    if fallback_nameservers.is_empty() {
        return Err(io::Error::new(
            error_kind,
            format!("{reason}; no explicit fallback DNS nameservers configured"),
        ));
    }
    Ok(fallback_nameservers.to_vec())
}

struct QueryEngine {
    nameservers: Vec<String>,
    timeout: Duration,
    retries: usize,
    random_order: bool,
    sequence: AtomicU64,
    exchange: Arc<dyn DnsExchange>,
}

enum ResolverQuery {
    Explicit(QueryEngine),
    #[cfg(windows)]
    System {
        timeout: Duration,
        retries: usize,
        lookup: Arc<dyn NativeLookup>,
        #[cfg(test)]
        observer: Option<Arc<NativeDnsModelObserver>>,
    },
}

#[cfg(all(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeDnsModelState {
    caller_state: &'static str,
    attempts: usize,
    current_attempt: bool,
    active_workers: usize,
    last_failure: &'static str,
}

#[cfg(all(windows, test))]
struct NativeDnsModelEvent {
    action: &'static str,
    state: NativeDnsModelState,
    resume: tokio::sync::oneshot::Sender<()>,
}

#[cfg(all(windows, test))]
struct NativeDnsModelObserver {
    events: tokio::sync::mpsc::UnboundedSender<NativeDnsModelEvent>,
    caller_state: AtomicU8,
    attempts: AtomicUsize,
    next_attempt: AtomicUsize,
    last_started_attempt: AtomicUsize,
    current_attempt: AtomicBool,
    active_workers: AtomicUsize,
    last_failure: AtomicU8,
}

#[cfg(all(windows, test))]
impl NativeDnsModelObserver {
    fn new(events: tokio::sync::mpsc::UnboundedSender<NativeDnsModelEvent>) -> Arc<Self> {
        Arc::new(Self {
            events,
            caller_state: AtomicU8::new(0),
            attempts: AtomicUsize::new(0),
            next_attempt: AtomicUsize::new(0),
            last_started_attempt: AtomicUsize::new(0),
            current_attempt: AtomicBool::new(false),
            active_workers: AtomicUsize::new(0),
            last_failure: AtomicU8::new(0),
        })
    }

    fn state(&self) -> NativeDnsModelState {
        NativeDnsModelState {
            caller_state: match self.caller_state.load(Ordering::Relaxed) {
                0 => "Idle",
                1 => "Waiting",
                2 => "Attempting",
                3 => "Succeeded",
                4 => "Failed",
                5 => "TimedOut",
                state => panic!("unknown native DNS caller state {state}"),
            },
            attempts: self.attempts.load(Ordering::Relaxed),
            current_attempt: self.current_attempt.load(Ordering::Relaxed),
            active_workers: self.active_workers.load(Ordering::Relaxed),
            last_failure: match self.last_failure.load(Ordering::Relaxed) {
                0 => "none",
                1 => "lookup",
                2 => "timeout",
                failure => panic!("unknown native DNS failure state {failure}"),
            },
        }
    }

    async fn emit(&self, action: &'static str) {
        let (resume, paused) = tokio::sync::oneshot::channel();
        self.events
            .send(NativeDnsModelEvent {
                action,
                state: self.state(),
                resume,
            })
            .unwrap();
        let _ = paused.await;
    }

    async fn begin(&self) {
        self.caller_state.store(1, Ordering::Relaxed);
        self.emit("BeginLookup").await;
    }

    fn prepare_attempt(&self, attempt: usize) {
        self.next_attempt.store(attempt, Ordering::Relaxed);
    }

    async fn start_attempt(&self) {
        let attempt = self.next_attempt.load(Ordering::Relaxed);
        self.attempts.store(attempt, Ordering::Relaxed);
        self.last_started_attempt.store(attempt, Ordering::Relaxed);
        self.current_attempt.store(true, Ordering::Relaxed);
        self.caller_state.store(2, Ordering::Relaxed);
        self.last_failure.store(0, Ordering::Relaxed);
        self.emit("StartAttempt").await;
    }

    async fn succeed(&self) {
        self.current_attempt.store(false, Ordering::Relaxed);
        self.caller_state.store(3, Ordering::Relaxed);
        self.last_failure.store(0, Ordering::Relaxed);
        self.emit("AttemptSucceeds").await;
    }

    async fn fail(&self, final_attempt: bool) {
        self.current_attempt.store(false, Ordering::Relaxed);
        self.caller_state
            .store(if final_attempt { 4 } else { 1 }, Ordering::Relaxed);
        self.last_failure.store(1, Ordering::Relaxed);
        self.emit(if final_attempt {
            "AttemptFailsFinal"
        } else {
            "AttemptFailsRetry"
        })
        .await;
    }

    async fn time_out(&self, attempt: usize, final_attempt: bool) {
        let started = self.last_started_attempt.load(Ordering::Relaxed) == attempt;
        self.current_attempt.store(false, Ordering::Relaxed);
        self.caller_state
            .store(if final_attempt { 5 } else { 1 }, Ordering::Relaxed);
        self.last_failure.store(2, Ordering::Relaxed);
        if started {
            self.emit(if final_attempt {
                "AttemptTimesOutFinal"
            } else {
                "AttemptTimesOutRetry"
            })
            .await;
        } else {
            self.attempts.store(attempt, Ordering::Relaxed);
            self.emit("PermitWaitTimesOut").await;
        }
    }

    async fn detached_worker_completed(&self) {
        self.emit("DetachedWorkerCompletes").await;
    }
}

#[cfg(windows)]
trait NativeLookup: Send + Sync {
    fn lookup(&self, host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>>;
}

#[cfg(windows)]
trait NativeLookupOperation: Send + Sync {
    fn run(&self, host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>>;
}

#[cfg(windows)]
struct TokioNativeLookup;

#[cfg(windows)]
impl NativeLookupOperation for TokioNativeLookup {
    fn run(&self, host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map(Iterator::collect)
        })
    }
}

#[cfg(windows)]
struct BoundedNativeLookup {
    permits: Arc<Semaphore>,
    operation: Arc<dyn NativeLookupOperation>,
}

#[cfg(windows)]
impl BoundedNativeLookup {
    fn new(limit: usize, operation: Arc<dyn NativeLookupOperation>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            operation,
        }
    }
}

#[cfg(windows)]
impl NativeLookup for BoundedNativeLookup {
    fn lookup(&self, host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
        let permits = self.permits.clone();
        let operation = self.operation.clone();
        Box::pin(async move {
            let permit = permits
                .acquire_owned()
                .await
                .map_err(|_| io::Error::other("Windows native DNS worker limit is unavailable"))?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _permit = permit;
                let result = operation.run(host).await;
                let _ = sender.send(result);
            });
            receiver
                .await
                .map_err(|_| io::Error::other("Windows native DNS worker exited unexpectedly"))?
        })
    }
}

#[cfg(windows)]
fn native_lookup() -> Arc<dyn NativeLookup> {
    const MAX_NATIVE_LOOKUPS: usize = 32;
    static LOOKUP: OnceLock<Arc<BoundedNativeLookup>> = OnceLock::new();
    LOOKUP
        .get_or_init(|| {
            Arc::new(BoundedNativeLookup::new(
                MAX_NATIVE_LOOKUPS,
                Arc::new(TokioNativeLookup),
            ))
        })
        .clone()
}

impl ResolverQuery {
    async fn lookup(&self, host: &str) -> io::Result<Vec<Ipv4Addr>> {
        match self {
            Self::Explicit(query) => query.lookup(host).await,
            #[cfg(windows)]
            Self::System {
                timeout: budget,
                retries,
                lookup,
                #[cfg(test)]
                observer,
            } => {
                let mut last_error = None;
                #[cfg(test)]
                if let Some(observer) = observer {
                    observer.begin().await;
                }
                for attempt in 0..=*retries {
                    #[cfg(not(test))]
                    let _ = attempt;
                    #[cfg(test)]
                    let attempt_number = attempt + 1;
                    #[cfg(test)]
                    let final_attempt = attempt == *retries;
                    #[cfg(test)]
                    if let Some(observer) = observer {
                        observer.prepare_attempt(attempt_number);
                    }
                    let addresses = match timeout(*budget, lookup.lookup(host.to_owned())).await {
                        Ok(Ok(addresses)) => addresses,
                        Ok(Err(error)) => {
                            last_error = Some(error);
                            #[cfg(test)]
                            if let Some(observer) = observer {
                                observer.fail(final_attempt).await;
                            }
                            continue;
                        }
                        Err(_) => {
                            last_error = Some(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!("DNS query for {host} timed out"),
                            ));
                            #[cfg(test)]
                            if let Some(observer) = observer {
                                observer.time_out(attempt_number, final_attempt).await;
                            }
                            continue;
                        }
                    };
                    let mut ipv4 = Vec::new();
                    for address in addresses {
                        if let IpAddr::V4(address) = address.ip()
                            && !ipv4.contains(&address)
                        {
                            ipv4.push(address);
                        }
                    }
                    if !ipv4.is_empty() {
                        #[cfg(test)]
                        if let Some(observer) = observer {
                            observer.succeed().await;
                        }
                        return Ok(ipv4);
                    }
                    last_error = Some(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no A records for {host}"),
                    ));
                    #[cfg(test)]
                    if let Some(observer) = observer {
                        observer.fail(final_attempt).await;
                    }
                }
                Err(last_error.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("DNS lookup failed for {host}"),
                    )
                }))
            }
        }
    }
}

impl QueryEngine {
    fn new(
        nameservers: Vec<String>,
        timeout: Duration,
        retries: usize,
        random_order: bool,
        exchange: Arc<dyn DnsExchange>,
    ) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(1, |duration| duration.as_nanos() as u64 | 1);
        Self {
            nameservers,
            timeout,
            retries,
            random_order,
            sequence: AtomicU64::new(seed),
            exchange,
        }
    }

    async fn lookup(&self, host: &str) -> io::Result<Vec<Ipv4Addr>> {
        if self.nameservers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no DNS nameservers configured",
            ));
        }
        let start = if self.random_order {
            next_random(&self.sequence) as usize % self.nameservers.len()
        } else {
            0
        };
        let mut last_error = None;
        for attempt in 0..=self.retries {
            let nameserver = &self.nameservers[(start + attempt) % self.nameservers.len()];
            let endpoint = resolve_nameserver(nameserver).await?;
            match timeout(self.timeout, self.exchange.query(endpoint, host)).await {
                Ok(Ok(addresses)) if !addresses.is_empty() => return Ok(addresses),
                Ok(Ok(_)) => {
                    last_error = Some(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no A records for {host}"),
                    ));
                }
                Ok(Err(error)) => last_error = Some(error),
                Err(_) => {
                    last_error = Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("DNS query for {host} timed out"),
                    ));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("DNS lookup failed for {host}"),
            )
        }))
    }
}

fn next_random(state: &AtomicU64) -> u64 {
    let mut current = state.load(Ordering::Relaxed);
    loop {
        let mut next = current;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        match state.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

async fn resolve_nameserver(value: &str) -> io::Result<SocketAddr> {
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Ok(address);
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 53));
    }
    let endpoint = if value.rsplit_once(':').is_some() {
        value.to_owned()
    } else {
        format!("{value}:53")
    };
    tokio::net::lookup_host(endpoint)
        .await?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "nameserver has no address"))
}

trait DnsExchange: Send + Sync {
    fn query(&self, nameserver: SocketAddr, host: &str) -> BoxFuture<io::Result<Vec<Ipv4Addr>>>;
}

#[derive(Default)]
struct UdpExchange {
    query_id: AtomicU16,
}

impl DnsExchange for UdpExchange {
    fn query(&self, nameserver: SocketAddr, host: &str) -> BoxFuture<io::Result<Vec<Ipv4Addr>>> {
        let query_id = self.query_id.fetch_add(1, Ordering::Relaxed);
        let host = host.to_owned();
        Box::pin(async move {
            let bind = if nameserver.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let socket = UdpSocket::bind(bind).await?;
            socket.connect(nameserver).await?;
            let query = encode_query(query_id, &host)?;
            socket.send(&query).await?;
            let mut response = [0u8; 4096];
            let length = socket.recv(&mut response).await?;
            decode_response(query_id, &response[..length])
        })
    }
}

fn encode_query(id: u16, host: &str) -> io::Result<Vec<u8>> {
    let mut packet = Vec::with_capacity(host.len() + 18);
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid DNS name",
            ));
        }
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    // Upstream origin resolution is currently IPv4-only, so request A records.
    packet.extend_from_slice(&1u16.to_be_bytes());
    packet.extend_from_slice(&1u16.to_be_bytes());
    Ok(packet)
}

fn decode_response(id: u16, packet: &[u8]) -> io::Result<Vec<Ipv4Addr>> {
    if packet.len() < 12 || u16::from_be_bytes([packet[0], packet[1]]) != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid DNS response",
        ));
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x000f != 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "DNS response error",
        ));
    }
    let questions = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let answers = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let authorities = u16::from_be_bytes([packet[8], packet[9]]) as usize;
    let additionals = u16::from_be_bytes([packet[10], packet[11]]) as usize;
    let records = answers + authorities + additionals;
    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(packet, offset)?;
        offset = offset
            .checked_add(4)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS question"))?;
    }
    let mut addresses = Vec::new();
    for _ in 0..records {
        offset = skip_name(packet, offset)?;
        if offset + 10 > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated DNS resource record",
            ));
        }
        let record_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let class = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]);
        let length = u16::from_be_bytes([packet[offset + 8], packet[offset + 9]]) as usize;
        offset += 10;
        if offset + length > packet.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated DNS record",
            ));
        }
        if record_type == 1 && class == 1 && length == 4 {
            addresses.push(Ipv4Addr::new(
                packet[offset],
                packet[offset + 1],
                packet[offset + 2],
                packet[offset + 3],
            ));
        }
        offset += length;
    }
    Ok(addresses)
}

fn skip_name(packet: &[u8], mut offset: usize) -> io::Result<usize> {
    loop {
        let length = *packet
            .get(offset)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS name"))?;
        if length & 0xc0 == 0xc0 {
            return offset
                .checked_add(2)
                .filter(|end| *end <= packet.len())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "truncated DNS pointer")
                });
        }
        offset += 1;
        if length == 0 {
            return Ok(offset);
        }
        offset = offset
            .checked_add(length as usize)
            .filter(|end| *end <= packet.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated DNS label"))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    struct FakeExchange {
        responses: Mutex<VecDeque<io::Result<Vec<Ipv4Addr>>>>,
        calls: AtomicUsize,
        nameservers: Mutex<Vec<SocketAddr>>,
    }

    impl FakeExchange {
        fn new(responses: Vec<io::Result<Vec<Ipv4Addr>>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
                nameservers: Mutex::new(Vec::new()),
            }
        }
    }

    impl DnsExchange for FakeExchange {
        fn query(
            &self,
            nameserver: SocketAddr,
            _host: &str,
        ) -> BoxFuture<io::Result<Vec<Ipv4Addr>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.nameservers.lock().unwrap().push(nameserver);
            let result = self.responses.lock().unwrap().pop_front().unwrap();
            Box::pin(async move { result })
        }
    }

    #[cfg(windows)]
    enum NativeResponse {
        Ready(io::Result<Vec<SocketAddr>>),
        Pending,
    }

    #[cfg(windows)]
    struct FakeNativeLookup {
        responses: Mutex<VecDeque<NativeResponse>>,
        calls: AtomicUsize,
    }

    #[cfg(windows)]
    impl FakeNativeLookup {
        fn new(responses: Vec<NativeResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[cfg(windows)]
    impl NativeLookup for FakeNativeLookup {
        fn lookup(&self, _host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.responses.lock().unwrap().pop_front().unwrap() {
                NativeResponse::Ready(result) => Box::pin(async move { result }),
                NativeResponse::Pending => Box::pin(std::future::pending()),
            }
        }
    }

    #[cfg(windows)]
    struct BlockingNativeOperation {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
    }

    #[cfg(windows)]
    impl NativeLookupOperation for BlockingNativeOperation {
        fn run(&self, _host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
            let calls = self.calls.clone();
            let active = self.active.clone();
            let release = self.release.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                active.fetch_add(1, Ordering::Relaxed);
                let permit = release.acquire().await.unwrap();
                permit.forget();
                active.fetch_sub(1, Ordering::Relaxed);
                Ok(vec!["127.0.0.1:0".parse().unwrap()])
            })
        }
    }

    #[cfg(windows)]
    #[derive(Debug)]
    enum ScriptedNativeCommand {
        Complete(io::Result<Vec<SocketAddr>>),
        ReleaseDetached,
        ReleaseSilently,
    }

    #[cfg(windows)]
    struct ScriptedNativeOperation {
        observer: Arc<NativeDnsModelObserver>,
        commands:
            tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<ScriptedNativeCommand>>,
    }

    #[cfg(windows)]
    impl NativeLookupOperation for ScriptedNativeOperation {
        fn run(&self, _host: String) -> BoxFuture<io::Result<Vec<SocketAddr>>> {
            let observer = self.observer.clone();
            let commands = self.commands.clone();
            Box::pin(async move {
                observer.active_workers.fetch_add(1, Ordering::Relaxed);
                let (command, receive) = tokio::sync::oneshot::channel();
                commands.send(command).unwrap();
                observer.start_attempt().await;
                match receive.await.unwrap() {
                    ScriptedNativeCommand::Complete(result) => {
                        observer.active_workers.fetch_sub(1, Ordering::Relaxed);
                        result
                    }
                    ScriptedNativeCommand::ReleaseDetached => {
                        observer.active_workers.fetch_sub(1, Ordering::Relaxed);
                        observer.detached_worker_completed().await;
                        Ok(vec!["127.0.0.1:0".parse().unwrap()])
                    }
                    ScriptedNativeCommand::ReleaseSilently => {
                        observer.active_workers.fetch_sub(1, Ordering::Relaxed);
                        Ok(vec!["127.0.0.1:0".parse().unwrap()])
                    }
                }
            })
        }
    }

    fn options() -> ResolverOptions {
        ResolverOptions {
            success_ttl: Duration::from_secs(30),
            failure_ttl: Duration::from_secs(10),
            query_timeout: Duration::from_secs(1),
            retries: 0,
            random_nameserver_order: false,
            fallback_nameservers: Vec::new(),
            nameserver_override: None,
        }
    }

    #[tokio::test]
    async fn successful_lookup_is_cached_until_configured_ttl() {
        let exchange = Arc::new(FakeExchange::new(vec![
            Ok(vec![Ipv4Addr::new(10, 0, 0, 1)]),
            Ok(vec![Ipv4Addr::new(10, 0, 0, 2)]),
        ]));
        let resolver = CachedResolver::with_exchange(
            options(),
            vec!["127.0.0.1".to_owned()],
            exchange.clone(),
        )
        .unwrap();
        let now = Instant::now();

        resolver.resolve_at("origin.test", now).await.unwrap();
        resolver
            .resolve_at("origin.test", now + Duration::from_secs(29))
            .await
            .unwrap();
        resolver
            .resolve_at("origin.test", now + Duration::from_secs(30))
            .await
            .unwrap();

        assert_eq!(exchange.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn failed_lookup_is_cached_until_failure_ttl() {
        let exchange = Arc::new(FakeExchange::new(vec![
            Err(io::Error::new(io::ErrorKind::TimedOut, "failed")),
            Ok(vec![Ipv4Addr::new(10, 0, 0, 1)]),
        ]));
        let resolver = CachedResolver::with_exchange(
            options(),
            vec!["127.0.0.1".to_owned()],
            exchange.clone(),
        )
        .unwrap();
        let now = Instant::now();

        assert!(resolver.resolve_at("origin.test", now).await.is_err());
        assert!(
            resolver
                .resolve_at("origin.test", now + Duration::from_secs(9))
                .await
                .is_err()
        );
        resolver
            .resolve_at("origin.test", now + Duration::from_secs(10))
            .await
            .unwrap();

        assert_eq!(exchange.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn query_retries_and_advances_nameservers() {
        let exchange = Arc::new(FakeExchange::new(vec![
            Err(io::Error::new(io::ErrorKind::TimedOut, "failed")),
            Ok(vec![Ipv4Addr::new(10, 0, 0, 1)]),
        ]));
        let engine = QueryEngine::new(
            vec!["127.0.0.1".to_owned(), "127.0.0.2".to_owned()],
            Duration::from_secs(1),
            1,
            false,
            exchange.clone(),
        );

        engine.lookup("origin.test").await.unwrap();

        assert_eq!(
            *exchange.nameservers.lock().unwrap(),
            vec![
                "127.0.0.1:53".parse().unwrap(),
                "127.0.0.2:53".parse().unwrap()
            ]
        );
    }

    #[tokio::test]
    async fn random_nameserver_order_changes_the_starting_server() {
        let exchange = Arc::new(FakeExchange::new(vec![Ok(vec![Ipv4Addr::LOCALHOST])]));
        let engine = QueryEngine::new(
            vec!["127.0.0.1".to_owned(), "127.0.0.2".to_owned()],
            Duration::from_secs(1),
            0,
            true,
            exchange.clone(),
        );
        engine.sequence.store(1, Ordering::Relaxed);

        engine.lookup("origin.test").await.unwrap();

        assert_eq!(
            *exchange.nameservers.lock().unwrap(),
            vec!["127.0.0.2:53".parse().unwrap()]
        );
    }

    #[tokio::test]
    async fn cached_addresses_rotate_round_robin() {
        let exchange = Arc::new(FakeExchange::new(vec![Ok(vec![
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 2),
        ])]));
        let resolver =
            CachedResolver::with_exchange(options(), vec!["127.0.0.1".to_owned()], exchange)
                .unwrap();
        let now = Instant::now();

        let first = resolver.resolve_at("origin.test", now).await.unwrap();
        let second = resolver.resolve_at("origin.test", now).await.unwrap();

        assert_eq!(first[0].ip(), "10.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(second[0].ip(), "10.0.0.2".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn nameserver_override_replaces_system_nameservers() {
        let exchange = Arc::new(FakeExchange::new(vec![Ok(vec![Ipv4Addr::LOCALHOST])]));
        let mut configured = options();
        configured.nameserver_override = Some("127.0.0.9:5353".to_owned());
        let nameservers = configured_nameservers(&configured, || {
            panic!("bridge resolver override must bypass /etc/resolv.conf")
        })
        .unwrap();
        let resolver =
            CachedResolver::with_exchange(configured, nameservers, exchange.clone()).unwrap();

        resolver
            .resolve_at("nucleus.test", Instant::now())
            .await
            .unwrap();

        assert_eq!(
            *exchange.nameservers.lock().unwrap(),
            vec!["127.0.0.9:5353".parse().unwrap()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn default_windows_resolver_uses_native_policy() {
        let resolver = CachedResolver::new(options()).unwrap();

        assert!(matches!(resolver.inner.query, ResolverQuery::System { .. }));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_native_resolver_retries_transient_failure() {
        let lookup = Arc::new(FakeNativeLookup::new(vec![
            NativeResponse::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "transient lookup failure",
            ))),
            NativeResponse::Ready(Ok(vec!["10.0.0.7:0".parse().unwrap()])),
        ]));
        let query = ResolverQuery::System {
            timeout: Duration::from_secs(1),
            retries: 1,
            lookup: lookup.clone(),
            observer: None,
        };

        assert_eq!(
            query.lookup("origin.test").await.unwrap(),
            vec![Ipv4Addr::new(10, 0, 0, 7)]
        );
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 2);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_native_resolver_returns_last_error_after_retries() {
        let lookup = Arc::new(FakeNativeLookup::new(vec![
            NativeResponse::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "first lookup failure",
            ))),
            NativeResponse::Ready(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "last lookup failure",
            ))),
        ]));
        let query = ResolverQuery::System {
            timeout: Duration::from_secs(1),
            retries: 1,
            lookup: lookup.clone(),
            observer: None,
        };

        let error = query.lookup("origin.test").await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 2);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_native_resolver_maps_bounded_timeout() {
        let lookup = Arc::new(FakeNativeLookup::new(vec![NativeResponse::Pending]));
        let query = ResolverQuery::System {
            timeout: Duration::from_millis(20),
            retries: 0,
            lookup: lookup.clone(),
            observer: None,
        };

        let error = query.lookup("origin.test").await.unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(lookup.calls.load(Ordering::Relaxed), 1);
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timed_out_windows_lookup_keeps_its_worker_permit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Semaphore::new(0));
        let lookup = Arc::new(BoundedNativeLookup::new(
            1,
            Arc::new(BlockingNativeOperation {
                calls: calls.clone(),
                active: active.clone(),
                release: release.clone(),
            }),
        ));
        let query = ResolverQuery::System {
            timeout: Duration::from_millis(50),
            retries: 1,
            lookup,
            observer: None,
        };

        assert_eq!(
            query.lookup("origin.test").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(active.load(Ordering::Relaxed), 1);

        release.add_permits(1);
        timeout(Duration::from_secs(1), async {
            while active.load(Ordering::Relaxed) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_native_dns_model_scenarios_conform_to_production() {
        use serde_json::Value;

        let graph: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-native-dns-lookup/fixtures/NativeDnsLookup.graph.json"
        ))
        .unwrap();
        let report: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-native-dns-lookup/fixtures/NativeDnsLookup.scenarios.json"
        ))
        .unwrap();
        assert_eq!(report["mapped_edge_count"], report["raw_edge_count"]);
        assert!(report["unmapped_edge_ids"].as_array().unwrap().is_empty());

        let edges = graph["edges"].as_array().unwrap();
        let nodes = graph["nodes"].as_array().unwrap();
        let scenarios = report["scenarios"].as_array().unwrap();
        assert_eq!(
            scenarios.len(),
            report["raw_edge_count"].as_u64().unwrap() as usize
        );

        for scenario in scenarios {
            let scenario_id = scenario["id"].as_u64().unwrap();
            let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
            let observer = NativeDnsModelObserver::new(events);
            let (commands, mut started) = tokio::sync::mpsc::unbounded_channel();
            let lookup = Arc::new(BoundedNativeLookup::new(
                2,
                Arc::new(ScriptedNativeOperation {
                    observer: observer.clone(),
                    commands,
                }),
            ));
            let query = ResolverQuery::System {
                timeout: Duration::from_millis(100),
                retries: 2,
                lookup,
                observer: Some(observer.clone()),
            };
            let query = tokio::spawn(async move { query.lookup("origin.test").await });
            let mut node = graph["initial"].as_u64().unwrap() as usize;
            let trace = scenario["trace_edge_ids"].as_array().unwrap();
            let mut current: Option<tokio::sync::oneshot::Sender<ScriptedNativeCommand>> = None;
            let mut detached: VecDeque<tokio::sync::oneshot::Sender<ScriptedNativeCommand>> =
                VecDeque::new();
            let mut resumes: Vec<tokio::sync::oneshot::Sender<()>> = Vec::new();

            for edge_id in trace {
                let edge = &edges[edge_id.as_u64().unwrap() as usize];
                assert_eq!(edge["from"].as_u64().unwrap() as usize, node);
                let action = edge["action"]["name"].as_str().unwrap();
                if action != "DetachedWorkerCompletes" {
                    for resume in resumes.drain(..) {
                        let _ = resume.send(());
                    }
                }
                match action {
                    "AttemptSucceeds" => current
                        .take()
                        .unwrap()
                        .send(ScriptedNativeCommand::Complete(Ok(vec![
                            "10.0.0.7:0".parse().unwrap(),
                        ])))
                        .unwrap(),
                    "AttemptFailsRetry" | "AttemptFailsFinal" => current
                        .take()
                        .unwrap()
                        .send(ScriptedNativeCommand::Complete(Err(io::Error::new(
                            io::ErrorKind::ConnectionReset,
                            "modeled native lookup failure",
                        ))))
                        .unwrap(),
                    "AttemptTimesOutRetry" | "AttemptTimesOutFinal" => {
                        detached.push_back(current.take().unwrap());
                    }
                    "DetachedWorkerCompletes" => detached
                        .pop_front()
                        .unwrap()
                        .send(ScriptedNativeCommand::ReleaseDetached)
                        .unwrap(),
                    "BeginLookup" | "StartAttempt" | "PermitWaitTimesOut" => {}
                    action => panic!("scenario {scenario_id} has unknown action {action}"),
                }

                let event = timeout(Duration::from_secs(2), observed.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.action, action, "scenario {scenario_id}");
                if action == "StartAttempt" {
                    current = Some(
                        timeout(Duration::from_secs(2), started.recv())
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                }
                node = edge["to"].as_u64().unwrap() as usize;
                let expected = &nodes[node]["state"];
                assert_eq!(event.state.caller_state, expected["caller_state"]);
                assert_eq!(event.state.attempts, expected["attempts"]);
                assert_eq!(event.state.current_attempt, expected["current_attempt"]);
                assert_eq!(event.state.active_workers, expected["active_workers"]);
                assert_eq!(event.state.last_failure, expected["last_failure"]);
                resumes.push(event.resume);
            }

            assert_eq!(
                scenario["expected_state"], nodes[node]["state"],
                "scenario {scenario_id} destination"
            );
            let terminal = matches!(
                scenario["expected_state"]["caller_state"].as_str().unwrap(),
                "Succeeded" | "Failed" | "TimedOut"
            );
            if terminal {
                for resume in resumes.drain(..) {
                    let _ = resume.send(());
                }
                let result = query.await.unwrap();
                match scenario["expected_state"]["caller_state"].as_str().unwrap() {
                    "Succeeded" => {
                        assert_eq!(result.unwrap(), [Ipv4Addr::new(10, 0, 0, 7)])
                    }
                    "Failed" => {
                        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionReset)
                    }
                    "TimedOut" => {
                        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut)
                    }
                    _ => unreachable!(),
                }
            } else {
                query.abort();
                let _ = query.await;
                for resume in resumes.drain(..) {
                    let _ = resume.send(());
                }
            }

            if let Some(command) = current {
                let _ = command.send(ScriptedNativeCommand::ReleaseSilently);
            }
            for command in detached {
                let _ = command.send(ScriptedNativeCommand::ReleaseSilently);
            }
            timeout(Duration::from_secs(2), async {
                while observer.active_workers.load(Ordering::Relaxed) != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[test]
    fn response_uses_a_records_from_the_additional_section() {
        // Arrange.
        let id = 0x1234;
        let mut packet = encode_query(id, "origin.test").unwrap();
        packet[2..4].copy_from_slice(&0x8180u16.to_be_bytes());
        packet[6..8].copy_from_slice(&1u16.to_be_bytes());
        packet[10..12].copy_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&[
            0xc0, 0x0c, // answer owner
            0x00, 0x05, // CNAME
            0x00, 0x01, // IN
            0x00, 0x00, 0x00, 0x3c, // TTL
            0x00, 0x02, // RDLENGTH
            0xc0, 0x0c, // alias target
            0xc0, 0x0c, // additional owner
            0x00, 0x01, // A
            0x00, 0x01, // IN
            0x00, 0x00, 0x00, 0x3c, // TTL
            0x00, 0x04, // RDLENGTH
            10, 0, 0, 1,
        ]);

        // Act.
        let addresses = decode_response(id, &packet).unwrap();

        // Assert.
        assert_eq!(addresses, vec![Ipv4Addr::new(10, 0, 0, 1)]);
    }

    #[test]
    fn upstream_dns_query_requests_ipv4_a_records() {
        let packet = encode_query(7, "origin.test").unwrap();

        assert_eq!(&packet[packet.len() - 4..], &[0, 1, 0, 1]);
    }

    #[test]
    fn default_policy_has_no_public_dns_fallback() {
        assert!(ResolverOptions::default().fallback_nameservers.is_empty());
    }

    #[test]
    fn empty_resolv_conf_without_explicit_fallback_fails_closed() {
        let error =
            configured_nameservers(&ResolverOptions::default(), || Ok(String::new())).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("no DNS nameservers found"));
        assert!(error.to_string().contains("no explicit fallback"));
    }

    #[test]
    fn resolver_construction_without_nameserver_fails() {
        let exchange = Arc::new(FakeExchange::new(Vec::new()));

        let error = match CachedResolver::with_exchange(options(), Vec::new(), exchange) {
            Ok(_) => panic!("resolver construction must reject an empty nameserver list"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "no DNS nameservers configured");
    }

    #[test]
    fn unreadable_resolv_conf_without_explicit_fallback_fails_closed() {
        let error = configured_nameservers(&ResolverOptions::default(), || {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(&format!(
            "failed to read {}",
            ovcc_platform::SYSTEM_RESOLVER_SOURCE
        )));
        assert!(error.to_string().contains("no explicit fallback"));
    }

    #[test]
    fn explicit_fallback_is_used_when_system_nameservers_are_absent() {
        let configured = ResolverOptions {
            fallback_nameservers: vec!["192.0.2.53".to_owned()],
            ..ResolverOptions::default()
        };

        let nameservers =
            configured_nameservers(&configured, || Ok("search example.test".into())).unwrap();

        assert_eq!(nameservers, vec!["192.0.2.53"]);
    }
}
