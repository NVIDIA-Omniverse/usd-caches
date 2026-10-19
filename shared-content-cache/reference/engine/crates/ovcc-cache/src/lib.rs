// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Cache engine providing single-flight (herd-locked) lookup/fill over a [`Storage`].
//!
//! [`Cache`] is the entry point, reading through [`Cache::get`] and opening a
//! writer through [`Cache::begin_fill`]. It codes against the
//! `ovcc-storage::Storage` trait and is unit-tested with an in-memory
//! (temp-dir-backed) fake `Storage`.
//!
//! # Single-flight (herd lock)
//!
//! A cache MISS triggers an upstream fill. Without coordination, concurrent
//! requests for the same missing key would each open a fill and hammer the
//! origin. [`Cache::acquire_fill`] elects one owner while waiters share its
//! committed entry. A waiter whose timeout expires fetches without admission,
//! and an owner older than the age limit no longer blocks peers.
//!
//! The lock is owned by the returned [`StoreWrite`] wrapper, so it is released
//! exactly when the writer is committed, aborted, or dropped. Distinct keys
//! never contend, and a fallback cannot replace an active writer.

use std::fs::File;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, watch};
use tokio::time::Instant;

use ovcc_core::{CacheStatus, Error, Result};
use ovcc_refmap::{Ref, RefMap};
use ovcc_storage::{
    CacheProvenance, InProgressRead, Lease, Lookup, StagedSegment, Storage, StoreWrite, StoredMeta,
};

const MAX_EVICTIONS_PER_MANAGER_STEP: usize = 100;
const MAX_MANAGER_STEP_DURATION: Duration = Duration::from_millis(200);
const PRODUCTIVE_MANAGER_DELAY: Duration = Duration::from_millis(50);
const IDLE_MANAGER_DELAY: Duration = Duration::from_secs(10);

/// Result of one bounded cache-manager pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagerStepReport {
    pub evicted_count: usize,
    pub origin_idle_evicted_count: usize,
    pub peer_copy_idle_evicted_count: usize,
    pub pressure_evicted_count: usize,
    pub next_delay: Duration,
}

/// Lifecycle of an explicitly shareable, known-length fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillProgressState {
    /// More body bytes may be written and published.
    Receiving,
    /// The exact expected body is readable; durable commit has not completed.
    BodyComplete,
    /// The exact body and cache metadata committed successfully.
    Committed,
    /// The exact body is readable, but durable cache publication failed.
    CommitFailed,
    /// The fill ended before an exact clean body could be committed.
    Aborted,
}

impl FillProgressState {
    const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::CommitFailed | Self::Aborted)
    }
}

/// Monotonic snapshot published by an explicitly shareable fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillProgress {
    published: u64,
    expected_size: u64,
    state: FillProgressState,
}

impl FillProgress {
    /// Body bytes successfully written to the local staging file.
    pub const fn published(&self) -> u64 {
        self.published
    }

    /// Exact body length declared when the shareable fill began.
    pub const fn expected_size(&self) -> u64 {
        self.expected_size
    }

    /// Current fill lifecycle state.
    pub const fn state(&self) -> FillProgressState {
        self.state
    }
}

/// One immutable file region bounded by a shareable fill's published frontier.
#[derive(Debug, Clone)]
pub struct FillReadWindow {
    file: Arc<File>,
    file_offset: u64,
    len: u64,
}

impl FillReadWindow {
    /// Shared read-only staging descriptor suitable for `sendfile`.
    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    /// Absolute file offset after the storage header and prior follower bytes.
    pub const fn file_offset(&self) -> u64 {
        self.file_offset
    }

    /// Maximum bytes that may be sent from this window.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Never true for a window from [`FillFollower::readable_window`], which
    /// withholds an empty region rather than publishing one.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Cloneable, non-HIT follower of an explicitly shareable local fill.
///
/// Each clone has its own progress receiver and logical body offset. The file
/// descriptor remains valid after registry cleanup, commit rename, or abort
/// unlink. Callers obtain only frontier-bounded [`FillReadWindow`] values and
/// advance their own offset after successful sends.
#[derive(Clone)]
pub struct FillFollower {
    meta: StoredMeta,
    read: InProgressRead,
    progress: watch::Receiver<FillProgress>,
    offset: u64,
}

impl FillFollower {
    /// Immutable response metadata captured when the owner began the fill.
    pub fn meta(&self) -> &StoredMeta {
        &self.meta
    }

    /// Exact expected body size.
    pub const fn expected_size(&self) -> u64 {
        self.meta.size
    }

    /// Body bytes independently consumed by this follower.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Latest monotonic progress snapshot.
    pub fn progress(&self) -> FillProgress {
        *self.progress.borrow()
    }

    /// Waits until the owner's progress changes.
    pub async fn wait_for_progress(
        &mut self,
    ) -> std::result::Result<FillProgress, watch::error::RecvError> {
        self.progress.changed().await?;
        Ok(*self.progress.borrow_and_update())
    }

    /// Returns the currently readable region, bounded by the published frontier.
    pub fn readable_window(&self) -> Result<Option<FillReadWindow>> {
        let progress = self.progress();
        if self.offset > progress.published || progress.published > progress.expected_size {
            return Err(Error::internal(
                "shareable fill frontier invariant violated",
            ));
        }
        let len = progress.published - self.offset;
        if len == 0 {
            return Ok(None);
        }
        let file_offset = self
            .read
            .body_offset
            .checked_add(self.offset)
            .ok_or_else(|| Error::cache("shareable fill file offset overflow"))?;
        Ok(Some(FillReadWindow {
            file: Arc::clone(&self.read.file),
            file_offset,
            len,
        }))
    }

    /// Advances this follower after a successful bounded read/send.
    pub fn advance(&mut self, bytes: u64) -> Result<()> {
        let published = self.progress().published;
        let next = self
            .offset
            .checked_add(bytes)
            .ok_or_else(|| Error::cache("shareable follower offset overflow"))?;
        if next > published || next > self.expected_size() {
            return Err(Error::cache(
                "shareable follower cannot advance past published bytes",
            ));
        }
        self.offset = next;
        Ok(())
    }
}

/// Canonical cache capacity, freshness, and fill-coordination limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictionPolicy {
    pub validity_200_secs: Option<u64>,
    pub validity_206_secs: Option<u64>,
    pub max_idle_secs: Option<u64>,
    pub peer_copy_max_idle_secs: Option<u64>,
    pub max_bytes: Option<u64>,
    pub min_free_bytes: Option<u64>,
    pub lock_age_secs: u64,
    pub lock_timeout_secs: u64,
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self {
            validity_200_secs: None,
            validity_206_secs: None,
            max_idle_secs: None,
            peer_copy_max_idle_secs: None,
            max_bytes: None,
            min_free_bytes: None,
            lock_age_secs: 60,
            lock_timeout_secs: 120,
        }
    }
}

fn effective_max_idle_secs(
    policy: EvictionPolicy,
    provenance: CacheProvenance,
    object_max_idle_secs: Option<u64>,
) -> Option<u64> {
    [
        policy.max_idle_secs,
        matches!(provenance, CacheProvenance::PeerCopy)
            .then_some(policy.peer_copy_max_idle_secs)
            .flatten(),
        object_max_idle_secs,
    ]
    .into_iter()
    .flatten()
    .min()
}

struct KeyLock {
    lock: Arc<AsyncMutex<()>>,
    started_at: Arc<Mutex<Option<Instant>>>,
    shareable_fill: Mutex<Option<Arc<ShareableFill>>>,
    fill_registered: watch::Sender<u64>,
}

impl KeyLock {
    fn new() -> Self {
        let (fill_registered, _) = watch::channel(0);
        Self {
            lock: Arc::new(AsyncMutex::new(())),
            started_at: Arc::new(Mutex::new(None)),
            shareable_fill: Mutex::new(None),
            fill_registered,
        }
    }
}

struct ShareableFill {
    meta: StoredMeta,
    read: InProgressRead,
    progress_tx: watch::Sender<FillProgress>,
}

impl ShareableFill {
    fn new(meta: StoredMeta, read: InProgressRead) -> Arc<Self> {
        let expected_size = meta.size;
        let (progress_tx, _progress_rx) = watch::channel(FillProgress {
            published: 0,
            expected_size,
            state: FillProgressState::Receiving,
        });
        Arc::new(Self {
            meta,
            read,
            progress_tx,
        })
    }

    fn progress(&self) -> FillProgress {
        *self.progress_tx.borrow()
    }

    fn publish(&self, published: u64, state: FillProgressState) {
        let current = self.progress();
        debug_assert!(published >= current.published);
        debug_assert!(published <= current.expected_size);
        debug_assert!(
            !current.state.is_terminal(),
            "terminal shareable fill progress cannot change"
        );
        self.progress_tx.send_replace(FillProgress {
            published,
            expected_size: current.expected_size,
            state,
        });
    }

    fn follower(&self) -> Option<FillFollower> {
        let progress = self.progress_tx.subscribe();
        if progress.borrow().state.is_terminal() {
            return None;
        }
        Some(FillFollower {
            meta: self.meta.clone(),
            read: self.read.clone(),
            progress,
            offset: 0,
        })
    }
}

struct ShareableRegistration {
    key_lock: Ref<String, KeyLock>,
    shared: Arc<ShareableFill>,
}

impl ShareableRegistration {
    fn unregister(&self) {
        let mut registered = self.key_lock.shareable_fill.lock();
        if registered
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.shared))
        {
            registered.take();
        }
    }
}

/// Result of coordinating a cache miss with an existing fill.
#[expect(
    clippy::large_enum_variant,
    reason = "boxing Lease would allocate for every coalesced waiter on the herd-control path"
)]
pub enum FillDecision {
    /// This request owns the only admission opportunity for the key.
    Owner(FillPermit),
    /// The active fill completed successfully while this request waited.
    Shared(Lease),
    /// The wait or owner-age limit elapsed, so this request must not admit.
    Bypass,
}

/// Opaque ownership token for one herd-locked fill.
pub struct FillPermit {
    _guard: OwnedMutexGuard<()>,
    key_lock: Ref<String, KeyLock>,
}

impl Drop for FillPermit {
    fn drop(&mut self) {
        *self.key_lock.started_at.lock() = None;
    }
}

/// Watches shareable-fill publication for one key.
///
/// Holding it keeps the key's herd-lock entry live, so a writer that registers
/// later publishes on the same entry this watcher subscribed to.
pub struct FillRegistration {
    changes: watch::Receiver<u64>,
    _key_lock: Ref<String, KeyLock>,
}

impl FillRegistration {
    /// Waits for the next shareable-fill publication on this key.
    pub async fn changed(&mut self) -> std::result::Result<(), watch::error::RecvError> {
        self.changes.changed().await
    }
}

/// Cache engine over a pluggable [`Storage`] backend.
pub struct Cache<S: Storage> {
    storage: S,
    policy: EvictionPolicy,
    /// In-process herd locks, one `Mutex<()>` per key with a live holder. A
    /// permit, a waiter, a shareable registration, or a registration watcher
    /// keeps its entry live, so every concurrent caller shares one lock, and
    /// the map drops the entry when the last of them lets go.
    key_locks: RefMap<String, KeyLock>,
}

impl<S: Storage> Cache<S> {
    /// Wrap a storage backend.
    pub fn new(storage: S) -> Self {
        Self::with_policy(storage, EvictionPolicy::default())
    }

    /// Wraps storage with the backend's canonical cache limits.
    pub fn with_policy(storage: S, policy: EvictionPolicy) -> Self {
        Self {
            storage,
            policy,
            key_locks: RefMap::new(),
        }
    }

    /// Borrows the underlying storage.
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Whether a non-disabled capacity limit is configured.
    pub fn capacity_limited(&self) -> bool {
        self.policy.max_bytes.is_some()
            || self
                .policy
                .min_free_bytes
                .is_some_and(|minimum| minimum > 0)
    }

    /// Look up `key`. Returns a leased hit (`HIT`), or `None` for a miss.
    ///
    /// A storage [`Lookup::SoftMiss`] (remembered metadata, no serveable bytes)
    /// collapses to `None` here, since it is still a cache miss for the caller, who
    /// must refill through the normal access check (I18). Use [`Cache::lookup`]
    /// to distinguish a soft miss from a cold miss.
    pub fn get(&self, key: &str) -> Result<Option<(Lease, CacheStatus)>> {
        if self.policy.validity_200_secs.is_none() && self.policy.validity_206_secs.is_none() {
            return match self.storage.lookup(key)? {
                Lookup::Hit(lease) => Ok(Some((lease, CacheStatus::Hit))),
                Lookup::SoftMiss(_) | Lookup::Miss => Ok(None),
            };
        }
        self.get_inner(key, now_unix(), false)
    }

    /// Looks up an entry whose absent expiry deliberately means never stale.
    #[doc(hidden)]
    pub fn get_preserving_expiry(&self, key: &str) -> Result<Option<(Lease, CacheStatus)>> {
        self.get_inner(key, now_unix(), true)
    }

    /// Deterministic lookup at `now`, exposed for policy tests.
    pub fn get_at(&self, key: &str, now: i64) -> Result<Option<(Lease, CacheStatus)>> {
        self.get_inner(key, now, false)
    }

    fn get_inner(
        &self,
        key: &str,
        now: i64,
        preserve_absent_expiry: bool,
    ) -> Result<Option<(Lease, CacheStatus)>> {
        match self.storage.lookup(key)? {
            Lookup::Hit(lease) if lease.meta.expires_at.is_some_and(|expiry| now >= expiry) => {
                // Names the generation this lookup observed, so a replacement
                // committed in between keeps its bytes. This request still
                // misses and the next one serves the replacement.
                let revision = lease.meta.revision;
                drop(lease);
                self.storage.evict_data(key, revision)?;
                Ok(None)
            }
            Lookup::Hit(lease)
                if !preserve_absent_expiry
                    && lease.meta.expires_at.is_none()
                    && (self.policy.validity_200_secs.is_some()
                        || self.policy.validity_206_secs.is_some()) =>
            {
                // Entries written before normal validity was enforced have no
                // creation deadline. Refilling once is the only safe migration.
                let revision = lease.meta.revision;
                drop(lease);
                self.storage.evict_data(key, revision)?;
                Ok(None)
            }
            Lookup::Hit(lease) => Ok(Some((lease, CacheStatus::Hit))),
            Lookup::SoftMiss(_) | Lookup::Miss => Ok(None),
        }
    }

    /// Look up `key`, surfacing the full storage [`Lookup`] state
    /// (`Hit | SoftMiss | Miss`) for callers that want to act on a soft miss.
    pub fn lookup(&self, key: &str) -> Result<Lookup> {
        self.storage.lookup(key)
    }

    /// Observes committed metadata without changing access history or taking a
    /// serve lease. Presence protocols use this instead of turning probes into
    /// eviction demand signals.
    pub fn peek(&self, key: &str) -> Result<Option<StoredMeta>> {
        self.storage.peek(key)
    }

    /// Resolves physical keys while keeping admission locks on the full key.
    /// The caller still owns authorization and representation compatibility.
    pub fn lookup_logical(
        &self,
        key: &ovcc_cachekey::LogicalKey,
    ) -> Result<Option<ovcc_storage::LogicalMatch<Lease>>> {
        self.storage.lookup_logical(key)
    }

    /// Logical-key presence never updates hit history or acquires a body lease.
    pub fn peek_logical(
        &self,
        key: &ovcc_cachekey::LogicalKey,
    ) -> Result<Option<ovcc_storage::LogicalMatch<StoredMeta>>> {
        self.storage.peek_logical(key)
    }

    /// Joins only an explicitly shareable local fill already active for `key`.
    ///
    /// This is a non-HIT registry read: it never calls storage, mutates access
    /// history, takes the herd lock, or waits on fill age/timeouts.
    pub fn join_in_progress(&self, key: &str) -> Option<FillFollower> {
        let key_lock = self.key_locks.get(key)?;
        let shared = key_lock.shareable_fill.lock().as_ref().cloned()?;
        shared.follower()
    }

    /// Notifies a waiter when a writer publishes a shareable staging file.
    /// Subscribing before checking the registry closes the registration race.
    pub fn subscribe_fill_registration(&self, key: &str) -> FillRegistration {
        let key_lock = self.key_lock(key);
        let changes = key_lock.fill_registered.subscribe();
        FillRegistration {
            changes,
            _key_lock: key_lock,
        }
    }

    /// Coordinates a missed key with its active fill.
    ///
    /// A waiter shares the committed entry when the owner finishes in time.
    /// Once either the wait limit or the owner's age limit elapses, it returns
    /// [`FillDecision::Bypass`], leaving the active owner's admission untouched.
    pub async fn acquire_fill(&self, key: &str) -> Result<FillDecision> {
        self.acquire_fill_inner(key, true).await
    }

    /// Coordinates a forced refill without sharing an already committed entry.
    #[doc(hidden)]
    pub async fn acquire_refill(&self, key: &str) -> Result<FillDecision> {
        self.acquire_fill_inner(key, false).await
    }

    /// Publishes a successful origin revalidation for the exact stale
    /// generation while retaining its immutable body bytes.
    ///
    /// The caller must hold `permit` from [`Cache::acquire_refill`] across the
    /// conditional origin request. A generation mismatch is reported as
    /// `None`, preventing a delayed `304 Not Modified` from refreshing bytes
    /// that another writer replaced. A successful refresh performs a normal
    /// lookup and returns its lease, so serving the client counts as access;
    /// the metadata-only mutation itself does not reset inactivity history.
    pub fn refresh_acquired(
        &self,
        key: &str,
        expected: &StoredMeta,
        permit: FillPermit,
    ) -> Result<Option<Lease>> {
        self.refresh_acquired_at(key, expected, permit, now_unix())
    }

    /// Publishes a successful origin revalidation with a caller-selected
    /// freshness deadline.
    ///
    /// This is used by cache locations whose validity differs from the
    /// backend's normal status policy. The exact-generation comparison and
    /// single-flight requirements are identical to [`Cache::refresh_acquired`].
    pub fn refresh_acquired_until(
        &self,
        key: &str,
        expected: &StoredMeta,
        permit: FillPermit,
        expires_at: i64,
    ) -> Result<Option<Lease>> {
        self.refresh_acquired_with_deadline(key, expected, permit, expires_at)
    }

    /// Deterministic form of [`Cache::refresh_acquired`] for policy tests.
    #[doc(hidden)]
    pub fn refresh_acquired_at(
        &self,
        key: &str,
        expected: &StoredMeta,
        permit: FillPermit,
        now: i64,
    ) -> Result<Option<Lease>> {
        let ttl = if expected.content_range.is_some() {
            self.policy.validity_206_secs
        } else {
            self.policy.validity_200_secs
        };
        let Some(ttl) = ttl else {
            return Ok(None);
        };
        let expires_at = now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX));
        self.refresh_acquired_with_deadline(key, expected, permit, expires_at)
    }

    fn refresh_acquired_with_deadline(
        &self,
        key: &str,
        expected: &StoredMeta,
        permit: FillPermit,
        expires_at: i64,
    ) -> Result<Option<Lease>> {
        let refreshed = self.storage.refresh_freshness(key, expected, expires_at)?;
        if !refreshed {
            return Ok(None);
        }

        let lease = match self.storage.lookup(key)? {
            Lookup::Hit(lease) => Some(lease),
            Lookup::SoftMiss(_) | Lookup::Miss => None,
        };
        drop(permit);
        Ok(lease)
    }

    async fn acquire_fill_inner(&self, key: &str, share_committed: bool) -> Result<FillDecision> {
        let key_lock = self.key_lock(key);
        let wait_deadline = Instant::now() + Duration::from_secs(self.policy.lock_timeout_secs);

        if share_committed && let Some((lease, _)) = self.get(key)? {
            return Ok(FillDecision::Shared(lease));
        }

        if let Ok(guard) = Arc::clone(&key_lock.lock).try_lock_owned() {
            *key_lock.started_at.lock() = Some(Instant::now());
            return Ok(FillDecision::Owner(FillPermit {
                _guard: guard,
                key_lock,
            }));
        }

        let age_deadline = key_lock
            .started_at
            .lock()
            .map(|started| started + Duration::from_secs(self.policy.lock_age_secs));
        let deadline = age_deadline
            .map(|age| age.min(wait_deadline))
            .unwrap_or(wait_deadline);
        if Instant::now() >= deadline {
            return Ok(FillDecision::Bypass);
        }

        let guard = match tokio::time::timeout_at(deadline, Arc::clone(&key_lock.lock).lock_owned())
            .await
        {
            Ok(guard) => guard,
            Err(_) => return Ok(FillDecision::Bypass),
        };
        if share_committed && let Some((lease, _)) = self.get(key)? {
            drop(guard);
            return Ok(FillDecision::Shared(lease));
        }
        *key_lock.started_at.lock() = Some(Instant::now());
        Ok(FillDecision::Owner(FillPermit {
            _guard: guard,
            key_lock,
        }))
    }

    /// Opens an uncontended single-flight admission for a missed `key`.
    ///
    /// Request paths use [`Cache::acquire_fill`] before fetching. This immediate
    /// form supports non-request producers and fails if another owner is active.
    pub fn begin_fill(&self, key: &str, meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
        let permit = self.try_acquire_fill(key)?;
        self.begin_fill_inner(key, meta, None, true, permit)
    }

    /// Admits metadata whose expiry was deliberately chosen by the caller,
    /// including `None` for a never-expiring bursty entry.
    #[doc(hidden)]
    pub fn begin_fill_preserving_expiry(
        &self,
        key: &str,
        meta: StoredMeta,
    ) -> Result<Box<dyn StoreWrite>> {
        let permit = self.try_acquire_fill(key)?;
        self.begin_fill_inner(key, meta, None, false, permit)
    }

    /// Deterministic admission at `now`, exposed for policy tests.
    #[doc(hidden)]
    pub fn begin_fill_at(
        &self,
        key: &str,
        meta: StoredMeta,
        now: i64,
    ) -> Result<Box<dyn StoreWrite>> {
        let permit = self.try_acquire_fill(key)?;
        self.begin_fill_inner(key, meta, Some(now), true, permit)
    }

    /// Opens storage under a permit returned by [`Cache::acquire_fill`].
    pub fn begin_acquired_fill(
        &self,
        key: &str,
        meta: StoredMeta,
        permit: FillPermit,
    ) -> Result<Box<dyn StoreWrite>> {
        self.begin_fill_inner(key, meta, None, true, permit)
    }

    /// Opens an opt-in known-length fill that authorized same-replica callers
    /// may follow through [`Cache::join_in_progress`].
    ///
    /// `meta.size` is the exact expected body length. Callers must authorize
    /// each follower independently before exposing these bytes.
    pub fn begin_shareable_acquired_fill(
        &self,
        key: &str,
        meta: StoredMeta,
        permit: FillPermit,
    ) -> Result<Box<dyn StoreWrite>> {
        let meta = self.prepare_fill_meta(meta, None, true);
        let inner = self.storage.begin_fill(key, meta.clone())?;
        let read = match inner.in_progress_read() {
            Ok(read) => read,
            Err(error) => {
                inner.abort();
                return Err(error);
            }
        };
        let shared = ShareableFill::new(meta, read);
        let key_lock = permit.key_lock.clone();
        let already_registered = key_lock.shareable_fill.lock().is_some();
        if already_registered {
            inner.abort();
            return Err(Error::internal(
                "shareable fill already registered under sole writer permit",
            ));
        }
        {
            let mut registered = key_lock.shareable_fill.lock();
            debug_assert!(registered.is_none());
            *registered = Some(Arc::clone(&shared));
        }
        key_lock
            .fill_registered
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(Box::new(ShareableWriter {
            inner: Some(inner),
            permit: Some(permit),
            registration: ShareableRegistration { key_lock, shared },
            failed: false,
            finished: false,
        }))
    }

    /// Opens storage under a permit while preserving caller-selected expiry.
    #[doc(hidden)]
    pub fn begin_acquired_fill_preserving_expiry(
        &self,
        key: &str,
        meta: StoredMeta,
        permit: FillPermit,
    ) -> Result<Box<dyn StoreWrite>> {
        self.begin_fill_inner(key, meta, None, false, permit)
    }

    /// Atomically admits an ordered segmented object under the same per-key
    /// single-flight and capacity policy as a contiguous fill.
    ///
    /// The call is synchronous because filesystem adoption and redb commits are
    /// synchronous. Async callers must run it in `tokio::task::spawn_blocking`.
    pub fn adopt_segmented(
        &self,
        key: &str,
        mut meta: StoredMeta,
        segments: &[StagedSegment],
    ) -> Result<Lease> {
        let _permit = self.try_acquire_fill(key)?;
        let total = segments.iter().try_fold(0u64, |total, segment| {
            total
                .checked_add(segment.size)
                .ok_or_else(|| Error::storage("segmented admission length overflow"))
        })?;
        meta.size = total;
        meta = self.prepare_fill_meta(meta, None, true);
        // Capacity reclamation belongs to the independent manager. Storage
        // invokes this only after securing and revalidating every source.
        let mut admit = || Ok(());
        self.storage
            .adopt_segmented_with_admission(key, meta, segments, &mut admit)
    }

    fn begin_fill_inner(
        &self,
        key: &str,
        meta: StoredMeta,
        now: Option<i64>,
        apply_default_validity: bool,
        permit: FillPermit,
    ) -> Result<Box<dyn StoreWrite>> {
        let meta = self.prepare_fill_meta(meta, now, apply_default_validity);
        let inner = self.storage.begin_fill(key, meta)?;
        Ok(Box::new(SingleFlightWriter {
            inner,
            _permit: permit,
        }))
    }

    fn prepare_fill_meta(
        &self,
        mut meta: StoredMeta,
        now: Option<i64>,
        apply_default_validity: bool,
    ) -> StoredMeta {
        let apply_validity = apply_default_validity
            && meta.expires_at.is_none()
            && (self.policy.validity_200_secs.is_some() || self.policy.validity_206_secs.is_some());
        if apply_validity {
            let policy_now = now.unwrap_or_else(now_unix);
            let ttl = if meta.content_range.is_some() {
                self.policy.validity_206_secs
            } else {
                self.policy.validity_200_secs
            };
            meta.expires_at =
                ttl.map(|ttl| policy_now.saturating_add(i64::try_from(ttl).unwrap_or(i64::MAX)));
        }
        meta.max_idle_secs =
            effective_max_idle_secs(self.policy, meta.provenance, meta.max_idle_secs);
        meta
    }

    /// Runs one bounded inactive/LRU eviction pass.
    ///
    /// The returned delay lets the caller schedule the next pass without
    /// coupling eviction to admission latency.
    pub fn manager_step(&self) -> Result<ManagerStepReport> {
        self.manager_step_at(now_unix())
    }

    /// Runs one deterministic cache-manager pass at Unix timestamp `now`.
    #[doc(hidden)]
    pub fn manager_step_at(&self, now: i64) -> Result<ManagerStepReport> {
        // Reclaims retired herd-lock entries and shrinks idle tables in
        // bounded steps, including when storage has nothing to evict.
        let maintenance = self.key_locks.maintain();
        let mut candidates = self.storage.eviction_candidates()?;
        let mut cached_bytes = candidates.iter().fold(0u64, |total, candidate| {
            total.saturating_add(candidate.size_bytes)
        });
        let min_free_bytes = self.policy.min_free_bytes.filter(|minimum| *minimum > 0);
        let mut available_bytes = min_free_bytes
            .map(|_| self.storage.available_bytes())
            .transpose()?;
        let idle_for = |candidate: &ovcc_storage::EvictionCandidate| {
            effective_max_idle_secs(self.policy, candidate.provenance, candidate.max_idle_secs)
        };
        let inactive = |candidate: &ovcc_storage::EvictionCandidate| {
            idle_for(candidate).is_some_and(|idle| {
                candidate.last_access <= now.saturating_sub(i64::try_from(idle).unwrap_or(i64::MAX))
            })
        };

        let under_pressure = |cached_bytes: u64, available_bytes: Option<u64>| {
            self.policy
                .max_bytes
                .is_some_and(|maximum| cached_bytes >= maximum)
                || min_free_bytes
                    .zip(available_bytes)
                    .is_some_and(|(minimum, available)| available <= minimum)
        };
        // Capacity pressure preserves raw LRU regardless of provenance. Without
        // pressure, adjusted deadlines keep active origin entries from starving
        // expired peer copies in each bounded deletion pass.
        if under_pressure(cached_bytes, available_bytes) {
            candidates.sort_by_key(|candidate| {
                (
                    candidate.last_access,
                    candidate.hit_count,
                    candidate.key.clone(),
                )
            });
        } else {
            candidates.sort_by_key(|candidate| {
                let deadline = match idle_for(candidate) {
                    Some(idle) => candidate
                        .last_access
                        .saturating_add(i64::try_from(idle).unwrap_or(i64::MAX)),
                    None => i64::MAX,
                };
                (
                    deadline,
                    candidate.last_access,
                    candidate.hit_count,
                    candidate.key.clone(),
                )
            });
        }
        let started = std::time::Instant::now();

        // Attempts carry the per-pass work bound because every call takes the
        // global storage mutation lock, while the report counts only what
        // storage actually reclaimed.
        let mut attempted = 0usize;
        let mut rejected = false;
        let mut evicted_count = 0usize;
        let mut origin_idle_evicted_count = 0usize;
        let mut peer_copy_idle_evicted_count = 0usize;
        let mut pressure_evicted_count = 0usize;
        let mut next_candidate = 0usize;
        while let Some(candidate) = candidates.get(next_candidate) {
            if attempted == MAX_EVICTIONS_PER_MANAGER_STEP
                || started.elapsed() >= MAX_MANAGER_STEP_DURATION
            {
                break;
            }

            let candidate_inactive = inactive(candidate);
            if candidate_inactive || under_pressure(cached_bytes, available_bytes) {
                // Storage refuses a decision whose generation was replaced and
                // reports the `FetchInProgress` no-op, so the running byte
                // total and the counters follow the reclamation rather than
                // the snapshot.
                attempted += 1;
                let evicted = self
                    .storage
                    .evict_data(&candidate.key, candidate.revision)?;
                if evicted {
                    cached_bytes = cached_bytes.saturating_sub(candidate.size_bytes);
                    if min_free_bytes.is_some() {
                        available_bytes = Some(self.storage.available_bytes()?);
                    }
                    evicted_count += 1;
                    if candidate_inactive {
                        match candidate.provenance {
                            CacheProvenance::Origin => origin_idle_evicted_count += 1,
                            CacheProvenance::PeerCopy => peer_copy_idle_evicted_count += 1,
                        }
                    } else {
                        pressure_evicted_count += 1;
                    }
                } else {
                    rejected = true;
                }
            }
            next_candidate += 1;
        }

        // A refusal that leaves the cache still over its limit is worth a
        // prompt retry against a fresher snapshot, unlike an idle candidate a
        // replacement has just made active again.
        let productive_work_remains = maintenance.shards_pending > 0
            || next_candidate < candidates.len()
            || (rejected && under_pressure(cached_bytes, available_bytes));
        Ok(ManagerStepReport {
            evicted_count,
            origin_idle_evicted_count,
            peer_copy_idle_evicted_count,
            pressure_evicted_count,
            next_delay: if productive_work_remains {
                PRODUCTIVE_MANAGER_DELAY
            } else {
                IDLE_MANAGER_DELAY
            },
        })
    }

    /// Evict data and metadata for the committed generation `revision`
    /// (`Cached -> Absent`). Never downloads (I2);
    /// skips `FetchInProgress` (I17) and every other generation, and reports
    /// whether storage performed the transition.
    pub fn evict_data(&self, key: &str, revision: u64) -> Result<bool> {
        self.storage.evict_data(key, revision)
    }

    /// Hard-evict `key`: remove both data and metadata (`-> Absent`).
    ///
    /// Deliberately skips the per-key [`FillPermit`], because a mutation purge
    /// has to be prompt rather than queued behind a multi-gigabyte fill. The
    /// backend makes the removal final for any writer already open on `key` by
    /// revoking its generation, so a racing fill fails at commit instead of
    /// resurrecting the purged entry.
    pub fn remove(&self, key: &str) -> Result<()> {
        self.storage.remove(key)
    }

    /// Gets (or creates) the shared per-key lock for `key`.
    fn key_lock(&self, key: &str) -> Ref<String, KeyLock> {
        self.key_locks
            .get_or_insert_with(key, || (key.to_owned(), KeyLock::new()))
    }

    fn try_acquire_fill(&self, key: &str) -> Result<FillPermit> {
        let key_lock = self.key_lock(key);
        let guard = Arc::clone(&key_lock.lock)
            .try_lock_owned()
            .map_err(|_| Error::cache("cache fill already active"))?;
        *key_lock.started_at.lock() = Some(Instant::now());
        Ok(FillPermit {
            _guard: guard,
            key_lock,
        })
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// A [`StoreWrite`] that owns the storage writer **and** the held per-key herd
/// lock guard. Dropping it (via `commit`, `abort`, or scope exit) releases the
/// lock, admitting the next fill for that key.
struct SingleFlightWriter {
    inner: Box<dyn StoreWrite>,
    _permit: FillPermit,
}

impl StoreWrite for SingleFlightWriter {
    fn write_chunk(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_chunk(buf)
    }

    fn write_chunks(&mut self, chunks: &mut [std::io::IoSlice<'_>]) -> Result<()> {
        self.inner.write_chunks(chunks)
    }

    fn commit(self: Box<Self>) -> Result<Lease> {
        let SingleFlightWriter { inner, _permit } = *self;
        let lease = inner.commit()?;
        drop(_permit);
        Ok(lease)
    }

    fn abort(self: Box<Self>) {
        let SingleFlightWriter { inner, _permit } = *self;
        inner.abort();
        drop(_permit);
    }
}

/// Store writer that publishes only successfully persisted staging bytes to
/// non-HIT followers and keeps the sole writer permit through completion.
struct ShareableWriter {
    inner: Option<Box<dyn StoreWrite>>,
    permit: Option<FillPermit>,
    registration: ShareableRegistration,
    failed: bool,
    finished: bool,
}

impl ShareableWriter {
    fn published(&self) -> u64 {
        self.registration.shared.progress().published
    }

    fn expected_size(&self) -> u64 {
        self.registration.shared.meta.size
    }

    fn fail(&mut self) {
        if !self.finished && !self.registration.shared.progress().state.is_terminal() {
            self.registration
                .shared
                .publish(self.published(), FillProgressState::Aborted);
        }
        self.failed = true;
        self.registration.unregister();
    }

    fn finish(&mut self) {
        self.finished = true;
        self.registration.unregister();
    }
}

impl StoreWrite for ShareableWriter {
    fn write_chunk(&mut self, buf: &[u8]) -> Result<()> {
        if self.failed || self.finished {
            return Err(Error::cache("shareable fill is no longer writable"));
        }
        let chunk_len = match u64::try_from(buf.len()) {
            Ok(length) => length,
            Err(_) => {
                self.fail();
                return Err(Error::cache("shareable fill chunk length overflow"));
            }
        };
        let next = match self.published().checked_add(chunk_len) {
            Some(next) => next,
            None => {
                self.fail();
                return Err(Error::cache("shareable fill length overflow"));
            }
        };
        if next > self.expected_size() {
            self.fail();
            return Err(Error::cache(
                "shareable fill exceeds its exact expected size",
            ));
        }
        let Some(inner) = self.inner.as_mut() else {
            self.fail();
            return Err(Error::internal("shareable fill lost its storage writer"));
        };
        let write_result = inner.write_chunk(buf);
        if let Err(error) = write_result {
            self.fail();
            return Err(error);
        }
        self.registration
            .shared
            .publish(next, FillProgressState::Receiving);
        Ok(())
    }

    fn in_progress_read(&self) -> Result<InProgressRead> {
        Ok(self.registration.shared.read.clone())
    }

    fn commit(mut self: Box<Self>) -> Result<Lease> {
        if self.failed {
            if let Some(inner) = self.inner.take() {
                inner.abort();
            }
            self.finish();
            drop(self.permit.take());
            return Err(Error::cache("cannot commit a failed shareable fill"));
        }
        if self.published() != self.expected_size() {
            self.fail();
            if let Some(inner) = self.inner.take() {
                inner.abort();
            }
            self.finish();
            drop(self.permit.take());
            return Err(Error::cache(
                "shareable fill body is shorter than its exact expected size",
            ));
        }

        self.registration
            .shared
            .publish(self.published(), FillProgressState::BodyComplete);
        let result = self
            .inner
            .take()
            .ok_or_else(|| Error::internal("shareable fill lost its storage writer"))?
            .commit();
        let state = if result.is_ok() {
            FillProgressState::Committed
        } else {
            FillProgressState::CommitFailed
        };
        self.registration.shared.publish(self.published(), state);
        self.finish();
        drop(self.permit.take());
        result
    }

    fn abort(mut self: Box<Self>) {
        self.fail();
        if let Some(inner) = self.inner.take() {
            inner.abort();
        }
        self.finish();
        drop(self.permit.take());
    }
}

impl Drop for ShareableWriter {
    fn drop(&mut self) {
        if !self.finished {
            self.fail();
            if let Some(inner) = self.inner.take() {
                inner.abort();
            }
            self.finish();
            drop(self.permit.take());
        }
    }
}

#[cfg(test)]
mod cache_retention_state_machine_tests;

#[cfg(test)]
mod cache_revalidation_state_machine_tests;

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Seek, Write};
    use std::path::PathBuf;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use ovcc_storage::{DiskStorage, EvictionCandidate, StagedSegment};
    use tempfile::TempDir;

    /// Positional exact reads for tests on either platform backend.
    trait ReadExactAt {
        fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()>;
    }

    impl ReadExactAt for std::fs::File {
        fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
            ovcc_platform::read_exact_at(self, buffer, offset)
        }
    }

    struct CommitGate {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    /// Shared, mutable state behind a [`FakeStorage`], so a writer handed out by
    /// `put_writer` can publish back into the index on `commit`.
    struct Shared {
        root: PathBuf,
        entries: Mutex<HashMap<String, (PathBuf, StoredMeta)>>,
        counter: AtomicU64,
        fail_write: AtomicBool,
        fail_commit: AtomicBool,
        commit_gate: Mutex<Option<CommitGate>>,
        /// Publishes a replacement generation during the next `lookup`, after
        /// the stale lease is opened, reproducing a fill that commits between
        /// an expired lookup and its delayed eviction without threads.
        replace_after_lookup: Mutex<Option<StoredMeta>>,
        lookup_calls: AtomicUsize,
        peek_calls: AtomicUsize,
        evict_calls: AtomicUsize,
    }

    /// In-memory fake `Storage` backed by a temp dir so a [`Lease`] can hold a
    /// real `std::fs::File`.
    struct FakeStorage {
        shared: Arc<Shared>,
        _dir: TempDir,
    }

    impl FakeStorage {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let shared = Arc::new(Shared {
                root: dir.path().to_path_buf(),
                entries: Mutex::new(HashMap::new()),
                counter: AtomicU64::new(0),
                fail_write: AtomicBool::new(false),
                fail_commit: AtomicBool::new(false),
                commit_gate: Mutex::new(None),
                replace_after_lookup: Mutex::new(None),
                lookup_calls: AtomicUsize::new(0),
                peek_calls: AtomicUsize::new(0),
                evict_calls: AtomicUsize::new(0),
            });
            Self { shared, _dir: dir }
        }

        fn set_commit_gate(&self, started: mpsc::Sender<()>, release: mpsc::Receiver<()>) {
            *self.shared.commit_gate.lock() = Some(CommitGate { started, release });
        }
    }

    impl Storage for FakeStorage {
        fn lookup(&self, key: &str) -> Result<Lookup> {
            self.shared.lookup_calls.fetch_add(1, Ordering::SeqCst);
            let mut entries = self.shared.entries.lock();
            let Some((path, meta)) = entries.get(key).cloned() else {
                return Ok(Lookup::Miss);
            };
            let file = File::open(&path)?;
            if let Some(replacement) = self.shared.replace_after_lookup.lock().take() {
                entries.insert(key.to_owned(), (path.clone(), replacement));
            }
            Ok(Lookup::Hit(Lease {
                path,
                meta,
                file,
                body_offset: 0,
            }))
        }

        fn peek(&self, key: &str) -> Result<Option<StoredMeta>> {
            self.shared.peek_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .shared
                .entries
                .lock()
                .get(key)
                .map(|(_, meta)| meta.clone()))
        }

        fn begin_fill(&self, key: &str, meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
            let id = self.shared.counter.fetch_add(1, Ordering::SeqCst);
            let staging = self.shared.root.join(format!("staging-{id}.tmp"));
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&staging)?;
            Ok(Box::new(FakeWriter {
                shared: Arc::clone(&self.shared),
                key: key.to_owned(),
                meta,
                staging,
                file,
                written: 0,
            }))
        }

        fn evict_data(&self, key: &str, revision: u64) -> Result<bool> {
            self.shared.evict_calls.fetch_add(1, Ordering::SeqCst);
            let mut entries = self.shared.entries.lock();
            if entries
                .get(key)
                .is_none_or(|(_, meta)| meta.revision != revision)
            {
                return Ok(false);
            }
            if let Some((path, _)) = entries.remove(key) {
                let _ = std::fs::remove_file(path);
            }
            Ok(true)
        }

        fn remove(&self, key: &str) -> Result<()> {
            let mut entries = self.shared.entries.lock();
            if let Some((path, _)) = entries.remove(key) {
                let _ = std::fs::remove_file(path);
            }
            Ok(())
        }

        fn recover_on_open(&self) -> Result<ovcc_storage::RecoveryReport> {
            Ok(ovcc_storage::RecoveryReport::default())
        }
    }

    struct FakeWriter {
        shared: Arc<Shared>,
        key: String,
        meta: StoredMeta,
        staging: PathBuf,
        file: File,
        written: u64,
    }

    impl StoreWrite for FakeWriter {
        fn write_chunk(&mut self, buf: &[u8]) -> Result<()> {
            if self.shared.fail_write.load(Ordering::SeqCst) {
                return Err(Error::storage("injected staging write failure"));
            }
            self.file.write_all(buf)?;
            self.written += buf.len() as u64;
            Ok(())
        }

        fn in_progress_read(&self) -> Result<InProgressRead> {
            Ok(InProgressRead {
                file: Arc::new(File::open(&self.staging)?),
                body_offset: 0,
            })
        }

        fn commit(mut self: Box<Self>) -> Result<Lease> {
            self.file.flush()?;
            if let Some(gate) = self.shared.commit_gate.lock().take() {
                gate.started.send(()).unwrap();
                gate.release.recv().unwrap();
            }
            if self.shared.fail_commit.load(Ordering::SeqCst) {
                let _ = std::fs::remove_file(&self.staging);
                return Err(Error::storage("injected metadata commit failure"));
            }
            let final_path = self.shared.root.join(format!(
                "object-{}.bin",
                self.shared.counter.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::rename(&self.staging, &final_path)?;
            let mut meta = self.meta.clone();
            meta.size = self.written;
            self.shared
                .entries
                .lock()
                .insert(self.key.clone(), (final_path.clone(), meta.clone()));
            let file = File::open(&final_path)?;
            Ok(Lease {
                path: final_path,
                meta,
                file,
                body_offset: 0,
            })
        }

        fn abort(self: Box<Self>) {
            let _ = std::fs::remove_file(&self.staging);
        }
    }

    #[test]
    fn get_on_empty_is_none() {
        let cache = Cache::new(FakeStorage::new());
        assert!(cache.get("missing").unwrap().is_none());
    }

    #[test]
    fn fill_then_get_returns_hit_with_bytes() {
        let cache = Cache::new(FakeStorage::new());
        let body = b"hello, ovcc";

        let mut writer = cache.begin_fill("k1", StoredMeta::default()).unwrap();
        writer.write_chunk(body).unwrap();
        writer.commit().unwrap();

        let (mut lease, status) = cache.get("k1").unwrap().expect("expected a hit");
        assert_eq!(status, CacheStatus::Hit);
        assert_eq!(lease.meta.size, body.len() as u64);

        let mut got = Vec::new();
        lease.file.read_to_end(&mut got).unwrap();
        assert_eq!(got, body);
    }

    fn shareable_meta(size: u64) -> StoredMeta {
        StoredMeta {
            size,
            provenance: CacheProvenance::PeerCopy,
            etag: Some("\"peer-v1\"".to_owned()),
            ..StoredMeta::default()
        }
    }

    #[test]
    fn disk_backed_shareable_fill_uses_raw_offsets_and_commits_normally() {
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(DiskStorage::open(temp.path()).unwrap());
        let permit = cache.try_acquire_fill("disk-tail").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("disk-tail", shareable_meta(6), permit)
            .unwrap();
        let follower = cache.join_in_progress("disk-tail").unwrap();

        writer.write_chunk(b"abc").unwrap();
        let first = follower.readable_window().unwrap().unwrap();
        assert_eq!(first.file_offset(), 0);
        assert_eq!(first.file().metadata().unwrap().len(), 3);
        assert_eq!(first.len(), 3);
        let mut bytes = [0u8; 3];
        first
            .file()
            .read_exact_at(&mut bytes, first.file_offset())
            .unwrap();
        assert_eq!(&bytes, b"abc");

        writer.write_chunk(b"def").unwrap();
        writer.commit().unwrap();
        assert_eq!(follower.progress().state(), FillProgressState::Committed);
        let (mut lease, status) = cache.get("disk-tail").unwrap().unwrap();
        assert_eq!(status, CacheStatus::Hit);
        assert_eq!(lease.meta.provenance, CacheProvenance::PeerCopy);
        assert_eq!(lease.body_offset, 0);
        assert_eq!(lease.file.metadata().unwrap().len(), 6);
        lease
            .file
            .seek(std::io::SeekFrom::Start(lease.body_offset))
            .unwrap();
        let mut body = Vec::new();
        lease.file.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"abcdef");
    }

    #[test]
    fn non_shareable_fills_are_invisible_and_join_does_no_storage_work() {
        let storage = FakeStorage::new();
        let shared = Arc::clone(&storage.shared);
        let cache = Cache::new(storage);
        let ordinary = cache.begin_fill("ordinary", shareable_meta(4)).unwrap();
        assert!(cache.join_in_progress("ordinary").is_none());
        ordinary.abort();

        let permit = cache.try_acquire_fill("shareable").unwrap();
        let writer = cache
            .begin_shareable_acquired_fill("shareable", shareable_meta(4), permit)
            .unwrap();
        let lookup_before = shared.lookup_calls.load(Ordering::SeqCst);
        let peek_before = shared.peek_calls.load(Ordering::SeqCst);
        let evict_before = shared.evict_calls.load(Ordering::SeqCst);
        let lock_count = cache.key_locks.len();

        assert!(cache.join_in_progress("shareable").is_some());
        assert!(cache.begin_fill("shareable", shareable_meta(4)).is_err());
        assert!(cache.join_in_progress("missing").is_none());
        assert_eq!(shared.lookup_calls.load(Ordering::SeqCst), lookup_before);
        assert_eq!(shared.peek_calls.load(Ordering::SeqCst), peek_before);
        assert_eq!(shared.evict_calls.load(Ordering::SeqCst), evict_before);
        assert_eq!(cache.key_locks.len(), lock_count);
        writer.abort();
    }

    #[tokio::test]
    async fn shareable_fill_has_monotonic_frontier_late_join_and_independent_clones() {
        let cache = Cache::new(FakeStorage::new());
        let permit = cache.try_acquire_fill("tail").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("tail", shareable_meta(12), permit)
            .unwrap();
        let mut early = cache.join_in_progress("tail").unwrap();
        assert_eq!(
            early.progress(),
            FillProgress {
                published: 0,
                expected_size: 12,
                state: FillProgressState::Receiving,
            }
        );

        writer.write_chunk(b"first").unwrap();
        assert_eq!(early.wait_for_progress().await.unwrap().published(), 5);
        writer.write_chunk(b"-second").unwrap();
        let progress = early.wait_for_progress().await.unwrap();
        assert_eq!(progress.published(), 12);
        assert_eq!(progress.state(), FillProgressState::Receiving);

        let mut late = cache.join_in_progress("tail").unwrap();
        assert_eq!(late.offset(), 0);
        assert_eq!(late.progress().published(), 12);
        assert_eq!(late.meta().etag.as_deref(), Some("\"peer-v1\""));
        let mut independent = late.clone();
        let window = late.readable_window().unwrap().unwrap();
        assert_eq!(window.file_offset(), 0);
        assert_eq!(window.len(), 12);
        let mut bytes = [0u8; 12];
        window
            .file()
            .read_exact_at(&mut bytes, window.file_offset())
            .unwrap();
        assert_eq!(&bytes, b"first-second");

        late.advance(5).unwrap();
        assert_eq!(late.offset(), 5);
        assert_eq!(late.readable_window().unwrap().unwrap().len(), 7);
        assert_eq!(independent.offset(), 0);
        assert_eq!(independent.readable_window().unwrap().unwrap().len(), 12);
        assert!(independent.advance(13).is_err());
        independent.advance(12).unwrap();

        writer.commit().unwrap();
        let late_terminal = late.wait_for_progress().await.unwrap();
        let independent_terminal = independent.wait_for_progress().await.unwrap();
        assert_eq!(late_terminal.state(), FillProgressState::Committed);
        assert_eq!(independent_terminal.state(), FillProgressState::Committed);
        assert_eq!(early.progress().published(), 12);
        assert_eq!(early.progress().state(), FillProgressState::Committed);
        assert!(cache.join_in_progress("tail").is_none());
    }

    #[test]
    fn shareable_commit_signals_body_complete_before_durable_commit() {
        let storage = FakeStorage::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        storage.set_commit_gate(started_tx, release_rx);
        let cache = Arc::new(Cache::new(storage));
        let permit = cache.try_acquire_fill("gated").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("gated", shareable_meta(4), permit)
            .unwrap();
        let follower = cache.join_in_progress("gated").unwrap();
        writer.write_chunk(b"body").unwrap();

        let commit_thread = thread::spawn(move || writer.commit());
        started_rx.recv().unwrap();
        assert_eq!(
            follower.progress(),
            FillProgress {
                published: 4,
                expected_size: 4,
                state: FillProgressState::BodyComplete,
            }
        );
        assert!(cache.join_in_progress("gated").is_some());

        release_tx.send(()).unwrap();
        commit_thread.join().unwrap().unwrap();
        assert_eq!(follower.progress().state(), FillProgressState::Committed);
        assert!(cache.join_in_progress("gated").is_none());
    }

    #[test]
    fn exact_body_remains_readable_after_commit_failure_unlinks_staging() {
        let storage = FakeStorage::new();
        storage.shared.fail_commit.store(true, Ordering::SeqCst);
        let cache = Cache::new(storage);
        let permit = cache.try_acquire_fill("commit-fail").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("commit-fail", shareable_meta(6), permit)
            .unwrap();
        let follower = cache.join_in_progress("commit-fail").unwrap();
        writer.write_chunk(b"abcdef").unwrap();

        assert!(writer.commit().is_err());
        assert_eq!(follower.progress().published(), 6);
        assert_eq!(follower.progress().state(), FillProgressState::CommitFailed);
        assert!(cache.join_in_progress("commit-fail").is_none());
        assert_eq!(cache.key_locks.len(), 0);
        let window = follower.readable_window().unwrap().unwrap();
        let mut bytes = [0u8; 6];
        window
            .file()
            .read_exact_at(&mut bytes, window.file_offset())
            .unwrap();
        assert_eq!(&bytes, b"abcdef");
    }

    #[test]
    fn shareable_fill_abort_drop_and_short_commit_signal_aborted() {
        let cache = Cache::new(FakeStorage::new());

        let permit = cache.try_acquire_fill("abort").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("abort", shareable_meta(4), permit)
            .unwrap();
        let aborted = cache.join_in_progress("abort").unwrap();
        writer.write_chunk(b"ab").unwrap();
        writer.abort();
        assert_eq!(aborted.progress().published(), 2);
        assert_eq!(aborted.progress().state(), FillProgressState::Aborted);
        assert!(cache.join_in_progress("abort").is_none());
        assert_eq!(cache.key_locks.len(), 0);

        let permit = cache.try_acquire_fill("drop").unwrap();
        let writer = cache
            .begin_shareable_acquired_fill("drop", shareable_meta(4), permit)
            .unwrap();
        let dropped = cache.join_in_progress("drop").unwrap();
        drop(writer);
        assert_eq!(dropped.progress().state(), FillProgressState::Aborted);
        assert!(cache.join_in_progress("drop").is_none());
        assert_eq!(cache.key_locks.len(), 0);

        let permit = cache.try_acquire_fill("short").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("short", shareable_meta(4), permit)
            .unwrap();
        let short = cache.join_in_progress("short").unwrap();
        writer.write_chunk(b"abc").unwrap();
        assert!(writer.commit().is_err());
        assert_eq!(short.progress().published(), 3);
        assert_eq!(short.progress().state(), FillProgressState::Aborted);
        assert!(cache.join_in_progress("short").is_none());
        assert_eq!(cache.key_locks.len(), 0);
    }

    #[test]
    fn shareable_fill_write_failure_and_overrun_never_publish_unsafe_bytes() {
        let storage = FakeStorage::new();
        let shared = Arc::clone(&storage.shared);
        let cache = Cache::new(storage);
        let permit = cache.try_acquire_fill("write-fail").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("write-fail", shareable_meta(4), permit)
            .unwrap();
        let failed = cache.join_in_progress("write-fail").unwrap();
        writer.write_chunk(b"ab").unwrap();
        shared.fail_write.store(true, Ordering::SeqCst);
        assert!(writer.write_chunk(b"cd").is_err());
        assert_eq!(failed.progress().published(), 2);
        assert_eq!(failed.progress().state(), FillProgressState::Aborted);
        assert!(cache.join_in_progress("write-fail").is_none());
        drop(writer);

        shared.fail_write.store(false, Ordering::SeqCst);
        let permit = cache.try_acquire_fill("overrun").unwrap();
        let mut writer = cache
            .begin_shareable_acquired_fill("overrun", shareable_meta(3), permit)
            .unwrap();
        let overrun = cache.join_in_progress("overrun").unwrap();
        assert!(writer.write_chunk(b"four").is_err());
        assert_eq!(overrun.progress().published(), 0);
        assert_eq!(overrun.progress().state(), FillProgressState::Aborted);
        assert!(cache.join_in_progress("overrun").is_none());
    }

    fn herd_policy(lock_age_secs: u64, lock_timeout_secs: u64) -> EvictionPolicy {
        EvictionPolicy {
            lock_age_secs,
            lock_timeout_secs,
            ..EvictionPolicy::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_waiter_shares_committed_fill() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(60, 120)));
        let permit = match cache.acquire_fill("shared").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let mut writer = cache
            .begin_acquired_fill("shared", StoredMeta::default(), permit)
            .unwrap();
        writer.write_chunk(b"shared body").unwrap();
        let waiter_cache = Arc::clone(&cache);
        let waiter =
            tokio::spawn(async move { waiter_cache.acquire_fill("shared").await.unwrap() });
        tokio::task::yield_now().await;

        // Act.
        writer.commit().unwrap();
        let decision = waiter.await.unwrap();

        // Assert.
        let FillDecision::Shared(mut lease) = decision else {
            panic!("waiter must share the owner's committed entry");
        };
        let mut body = Vec::new();
        lease.file.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"shared body");
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_wait_timeout_bypasses_admission() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(60, 5)));
        let owner = match cache.acquire_fill("timeout").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let waiter_cache = Arc::clone(&cache);
        let waiter =
            tokio::spawn(async move { waiter_cache.acquire_fill("timeout").await.unwrap() });
        tokio::task::yield_now().await;

        // Act.
        tokio::time::advance(Duration::from_secs(5)).await;
        let decision = waiter.await.unwrap();

        // Assert.
        assert!(matches!(decision, FillDecision::Bypass));
        assert_eq!(cache.key_locks.len(), 1);
        drop(owner);
        assert_eq!(cache.key_locks.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_stale_owner_age_releases_waiter() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(3, 60)));
        let owner = match cache.acquire_fill("stale").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let waiter_cache = Arc::clone(&cache);
        let waiter = tokio::spawn(async move { waiter_cache.acquire_fill("stale").await.unwrap() });
        tokio::task::yield_now().await;

        // Act.
        tokio::time::advance(Duration::from_secs(3)).await;
        let decision = waiter.await.unwrap();

        // Assert.
        assert!(matches!(decision, FillDecision::Bypass));
        assert!(cache.get("stale").unwrap().is_none());
        assert_eq!(cache.key_locks.len(), 1);
        drop(owner);
        assert_eq!(cache.key_locks.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_cancelled_waiter_leaves_owner_safe() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(60, 120)));
        let permit = match cache.acquire_fill("cancel").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let mut writer = cache
            .begin_acquired_fill("cancel", StoredMeta::default(), permit)
            .unwrap();
        writer.write_chunk(b"owner body").unwrap();
        let waiter_cache = Arc::clone(&cache);
        let waiter =
            tokio::spawn(async move { waiter_cache.acquire_fill("cancel").await.unwrap() });
        tokio::task::yield_now().await;

        // Act.
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(cache.key_locks.len(), 1);
        writer.commit().unwrap();
        let decision = cache.acquire_fill("cancel").await.unwrap();

        // Assert.
        assert!(matches!(decision, FillDecision::Shared(_)));
        assert_eq!(cache.key_locks.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_fallback_cannot_replace_active_writer() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(60, 2)));
        let permit = match cache.acquire_fill("active").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let mut writer = cache
            .begin_acquired_fill("active", StoredMeta::default(), permit)
            .unwrap();
        writer.write_chunk(b"owner").unwrap();
        let waiter_cache = Arc::clone(&cache);
        let waiter =
            tokio::spawn(async move { waiter_cache.acquire_fill("active").await.unwrap() });
        tokio::task::yield_now().await;

        // Act.
        tokio::time::advance(Duration::from_secs(2)).await;
        let fallback = waiter.await.unwrap();
        let competing = cache.begin_fill("active", StoredMeta::default());
        writer.commit().unwrap();

        // Assert.
        assert!(matches!(fallback, FillDecision::Bypass));
        assert!(competing.is_err());
        let (mut lease, _) = cache.get("active").unwrap().unwrap();
        let mut body = Vec::new();
        lease.file.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"owner");
    }

    #[tokio::test(start_paused = true)]
    async fn cache_fill_herd_lock_entry_lives_only_while_held() {
        // Arrange.
        let cache = Arc::new(Cache::with_policy(FakeStorage::new(), herd_policy(60, 120)));
        let permit = match cache.acquire_fill("held").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("cold key must elect an owner"),
        };
        let mut writer = cache
            .begin_acquired_fill("held", StoredMeta::default(), permit)
            .unwrap();
        writer.write_chunk(b"held body").unwrap();
        let waiter_cache = Arc::clone(&cache);
        let waiter = tokio::spawn(async move { waiter_cache.acquire_fill("held").await.unwrap() });
        tokio::task::yield_now().await;
        let while_held = cache.key_locks.len();

        // Act.
        writer.commit().unwrap();
        let decision = waiter.await.unwrap();
        let after_release = cache.key_locks.len();
        for index in 0..64 {
            let key = format!("churn-{index}");
            let FillDecision::Owner(permit) = cache.acquire_fill(&key).await.unwrap() else {
                panic!("cold key must elect an owner");
            };
            cache
                .begin_acquired_fill(&key, StoredMeta::default(), permit)
                .unwrap()
                .commit()
                .unwrap();
        }

        // Assert.
        assert_eq!(while_held, 1);
        assert!(matches!(decision, FillDecision::Shared(_)));
        assert_eq!(after_release, 0);
        assert_eq!(cache.key_locks.len(), 0);
        assert!(matches!(
            cache.acquire_refill("held").await.unwrap(),
            FillDecision::Owner(_)
        ));
    }

    #[tokio::test]
    async fn cache_fill_registration_watcher_sees_later_shareable_writer() {
        // Arrange.
        let cache = Cache::new(FakeStorage::new());
        let mut registration = cache.subscribe_fill_registration("watched");

        // Act.
        let permit = cache.try_acquire_fill("watched").unwrap();
        let writer = cache
            .begin_shareable_acquired_fill("watched", shareable_meta(4), permit)
            .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), registration.changed()).await;

        // Assert.
        assert!(matches!(changed, Ok(Ok(()))));
        assert!(cache.join_in_progress("watched").is_some());
        writer.abort();
        assert_eq!(cache.key_locks.len(), 1);
        let replacement = cache.try_acquire_fill("watched").unwrap();
        let writer = cache
            .begin_shareable_acquired_fill("watched", shareable_meta(4), replacement)
            .unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(5), registration.changed()).await;
        assert!(matches!(changed, Ok(Ok(()))));
        drop(registration);
        assert_eq!(cache.key_locks.len(), 1);
        drop(writer);
        assert_eq!(cache.key_locks.len(), 0);
    }

    #[test]
    fn cache_fill_herd_idle_manager_reclaims_entries_and_table_capacity() {
        let mut cache = Cache::new(FakeStorage::new());
        cache.key_locks = ovcc_refmap::Builder::new()
            .shards(1)
            .maintain_steps(1)
            .build();
        let initial_capacity = cache.key_locks.stats().capacity;
        let permits: Vec<_> = (0..1024)
            .map(|index| cache.try_acquire_fill(&format!("burst-{index}")).unwrap())
            .collect();
        assert_eq!(cache.key_locks.len(), permits.len());
        assert!(cache.key_locks.stats().capacity > initial_capacity);

        drop(permits);
        assert_eq!(cache.key_locks.len(), 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut follow_ups = 0;
        loop {
            let report = cache.manager_step().unwrap();
            assert_eq!(report.evicted_count, 0);
            let stats = cache.key_locks.stats();
            if stats.capacity == initial_capacity && stats.retired == 0 && stats.migrating == 0 {
                assert_eq!(report.next_delay, IDLE_MANAGER_DELAY);
                break;
            }
            assert_eq!(report.next_delay, PRODUCTIVE_MANAGER_DELAY);
            follow_ups += 1;
            assert!(
                std::time::Instant::now() < deadline,
                "undrained map: {stats:?}"
            );
            std::thread::yield_now();
        }
        assert!(
            follow_ups > 0,
            "the burst must require multiple manager passes"
        );
    }

    #[test]
    fn distinct_keys_proceed_concurrently() {
        const N: usize = 4;
        let cache = Arc::new(Cache::new(FakeStorage::new()));
        // A barrier of N only releases once all N fills are simultaneously
        // live; if distinct keys serialized, this would deadlock.
        let barrier = Arc::new(Barrier::new(N));
        let max_active = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..N {
            let cache = Arc::clone(&cache);
            let barrier = Arc::clone(&barrier);
            let max_active = Arc::clone(&max_active);
            let active = Arc::clone(&active);
            handles.push(thread::spawn(move || {
                let writer = cache
                    .begin_fill(&format!("key-{i}"), StoredMeta::default())
                    .unwrap();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_active.fetch_max(now, Ordering::SeqCst);
                barrier.wait();
                active.fetch_sub(1, Ordering::SeqCst);
                writer.abort();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(max_active.load(Ordering::SeqCst), N);
    }

    #[derive(Default)]
    struct PolicyStorage {
        candidates: Mutex<Vec<EvictionCandidate>>,
        evicted: Mutex<Vec<String>>,
        available: AtomicU64,
        release_bytes_on_evict: AtomicBool,
        reject_before_admission: AtomicBool,
        /// Bumps every stored revision right after the manager snapshots
        /// candidates, modelling a fill that commits between selection and the
        /// delayed eviction.
        replace_after_snapshot: AtomicBool,
        /// Counts every `evict_data` call, including refusals, so a pass's
        /// storage work stays observable when its report shows none.
        evict_attempts: AtomicUsize,
    }

    impl Storage for PolicyStorage {
        fn lookup(&self, _key: &str) -> Result<Lookup> {
            Ok(Lookup::Miss)
        }

        fn peek(&self, _key: &str) -> Result<Option<StoredMeta>> {
            Ok(None)
        }

        fn begin_fill(&self, _key: &str, _meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
            Err(Error::storage("not used by policy tests"))
        }

        fn adopt_segmented_with_admission(
            &self,
            _key: &str,
            _meta: StoredMeta,
            _segments: &[StagedSegment],
            admit: &mut dyn FnMut() -> Result<()>,
        ) -> Result<Lease> {
            if self.reject_before_admission.load(Ordering::SeqCst) {
                return Err(Error::storage("source changed before admission"));
            }
            admit()?;
            Err(Error::storage(
                "segmented adoption is unsupported by policy test storage",
            ))
        }

        fn evict_data(&self, key: &str, revision: u64) -> Result<bool> {
            self.evict_attempts.fetch_add(1, Ordering::SeqCst);
            let mut candidates = self.candidates.lock();
            let Some(index) = candidates
                .iter()
                .position(|candidate| candidate.key == key && candidate.revision == revision)
            else {
                return Ok(false);
            };
            let candidate = candidates.remove(index);
            drop(candidates);
            self.evicted.lock().push(key.to_owned());
            if self.release_bytes_on_evict.load(Ordering::SeqCst) {
                self.available
                    .fetch_add(candidate.size_bytes, Ordering::SeqCst);
            }
            Ok(true)
        }

        fn remove(&self, _key: &str) -> Result<()> {
            Ok(())
        }

        fn recover_on_open(&self) -> Result<ovcc_storage::RecoveryReport> {
            Ok(ovcc_storage::RecoveryReport::default())
        }

        fn eviction_candidates(&self) -> Result<Vec<EvictionCandidate>> {
            let mut candidates = self.candidates.lock();
            let snapshot = candidates.clone();
            if self.replace_after_snapshot.swap(false, Ordering::SeqCst) {
                for candidate in candidates.iter_mut() {
                    candidate.revision = candidate.revision.wrapping_add(1);
                }
            }
            Ok(snapshot)
        }

        fn available_bytes(&self) -> Result<u64> {
            Ok(self.available.load(Ordering::SeqCst))
        }
    }

    fn candidate(
        key: &str,
        size_bytes: u64,
        last_access: i64,
        hit_count: u64,
    ) -> EvictionCandidate {
        candidate_with_provenance(
            key,
            size_bytes,
            last_access,
            hit_count,
            CacheProvenance::Origin,
        )
    }

    fn candidate_with_provenance(
        key: &str,
        size_bytes: u64,
        last_access: i64,
        hit_count: u64,
        provenance: CacheProvenance,
    ) -> EvictionCandidate {
        EvictionCandidate {
            key: key.to_owned(),
            revision: 0,
            size_bytes,
            last_access,
            hit_count,
            provenance,
            max_idle_secs: None,
        }
    }

    #[test]
    fn cache_get_ttl_expires_at_deadline() {
        // Arrange.
        let cache = Cache::new(FakeStorage::new());
        let mut writer = cache
            .begin_fill(
                "ttl",
                StoredMeta {
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();

        // Act.
        let before = cache.get_at("ttl", 99).unwrap();
        let at_deadline = cache.get_at("ttl", 100).unwrap();

        // Assert.
        assert!(before.is_some());
        assert!(at_deadline.is_none());
    }

    #[test]
    fn cache_get_expired_hit_preserves_replacement_generation() {
        // Arrange. Generation one is expired, so a lookup at its deadline
        // decides to evict it.
        let cache = Cache::new(FakeStorage::new());
        let mut writer = cache
            .begin_fill(
                "expired",
                StoredMeta {
                    revision: 1,
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"stale").unwrap();
        writer.commit().unwrap();
        // Generation two commits between that lookup and the eviction.
        *cache.storage().shared.replace_after_lookup.lock() = Some(StoredMeta {
            revision: 2,
            expires_at: Some(1_000),
            ..StoredMeta::default()
        });

        // Act.
        let expired = cache.get_at("expired", 100).unwrap();

        // Assert. Expiry still governs the generation the lookup observed.
        assert!(expired.is_none());

        // Act.
        let served = cache.get_at("expired", 100).unwrap();

        // Assert. Generation two kept its bytes, so the next lookup serves it.
        let (lease, status) = served.expect("the replacement generation survives");
        assert_eq!(status, CacheStatus::Hit);
        assert_eq!(lease.meta.revision, 2);
    }

    #[test]
    fn cache_fill_applies_status_specific_normal_validity() {
        // Arrange.
        let cache = Cache::with_policy(
            FakeStorage::new(),
            EvictionPolicy {
                validity_200_secs: Some(86_400),
                validity_206_secs: Some(43_200),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let full = cache
            .begin_fill_at("full", StoredMeta::default(), 1_000)
            .unwrap()
            .commit()
            .unwrap();
        let partial = cache
            .begin_fill_at(
                "partial",
                StoredMeta {
                    content_range: Some("bytes 0-0/2".to_owned()),
                    ..StoredMeta::default()
                },
                1_000,
            )
            .unwrap()
            .commit()
            .unwrap();

        // Assert.
        assert_eq!(full.meta.expires_at, Some(87_400));
        assert_eq!(partial.meta.expires_at, Some(44_200));
    }

    #[tokio::test]
    async fn cache_refresh_acquired_extends_exact_body_without_refill() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(temp.path()).unwrap(),
            EvictionPolicy {
                validity_200_secs: Some(10),
                validity_206_secs: Some(5),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_at(
                "revalidate",
                StoredMeta {
                    etag: Some("\"etag-1\"".to_owned()),
                    last_modified: Some("Mon, 24 Aug 2026 12:00:00 GMT".to_owned()),
                    ..StoredMeta::default()
                },
                100,
            )
            .unwrap();
        writer.write_chunk(b"unchanged body").unwrap();
        writer.commit().unwrap();
        let stale = match cache.lookup("revalidate").unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected stale generation, got {other:?}"),
        };
        assert_eq!(stale.meta.expires_at, Some(110));
        let expected = stale.meta.clone();
        drop(stale);
        let permit = match cache.acquire_refill("revalidate").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("uncontended revalidation must own the herd lock"),
        };

        // Act.
        let mut refreshed = cache
            .refresh_acquired_at("revalidate", &expected, permit, 110)
            .unwrap()
            .expect("exact stale generation refreshes");

        // Assert.
        assert_eq!(refreshed.meta.expires_at, Some(120));
        refreshed
            .file
            .seek(std::io::SeekFrom::Start(refreshed.body_offset))
            .unwrap();
        let mut body = Vec::new();
        refreshed.file.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"unchanged body");
        assert!(cache.get_at("revalidate", 119).unwrap().is_some());
    }

    #[tokio::test]
    async fn cache_refresh_acquired_uses_partial_content_validity() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(temp.path()).unwrap(),
            EvictionPolicy {
                validity_200_secs: Some(10),
                validity_206_secs: Some(5),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_at(
                "partial-revalidate",
                StoredMeta {
                    status: 206,
                    etag: Some("\"slice-1\"".to_owned()),
                    content_range: Some("bytes 0-3/8".to_owned()),
                    ..StoredMeta::default()
                },
                100,
            )
            .unwrap();
        writer.write_chunk(b"abcd").unwrap();
        writer.commit().unwrap();
        let stale = match cache.lookup("partial-revalidate").unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected stale partial generation, got {other:?}"),
        };
        assert_eq!(stale.meta.expires_at, Some(105));
        let expected = stale.meta.clone();
        drop(stale);
        let permit = match cache.acquire_refill("partial-revalidate").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("uncontended revalidation must own the herd lock"),
        };

        // Act.
        let refreshed = cache
            .refresh_acquired_at("partial-revalidate", &expected, permit, 105)
            .unwrap()
            .expect("exact stale partial generation refreshes");

        // Assert.
        assert_eq!(refreshed.meta.status, 206);
        assert_eq!(refreshed.meta.expires_at, Some(110));
        assert_eq!(refreshed.meta.content_range.as_deref(), Some("bytes 0-3/8"));
    }

    #[tokio::test]
    async fn cache_refresh_acquired_accepts_location_specific_deadline() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(temp.path()).unwrap(),
            EvictionPolicy {
                validity_200_secs: Some(3_600),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_preserving_expiry(
                "location-validity",
                StoredMeta {
                    etag: Some("\"list-v1\"".to_owned()),
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"cached list").unwrap();
        writer.commit().unwrap();
        let stale = match cache.lookup("location-validity").unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected stale location entry, got {other:?}"),
        };
        let expected = stale.meta.clone();
        drop(stale);
        let permit = match cache.acquire_refill("location-validity").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("uncontended revalidation must own the herd lock"),
        };

        // Act.
        let refreshed = cache
            .refresh_acquired_until("location-validity", &expected, permit, 130)
            .unwrap()
            .expect("exact location entry refreshes");

        // Assert.
        assert_eq!(refreshed.meta.expires_at, Some(130));
        assert_ne!(refreshed.meta.expires_at, Some(3_700));
    }

    #[tokio::test]
    async fn cache_refresh_acquired_changed_revision_returns_none_and_releases_permit() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(temp.path()).unwrap(),
            EvictionPolicy {
                validity_200_secs: Some(10),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_at(
                "revalidation-race",
                StoredMeta {
                    etag: Some("\"stable\"".to_owned()),
                    ..StoredMeta::default()
                },
                100,
            )
            .unwrap();
        writer.write_chunk(b"old!").unwrap();
        writer.commit().unwrap();
        let expected = cache.peek("revalidation-race").unwrap().unwrap();
        let permit = match cache.acquire_refill("revalidation-race").await.unwrap() {
            FillDecision::Owner(permit) => permit,
            _ => panic!("uncontended revalidation must own the herd lock"),
        };
        let mut replacement = cache
            .storage()
            .begin_fill("revalidation-race", expected.clone())
            .unwrap();
        replacement.write_chunk(b"new!").unwrap();
        replacement.commit().unwrap();

        // Act.
        let refreshed = cache
            .refresh_acquired_at("revalidation-race", &expected, permit, 110)
            .unwrap();

        // Assert.
        assert!(refreshed.is_none());
        let current = cache.peek("revalidation-race").unwrap().unwrap();
        assert_ne!(current.revision, expected.revision);
        assert_eq!(current.etag, expected.etag);
        let reacquired = cache.acquire_refill("revalidation-race").await.unwrap();
        assert!(matches!(reacquired, FillDecision::Owner(_)));
    }

    #[tokio::test]
    async fn cache_refresh_acquired_without_validity_returns_none_and_releases_permit() {
        // Arrange.
        let temp = tempfile::tempdir().unwrap();
        let cache = Cache::new(DiskStorage::open(temp.path()).unwrap());
        let mut writer = cache
            .begin_fill_preserving_expiry(
                "revalidation-without-validity",
                StoredMeta {
                    etag: Some("\"stable\"".to_owned()),
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();
        let expected = cache
            .peek("revalidation-without-validity")
            .unwrap()
            .unwrap();
        let permit = match cache
            .acquire_refill("revalidation-without-validity")
            .await
            .unwrap()
        {
            FillDecision::Owner(permit) => permit,
            _ => panic!("uncontended revalidation must own the herd lock"),
        };

        // Act.
        let refreshed = cache
            .refresh_acquired_at("revalidation-without-validity", &expected, permit, 100)
            .unwrap();

        // Assert.
        assert!(refreshed.is_none());
        let reacquired = cache
            .acquire_refill("revalidation-without-validity")
            .await
            .unwrap();
        assert!(matches!(reacquired, FillDecision::Owner(_)));
    }

    #[test]
    fn cache_normal_lookup_refills_pre_validity_entry_once() {
        // Arrange.
        let cache = Cache::with_policy(
            FakeStorage::new(),
            EvictionPolicy {
                validity_200_secs: Some(86_400),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_preserving_expiry("legacy", StoredMeta::default())
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();

        // Act and assert.
        assert!(cache.get("legacy").unwrap().is_none());
    }

    #[test]
    fn cache_explicit_never_expiring_lookup_preserves_entry() {
        // Arrange.
        let cache = Cache::with_policy(
            FakeStorage::new(),
            EvictionPolicy {
                validity_200_secs: Some(86_400),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill_preserving_expiry("never", StoredMeta::default())
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();

        // Act and assert.
        assert!(cache.get_preserving_expiry("never").unwrap().is_some());
    }

    #[test]
    fn cache_admission_succeeds_despite_manager_pressure() {
        // Arrange.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_idle_secs: Some(0),
                max_bytes: Some(0),
                min_free_bytes: Some(u64::MAX),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let mut writer = cache
            .begin_fill(
                "admitted",
                StoredMeta {
                    size: 8,
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"admitted").unwrap();
        let committed = writer.commit();

        // Assert.
        assert!(committed.is_ok());
        assert!(cache.get("admitted").unwrap().is_some());
    }

    #[test]
    fn cache_manager_evicts_inactive_entries_oldest_first() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![
                candidate("hot", 10, 95, 10),
                candidate("idle-newer", 10, 20, 2),
                candidate("idle-oldest", 10, 10, 100),
            ]),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(50),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(
            *cache.storage().evicted.lock(),
            vec!["idle-oldest", "idle-newer"]
        );
        assert_eq!(report.evicted_count, 2);
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_uses_shorter_idle_horizon_for_peer_copies() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![
                candidate("origin", 10, 70, 1),
                candidate_with_provenance("peer-copy", 10, 80, 1, CacheProvenance::PeerCopy),
            ]),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(50),
                peer_copy_max_idle_secs: Some(20),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(*cache.storage().evicted.lock(), vec!["peer-copy"]);
        assert_eq!(report.evicted_count, 1);
        assert_eq!(report.peer_copy_idle_evicted_count, 1);
        assert_eq!(report.origin_idle_evicted_count, 0);
        assert_eq!(report.pressure_evicted_count, 0);
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_uses_minimum_global_peer_and_object_idle_horizon() {
        // Arrange.
        let mut object_limited =
            candidate_with_provenance("object-limited", 10, 85, 1, CacheProvenance::PeerCopy);
        object_limited.max_idle_secs = Some(10);
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![object_limited]),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(50),
                peer_copy_max_idle_secs: Some(20),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(*cache.storage().evicted.lock(), vec!["object-limited"]);
        assert_eq!(report.peer_copy_idle_evicted_count, 1);
    }

    #[test]
    fn cache_manager_step_ignores_replaced_generation() {
        // Arrange. One idle candidate whose generation is replaced between the
        // manager's snapshot and its eviction.
        let storage = PolicyStorage::default();
        storage
            .candidates
            .lock()
            .push(candidate("replaced", 100, 0, 0));
        storage.replace_after_snapshot.store(true, Ordering::SeqCst);
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(5),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert. Nothing was reclaimed, so nothing is reported or subtracted.
        assert_eq!(report.evicted_count, 0);
        assert_eq!(report.origin_idle_evicted_count, 0);
        assert_eq!(report.pressure_evicted_count, 0);
        assert_eq!(cache.storage().evict_attempts.load(Ordering::SeqCst), 1);
        assert!(cache.storage().evicted.lock().is_empty());
        assert_eq!(cache.storage().candidates.lock().len(), 1);
        assert_eq!(cache.storage().candidates.lock()[0].revision, 1);
        // An idle candidate a replacement just made active again is not worth
        // an immediate retry.
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_step_bounds_storage_work_when_every_eviction_is_refused() {
        // Arrange. More candidates than one pass may touch, every one of them
        // replaced after the snapshot.
        let candidates = (0..101)
            .map(|index| candidate(&format!("key-{index:03}"), 1, 0, 0))
            .collect();
        let storage = PolicyStorage {
            candidates: Mutex::new(candidates),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        storage.replace_after_snapshot.store(true, Ordering::SeqCst);
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(1),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert. Refusals still consume the work budget, so storage is not
        // asked to mutate more than a bounded pass allows.
        assert_eq!(
            cache.storage().evict_attempts.load(Ordering::SeqCst),
            MAX_EVICTIONS_PER_MANAGER_STEP
        );
        assert_eq!(report.evicted_count, 0);
        assert!(cache.storage().evicted.lock().is_empty());
        assert_eq!(report.next_delay, Duration::from_millis(50));
    }

    #[test]
    fn cache_admission_persists_the_effective_peer_idle_horizon() {
        // Arrange.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_idle_secs: Some(50),
                peer_copy_max_idle_secs: Some(20),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill(
                "peer-copy",
                StoredMeta {
                    provenance: CacheProvenance::PeerCopy,
                    max_idle_secs: Some(30),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();

        // Act.
        let lease = writer.commit().unwrap();

        // Assert.
        assert_eq!(lease.meta.max_idle_secs, Some(20));
        assert_eq!(
            cache.peek("peer-copy").unwrap().unwrap().max_idle_secs,
            Some(20)
        );
    }

    #[test]
    fn azure_get_backend_idle_eviction_unlinks_disk_body() {
        // Arrange.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_idle_secs: Some(50),
                peer_copy_max_idle_secs: Some(20),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill(
                "azure-ephemeral",
                StoredMeta {
                    max_idle_secs: Some(7),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"ephemeral artifact body").unwrap();
        let lease = writer.commit().unwrap();
        let body_path = lease.path.clone();
        let last_access = cache.storage().eviction_candidates().unwrap()[0].last_access;
        assert!(body_path.is_file());
        drop(lease);

        // Act.
        let before_deadline = cache.manager_step_at(last_access + 6).unwrap();
        let at_deadline = cache.manager_step_at(last_access + 7).unwrap();

        // Assert.
        assert_eq!(before_deadline.evicted_count, 0);
        assert_eq!(at_deadline.evicted_count, 1);
        assert_eq!(at_deadline.origin_idle_evicted_count, 1);
        assert!(!body_path.exists());
        assert!(matches!(
            cache.lookup("azure-ephemeral").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn general_evict_metadata_all_policy_paths_stay_absent_after_restart() -> Result<()> {
        for scenario in [
            "capacity",
            "min-free",
            "origin-idle",
            "peer-idle",
            "expiry",
            "legacy",
        ] {
            // Arrange.
            let dir = tempfile::tempdir()?;
            let storage = DiskStorage::open(dir.path())?;
            let policy = match scenario {
                "capacity" => EvictionPolicy {
                    max_bytes: Some(1),
                    ..EvictionPolicy::default()
                },
                "min-free" => EvictionPolicy {
                    min_free_bytes: Some(u64::MAX),
                    ..EvictionPolicy::default()
                },
                "origin-idle" => EvictionPolicy {
                    max_idle_secs: Some(1),
                    ..EvictionPolicy::default()
                },
                "peer-idle" => EvictionPolicy {
                    peer_copy_max_idle_secs: Some(1),
                    ..EvictionPolicy::default()
                },
                _ => EvictionPolicy {
                    validity_200_secs: Some(1),
                    ..EvictionPolicy::default()
                },
            };
            // Direct storage admission also covers records predating validity policy.
            let mut writer = storage.begin_fill(
                scenario,
                StoredMeta {
                    expires_at: (scenario == "expiry").then_some(1),
                    provenance: if scenario == "peer-idle" {
                        CacheProvenance::PeerCopy
                    } else {
                        CacheProvenance::Origin
                    },
                    ..StoredMeta::default()
                },
            )?;
            writer.write_chunk(b"body")?;
            let path = writer.commit()?.path.clone();
            let last_access = storage.eviction_candidates()?[0].last_access;
            let cache = Cache::with_policy(storage, policy);

            // Act.
            if matches!(scenario, "expiry" | "legacy") {
                assert!(cache.get_at(scenario, 2)?.is_none(), "{scenario}");
            } else {
                assert_eq!(
                    cache.manager_step_at(last_access + 1)?.evicted_count,
                    1,
                    "{scenario}"
                );
            }

            // Assert.
            assert!(!path.exists(), "{scenario}");
            assert!(
                matches!(cache.lookup(scenario)?, Lookup::Miss),
                "{scenario}"
            );
            drop(cache);
            let reopened = DiskStorage::open(dir.path())?;
            assert!(
                matches!(reopened.lookup(scenario)?, Lookup::Miss),
                "{scenario}"
            );
        }
        Ok(())
    }

    #[test]
    fn general_get_optional_keys_idle_eviction_retires_all_four_physical_keys() {
        use ovcc_cachekey::{CacheKey, KeyPart, LogicalKey, PhysicalKeyParts};
        use ovcc_storage::KeyPolicy;
        struct Policy;
        impl KeyPolicy for Policy {
            fn validate(&self, _: &LogicalKey) -> Result<()> {
                Ok(())
            }
            fn render(&self, parts: PhysicalKeyParts<'_>) -> CacheKey {
                CacheKey::from_raw(format!(
                    "object|{}|{}",
                    parts.get("a").flatten().unwrap_or(""),
                    parts.get("b").flatten().unwrap_or("")
                ))
            }
        }
        let logical = |a: Option<&str>, b: Option<&str>| {
            LogicalKey::new(
                "idle-test".into(),
                [("a", a), ("b", b)]
                    .into_iter()
                    .map(|(name, value)| KeyPart {
                        name: name.into(),
                        value: value.map(str::to_owned),
                        optional: true,
                    })
                    .collect(),
            )
            .unwrap()
        };
        // Arrange. Two optional parts register four keys for one body.
        let dir = tempfile::tempdir().unwrap();
        let storage =
            DiskStorage::open_with_key_policy(dir.path(), Some(Arc::new(Policy))).unwrap();
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(7),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache
            .begin_fill(
                "object|A|B",
                StoredMeta {
                    logical_key: Some(logical(Some("A"), Some("B"))),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"one body").unwrap();
        let path = writer.commit().unwrap().path.clone();
        for (a, b) in [
            (Some("A"), Some("B")),
            (None, Some("B")),
            (Some("A"), None),
            (None, None),
        ] {
            assert!(cache.lookup_logical(&logical(a, b)).unwrap().is_some());
        }
        let candidates = cache.storage().eviction_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].hit_count, 4);
        // Act.
        let before = cache
            .manager_step_at(candidates[0].last_access + 6)
            .unwrap();
        let expired = cache
            .manager_step_at(candidates[0].last_access + 7)
            .unwrap();
        // Assert.
        assert_eq!(before.evicted_count, 0);
        assert_eq!(expired.origin_idle_evicted_count, 1);
        assert!(!path.exists());
        for (a, b) in [
            (Some("A"), Some("B")),
            (None, Some("B")),
            (Some("A"), None),
            (None, None),
        ] {
            assert!(cache.peek_logical(&logical(a, b)).unwrap().is_none());
        }
    }

    #[test]
    fn cache_manager_evicts_lru_when_cached_bytes_reach_maximum() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![
                candidate("new", 40, 20, 1),
                candidate_with_provenance("old", 60, 10, 1, CacheProvenance::PeerCopy),
            ]),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_bytes: Some(100),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(*cache.storage().evicted.lock(), vec!["old"]);
        assert_eq!(report.evicted_count, 1);
        assert_eq!(report.pressure_evicted_count, 1);
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_pressure_prefers_older_origin_over_newer_peer_copy() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![
                candidate("origin-oldest", 60, 10, 1),
                candidate_with_provenance("peer-newer", 40, 20, 1, CacheProvenance::PeerCopy),
            ]),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_bytes: Some(100),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(*cache.storage().evicted.lock(), vec!["origin-oldest"]);
        assert_eq!(report.pressure_evicted_count, 1);
    }

    #[test]
    fn segmented_replacement_admission_never_evicts_unrelated_entry() {
        fn unrelated_survives(replacement_size: usize) {
            let dir = tempfile::tempdir().unwrap();
            let cache = Cache::with_policy(
                DiskStorage::open(dir.path()).unwrap(),
                EvictionPolicy {
                    max_bytes: Some(100),
                    ..EvictionPolicy::default()
                },
            );

            for (key, byte) in [("unrelated", b'a'), ("target", b'b')] {
                let mut writer = cache.begin_fill(key, StoredMeta::default()).unwrap();
                writer.write_chunk(&[byte; 50]).unwrap();
                drop(writer.commit().unwrap());
            }

            let source = dir.path().join(format!("replacement-{replacement_size}"));
            std::fs::write(&source, vec![b'c'; replacement_size]).unwrap();
            let segment = StagedSegment::new("block", source, replacement_size as u64);
            drop(
                cache
                    .adopt_segmented("target", StoredMeta::default(), &[segment])
                    .unwrap(),
            );

            assert!(cache.get("unrelated").unwrap().is_some());
        }

        unrelated_survives(50);
        unrelated_survives(30);
        unrelated_survives(70);
    }

    #[test]
    fn cache_manager_evicts_lru_when_free_bytes_reach_minimum() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![candidate("old", 40, 10, 1)]),
            available: AtomicU64::new(50),
            release_bytes_on_evict: AtomicBool::new(true),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                min_free_bytes: Some(50),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(*cache.storage().evicted.lock(), vec!["old"]);
        assert_eq!(report.evicted_count, 1);
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_disables_free_space_check_when_minimum_is_zero() {
        // Arrange.
        let storage = PolicyStorage {
            candidates: Mutex::new(vec![candidate("kept", 40, 10, 1)]),
            available: AtomicU64::new(0),
            release_bytes_on_evict: AtomicBool::new(true),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                min_free_bytes: Some(0),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert!(cache.storage().evicted.lock().is_empty());
        assert!(!cache.capacity_limited());
        assert_eq!(report.evicted_count, 0);
        assert_eq!(report.next_delay, Duration::from_secs(10));
    }

    #[test]
    fn cache_manager_bounds_productive_pass_to_one_hundred_evictions() {
        // Arrange.
        let candidates = (0..101)
            .map(|index| candidate(&format!("key-{index:03}"), 1, 0, 0))
            .collect();
        let storage = PolicyStorage {
            candidates: Mutex::new(candidates),
            available: AtomicU64::new(u64::MAX),
            ..PolicyStorage::default()
        };
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_idle_secs: Some(1),
                ..EvictionPolicy::default()
            },
        );

        // Act.
        let report = cache.manager_step_at(100).unwrap();

        // Assert.
        assert_eq!(report.evicted_count, 100);
        assert_eq!(cache.storage().evicted.lock().len(), 100);
        assert_eq!(cache.storage().candidates.lock().len(), 1);
        assert_eq!(report.next_delay, Duration::from_millis(50));
    }

    #[test]
    fn cache_manager_excludes_active_fill_from_eviction() {
        // Arrange.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_bytes: Some(0),
                ..EvictionPolicy::default()
            },
        );
        let mut active = cache
            .begin_fill(
                "active",
                StoredMeta {
                    size: 8,
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        active.write_chunk(b"12345678").unwrap();

        // Act.
        let report = cache.manager_step_at(100).unwrap();
        let committed = active.commit();

        // Assert.
        assert_eq!(report.evicted_count, 0);
        assert_eq!(report.next_delay, Duration::from_secs(10));
        assert!(committed.is_ok());
        assert!(cache.get("active").unwrap().is_some());
    }

    #[test]
    fn cache_manager_keeps_open_lease_readable_after_eviction() {
        // Arrange.
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(dir.path()).unwrap(),
            EvictionPolicy {
                max_bytes: Some(1),
                ..EvictionPolicy::default()
            },
        );
        let mut writer = cache.begin_fill("leased", StoredMeta::default()).unwrap();
        writer.write_chunk(b"leased body").unwrap();
        writer.commit().unwrap();
        let (mut lease, _) = cache.get("leased").unwrap().unwrap();

        // Act.
        let report = cache.manager_step_at(100).unwrap();
        lease
            .file
            .seek(std::io::SeekFrom::Start(lease.body_offset))
            .unwrap();
        let mut body = Vec::new();
        lease.file.read_to_end(&mut body).unwrap();

        // Assert.
        assert_eq!(report.evicted_count, 1);
        assert_eq!(body, b"leased body");
        assert!(cache.get("leased").unwrap().is_none());
    }

    #[test]
    fn invalid_segment_source_does_not_evict_unrelated_entry() {
        let temp = TempDir::new().unwrap();
        let cache = Cache::with_policy(
            DiskStorage::open(temp.path()).unwrap(),
            EvictionPolicy {
                max_bytes: Some(6),
                ..EvictionPolicy::default()
            },
        );
        let mut victim = cache
            .begin_fill(
                "victim",
                StoredMeta {
                    size: 4,
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        victim.write_chunk(b"keep").unwrap();
        drop(victim.commit().unwrap());

        let missing = StagedSegment::new("missing", temp.path().join("gone"), 4);
        assert!(
            cache
                .adopt_segmented("replacement", StoredMeta::default(), &[missing])
                .is_err()
        );
        assert!(cache.get("victim").unwrap().is_some());
    }

    #[test]
    fn storage_boundary_rejection_happens_before_policy_eviction() {
        let storage = PolicyStorage::default();
        storage
            .candidates
            .lock()
            .push(candidate("unrelated", 4, 0, 0));
        storage
            .reject_before_admission
            .store(true, Ordering::SeqCst);
        let cache = Cache::with_policy(
            storage,
            EvictionPolicy {
                max_bytes: Some(6),
                ..EvictionPolicy::default()
            },
        );
        let segment = StagedSegment::new("changed", PathBuf::from("/not-used"), 4);
        assert!(
            cache
                .adopt_segmented("new", StoredMeta::default(), &[segment])
                .is_err()
        );
        assert!(cache.storage.evicted.lock().is_empty());
        assert_eq!(cache.storage.candidates.lock().len(), 1);
    }
}
