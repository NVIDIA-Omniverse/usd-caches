// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Filesystem-first cache storage for OVCC, built as a *separated* metadata/data cache.
//!
//! The guiding principle is **correctness over performance.** Where the two conflict
//! the simple, obviously-correct path is taken first (e.g. synchronous metadata
//! writes), leaving optimization as explicitly-deferred follow-up.
//!
//! # Two authorities, one state machine
//!
//! - **Data = immutable payload files** — the authority for *bytes*. One
//!   contiguous file per key at `objects/<aa>/<rest>.raw`, where `<aa><rest>` is
//!   hex `sha256(key)`. It contains exactly the payload from offset zero and is
//!   published `tmp -> fsync -> atomic rename` only when complete (I4/I14).
//!   redb holds its layout, exact length, validators, and response metadata.
//! - **Metadata = redb** — the authority for *state + history*. One table
//!   `key -> MetadataRecord`, every transition in an ACID write transaction
//!   (C1 crash-safety with no hand-rolled WAL).
//! - **The per-key [`CacheState`] is the source of truth.** File presence is
//!   *validated against* the declared state (I8/I9/I15), never used to silently
//!   infer it. Divergence is named and recovered, not silent.
//!
//! # States
//!
//! ```text
//! S0 ABSENT            metadata=no  data=no      consistent
//! S1 CACHED            metadata=yes data=yes      consistent; serveable
//! S2 METADATA_ONLY     metadata=yes data=no       valid steady state (keeps history)
//! S3 DATA_ONLY         metadata=no  data=yes      INCONSISTENT (orphan) -> delete file
//! S4 FETCH_IN_PROGRESS metadata=yes data=no, live at most one active fetch per key
//! ```
//!
//! `S5 TOMBSTONE` (negative caching) is **deliberately excluded for OVCC**, since
//! with per-request auth and presigned URLs a 404/403 is often signature-specific, so
//! caching "absent" would be a correctness/security bug.
//!
//! # On-disk layout
//!
//! ```text
//! <root>/
//! ├── metadata.redb       # redb store, key -> MetadataRecord (ACID); state authority
//! ├── objects/<aa>/<rest>.raw      # contiguous payload, directly mmap-able
//! ├── objects/<aa>/<rest>.segments # manifest + immutable fragments
//! └── staging/                     # in-flight and deferred generations
//! ```

#[cfg(feature = "test-fixture-inspection")]
pub mod fixture_inspection;

#[cfg(test)]
mod raw_files;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IoSlice, Read, Write};
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use ovcc_cachekey::LogicalKey;
use ovcc_core::{Error, Result};

mod optional_keys;
mod path_leases;
pub use optional_keys::{KeyPolicy, LogicalMatch};
pub use path_leases::{
    LeaseClock, LocalPathLookup, LocalPathSource, MAX_PATH_LEASE_MILLIS, PathCandidate,
    PathLeaseGrant, SystemLeaseClock,
};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// Matches the bounded record batches used by physical-key backfill.
const RECOVERY_DELETE_BATCH: usize = 128;

/// The redb table mapping a cache key -> a version-tagged `MetadataRecord`. The
/// single source of truth for cache *state*; the data files are the source of
/// truth for *bytes*. Values are `encode_record`/`decode_record` framed (a
/// `u16` schema version prefix + a `postcard` body).
const METADATA: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

/// Schema version stamped on every metadata value. Bumped when the encoded
/// `MetadataRecord` layout changes incompatibly. Keep version dispatch and add
/// explicit conversions for supported predecessors when this version changes.
/// Unsupported records are ignored rather than mis-decoded.
const METADATA_SCHEMA_VERSION: u16 = 9;

/// Magic prefix identifying an OVCC segmented-object manifest.
const MANIFEST_MAGIC: &[u8; 8] = b"OVCCSEGM";
/// Segmented manifest format version. It evolves independently from the
/// metadata schema so each format can be versioned independently.
const MANIFEST_FORMAT_VERSION: u64 = 1;
/// Manifest filename within an atomically adopted segmented object directory.
const MANIFEST_FILENAME: &str = "manifest.ovcc";
/// Fixed manifest envelope: magic + version + generation + key length + segment
/// count + total logical length. Key and entries follow; SHA-256 closes it.
const MANIFEST_FIXED_LEN: usize = 8 + 8 + 8 + 4 + 4 + 8;
/// SHA-256 digest length at the end of every manifest.
const MANIFEST_DIGEST_LEN: usize = 32;
/// Corrupt manifests cannot force unbounded metadata allocation.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
/// Azure's maximum committed block count and OVCC's supported segment bound.
const MAX_MANIFEST_SEGMENTS: usize = 50_000;
#[cfg(test)]
type SegmentPrepareHook = (String, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);
#[cfg(test)]
static SEGMENT_PREPARE_BARRIERS: OnceLock<Mutex<Option<SegmentPrepareHook>>> = OnceLock::new();
#[cfg(test)]
type LookupPinHook = (String, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);
#[cfg(test)]
static LOOKUP_PIN_BARRIERS: OnceLock<Mutex<Option<LookupPinHook>>> = OnceLock::new();
#[cfg(test)]
type LookupRecordHook = (String, Arc<std::sync::Barrier>, Arc<std::sync::Barrier>);
#[cfg(test)]
static LOOKUP_RECORD_BARRIERS: OnceLock<Mutex<Option<LookupRecordHook>>> = OnceLock::new();
#[cfg(test)]
static FAIL_PUBLISH_SYNC_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static FAIL_DEFERRED_CLEANUP_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
#[cfg(test)]
static FAIL_FILL_DESTINATION_CLEANUP_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
#[cfg(test)]
struct CleanupThreadReporter {
    root: PathBuf,
    sender: std::sync::mpsc::Sender<std::thread::ThreadId>,
}
#[cfg(test)]
static CLEANUP_THREAD_REPORTER: OnceLock<Mutex<Option<CleanupThreadReporter>>> = OnceLock::new();
#[cfg(test)]
static CLEANUP_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestEntry {
    id: String,
    filename: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentManifest {
    key: String,
    generation: u64,
    total_length: u64,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Clone)]
struct ValidatedSource {
    id: String,
    path: PathBuf,
    filename: String,
    size: u64,
    identity: ObjectIdentity,
}

#[derive(Debug)]
struct ValidatedSegments {
    entries: Vec<ManifestEntry>,
    unique_sources: Vec<ValidatedSource>,
    source_parents: HashSet<PathBuf>,
    total_length: u64,
}

/// Flushes coalesced access-history bumps once this many distinct keys are pending.
/// Bounds memory + redb file growth while keeping the per-hit cost in memory.
const PENDING_FLUSH_THRESHOLD: usize = 4096;

/// One-shot failures at fill boundaries used by deterministic cleanup tests.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FaultPoint {
    BeginStagingCreate,
    CommitSizePublication,
    CommitFlush,
    CommitSyncAll,
    CommitObjectDirectoryCreate,
    CommitRename,
    CommitMetadataPublication,
    CommitFinalReopen,
    BackfillBatch,
    PhysicalKeyRegistration,
}

#[cfg(test)]
std::thread_local! {
    static INJECTED_FAULT: std::cell::Cell<Option<FaultPoint>> =
        const { std::cell::Cell::new(None) };
    static CRASH_ON_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn inject_fault(point: FaultPoint) {
    INJECTED_FAULT.with(|injected| {
        assert!(
            injected.replace(Some(point)).is_none(),
            "a storage fault is already armed"
        );
    });
}

#[cfg(test)]
fn fail_if_injected(point: FaultPoint) -> Result<()> {
    INJECTED_FAULT.with(|injected| {
        if injected.get() == Some(point) {
            injected.set(None);
            if CRASH_ON_FAULT.get() {
                // Child-process tests terminate without running storage/file destructors.
                std::process::exit(86);
            }
            Err(Error::storage(format!("injected storage fault: {point:?}")))
        } else {
            Ok(())
        }
    })
}

#[cfg(test)]
thread_local! {
    static RECORD_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Owns encoded bytes through lookup and its optional alias-generation guard.
struct LookupRecordSnapshot {
    bytes: Vec<u8>,
    identity: Option<(CacheState, ObjectLayout, u64)>,
}

impl LookupRecordSnapshot {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            identity: None,
        }
    }
}

/// An in-memory, not-yet-persisted access-history bump for one key.
#[derive(Debug, Default, Clone, Copy)]
struct PendingBump {
    hits: u64,
    last_access: i64,
}

impl PendingBump {
    fn record_hit(&mut self, last_access: i64) {
        self.hits = self.hits.saturating_add(1);
        // Timestamps sampled before locking can arrive in reverse order.
        self.last_access = self.last_access.max(last_access);
    }
}

/// Per-key cache state. This — not file presence — is the source of truth; the
/// data file is validated against it (I7/I8/I9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheState {
    /// S1: metadata + a validated data file exist; serveable.
    Cached,
    /// S2: metadata exists, data intentionally absent; history is retained.
    MetadataOnly,
    /// S4: a fill is (or was) in flight; at most one active per key (I3).
    FetchInProgress,
}

/// Physical byte layout selected by a metadata record.
///
/// The discriminator is intentionally compact. Ordered segmented descriptors
/// live only in the versioned on-disk manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectLayout {
    /// Reserved retired header-file discriminator (0). Never reorder or reuse:
    /// postcard persists enum positions, including Segmented (1) and Raw (2).
    Contiguous,
    /// Atomically adopted directory containing immutable fragments + manifest.
    Segmented,
    /// Payload-only file, with format and exact byte length held in redb.
    Raw,
}

/// Source of the bytes in one cache generation.
///
/// This is durable policy metadata, not response metadata. Peer copies use a
/// shorter inactivity horizon while origin generations retain normal policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheProvenance {
    Origin,
    PeerCopy,
}

/// The durable metadata record for one cache key (redb value). Carries cache
/// *state* plus the *history* used by eviction scoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataRecord {
    /// Logical identity used to rebuild physical-key registrations.
    pub logical_key: Option<LogicalKey>,
    /// Registered keys for bounded owner/revision-guarded cleanup.
    pub physical_keys: Vec<String>,
    /// The cache key, whose hash selects the object's path.
    pub key: String,
    /// Authoritative per-key state.
    pub state: CacheState,
    /// Compact physical-layout discriminator. Segment order remains on disk.
    pub layout: ObjectLayout,
    /// Opaque committed-generation identity. Segmented records also link this
    /// value to their manifest.
    pub generation: u64,
    /// Source of the current generation or active fill.
    pub provenance: CacheProvenance,
    /// Effective inactivity ceiling assigned when this generation was admitted.
    /// `None` preserves the policy-only behavior of older records.
    pub max_idle_secs: Option<u64>,
    /// Origin response status replayed on a HIT. Normally `200`/`206`; the
    /// canonical bursty path can also persist a short-lived `404`.
    pub status: u16,
    /// Strong validator from the origin, if any.
    pub etag: Option<String>,
    /// Origin `Last-Modified`, if any. Used for mutation detection, where a re-check
    /// that observes a different upstream `Last-Modified` means the cached
    /// object is stale and must be purged.
    pub last_modified: Option<String>,
    /// Stored `Content-Type`, if known (seeded by write-through). Relayed on a
    /// HIT; `None` for normal MISS fills.
    pub content_type: Option<String>,
    /// Relayable origin response headers captured at fill time, replayed verbatim
    /// on a HIT so the served response is header-faithful to the MISS. Excludes
    /// hop-by-hop and length headers, which the serve path re-derives.
    pub headers: Vec<(String, String)>,
    /// Origin representation length when it cannot be derived from the stored
    /// body, notably for a zero-body HEAD snapshot.
    pub representation_length: Option<u64>,
    /// Freshness deadline in unix seconds. `None` is reserved for policies that
    /// deliberately never expire an entry.
    pub expires_at: Option<i64>,
    /// Whether the request identity names an immutable object generation.
    pub versionable: bool,
    /// `Content-Range` of a cached partial response (a `Range` GET admitted as
    /// `206`). `Some` marks the entry as partial content, so a HIT serves it back
    /// as `206 Partial Content` with this header. `None` is a normal full `200`
    /// body. The cache key already includes the request `Range`, so partial and
    /// full entries never collide.
    pub content_range: Option<String>,
    /// Body length in bytes (derived from the data file on commit).
    pub size_bytes: u64,
    /// Last access (unix seconds) — history for eviction scoring.
    pub last_access: i64,
    /// Number of hits served — history for eviction scoring.
    pub hit_count: u64,
    /// Estimated cost to refetch from upstream — history for eviction scoring.
    pub fetch_cost: u32,
    /// Estimated upstream fetch duration — history for eviction scoring.
    pub fetch_duration: u32,
}

impl MetadataRecord {
    /// True if this record carries history worth preserving across a failed
    /// fetch (so a failed *refill* lands in `MetadataOnly`, while a failed
    /// *cold* fill — no history — is dropped to `Absent`). See C5.
    fn has_history(&self) -> bool {
        self.hit_count > 0 || self.size_bytes > 0
    }
}

/// Metadata projected for callers, namely `size` plus an optional `etag`. Returned both on a
/// hit ([`Lookup::Hit`]) and on a soft miss ([`Lookup::SoftMiss`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeta {
    /// Supplied by policy on admission; absent for legacy exact-only callers.
    pub logical_key: Option<LogicalKey>,
    pub size: u64,
    /// Opaque storage revision for exact-generation metadata mutations.
    /// Callers must preserve it but must not derive cache identity from it.
    pub revision: u64,
    /// Source assigned to a newly admitted generation.
    pub provenance: CacheProvenance,
    /// Effective inactivity ceiling for this generation. Admission clamps a
    /// caller-supplied backend/peer ceiling against the local cache policy.
    pub max_idle_secs: Option<u64>,
    /// Origin response status replayed on a HIT.
    pub status: u16,
    pub etag: Option<String>,
    /// Origin `Last-Modified`, if any (mutation-detection validator).
    pub last_modified: Option<String>,
    /// Stored `Content-Type`, if known. Seeded by the write-through PUT so the
    /// consumer GET HIT relays the published media type. `None` for entries
    /// admitted via a normal MISS fill (the HIT then omits `Content-Type`, as
    /// before).
    pub content_type: Option<String>,
    /// Relayable origin response headers to replay on a HIT; see
    /// [`MetadataRecord::headers`]. Empty when no headers were captured.
    pub headers: Vec<(String, String)>,
    /// Origin representation length retained independently from physical body
    /// size. `None` means the origin did not declare one.
    pub representation_length: Option<u64>,
    /// Freshness deadline in unix seconds, or `None` for entries that never
    /// expire; see [`MetadataRecord::expires_at`].
    pub expires_at: Option<i64>,
    /// Whether the request identity names an immutable object generation.
    pub versionable: bool,
    /// `Content-Range` for a cached partial (`206`) response; `None` for a full
    /// `200` body. See [`MetadataRecord::content_range`].
    pub content_range: Option<String>,
}

impl Default for StoredMeta {
    fn default() -> Self {
        Self {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: None,
            status: 200,
            etag: None,
            last_modified: None,
            content_type: None,
            headers: Vec::new(),
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: None,
        }
    }
}

/// One immutable staged fragment supplied to [`Storage::adopt_segmented`].
///
/// Repeated `id` values are legal and remain repeated in logical order. Every
/// occurrence of one id must name the same path and exact size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSegment {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
}

impl StagedSegment {
    /// Records a fragment as already staged, without touching the filesystem to confirm it.
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>, size: u64) -> Self {
        Self {
            id: id.into(),
            path: path.into(),
            size,
        }
    }
}

/// Conservative incremental filesystem growth for publishing a segmented
/// manifest. Fragment bodies already exist on the cache volume; this reserves
/// the block-rounded manifest plus directory and redb metadata blocks.
pub fn segmented_adoption_disk_growth(key: &str, segments: &[StagedSegment]) -> Result<u64> {
    if segments.is_empty() || segments.len() > MAX_MANIFEST_SEGMENTS {
        return Err(Error::storage("unsupported segmented object count"));
    }
    let mut encoded = (MANIFEST_FIXED_LEN + MANIFEST_DIGEST_LEN)
        .checked_add(key.len())
        .ok_or_else(|| Error::storage("manifest encoded length overflow"))?;
    for segment in segments {
        if segment.id.is_empty() || segment.id.len() > u32::MAX as usize {
            return Err(Error::storage("invalid segment id length"));
        }
        encoded = encoded
            .checked_add(4 + 4 + 8)
            .and_then(|length| length.checked_add(segment.id.len()))
            .and_then(|length| length.checked_add(segment_filename(&segment.id).len()))
            .ok_or_else(|| Error::storage("manifest encoded length overflow"))?;
    }
    if encoded as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::storage("segmented manifest is too large"));
    }
    let block = 4096u64;
    let manifest_blocks = (encoded as u64)
        .saturating_add(block - 1)
        .checked_div(block)
        .unwrap_or(0)
        .saturating_mul(block);
    Ok(manifest_blocks.saturating_add(2 * block))
}

/// The physical representation behind a [`Lease`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseLayout {
    Contiguous,
    Segmented,
}

/// One ordered logical segment in a segmented lease.
///
/// A duplicate source appears more than once with distinct ordinals but the
/// same private filename. Callers need only retain the current descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDescriptor {
    ordinal: usize,
    size: u64,
    filename: String,
    generation: u64,
}

impl SegmentDescriptor {
    /// Zero-based logical position in the persisted manifest.
    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Exact number of bytes to send for this occurrence.
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Ordered descriptors for a segmented [`Lease`].
#[derive(Debug, Clone)]
pub struct SegmentDescriptors {
    descriptors: Arc<Vec<SegmentDescriptor>>,
    next: usize,
}

impl Iterator for SegmentDescriptors {
    type Item = SegmentDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        let descriptor = self.descriptors.get(self.next)?.clone();
        self.next += 1;
        Some(descriptor)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.descriptors.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for SegmentDescriptors {}

/// Cloneable capability for opening one validated segment on a blocking worker.
/// It owns one duplicate directory descriptor regardless of segment count.
#[derive(Clone)]
pub struct SegmentOpener {
    directory: Arc<File>,
    descriptors: Arc<Vec<SegmentDescriptor>>,
    generation: u64,
    _pin: Arc<GenerationPin>,
}

impl std::fmt::Debug for SegmentOpener {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SegmentOpener")
            .field("directory", &self.directory)
            .field("generation", &self.generation)
            .field("segment_count", &self.descriptors.len())
            .finish()
    }
}

impl SegmentOpener {
    /// Opens a fragment only while the descriptor still matches the lease's generation.
    pub fn open(&self, descriptor: &SegmentDescriptor) -> Result<File> {
        let expected = self
            .descriptors
            .get(descriptor.ordinal)
            .filter(|expected| {
                expected.filename == descriptor.filename
                    && expected.size == descriptor.size
                    && expected.generation == descriptor.generation
                    && descriptor.generation == self.generation
            })
            .ok_or_else(|| Error::storage("segment descriptor does not belong to lease"))?;
        open_regular_at(&self.directory, &expected.filename, expected.size)
    }
}

/// Stable identity of a file or directory on the cache filesystem.
type ObjectIdentity = ovcc_platform::FileIdentity;

struct GenerationPin {
    inner: Arc<Inner>,
    identity: ObjectIdentity,
}

impl Drop for GenerationPin {
    fn drop(&mut self) {
        let final_pin = self.inner.release_generation(self.identity);
        if final_pin && self.inner.has_deferred_for(self.identity) {
            enqueue_deferred_cleanup(Arc::clone(&self.inner));
        }
    }
}

/// Moves physical reclamation off whichever thread releases the final lease.
/// The worker owns `Inner` strongly, so cleanup remains valid after both the
/// storage handle and an async runtime have gone away.
fn enqueue_deferred_cleanup(inner: Arc<Inner>) {
    static CLEANUP_SENDER: OnceLock<std::sync::mpsc::Sender<Arc<Inner>>> = OnceLock::new();
    let sender = CLEANUP_SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Arc<Inner>>();
        std::thread::Builder::new()
            .name("ovcc-storage-cleanup".to_owned())
            .spawn(move || {
                for inner in receiver {
                    let operation = inner.write_operations();
                    let _ = inner.drain_deferred();
                    drop(operation);
                    #[cfg(test)]
                    let completed_root = inner.root.clone();
                    drop(inner);
                    #[cfg(test)]
                    if let Some(reporter) = CLEANUP_THREAD_REPORTER
                        .get_or_init(|| Mutex::new(None))
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .filter(|reporter| reporter.root == completed_root)
                    {
                        let _ = reporter.sender.send(std::thread::current().id());
                    }
                }
            })
            .expect("storage cleanup worker must start");
        sender
    });
    // The receiver belongs to a process-lifetime worker. Sending never waits
    // for filesystem work.
    let _ = sender.send(inner);
}

#[derive(Clone)]
struct LeaseRegistration {
    pin: Arc<GenerationPin>,
    generation: u64,
    descriptors: Arc<Vec<SegmentDescriptor>>,
}

/// Segmented leases keyed by their held directory handle
/// ([`ovcc_platform::handle_key`]).
fn lease_registry() -> &'static Mutex<HashMap<u64, LeaseRegistration>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, LeaseRegistration>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_lease_registry() -> std::sync::MutexGuard<'static, HashMap<u64, LeaseRegistration>> {
    lease_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A leased cache hit. While alive the object's bytes are durable on disk and
/// `file` is either the existing contiguous body fd or a held segmented
/// directory fd. Use [`Lease::layout`] before serving; segmented callers iterate
/// [`Lease::segments`] and open one current fragment with
/// [`Lease::open_segment`].
///
/// The fd is opened *before* state is validated (I16): an open fd survives a
/// racing eviction `unlink`, so an in-flight serve always completes.
#[derive(Debug)]
pub struct Lease {
    pub path: PathBuf,
    pub meta: StoredMeta,
    pub file: File,
    /// Byte offset for `sendfile`; zero for raw files.
    pub body_offset: u64,
}

impl Lease {
    pub fn meta(&self) -> &StoredMeta {
        &self.meta
    }
    /// Returns the representation behind this lease.
    pub fn layout(&self) -> LeaseLayout {
        if lock_lease_registry().contains_key(&ovcc_platform::handle_key(&self.file)) {
            LeaseLayout::Segmented
        } else {
            LeaseLayout::Contiguous
        }
    }

    /// Returns ordered logical segment descriptors, or `None` for a contiguous
    /// lease. The iterator owns only compact manifest metadata, never fragment
    /// file descriptors.
    pub fn segments(&self) -> Option<SegmentDescriptors> {
        let descriptors = lock_lease_registry()
            .get(&ovcc_platform::handle_key(&self.file))
            .map(|registration| Arc::clone(&registration.descriptors))?;
        Some(SegmentDescriptors {
            descriptors,
            next: 0,
        })
    }

    /// Lazily opens one descriptor relative to the held generation directory.
    ///
    /// `O_NOFOLLOW` plus an exact regular-file/size check prevents a replaced
    /// path from escaping the pinned directory. The caller may drop this file
    /// before advancing to the next descriptor, so 1,969-segment objects do not
    /// consume 1,969 descriptors.
    pub fn open_segment(&self, descriptor: &SegmentDescriptor) -> Result<File> {
        self.segment_opener()?.open(descriptor)
    }

    /// Creates a bounded, `Send + Sync` segment-open capability suitable for
    /// `spawn_blocking`; it duplicates only the held directory descriptor.
    pub fn segment_opener(&self) -> Result<SegmentOpener> {
        let registration = lock_lease_registry()
            .get(&ovcc_platform::handle_key(&self.file))
            .cloned()
            .ok_or_else(|| Error::storage("lease is not segmented"))?;
        Ok(SegmentOpener {
            directory: Arc::new(self.file.try_clone()?),
            descriptors: registration.descriptors,
            generation: registration.generation,
            _pin: registration.pin,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // Dropping the registration releases its strong generation owner. The
        // final owner synchronously drains identity-guarded deferred cleanup.
        let registration = lock_lease_registry().remove(&ovcc_platform::handle_key(&self.file));
        drop(registration);
    }
}

/// Read-only access to the body of a live contiguous fill.
///
/// This is deliberately not a [`Lease`]: it carries no committed-cache
/// authority and is never returned by [`Storage::lookup`] or [`Storage::peek`].
/// Its inode remains readable while the writer appends and after the
/// staging pathname is renamed or unlinked.
#[derive(Clone)]
pub struct InProgressRead {
    /// Read-only descriptor for the live staging inode.
    pub file: Arc<File>,
    /// Byte offset of the body, always zero for disk fills.
    pub body_offset: u64,
}

impl std::fmt::Debug for InProgressRead {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InProgressRead")
            .field("file", &self.file)
            .field("body_offset", &self.body_offset)
            .finish()
    }
}

/// Outcome of a [`Storage::lookup`]. The serve path acts on the variant, with
/// `Hit` -> serve and `SoftMiss`/`Miss` -> a single-flight fetch (through auth, I18).
#[derive(Debug)]
pub enum Lookup {
    /// S1 Cached and the data file opened+validated, so serve it.
    Hit(Lease),
    /// S2/S4: metadata is remembered but there are no serveable bytes. The
    /// remembered [`StoredMeta`] is advisory; a refill still goes through the
    /// normal access check (I18).
    SoftMiss(StoredMeta),
    /// S0: no record at all.
    Miss,
}

/// One serveable entry considered by cache eviction policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionCandidate {
    pub key: String,
    /// Names the committed generation this candidate describes. Policy hands
    /// it back to [`Storage::evict_data`] so a delayed decision cannot retire
    /// a replacement.
    pub revision: u64,
    pub size_bytes: u64,
    pub last_access: i64,
    pub hit_count: u64,
    pub provenance: CacheProvenance,
    pub max_idle_secs: Option<u64>,
}

/// Streaming writer for a cache admission (a fill). Bytes are written to a
/// staging file from offset zero; `commit` publishes atomically
/// (`fsync` + `rename`) and transitions the record to `Cached`.
pub trait StoreWrite: Send {
    /// Append a chunk of *body* bytes to the staging file.
    fn write_chunk(&mut self, buf: &[u8]) -> Result<()>;
    /// Appends all slices in order. Implementations may advance the views.
    fn write_chunks(&mut self, chunks: &mut [IoSlice<'_>]) -> Result<()> {
        for chunk in chunks {
            self.write_chunk(chunk)?;
        }
        Ok(())
    }
    /// Returns read-only access to this live payload file.
    ///
    /// Backends that cannot expose a file-backed contiguous fill retain object
    /// safety and source compatibility through this unsupported default.
    fn in_progress_read(&self) -> Result<InProgressRead> {
        Err(Error::storage(
            "in-progress reads are unsupported by this storage writer",
        ))
    }
    /// Publish staging -> committed (`Cached`) and return a `Lease`.
    ///
    /// Fails without publishing when the fill was revoked, meaning the key was
    /// removed or a newer fill replaced it while this writer was open. That
    /// error reports `ovcc_core::Error::is_fill_revoked`, because removal is
    /// final for every writer opened before it.
    fn commit(self: Box<Self>) -> Result<Lease>;
    /// Discard the staging file and revert the in-progress state.
    ///
    /// A revoked writer discards its staging bytes without restoring its
    /// pre-fill metadata, so an abort cannot resurrect a removed key.
    fn abort(self: Box<Self>);
    /// Drops process-local writer state without running durable cleanup.
    #[cfg(test)]
    fn simulate_crash(self: Box<Self>) {
        panic!("crash simulation is unsupported by this storage writer");
    }
    /// Exposes the concrete writer only to its crate-owned fault tests.
    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The cache storage seam, a separated metadata(redb)/data(files) store.
pub trait Storage: Send + Sync {
    fn local_path_source(&self, _key: &str) -> Option<LocalPathSource> {
        None
    }
    /// Optional-key matching is opt-in; legacy storage implementations stay exact-only.
    fn lookup_logical(&self, _key: &LogicalKey) -> Result<Option<LogicalMatch<Lease>>> {
        Ok(None)
    }
    /// Metadata-only counterpart to [`Self::lookup_logical`], acquiring no lease.
    fn peek_logical(&self, _key: &LogicalKey) -> Result<Option<LogicalMatch<StoredMeta>>> {
        Ok(None)
    }
    /// Look up `key`, surfacing state as `Hit` | `SoftMiss` | `Miss`. The serve
    /// path opens the fd before validating state (I16). A returned `Hit` is
    /// generation-consistent, so its `StoredMeta` and its descriptor describe
    /// the same committed generation. An implementation that reads metadata
    /// before acquiring the descriptor re-validates the current record
    /// afterwards and reports a replacement landing in that window as a
    /// `SoftMiss`, while an already-returned lease keeps serving its own
    /// generation.
    fn lookup(&self, key: &str) -> Result<Lookup>;
    /// Observes a committed object's metadata without changing access history,
    /// acquiring a serve lease, or repairing inconsistent state.
    fn peek(&self, key: &str) -> Result<Option<StoredMeta>>;
    /// Extends freshness for the exact committed generation described by
    /// `expected`, without rewriting its bytes or changing access history.
    /// Implementations compare the opaque storage revision and complete
    /// projected metadata while holding their state-mutation lock. They return
    /// false if the generation changed, disappeared, or stopped being
    /// serveable. The cache layer calls this only while holding the per-key
    /// single-flight permit after an origin 304 response.
    fn refresh_freshness(
        &self,
        _key: &str,
        _expected: &StoredMeta,
        _expires_at: i64,
    ) -> Result<bool> {
        Ok(false)
    }
    /// Begin a single-flight fill, marking the record `FetchInProgress` and returning
    /// a streaming writer. Mutual exclusion (I3) is enforced by the caller's
    /// herd lock (`ovcc-cache`); redb only records the in-progress state so
    /// eviction can skip it (I17) and recovery can clear it (C5).
    fn begin_fill(&self, key: &str, meta: StoredMeta) -> Result<Box<dyn StoreWrite>>;
    /// Validates every segmented source before cache policy may evict unrelated
    /// entries. Adoption repeats this validation at the mutation boundary.
    fn validate_segmented_sources(&self, _segments: &[StagedSegment]) -> Result<()> {
        Ok(())
    }
    /// Atomically adopts an ordered segmented object from immutable staged
    /// fragment paths. The full list is prevalidated before any mutation;
    /// successful publication consumes each unique source path exactly once.
    ///
    /// This API is synchronous. Async callers must invoke it from
    /// `tokio::task::spawn_blocking`.
    fn adopt_segmented(
        &self,
        _key: &str,
        _meta: StoredMeta,
        _segments: &[StagedSegment],
    ) -> Result<Lease> {
        Err(Error::storage(
            "segmented adoption is unsupported by this storage backend",
        ))
    }
    /// Adopts after invoking `admit` at the storage-defined point where every
    /// source has been validated and privately secured, but no cache state has
    /// been published. Cache policy uses this seam so unrelated eviction cannot
    /// precede source validation or lose a TOCTOU race.
    fn adopt_segmented_with_admission(
        &self,
        key: &str,
        meta: StoredMeta,
        segments: &[StagedSegment],
        admit: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Lease> {
        self.validate_segmented_sources(segments)?;
        admit()?;
        self.adopt_segmented(key, meta, segments)
    }
    /// Evict the data and metadata for the committed generation `revision`
    /// (`Cached -> Absent`), including history and physical-key mappings.
    /// Never downloads (I2); a no-op for non-`Cached` states (I17) and for any
    /// other generation.
    ///
    /// Implementations compare the opaque storage revision under their
    /// state-mutation lock, so a decision taken against a snapshot cannot
    /// retire a replacement generation. A successful
    /// [`Storage::refresh_freshness`] leaves that revision alone, so an
    /// expiry-driven decision delayed past a refresh still evicts. Reporting
    /// whether the transition happened lets policy account for what storage
    /// reclaimed rather than for what it selected.
    fn evict_data(&self, key: &str, revision: u64) -> Result<bool>;
    /// Hard-evict `key`: remove both data and metadata (`-> Absent`).
    ///
    /// Removal is final for every [`StoreWrite`] opened before it. The fill's
    /// `FetchInProgress` generation is gone, so a later `commit` is refused and
    /// a later `abort` restores nothing. Removal never waits for an in-flight
    /// fill, which keeps a mutation purge prompt.
    fn remove(&self, key: &str) -> Result<()>;
    /// Snapshots serveable entries and their eviction history. In-progress
    /// fills are deliberately absent, so policy cannot select them.
    fn eviction_candidates(&self) -> Result<Vec<EvictionCandidate>> {
        Ok(Vec::new())
    }
    /// Free bytes on the filesystem containing this storage root.
    fn available_bytes(&self) -> Result<u64> {
        Ok(u64::MAX)
    }
    /// Startup classification + crash recovery. Converges every key to
    /// `Absent | Cached | MetadataOnly` and never downloads (I1/I10/I11).
    fn recover_on_open(&self) -> Result<RecoveryReport>;
}

/// Tally of what [`Storage::recover_on_open`] did, for observability + tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Records left in `Cached` (file present + layout validated). B2.
    pub cached: usize,
    /// Pre-existing `MetadataOnly` records discarded during recovery.
    pub metadata_only: usize,
    /// `FetchInProgress` reconciled to `Cached` (file was already renamed). C-window.
    pub promoted_to_cached: usize,
    /// `Cached`/`FetchInProgress` with no/invalid file -> `MetadataOnly`. B3/C3/C5.
    pub reconciled_to_metadata_only: usize,
    /// Records dropped to `Absent` (failed cold fill, no history). C5.
    pub removed_records: usize,
    /// `FetchInProgress` records left to a still-open writer rather than
    /// reconciled, so a runtime pass does not revoke a live fill.
    pub skipped_active_fills: usize,
    /// Orphan data files with no `Cached` record, deleted. B4/C4.
    pub deleted_orphan_files: usize,
    /// Orphan segmented object directories with no `Cached` record, deleted.
    pub deleted_orphan_directories: usize,
    /// Leftover staging `*.tmp` files swept. C2/C5.
    pub swept_staging: usize,
    /// Leftover segmented staging/deferred directories swept.
    pub swept_staging_directories: usize,
    /// Deferred pinned-generation paths physically deleted during this pass.
    pub drained_deferred: usize,
    /// Records dropped because their stored format version is not the current
    /// one, so an engine upgrade cold-starts those entries.
    pub discarded_stale_format: usize,
}

/// Shared, cheaply-clonable backend state behind an `Arc` so a streaming
/// [`DiskWriter`] can outlive the borrow that created it and still reach redb on
/// `commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeferredCleanup {
    path: PathBuf,
    identity: ObjectIdentity,
}

#[derive(Debug)]
struct PublicationError {
    error: Error,
    published: bool,
}

struct ActiveStaging {
    inner: Arc<Inner>,
    path: PathBuf,
}

impl ActiveStaging {
    fn new(inner: &Arc<Inner>, path: &Path) -> Self {
        inner.register_staging(path.to_path_buf());
        Self {
            inner: Arc::clone(inner),
            path: path.to_path_buf(),
        }
    }
}

impl Drop for ActiveStaging {
    fn drop(&mut self) {
        self.inner.unregister_staging(&self.path);
    }
}

/// Marks one key generation as a live fill for as long as its writer exists, so
/// recovery leaves that writer's record alone the way `sweep_staging` leaves
/// its staging file alone.
struct ActiveFill {
    inner: Arc<Inner>,
    key: String,
    generation: u64,
}

impl ActiveFill {
    fn new(inner: &Arc<Inner>, key: &str, generation: u64) -> Self {
        inner.register_fill(key, generation);
        Self {
            inner: Arc::clone(inner),
            key: key.to_string(),
            generation,
        }
    }
}

impl Drop for ActiveFill {
    fn drop(&mut self) {
        self.inner.unregister_fill(&self.key, self.generation);
    }
}

struct Inner {
    path_leases: Mutex<path_leases::Catalog>,
    lease_clock: Arc<dyn LeaseClock>,
    key_policy: Option<Arc<dyn KeyPolicy>>,
    key_index_ready: AtomicBool,
    root: PathBuf,
    objects_dir: PathBuf,
    staging_dir: PathBuf,
    db: Database,
    /// Coalesced, not-yet-persisted access-history bumps (hot-path HITs).
    pending: Mutex<HashMap<String, PendingBump>>,
    /// Serializes publication, recovery, eviction, and removal mutations.
    /// Lookups remain lock-free after pinning their opened generation.
    operations: Mutex<()>,
    #[cfg(test)]
    fail_retire_key: Mutex<Option<String>>,
    /// Active segmented generation pins. Lease drop only decrements this map.
    pins: Mutex<HashMap<ObjectIdentity, usize>>,
    /// Private staging paths currently owned by live writers/adoptions.
    active_staging: Mutex<HashSet<PathBuf>>,
    /// Key-generation claims owned by live fill writers.
    active_fills: Mutex<HashMap<(String, u64), usize>>,
    /// Physical deletions deferred while a segmented generation is pinned.
    deferred: Mutex<VecDeque<DeferredCleanup>>,
}

impl Inner {
    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingBump>> {
        // Recovers the map rather than propagating poison because history is non-critical.
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write_operations(&self) -> std::sync::MutexGuard<'_, ()> {
        self.operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_pins(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectIdentity, usize>> {
        self.pins
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register_staging(&self, path: PathBuf) {
        self.active_staging
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(path);
    }

    fn unregister_staging(&self, path: &Path) {
        self.active_staging
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(path);
    }

    fn lock_active_fills(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u64), usize>> {
        self.active_fills
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register_fill(&self, key: &str, generation: u64) {
        *self
            .lock_active_fills()
            .entry((key.to_string(), generation))
            .or_insert(0) += 1;
    }

    fn unregister_fill(&self, key: &str, generation: u64) {
        let mut fills = self.lock_active_fills();
        let claim = (key.to_string(), generation);
        let Some(count) = fills.get_mut(&claim) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            fills.remove(&claim);
        }
    }

    /// Whether a writer still owns the exact in-progress generation.
    fn fill_is_live(&self, key: &str, generation: u64) -> bool {
        self.lock_active_fills()
            .contains_key(&(key.to_string(), generation))
    }

    fn lock_deferred(&self) -> std::sync::MutexGuard<'_, VecDeque<DeferredCleanup>> {
        self.deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn pin_generation(&self, identity: ObjectIdentity) {
        let mut pins = self.lock_pins();
        *pins.entry(identity).or_insert(0) += 1;
    }

    /// Releases one pin and reports whether the final pin was removed.
    fn release_generation(&self, identity: ObjectIdentity) -> bool {
        let mut pins = self.lock_pins();
        let Some(count) = pins.get_mut(&identity) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            pins.remove(&identity);
            return true;
        }
        false
    }

    fn generation_is_pinned(&self, identity: ObjectIdentity) -> bool {
        self.lock_pins().contains_key(&identity)
    }

    fn defer_cleanup(&self, path: PathBuf, identity: ObjectIdentity) {
        let mut deferred = self.lock_deferred();
        if !deferred
            .iter()
            .any(|entry| entry.path == path && entry.identity == identity)
        {
            deferred.push_back(DeferredCleanup { path, identity });
        }
    }

    fn has_deferred_for(&self, identity: ObjectIdentity) -> bool {
        self.lock_deferred()
            .iter()
            .any(|entry| entry.identity == identity)
    }

    /// Deletes eligible deferred generations in deterministic queue order.
    /// Must be called by a synchronous storage operation, never from Lease drop.
    fn drain_deferred(&self) -> Result<usize> {
        let mut deferred = self.lock_deferred();
        let mut retained = VecDeque::with_capacity(deferred.len());
        let mut deleted = 0;
        while let Some(entry) = deferred.pop_front() {
            if self.generation_is_pinned(entry.identity) {
                retained.push_back(entry);
                continue;
            }
            #[cfg(test)]
            let injected_failure = {
                let mut path = FAIL_DEFERRED_CLEANUP_PATH
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if path.as_ref() == Some(&entry.path) {
                    path.take();
                    true
                } else {
                    false
                }
            };
            #[cfg(test)]
            if injected_failure {
                retained.push_back(entry);
                retained.append(&mut deferred);
                *deferred = retained;
                return Err(Error::storage("injected deferred cleanup failure"));
            }
            let cleanup = match ovcc_platform::stat_nofollow(&entry.path) {
                Ok(stat) if stat.identity == entry.identity => remove_object_path(&entry.path)
                    .and_then(|()| sync_parent(&entry.path))
                    .map(|()| true),
                Ok(_) => {
                    // The path was reused for another generation. Identity
                    // mismatch makes deletion unsafe; forget only the stale job.
                    Ok(false)
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(err) => Err(err.into()),
            };
            match cleanup {
                Ok(true) => deleted += 1,
                Ok(false) => {}
                Err(error) => {
                    retained.push_back(entry);
                    retained.append(&mut deferred);
                    *deferred = retained;
                    return Err(error);
                }
            }
        }
        *deferred = retained;
        Ok(deleted)
    }

    /// Removes an object from its public path. A pinned directory is renamed
    /// into storage-owned staging and queued instead of being unlinked.
    fn retire_path(&self, path: &Path) -> Result<()> {
        let identity = match ovcc_platform::stat_nofollow(path) {
            Ok(stat) => stat.identity,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        if self.generation_is_pinned(identity) {
            let deferred = self.deferred_path();
            match ovcc_platform::rename(path, &deferred) {
                Ok(()) => {
                    // The rename is already externally visible. Queue cleanup
                    // before either fallible durability barrier so an fsync
                    // failure cannot lose ownership of the retired generation.
                    self.defer_cleanup(deferred.clone(), identity);
                    sync_parent(path)?;
                    sync_parent(&deferred)?;
                    Ok(())
                }
                Err(err) => {
                    // State has already been hidden from lookup. Keep an
                    // identity-guarded job at the public residue for retry.
                    self.defer_cleanup(path.to_path_buf(), identity);
                    Err(err.into())
                }
            }
        } else {
            remove_object_path(path)?;
            sync_parent(path)
        }
    }

    /// Publishes `staged` at `destination`, safely replacing either a file or a
    /// non-empty directory via [`ovcc_platform::exchange_paths`] (atomic
    /// `RENAME_EXCHANGE` on Linux). Every caller publishes while the key's
    /// record is not yet `Cached`, so no lookup trusts `destination` during
    /// the non-atomic Windows exchange, and `staged` lives in the staging
    /// directory that recovery sweeps.
    fn publish_staged(
        &self,
        staged: &Path,
        destination: &Path,
    ) -> std::result::Result<(), PublicationError> {
        let destination_identity = match ovcc_platform::stat_nofollow(destination) {
            Ok(stat) => Some(stat.identity),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(PublicationError {
                    error: err.into(),
                    published: false,
                });
            }
        };

        if let Some(old_identity) = destination_identity {
            let retired = ovcc_platform::exchange_paths(staged, destination).map_err(|error| {
                PublicationError {
                    error: error.into(),
                    published: false,
                }
            })?;
            // Queue the old generation at the path reported by the platform
            // before either fallible durability step so a surfaced fsync error
            // cannot orphan capacity until restart.
            self.defer_cleanup(retired.clone(), old_identity);
            #[cfg(test)]
            let fail_sync = {
                let mut path = FAIL_PUBLISH_SYNC_PATH
                    .get_or_init(|| Mutex::new(None))
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if path.as_ref().is_some_and(|path| path == destination) {
                    path.take();
                    true
                } else {
                    false
                }
            };
            #[cfg(test)]
            if fail_sync {
                return Err(PublicationError {
                    error: Error::storage("injected post-exchange fsync failure"),
                    published: true,
                });
            }
            sync_parent(&retired).map_err(|error| PublicationError {
                error,
                published: true,
            })?;
            sync_parent(destination).map_err(|error| PublicationError {
                error,
                published: true,
            })?;

            // Deletes immediately when unpinned and retains the identity-
            // guarded job otherwise.
            self.drain_deferred().map_err(|error| PublicationError {
                error,
                published: true,
            })?;
        } else {
            ovcc_platform::rename(staged, destination).map_err(|error| PublicationError {
                error: error.into(),
                published: false,
            })?;
            sync_parent(staged).map_err(|error| PublicationError {
                error,
                published: true,
            })?;
            sync_parent(destination).map_err(|error| PublicationError {
                error,
                published: true,
            })?;
        }
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Persists any coalesced access-history on clean shutdown (best-effort).
        let _ = self.flush_pending();
    }
}

impl Inner {
    /// Maps a cache key to its `(rel_path, absolute path)` under `objects/`.
    fn object_paths(&self, key: &str) -> (String, PathBuf) {
        let hex = sha256_hex_bytes(key);
        let hex = std::str::from_utf8(&hex).expect("hex is ASCII");
        let rel_path = format!("{}/{}.raw", &hex[..2], &hex[2..]);
        let abs = self.hashed_object_path(hex, ObjectLayout::Raw);
        (rel_path, abs)
    }

    fn hashed_object_path(&self, hex: &str, layout: ObjectLayout) -> PathBuf {
        let mut path = PathBuf::with_capacity(self.objects_dir.as_os_str().len() + 75);
        path.push(&self.objects_dir);
        path.push(&hex[..2]);
        path.push(&hex[2..]);
        match layout {
            ObjectLayout::Raw => {
                path.set_extension("raw");
            }
            ObjectLayout::Segmented => {
                path.set_extension("segments");
            }
            ObjectLayout::Contiguous => {}
        }
        path
    }

    fn object_path(&self, key: &str, layout: ObjectLayout) -> PathBuf {
        let hex = sha256_hex_bytes(key);
        self.hashed_object_path(std::str::from_utf8(&hex).expect("hex is ASCII"), layout)
    }

    fn segmented_object_path(&self, key: &str) -> PathBuf {
        self.object_path(key, ObjectLayout::Segmented)
    }

    fn record_path(&self, record: &MetadataRecord) -> PathBuf {
        self.object_path(&record.key, record.layout)
    }

    /// A process-unique staging path. `create_new` on open guarantees no two
    /// concurrent writers collide even if the clock is coarse.
    fn staging_path(&self) -> PathBuf {
        self.staging_dir.join(unique_staging_component("tmp"))
    }

    /// Private directory used while staged fragments and their manifest become
    /// durable before one atomic adoption into `objects/`.
    fn segmented_staging_path(&self) -> PathBuf {
        self.staging_dir.join(unique_staging_component("segments"))
    }

    /// A private retirement name for a pinned generation removed from its
    /// public object path.
    fn deferred_path(&self) -> PathBuf {
        self.staging_dir.join(unique_staging_component("deferred"))
    }

    fn with_record_bytes<T>(
        &self,
        key: &str,
        read: impl FnOnce(Option<&[u8]>) -> Result<T>,
    ) -> Result<T> {
        let txn = self.db.begin_read().map_err(map_redb)?;
        let table = match txn.open_table(METADATA) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return read(None),
            Err(err) => return Err(Error::storage(err.to_string())),
        };
        let value = table.get(key).map_err(map_redb)?;
        read(value.as_ref().map(|bytes| bytes.value()))
    }

    /// Unknown forward schemas remain misses rather than being misdecoded.
    fn get_record(&self, key: &str) -> Result<Option<MetadataRecord>> {
        self.with_record_bytes(key, |bytes| match bytes {
            Some(bytes) => decode_record(bytes),
            None => Ok(None),
        })
    }

    fn get_lookup_record(
        &self,
        key: &str,
        snapshot: &mut LookupRecordSnapshot,
    ) -> Result<Option<MetadataRecord>> {
        snapshot.identity = None;
        snapshot.bytes.clear();
        self.with_record_bytes(key, |bytes| {
            let Some(bytes) = bytes else { return Ok(None) };
            let record = decode_record(bytes)?;
            if let Some(record) = &record {
                snapshot.bytes.extend_from_slice(bytes);
                snapshot.identity = Some((record.state, record.layout, record.generation));
            }
            Ok(record)
        })
    }

    /// A fresh transaction still validates ownership after open. Unchanged bytes
    /// reuse the decoded identity; history-only rewrites take the ordinary check.
    fn lookup_record_is_current(
        &self,
        key: &str,
        snapshot: &LookupRecordSnapshot,
        generation: u64,
        layout: Option<ObjectLayout>,
    ) -> Result<bool> {
        self.with_record_bytes(key, |bytes| {
            let Some(bytes) = bytes else { return Ok(false) };
            let identity = if bytes == snapshot.bytes {
                snapshot.identity
            } else {
                decode_record(bytes)?.map(|record| (record.state, record.layout, record.generation))
            };
            Ok(
                identity.is_some_and(|(state, current_layout, current_generation)| {
                    state == CacheState::Cached
                        && current_generation == generation
                        && layout.is_none_or(|expected| expected == current_layout)
                }),
            )
        })
    }

    /// Inserts/overwrites the record for `key` in one durable ACID write txn
    /// (`Immediate`: fsync before return). Used for every real *state*
    /// transition, so a HIT after restart is guaranteed (C1/C13/I13).
    fn put_record(&self, record: &MetadataRecord) -> Result<()> {
        self.invalidate_local_path(&record.key);
        let record = self.prepare_key_record(record)?;
        let bytes = encode_record(&record)?;
        let txn = self.db.begin_write().map_err(map_redb)?;
        {
            self.update_physical_keys(&txn, &record)?;
            #[cfg(test)]
            if record.state == CacheState::Cached && !record.physical_keys.is_empty() {
                fail_if_injected(FaultPoint::PhysicalKeyRegistration)?;
            }
            let mut table = txn.open_table(METADATA).map_err(map_redb)?;
            table
                .insert(record.key.as_str(), bytes.as_slice())
                .map_err(map_redb)?;
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }

    /// Accumulates a non-critical access-history bump in memory (the hot serve
    /// path). A redb write transaction costs ~ms, but a HIT must be ~µs, so the
    /// implementation coalesces bumps and flushes them in rare batches (on threshold, before any
    /// state mutation, and on drop). Losing the last few in-memory bumps on a
    /// crash is harmless — it is eviction-scoring history, never cache state.
    fn record_hit(&self, key: &str) {
        let last_access = now_unix();
        let should_flush = {
            let mut pending = self.lock_pending();
            if let Some(entry) = pending.get_mut(key) {
                entry.record_hit(last_access);
            } else {
                pending.insert(
                    key.to_owned(),
                    PendingBump {
                        hits: 1,
                        last_access,
                    },
                );
            }
            pending.len() >= PENDING_FLUSH_THRESHOLD
        };
        if should_flush {
            // Drops any flush error because history is non-critical (best-effort).
            let _ = self.flush_pending();
        }
    }

    /// Merges pending access history in one durable transaction. A bump for a
    /// key whose record no longer exists is discarded (no resurrection).
    fn flush_pending(&self) -> Result<()> {
        let drained: Vec<(String, PendingBump)> = {
            let mut pending = self.lock_pending();
            if pending.is_empty() {
                return Ok(());
            }
            pending.drain().collect()
        };

        let txn = self.db.begin_write().map_err(map_redb)?;
        {
            let mut table = txn.open_table(METADATA).map_err(map_redb)?;
            for (key, bump) in drained {
                let current = match table.get(key.as_str()).map_err(map_redb)? {
                    // Discards the bump when the record was removed, or when it
                    // was written by an unknown (newer) schema version we must
                    // not rewrite at our older layout.
                    Some(bytes) => match decode_record(bytes.value())? {
                        Some(record) => record,
                        None => continue,
                    },
                    None => continue,
                };
                let mut next = current;
                next.hit_count = next.hit_count.saturating_add(bump.hits);
                next.last_access = next.last_access.max(bump.last_access);
                let encoded = encode_record(&next)?;
                table
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(map_redb)?;
            }
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }

    /// Delete the record for `key` in one ACID write txn.
    fn delete_record(&self, key: &str) -> Result<()> {
        self.invalidate_local_path(key);
        let txn = self.db.begin_write().map_err(map_redb)?;
        {
            let mut table = txn.open_table(METADATA).map_err(map_redb)?;
            let previous = table
                .get(key)
                .map_err(map_redb)?
                .and_then(|v| decode_record(v.value()).ok().flatten());
            if let Some(previous) = previous {
                self.remove_physical_keys(&txn, &previous)?;
            }
            table.remove(key).map_err(map_redb)?;
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }

    /// Restores the stable state that preceded an unsuccessful fill.
    fn restore_fill_state(
        &self,
        key: &str,
        fallback_record: Option<&MetadataRecord>,
    ) -> Result<()> {
        match fallback_record {
            Some(record) => self.put_record(record),
            None => self.delete_record(key),
        }
    }

    /// Whether `key`'s current record still carries `generation`.
    ///
    /// Only the generation is compared, because a coalesced history flush
    /// rewrites `hit_count` and `last_access` under an unchanged generation and
    /// is not a takeover. A caller that also depends on the state tests it.
    fn generation_is_current(&self, key: &str, generation: u64) -> Result<bool> {
        Ok(self
            .get_record(key)?
            .is_some_and(|record| record.generation == generation))
    }

    /// Whether `key` is still the in-progress fill claimed by `generation`.
    fn fill_is_current(&self, key: &str, generation: u64) -> Result<bool> {
        Ok(self.get_record(key)?.is_some_and(|record| {
            record.generation == generation && record.state == CacheState::FetchInProgress
        }))
    }

    /// Snapshot every record (key + value) for the recovery scan.
    fn all_records(&self) -> Result<Vec<MetadataRecord>> {
        let txn = self.db.begin_read().map_err(map_redb)?;
        let table = match txn.open_table(METADATA) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(Error::storage(err.to_string())),
        };
        let mut out = Vec::new();
        for entry in table.iter().map_err(map_redb)? {
            let (_k, v) = entry.map_err(map_redb)?;
            // Skip records written by an unknown (newer) schema version; they
            // are purged by recovery, so startup still boots cleanly after a
            // rollback or format bump.
            if let Some(record) = decode_record(v.value())? {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Deletes records whose stored value is not a current decodable schema,
    /// returning how many were removed. Their data files lose protection and the
    /// orphan sweep reclaims them, so a format change cold-starts those entries.
    fn purge_stale_records(&self) -> Result<usize> {
        let stale: Vec<String> = {
            let txn = self.db.begin_read().map_err(map_redb)?;
            let table = match txn.open_table(METADATA) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
                Err(err) => return Err(Error::storage(err.to_string())),
            };
            let mut stale = Vec::new();
            for entry in table.iter().map_err(map_redb)? {
                let (k, v) = entry.map_err(map_redb)?;
                if !matches!(decode_record(v.value()), Ok(Some(_))) {
                    stale.push(k.value().to_string());
                }
            }
            stale
        };
        for key in &stale {
            self.delete_record(key)?;
        }
        Ok(stale.len())
    }
}

/// Disk-backed [`Storage`]: raw payload files (byte authority) plus a
/// redb metadata store (state authority).
///
/// Cheap to clone-share via the internal `Arc`. The API is synchronous; async
/// callers should drive it from `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct DiskStorage {
    inner: Arc<Inner>,
}

impl DiskStorage {
    /// Opens (creating if absent) a disk cache rooted at `root`, then runs
    /// [`Storage::recover_on_open`] so the store starts in a consistent state.
    ///
    /// Creates `root`, `root/objects`, `root/staging`, and opens the redb store
    /// at `root/metadata.redb`. Reopening the same `root` recovers all committed
    /// records, so cache hits survive restarts.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_key_policy(root, None)
    }

    /// Opts into logical-key lookup and recovery backfill. Policy has no network
    /// or body-I/O authority; the ordinary exact-only entry point stays unchanged.
    pub fn open_with_key_policy(
        root: &Path,
        key_policy: Option<Arc<dyn KeyPolicy>>,
    ) -> Result<Self> {
        Self::open_with_policy_and_clock(root, key_policy, Arc::new(SystemLeaseClock))
    }

    pub fn open_with_lease_clock(root: &Path, clock: Arc<dyn LeaseClock>) -> Result<Self> {
        Self::open_with_policy_and_clock(root, None, clock)
    }

    fn open_with_policy_and_clock(
        root: &Path,
        key_policy: Option<Arc<dyn KeyPolicy>>,
        lease_clock: Arc<dyn LeaseClock>,
    ) -> Result<Self> {
        fs::create_dir_all(root)?;
        let objects_dir = root.join("objects");
        let staging_dir = root.join("staging");
        fs::create_dir_all(&objects_dir)?;
        fs::create_dir_all(&staging_dir)?;

        let db = open_metadata_db(&root.join("metadata.redb"))?;
        let storage = Self {
            inner: Arc::new(Inner {
                path_leases: Mutex::new(path_leases::Catalog::default()),
                lease_clock,
                key_policy,
                key_index_ready: AtomicBool::new(false),
                root: root.to_path_buf(),
                objects_dir,
                staging_dir,
                db,
                pending: Mutex::new(HashMap::new()),
                operations: Mutex::new(()),
                #[cfg(test)]
                fail_retire_key: Mutex::new(None),
                pins: Mutex::new(HashMap::new()),
                active_staging: Mutex::new(HashSet::new()),
                active_fills: Mutex::new(HashMap::new()),
                deferred: Mutex::new(VecDeque::new()),
            }),
        };
        storage.recover_on_open()?;
        path_leases::recover(&storage)?;
        Ok(storage)
    }

    /// Projects a record's [`StoredMeta`].
    fn into_meta(record: MetadataRecord) -> StoredMeta {
        StoredMeta {
            logical_key: record.logical_key,
            size: record.size_bytes,
            revision: record.generation,
            provenance: record.provenance,
            max_idle_secs: record.max_idle_secs,
            status: record.status,
            etag: record.etag,
            last_modified: record.last_modified,
            content_type: record.content_type,
            headers: record.headers,
            representation_length: record.representation_length,
            expires_at: record.expires_at,
            versionable: record.versionable,
            content_range: record.content_range,
        }
    }

    fn meta_of(record: &MetadataRecord) -> StoredMeta {
        StoredMeta {
            logical_key: record.logical_key.clone(),
            size: record.size_bytes,
            revision: record.generation,
            provenance: record.provenance,
            max_idle_secs: record.max_idle_secs,
            status: record.status,
            etag: record.etag.clone(),
            last_modified: record.last_modified.clone(),
            content_type: record.content_type.clone(),
            headers: record.headers.clone(),
            representation_length: record.representation_length,
            expires_at: record.expires_at,
            versionable: record.versionable,
            content_range: record.content_range.clone(),
        }
    }

    fn lookup_with_snapshot(
        &self,
        key: &str,
        snapshot: &mut LookupRecordSnapshot,
    ) -> Result<Lookup> {
        let Some(record) = self.inner.get_lookup_record(key, snapshot)? else {
            return Ok(Lookup::Miss); // S0
        };

        match record.state {
            CacheState::MetadataOnly | CacheState::FetchInProgress => {
                // S2/S4 state with remembered metadata and no serveable bytes.
                Ok(Lookup::SoftMiss(Self::into_meta(record)))
            }
            CacheState::Cached => {
                let path = self.inner.record_path(&record);
                match record.layout {
                    ObjectLayout::Raw => {
                        #[cfg(test)]
                        let barriers = {
                            let mut hook = LOOKUP_RECORD_BARRIERS
                                .get_or_init(|| Mutex::new(None))
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if hook
                                .as_ref()
                                .is_some_and(|(hook_key, _, _)| hook_key == key)
                            {
                                hook.take().map(|(_, entered, release)| (entered, release))
                            } else {
                                None
                            }
                        };
                        #[cfg(test)]
                        if let Some((entered, release)) = barriers {
                            entered.wait();
                            release.wait();
                        }
                        // I16: opens the fd before validating. An open fd
                        // survives racing unlink, so an in-flight serve completes.
                        let file = match File::open(&path) {
                            Ok(file) => file,
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                                self.invalidate_lookup_if_current(&record, &path, false)?;
                                return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                            }
                            Err(err) => return Err(err.into()),
                        };

                        // The record read and this open are not atomic, so a replacement landing
                        // in that window pairs the new generation's fd with the
                        // old metadata. Requiring the same still-Cached
                        // contiguous generation rejects that pairing, mirroring
                        // `open_logical_match`.
                        if !self.inner.lookup_record_is_current(
                            key,
                            snapshot,
                            record.generation,
                            Some(ObjectLayout::Raw),
                        )? {
                            return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                        }

                        // Rechecks ownership under the mutation lock before retiring
                        // malformed bytes, so a concurrent replacement survives.
                        if !raw_file_validates(&file, &record)? {
                            self.invalidate_lookup_if_current(&record, &path, true)?;
                            return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                        }

                        self.bump_history(&record);
                        Ok(Lookup::Hit(Lease {
                            path,
                            meta: Self::into_meta(record),
                            file,
                            body_offset: 0,
                        }))
                    }
                    ObjectLayout::Contiguous => Err(Error::storage("retired object layout")),
                    ObjectLayout::Segmented => {
                        // Serialize directory open + pin with replacement and
                        // removal. Once pinned, the expensive manifest
                        // validation can safely proceed without this lock.
                        let operation = self.inner.write_operations();
                        // Holds the directory fd across complete validation and
                        // every later lazy open.
                        let directory = match ovcc_platform::open_directory(&path) {
                            Ok(directory) => directory,
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                                drop(operation);
                                self.invalidate_lookup_if_current(&record, &path, false)?;
                                return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                            }
                            Err(err) => return Err(err.into()),
                        };
                        if !directory.metadata()?.file_type().is_dir() {
                            drop(operation);
                            self.invalidate_lookup_if_current(&record, &path, true)?;
                            return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                        }
                        let identity = ovcc_platform::file_identity(&directory)?;
                        #[cfg(test)]
                        let barriers = {
                            let mut hook = LOOKUP_PIN_BARRIERS
                                .get_or_init(|| Mutex::new(None))
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if hook
                                .as_ref()
                                .is_some_and(|(hook_key, _, _)| hook_key == key)
                            {
                                hook.take().map(|(_, entered, release)| (entered, release))
                            } else {
                                None
                            }
                        };
                        #[cfg(test)]
                        if let Some((entered, release)) = barriers {
                            entered.wait();
                            release.wait();
                        }
                        // Pin immediately after open, before validation. A racing
                        // replacement/removal must defer unlinking children that
                        // this lazy directory lease may later open.
                        self.inner.pin_generation(identity);
                        drop(operation);
                        let manifest = match validate_segmented_directory(
                            &directory,
                            key,
                            record.generation,
                            Some(record.size_bytes),
                        ) {
                            Ok(manifest) => manifest,
                            Err(_) => {
                                self.inner.release_generation(identity);
                                self.invalidate_lookup_if_current(&record, &path, true)?;
                                return Ok(Lookup::SoftMiss(Self::into_meta(record)));
                            }
                        };

                        self.bump_history(&record);
                        Ok(Lookup::Hit(self.segmented_lease(
                            path,
                            Self::into_meta(record),
                            directory,
                            &manifest,
                            Some(identity),
                        )?))
                    }
                }
            }
        }
    }
}

impl Storage for DiskStorage {
    fn local_path_source(&self, key: &str) -> Option<LocalPathSource> {
        Some(DiskStorage::local_path_source(self, key))
    }
    fn lookup_logical(&self, key: &LogicalKey) -> Result<Option<LogicalMatch<Lease>>> {
        DiskStorage::lookup_logical(self, key)
    }

    fn peek_logical(&self, key: &LogicalKey) -> Result<Option<LogicalMatch<StoredMeta>>> {
        DiskStorage::peek_logical(self, key)
    }
    fn validate_segmented_sources(&self, segments: &[StagedSegment]) -> Result<()> {
        prevalidate_segments(&self.inner, segments).map(|_| ())
    }

    fn lookup(&self, key: &str) -> Result<Lookup> {
        self.lookup_with_snapshot(key, &mut LookupRecordSnapshot::new())
    }

    fn peek(&self, key: &str) -> Result<Option<StoredMeta>> {
        let Some(record) = self.inner.get_record(key)? else {
            return Ok(None);
        };
        if record.state != CacheState::Cached {
            return Ok(None);
        }
        let path = self.inner.record_path(&record);
        match record.layout {
            ObjectLayout::Raw => {
                let file = match File::open(path) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                if !raw_file_validates(&file, &record)? {
                    return Ok(None);
                }
            }
            ObjectLayout::Contiguous => return Ok(None),
            ObjectLayout::Segmented => {
                let directory = match ovcc_platform::open_directory(&path) {
                    Ok(directory) => directory,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
                if !directory.metadata()?.file_type().is_dir()
                    || validate_segmented_directory(
                        &directory,
                        key,
                        record.generation,
                        Some(record.size_bytes),
                    )
                    .is_err()
                {
                    return Ok(None);
                }
            }
        }
        Ok(Some(Self::into_meta(record)))
    }

    fn refresh_freshness(&self, key: &str, expected: &StoredMeta, expires_at: i64) -> Result<bool> {
        let _operation = self.inner.write_operations();
        // Coalesced hit history must be folded into the durable record before
        // this state mutation, without making the refresh itself an access.
        self.inner.flush_pending()?;

        let Some(mut record) = self.inner.get_record(key)? else {
            return Ok(false);
        };
        if record.state != CacheState::Cached || Self::meta_of(&record) != *expected {
            return Ok(false);
        }

        record.expires_at = Some(expires_at);
        self.inner.put_record(&record)?;
        Ok(true)
    }

    fn begin_fill(&self, key: &str, meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
        self.inner.validate_key_admission(key, &meta)?;
        let _operation = self.inner.write_operations();
        self.inner.drain_deferred()?;
        fs::create_dir_all(&self.inner.staging_dir)?;
        // Persists coalesced history before mutating state, so that the read and
        // rewritten record carries any in-memory hit bumps.
        self.inner.flush_pending()?;

        // Retires any old public generation before replacement metadata becomes
        // durable. Existing leases keep their open descriptors, while recovery
        // cannot mistake the old body for the new FetchInProgress record.
        let prior = self.inner.get_record(key)?;
        let fallback_record = prior
            .as_ref()
            .filter(|record| record.has_history())
            .cloned()
            .map(|mut record| {
                record.state = CacheState::MetadataOnly;
                record
            });
        self.inner.retire_path(&self.inner.object_paths(key).1)?;
        self.inner
            .retire_path(&self.inner.segmented_object_path(key))?;

        let generation = next_distinct_generation(
            key,
            meta.size,
            prior.as_ref().map(|record| record.generation),
        );

        // Preserves prior history and transitions the record to FetchInProgress.
        let record = match prior {
            Some(mut rec) => {
                rec.logical_key = meta.logical_key.clone();
                rec.state = CacheState::FetchInProgress;
                rec.layout = ObjectLayout::Raw;
                rec.generation = generation;
                rec.provenance = meta.provenance;
                rec.max_idle_secs = meta.max_idle_secs;
                rec.status = meta.status;
                rec.etag = meta.etag.clone();
                rec.last_modified = meta.last_modified.clone();
                rec.content_type = meta.content_type.clone();
                rec.headers = meta.headers.clone();
                rec.representation_length = meta.representation_length;
                rec.expires_at = meta.expires_at;
                rec.versionable = meta.versionable;
                rec.content_range = meta.content_range.clone();
                rec
            }
            None => MetadataRecord {
                logical_key: meta.logical_key.clone(),
                physical_keys: Vec::new(),
                key: key.to_string(),
                state: CacheState::FetchInProgress,
                layout: ObjectLayout::Raw,
                generation,
                provenance: meta.provenance,
                max_idle_secs: meta.max_idle_secs,
                status: meta.status,
                etag: meta.etag.clone(),
                last_modified: meta.last_modified.clone(),
                content_type: meta.content_type.clone(),
                headers: meta.headers.clone(),
                representation_length: meta.representation_length,
                expires_at: meta.expires_at,
                versionable: meta.versionable,
                content_range: meta.content_range.clone(),
                size_bytes: 0,
                last_access: now_unix(),
                hit_count: 0,
                fetch_cost: 0,
                fetch_duration: 0,
            },
        };
        self.inner.put_record(&record)?;

        // Staging contains only payload bytes, including while a follower reads it.
        let staging_path = self.inner.staging_path();
        let active_staging = ActiveStaging::new(&self.inner, &staging_path);
        let active_fill = ActiveFill::new(&self.inner, key, record.generation);
        let staged = (|| -> Result<File> {
            #[cfg(test)]
            fail_if_injected(FaultPoint::BeginStagingCreate)?;
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&staging_path)?;
            Ok(file)
        })();
        let file = match staged {
            Ok(staged) => staged,
            Err(error) => {
                let state_cleanup = self.inner.restore_fill_state(key, fallback_record.as_ref());
                let staging_cleanup = remove_file_if_present(&staging_path);
                return Err(with_cleanup_error(
                    error,
                    state_cleanup.and(staging_cleanup),
                ));
            }
        };

        Ok(Box::new(DiskWriter {
            inner: Arc::clone(&self.inner),
            staging_path,
            published_path: None,
            published_file: None,
            _active_staging: active_staging,
            _active_fill: active_fill,
            file: Some(file),
            record: Some(record),
            fallback_record,
            state_restored: false,
            revoked: None,
            status: meta.status,
            etag: meta.etag,
            last_modified: meta.last_modified,
            content_type: meta.content_type,
            headers: meta.headers,
            representation_length: meta.representation_length,
            expires_at: meta.expires_at,
            versionable: meta.versionable,
            content_range: meta.content_range,
            written: 0,
        }))
    }

    fn adopt_segmented(
        &self,
        key: &str,
        meta: StoredMeta,
        segments: &[StagedSegment],
    ) -> Result<Lease> {
        self.adopt_segmented_with_admission(key, meta, segments, &mut || Ok(()))
    }

    fn adopt_segmented_with_admission(
        &self,
        key: &str,
        meta: StoredMeta,
        segments: &[StagedSegment],
        admit: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Lease> {
        self.inner.validate_key_admission(key, &meta)?;
        segmented_adoption_disk_growth(key, segments)?;
        // No storage state or source path changes before the complete ordered
        // input passes existence/type/size/id/overflow/volume validation.
        let validated = prevalidate_segments(&self.inner, segments)?;
        let destination = self.inner.segmented_object_path(key);
        ensure_object_parent(&self.inner.objects_dir, &destination)?;
        let generation = next_generation(key, validated.total_length);

        let staging_path = self.inner.segmented_staging_path();
        let _active_staging = ActiveStaging::new(&self.inner, &staging_path);
        fs::create_dir(&staging_path)?;
        sync_parent(&staging_path)?;
        #[cfg(test)]
        let barriers = {
            let mut hook = SEGMENT_PREPARE_BARRIERS
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if hook
                .as_ref()
                .is_some_and(|(hook_key, _, _)| hook_key == key)
            {
                hook.take().map(|(_, entered, release)| (entered, release))
            } else {
                None
            }
        };
        #[cfg(test)]
        if let Some((entered, release)) = barriers {
            entered.wait();
            release.wait();
        }
        let manifest = SegmentManifest {
            key: key.to_owned(),
            generation,
            total_length: validated.total_length,
            entries: validated.entries.clone(),
        };
        let mut moved = Vec::with_capacity(validated.unique_sources.len());
        let prepared = (|| -> Result<(File, SegmentManifest)> {
            for source in &validated.unique_sources {
                let staged_path = staging_path.join(&source.filename);
                ovcc_platform::rename(&source.path, &staged_path)?;
                moved.push(source.clone());
                let stat = ovcc_platform::stat_nofollow(&staged_path)?;
                if !stat.metadata.file_type().is_file()
                    || stat.identity != source.identity
                    || stat.metadata.len() != source.size
                {
                    return Err(Error::storage(
                        "staged segment identity changed during adoption",
                    ));
                }
                ovcc_platform::sync_file(&staged_path)?;
            }
            for parent in &validated.source_parents {
                sync_directory(parent)?;
            }
            sync_directory(&staging_path)?;

            write_manifest(&staging_path, &manifest)?;
            sync_directory(&staging_path)?;

            let directory = ovcc_platform::open_directory(&staging_path)?;
            let parsed = validate_segmented_directory(
                &directory,
                key,
                generation,
                Some(validated.total_length),
            )?;
            Ok((directory, parsed))
        })();

        let (directory, parsed_manifest) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                rollback_moved_sources(&staging_path, &moved);
                let _ = remove_object_path(&staging_path);
                let _ = sync_parent(&staging_path);
                return Err(error);
            }
        };
        if let Err(error) = admit() {
            rollback_moved_sources(&staging_path, &moved);
            let _ = remove_object_path(&staging_path);
            let _ = sync_parent(&staging_path);
            return Err(error);
        }

        // Only the short state transition + directory rename + redb commit
        // exclude lookups. Fragment and manifest durability work above happens
        // in a private directory without stalling unrelated HITs.
        let _operation = self.inner.write_operations();
        if let Err(error) = self
            .inner
            .drain_deferred()
            .and_then(|_| self.inner.flush_pending())
        {
            rollback_moved_sources(&staging_path, &moved);
            let _ = remove_object_path(&staging_path);
            let _ = sync_parent(&staging_path);
            return Err(error);
        }
        let prior = match self.inner.get_record(key) {
            Ok(prior) => prior,
            Err(error) => {
                rollback_moved_sources(&staging_path, &moved);
                let _ = remove_object_path(&staging_path);
                let _ = sync_parent(&staging_path);
                return Err(error);
            }
        };
        let prior_had_history = prior
            .as_ref()
            .map(MetadataRecord::has_history)
            .unwrap_or(false);
        let mut record = match prior {
            Some(mut record) => {
                record.logical_key = meta.logical_key.clone();
                record.state = CacheState::FetchInProgress;
                record.layout = ObjectLayout::Segmented;
                record.generation = generation;
                record.provenance = meta.provenance;
                record.max_idle_secs = meta.max_idle_secs;
                record.status = meta.status;
                record.etag = meta.etag.clone();
                record.last_modified = meta.last_modified.clone();
                record.content_type = meta.content_type.clone();
                record.headers = meta.headers.clone();
                record.representation_length = meta.representation_length;
                record.expires_at = meta.expires_at;
                record.versionable = meta.versionable;
                record.content_range = meta.content_range.clone();
                record
            }
            None => MetadataRecord {
                logical_key: meta.logical_key.clone(),
                physical_keys: Vec::new(),
                key: key.to_owned(),
                state: CacheState::FetchInProgress,
                layout: ObjectLayout::Segmented,
                generation,
                provenance: meta.provenance,
                max_idle_secs: meta.max_idle_secs,
                status: meta.status,
                etag: meta.etag.clone(),
                last_modified: meta.last_modified.clone(),
                content_type: meta.content_type.clone(),
                headers: meta.headers.clone(),
                representation_length: meta.representation_length,
                expires_at: meta.expires_at,
                versionable: meta.versionable,
                content_range: meta.content_range.clone(),
                size_bytes: 0,
                last_access: now_unix(),
                hit_count: 0,
                fetch_cost: 0,
                fetch_duration: 0,
            },
        };
        // Recovery can promote a fully published directory if the final Cached
        // transaction is interrupted.
        record.size_bytes = validated.total_length;
        if let Err(error) = self.inner.put_record(&record) {
            rollback_moved_sources(&staging_path, &moved);
            let _ = remove_object_path(&staging_path);
            let _ = sync_parent(&staging_path);
            return Err(error);
        }

        if let Err(publication) = self.inner.publish_staged(&staging_path, &destination) {
            if !publication.published {
                rollback_moved_sources(&staging_path, &moved);
                let _ = remove_object_path(&staging_path);
                let _ = sync_parent(&staging_path);
                self.revert_segmented_fill(&record, prior_had_history);
            }
            // Once rename/exchange linearized, the FetchInProgress record and
            // complete final directory are intentionally left for recovery.
            return Err(publication.error);
        }

        record.state = CacheState::Cached;
        record.size_bytes = validated.total_length;
        record.last_access = now_unix();
        record = self.inner.prepare_key_record(&record)?;
        self.inner.put_record(&record)?;
        // A key has exactly one active physical representation. Retire a
        // raw sibling immediately; a pinned contiguous fd remains
        // readable after unlink.
        self.inner.retire_path(&self.inner.object_paths(key).1)?;

        self.segmented_lease(
            destination,
            StoredMeta {
                logical_key: record.logical_key,
                size: validated.total_length,
                revision: generation,
                provenance: meta.provenance,
                max_idle_secs: meta.max_idle_secs,
                status: meta.status,
                etag: meta.etag,
                last_modified: meta.last_modified,
                content_type: meta.content_type,
                headers: meta.headers,
                representation_length: meta.representation_length,
                expires_at: meta.expires_at,
                versionable: meta.versionable,
                content_range: meta.content_range,
            },
            directory,
            &parsed_manifest,
            None,
        )
    }

    fn evict_data(&self, key: &str, revision: u64) -> Result<bool> {
        let _operation = self.inner.write_operations();
        self.inner.drain_deferred()?;
        self.inner.flush_pending()?;
        let Some(record) = self.inner.get_record(key)? else {
            return Ok(false);
        };
        // I17: only Cached data is data-evictable; skips in-flight fetches.
        if record.state != CacheState::Cached {
            return Ok(false);
        }
        // Every publication of a `Cached` record takes the lock held here, as
        // do `remove`, `begin_fill`, `adopt_segmented`, recovery and the abort
        // restore, so a replacement lands wholly before this compare, which
        // then refuses the superseded decision, or wholly after, which
        // republishes over the eviction. The coalesced history flush is the
        // one record write that runs unlocked, and it only adds `hit_count`
        // and `last_access` under an unchanged generation.
        if !self.inner.generation_is_current(key, revision)? {
            return Ok(false);
        }
        if self.inner.local_path_blocks_eviction(key, revision) {
            return Ok(false);
        }
        // Delete metadata and physical-key mappings before retiring bytes. A
        // crash leaves only an orphan for recovery, never retained history.
        self.inner.delete_record(key)?;

        self.inner.retire_path(&self.inner.object_paths(key).1)?;
        self.inner
            .retire_path(&self.inner.segmented_object_path(key))?;
        Ok(true)
    }

    fn remove(&self, key: &str) -> Result<()> {
        let _operation = self.inner.write_operations();
        // Drops any pending bumps for this key so a later flush cannot resurrect
        // history onto the now-removed record (the flush also discards orphans,
        // but clearing here is cheaper and clearer).
        self.inner.lock_pending().remove(key);
        // Remove the serveable state first. A physical cleanup failure may
        // leave only an unprotected orphan; it can never leave stale bytes HIT.
        self.inner.delete_record(key)?;
        // Unrelated deferred cleanup occurs only after the requested key is
        // durably non-serveable.
        self.inner.drain_deferred()?;
        #[cfg(test)]
        let fail_retire = {
            let mut fail_key = self
                .inner
                .fail_retire_key
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if fail_key.as_deref() == Some(key) {
                fail_key.take();
                true
            } else {
                false
            }
        };
        #[cfg(test)]
        if fail_retire {
            return Err(Error::storage("injected retirement failure"));
        }
        self.inner.retire_path(&self.inner.object_paths(key).1)?;
        self.inner
            .retire_path(&self.inner.segmented_object_path(key))?;
        Ok(())
    }

    fn eviction_candidates(&self) -> Result<Vec<EvictionCandidate>> {
        let _operation = self.inner.write_operations();
        self.inner.drain_deferred()?;
        self.inner.flush_pending()?;
        Ok(self
            .inner
            .all_records()?
            .into_iter()
            .filter(|record| record.state == CacheState::Cached)
            .map(|record| EvictionCandidate {
                key: record.key,
                revision: record.generation,
                size_bytes: record.size_bytes,
                last_access: record.last_access,
                hit_count: record.hit_count,
                provenance: record.provenance,
                max_idle_secs: record.max_idle_secs,
            })
            .collect())
    }

    fn available_bytes(&self) -> Result<u64> {
        let _operation = self.inner.write_operations();
        self.inner.drain_deferred()?;
        ovcc_platform::available_bytes(&self.inner.root)
            .map_err(|err| Error::storage(err.to_string()))
    }

    fn recover_on_open(&self) -> Result<RecoveryReport> {
        let _operation = self.inner.write_operations();
        self.inner.key_index_ready.store(false, Ordering::Release);
        // A derived index must not block reconciliation of authoritative
        // records when one of its values is corrupt.
        self.inner.reset_physical_keys()?;
        // Persists any coalesced history before reclassifying records.
        self.inner.flush_pending()?;
        let mut report = RecoveryReport::default();
        report.drained_deferred += self.inner.drain_deferred()?;

        // Drops records left by an older/newer engine whose format version differs,
        // before the scan below decodes the rest; their files fall to the orphan sweep.
        report.discarded_stale_format += self.inner.purge_stale_records()?;

        // Sweeps leftover staging temp files (C2/C5 partial downloads).
        let (files, directories) = sweep_staging(&self.inner)?;
        report.swept_staging += files;
        report.swept_staging_directories += directories;

        // Reconciles every record against file presence and collects the set of
        // files that should survive (those that end as Cached).
        let mut protected: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut metadata_only = Vec::new();
        for record in self.inner.all_records()? {
            let path = self.inner.record_path(&record);
            let object_ok = object_validates(&path, &record);

            match record.state {
                CacheState::Cached => {
                    if object_ok {
                        // B2/S1: keeps.
                        protected.insert(path);
                        report.cached += 1;
                    } else {
                        // B3/C3/I9: Cached but no valid file -> MetadataOnly.
                        self.reconcile_to_metadata_only(&record)?;
                        report.reconciled_to_metadata_only += 1;
                    }
                }
                CacheState::FetchInProgress => {
                    if self.inner.fill_is_live(&record.key, record.generation) {
                        // Recovery also runs while the process serves, and a
                        // writer still holding this key has not failed.
                        // Reclassifying its record would revoke the fill, so
                        // the record and any bytes it already published stay
                        // its own, exactly as `sweep_staging` spares a live
                        // writer's staging file.
                        if object_ok {
                            protected.insert(path);
                        }
                        report.skipped_active_fills += 1;
                    } else if object_ok {
                        // C-window: crash after rename, before commit txn. The
                        // bytes are durable and validate, so it promotes to Cached.
                        let mut next = record.clone();
                        next.state = CacheState::Cached;
                        next.size_bytes = object_len(&path, &next)?;
                        self.inner.put_record(&next)?;
                        protected.insert(path);
                        report.promoted_to_cached += 1;
                    } else if record.has_history() {
                        // C5: failed refill with history -> MetadataOnly.
                        self.reconcile_to_metadata_only(&record)?;
                        report.reconciled_to_metadata_only += 1;
                    } else {
                        // C5: failed cold fill, no history -> Absent.
                        self.inner.delete_record(&record.key)?;
                        report.removed_records += 1;
                    }
                }
                CacheState::MetadataOnly => {
                    // Older eviction paths kept history after removing bytes.
                    // Recovery drops that residue before rebuilding the index.
                    metadata_only.push(record);
                    report.metadata_only += 1;
                }
            }
        }

        for batch in metadata_only.chunks(RECOVERY_DELETE_BATCH) {
            let txn = self.inner.db.begin_write().map_err(map_redb)?;
            {
                let mut table = txn.open_table(METADATA).map_err(map_redb)?;
                for record in batch {
                    self.inner.remove_physical_keys(&txn, record)?;
                    table.remove(record.key.as_str()).map_err(map_redb)?;
                }
            }
            txn.commit().map_err(map_redb)?;
            report.removed_records += batch.len();
        }

        // Sweeps orphans, deleting any data file not protected by a Cached
        // record (B4 no-record orphans, C4 deleted-record residue, and
        // stale MetadataOnly files).
        let (files, directories) = sweep_orphan_objects(&self.inner, &protected)?;
        report.deleted_orphan_files += files;
        report.deleted_orphan_directories += directories;

        self.rebuild_physical_keys()?;
        Ok(report)
    }
}

impl DiskStorage {
    fn segmented_lease(
        &self,
        path: PathBuf,
        mut meta: StoredMeta,
        directory: File,
        manifest: &SegmentManifest,
        pinned_identity: Option<ObjectIdentity>,
    ) -> Result<Lease> {
        let identity = if let Some(identity) = pinned_identity {
            identity
        } else {
            if !directory.metadata()?.file_type().is_dir() {
                return Err(Error::storage("segmented object is not a directory"));
            }
            ovcc_platform::file_identity(&directory)?
        };
        let descriptors = Arc::new(
            manifest
                .entries
                .iter()
                .enumerate()
                .map(|(ordinal, entry)| SegmentDescriptor {
                    ordinal,
                    size: entry.size,
                    filename: entry.filename.clone(),
                    generation: manifest.generation,
                })
                .collect(),
        );

        if pinned_identity.is_none() {
            self.inner.pin_generation(identity);
        }
        let handle_key = ovcc_platform::handle_key(&directory);
        let mut registry = lock_lease_registry();
        if registry.contains_key(&handle_key) {
            let _ = self.inner.release_generation(identity);
            return Err(Error::storage("segmented lease fd collision"));
        }
        let pin = Arc::new(GenerationPin {
            inner: Arc::clone(&self.inner),
            identity,
        });
        let registration = LeaseRegistration {
            pin,
            generation: manifest.generation,
            descriptors,
        };
        registry.insert(handle_key, registration);
        drop(registry);

        meta.size = manifest.total_length;
        Ok(Lease {
            path,
            meta,
            file: directory,
            body_offset: 0,
        })
    }

    fn revert_segmented_fill(&self, record: &MetadataRecord, prior_had_history: bool) {
        if prior_had_history {
            let mut next = record.clone();
            next.state = CacheState::MetadataOnly;
            let _ = self.inner.put_record(&next);
        } else {
            let _ = self.inner.delete_record(&record.key);
        }
    }

    /// Reconciles invalid bytes only if the record observed under the shared
    /// lookup lock is still current after upgrading to exclusive mutation.
    fn invalidate_lookup_if_current(
        &self,
        observed: &MetadataRecord,
        path: &Path,
        retire: bool,
    ) -> Result<()> {
        let _operation = self.inner.write_operations();
        if self.inner.get_record(&observed.key)?.as_ref() != Some(observed) {
            return Ok(());
        }
        self.reconcile_to_metadata_only(observed)?;
        if retire {
            self.inner.retire_path(path)?;
        }
        Ok(())
    }

    /// Drives a record to `MetadataOnly` (the I9 transition), preserving history.
    fn reconcile_to_metadata_only(&self, record: &MetadataRecord) -> Result<()> {
        let mut next = record.clone();
        next.state = CacheState::MetadataOnly;
        self.inner.put_record(&next)
    }

    /// Record a hit on the hot serve path. The actual `hit_count`/`last_access`
    /// update is coalesced in memory and flushed in rare batches (a redb write
    /// txn costs ~ms; a HIT must be ~µs). A racing evict that later flips the
    /// state is respected, so the flush only *adds* to whatever record then exists
    /// and never resurrects it to `Cached`.
    fn bump_history(&self, record: &MetadataRecord) {
        self.inner.record_hit(&record.key);
    }
}

/// Streaming admission writer. Body bytes land in a `staging/` temp file from
/// offset zero; `commit` fsyncs, renames atomically into the
/// sharded `objects/` tree, flips the record to `Cached`, and hands back a
/// readable [`Lease`].
struct DiskWriter {
    inner: Arc<Inner>,
    staging_path: PathBuf,
    /// Destination set only after the atomic rename, so failed commits can
    /// remove bytes that reached the published namespace.
    published_path: Option<PathBuf>,
    /// Open descriptor for this writer's published destination. It pins the
    /// inode across cleanup retries so a replacement cannot reuse its identity.
    published_file: Option<File>,
    /// `Some` while body writes can still reach the staging file.
    _active_staging: ActiveStaging,
    /// Keeps the key exempt from recovery reclassification while this fill runs.
    _active_fill: ActiveFill,
    file: Option<File>,
    /// The `FetchInProgress` record written at `begin_fill`; its presence keeps
    /// failure cleanup armed.
    record: Option<MetadataRecord>,
    /// Stable pre-fill metadata restored on failure; `None` means `Absent`.
    fallback_record: Option<MetadataRecord>,
    /// Whether the pre-fill state is already restored, so a retried cleanup
    /// does not repeat a step it finished.
    state_restored: bool,
    /// The revocation verdict, decided on the first cleanup pass and reused by
    /// later ones.
    revoked: Option<bool>,
    status: u16,
    etag: Option<String>,
    last_modified: Option<String>,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    representation_length: Option<u64>,
    expires_at: Option<i64>,
    versionable: bool,
    content_range: Option<String>,
    written: u64,
}

impl DiskWriter {
    /// Pins this writer's published inode before cleanup restores metadata.
    fn capture_published_identity(&mut self) -> Result<Option<ObjectIdentity>> {
        let Some(path) = self.published_path.as_ref() else {
            return Ok(None);
        };
        if self.published_file.is_none() {
            self.published_file = match File::open(path) {
                Ok(file) => Some(file),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
        }
        self.published_file
            .as_ref()
            .map(|file| ovcc_platform::file_identity(file).map_err(Error::from))
            .transpose()
    }

    /// Hides any published bytes before unlinking them and removes staging.
    ///
    /// The cleanup remains armed until every step succeeds, so `Drop` retries a
    /// transient cleanup failure after `commit` or `abort` returns. Every step
    /// is idempotent across those retries, and the revocation verdict is
    /// decided on the first pass and then reused, because a successful restore
    /// installs the fallback generation and a re-derived verdict would read
    /// this writer as revoked by its own rollback.
    ///
    /// Restoring the pre-fill state, `FetchInProgress` back to `MetadataOnly`
    /// when there was history and to `Absent` for a cold fill, and unlinking a
    /// published destination both require this fill to still own the key, and
    /// run under the mutation lock so the verdict cannot race `remove` or a
    /// superseding `begin_fill`. A revoked writer reclaims only its own private
    /// staging file, because its fallback record would resurrect what the
    /// removal erased and the destination now belongs to whoever took the key.
    /// A cleanup retry removes the public path only while its captured file
    /// identity still matches, so it cannot unlink a replacement generation.
    fn cleanup_uncommitted(&mut self) -> Result<()> {
        drop(self.file.take());

        // A cleared record implies a cleared destination, so the guarded block
        // below owns every step that reaches the public namespace.
        let mut cleanup: Result<()> = Ok(());
        if let Some(record) = self.record.clone() {
            let inner = Arc::clone(&self.inner);
            let _operation = inner.write_operations();
            let verdict = match self.revoked {
                Some(remembered) => Ok(remembered),
                None => self
                    .inner
                    .fill_is_current(&record.key, record.generation)
                    .map(|owned| !owned),
            };
            match verdict {
                Ok(revoked) => {
                    self.revoked = Some(revoked);
                    if !revoked {
                        let destination = self.published_path.clone();
                        match self.capture_published_identity() {
                            Ok(identity) => {
                                // Restoring metadata first prevents lookups from
                                // opening a destination renamed before a later
                                // commit step failed.
                                let state_cleanup = if self.state_restored {
                                    Ok(())
                                } else {
                                    self.inner.restore_fill_state(
                                        &record.key,
                                        self.fallback_record.as_ref(),
                                    )
                                };
                                if state_cleanup.is_ok() {
                                    self.state_restored = true;
                                }
                                let destination_cleanup = match (destination.as_deref(), identity) {
                                    (Some(path), Some(identity)) => {
                                        remove_fill_destination(path, identity)
                                    }
                                    _ => Ok(()),
                                };
                                cleanup = state_cleanup.and(destination_cleanup);
                            }
                            Err(error) => cleanup = Err(error),
                        }
                    }
                }
                Err(error) => cleanup = Err(error),
            }
        }
        let staging_cleanup = remove_file_if_present(&self.staging_path);
        let cleanup = cleanup.and(staging_cleanup);
        if cleanup.is_ok() {
            self.record = None;
            self.fallback_record = None;
            self.published_path = None;
            self.published_file = None;
        }
        cleanup
    }

    fn commit_inner(&mut self) -> Result<Lease> {
        let mut record = self
            .record
            .as_ref()
            .cloned()
            .ok_or_else(|| Error::storage("commit after commit/abort"))?;

        // I4/I14: fsyncs the complete file, then atomic-renames into place so it
        // is visible only when whole.
        {
            let file = self
                .file
                .as_mut()
                .ok_or_else(|| Error::storage("commit after commit/abort"))?;
            #[cfg(test)]
            fail_if_injected(FaultPoint::CommitFlush)?;
            file.flush()?;
            #[cfg(test)]
            fail_if_injected(FaultPoint::CommitSyncAll)?;
            file.sync_all()?;
        }
        drop(self.file.take());

        let _operation = self.inner.write_operations();
        self.inner.drain_deferred()?;

        // A fill owns its key only while the generation it recorded at
        // `begin_fill` is still the current record. `remove` deletes that
        // record and a superseding fill installs a different generation, and
        // either revokes this writer. Publishing anyway would rebuild a purged
        // key out of bytes fetched before the purge decision. Every publication
        // of a `Cached` record takes the lock held here, as do `remove`,
        // `begin_fill`, `adopt_segmented`, recovery and the abort restore, so
        // nothing can interleave between this check and the `Cached`
        // transaction below. The coalesced history flush is the one record
        // write that runs unlocked, and it only adds `hit_count` and
        // `last_access` under an unchanged generation.
        if !self.inner.fill_is_current(&record.key, record.generation)? {
            return Err(Error::fill_revoked(
                "fill was revoked before commit because the key was removed or replaced",
            ));
        }

        // Recovery needs the exact length before a headerless file becomes public.
        record.size_bytes = self.written;
        #[cfg(test)]
        fail_if_injected(FaultPoint::CommitSizePublication)?;
        self.inner.put_record(&record)?;

        let (_, dest) = self.inner.object_paths(&record.key);
        #[cfg(test)]
        fail_if_injected(FaultPoint::CommitObjectDirectoryCreate)?;
        ensure_object_parent(&self.inner.objects_dir, &dest)?;
        #[cfg(test)]
        fail_if_injected(FaultPoint::CommitRename)?;
        match self.inner.publish_staged(&self.staging_path, &dest) {
            Ok(()) => self.published_path = Some(dest.clone()),
            Err(publication) => {
                if publication.published {
                    self.published_path = Some(dest.clone());
                }
                return Err(publication.error);
            }
        }

        // Holding the readable fd before publication leaves no fallible file
        // operation after the state becomes Cached.
        #[cfg(test)]
        fail_if_injected(FaultPoint::CommitFinalReopen)?;
        let file = File::open(&dest)?;

        record.state = CacheState::Cached;
        record.layout = ObjectLayout::Raw;
        record.size_bytes = self.written;
        record.status = self.status;
        record.etag = self.etag.clone();
        record.last_modified = self.last_modified.clone();
        record.content_type = self.content_type.clone();
        record.headers = self.headers.clone();
        record.representation_length = self.representation_length;
        record.expires_at = self.expires_at;
        record.versionable = self.versionable;
        record.content_range = self.content_range.clone();
        record.last_access = now_unix();
        #[cfg(test)]
        fail_if_injected(FaultPoint::CommitMetadataPublication)?;
        record = self.inner.prepare_key_record(&record)?;
        self.inner.put_record(&record)?;
        // A contiguous replacement makes any segmented sibling inactive.
        self.inner
            .retire_path(&self.inner.segmented_object_path(&record.key))?;

        // Disarms rollback only after every fallible commit step succeeds.
        self.record = None;
        self.fallback_record = None;
        self.published_path = None;
        self.published_file = None;

        Ok(Lease {
            path: dest,
            meta: StoredMeta {
                logical_key: record.logical_key.clone(),
                size: self.written,
                revision: record.generation,
                provenance: record.provenance,
                max_idle_secs: record.max_idle_secs,
                status: self.status,
                etag: self.etag.take(),
                last_modified: self.last_modified.take(),
                content_type: self.content_type.take(),
                headers: std::mem::take(&mut self.headers),
                representation_length: self.representation_length,
                expires_at: self.expires_at,
                versionable: self.versionable,
                content_range: self.content_range.take(),
            },
            file,
            body_offset: 0,
        })
    }
}

fn write_all_vectored<W: Write>(writer: &mut W, mut chunks: &mut [IoSlice<'_>]) -> io::Result<u64> {
    let mut written = 0;
    IoSlice::advance_slices(&mut chunks, 0);
    while !chunks.is_empty() {
        match writer.write_vectored(chunks) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => {
                written += count as u64;
                IoSlice::advance_slices(&mut chunks, count);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(written)
}

impl StoreWrite for DiskWriter {
    fn write_chunk(&mut self, buf: &[u8]) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::storage("write_chunk after commit/abort"))?;
        file.write_all(buf)?;
        self.written += buf.len() as u64;
        Ok(())
    }

    fn write_chunks(&mut self, chunks: &mut [IoSlice<'_>]) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::storage("write_chunks after commit/abort"))?;
        self.written += write_all_vectored(file, chunks)?;
        Ok(())
    }

    fn in_progress_read(&self) -> Result<InProgressRead> {
        if self.file.is_none() {
            return Err(Error::storage(
                "in_progress_read after writer commit or abort",
            ));
        }
        Ok(InProgressRead {
            file: Arc::new(File::open(&self.staging_path)?),
            body_offset: 0,
        })
    }

    fn commit(mut self: Box<Self>) -> Result<Lease> {
        match self.commit_inner() {
            Ok(lease) => Ok(lease),
            Err(error) => {
                let cleanup = self.cleanup_uncommitted();
                Err(with_cleanup_error(error, cleanup))
            }
        }
    }

    fn abort(mut self: Box<Self>) {
        let _ = self.cleanup_uncommitted();
    }

    #[cfg(test)]
    fn simulate_crash(mut self: Box<Self>) {
        drop(self.file.take());
        self.record = None;
        self.fallback_record = None;
        self.published_path = None;
        self.published_file = None;
    }

    #[cfg(test)]
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl Drop for DiskWriter {
    fn drop(&mut self) {
        // Safety net if neither commit nor abort ran (e.g. an early `?` upstack).
        if self.record.is_some() {
            let _ = self.cleanup_uncommitted();
        }
    }
}

// --- Segmented-object helpers ----------------------------------------------

fn unique_staging_component(suffix: &str) -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}-{}.{suffix}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn ensure_object_parent(objects_dir: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::storage("object path has no parent"))?;
    fs::create_dir_all(parent)?;
    sync_directory(objects_dir)?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<()> {
    ovcc_platform::sync_directory(path)?;
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::storage("path has no parent directory"))?;
    sync_directory(parent)
}

fn remove_object_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Removes `path` if it exists; a missing file is not an error.
fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Removes a failed fill's published destination, with a one-shot test seam for
/// witnessing cleanup retry after metadata restoration has already completed.
fn remove_fill_destination(path: &Path, expected: ObjectIdentity) -> Result<()> {
    let identity = match ovcc_platform::stat_nofollow(path) {
        Ok(stat) => stat.identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if identity != expected {
        return Ok(());
    }
    #[cfg(test)]
    {
        let mut fail_path = FAIL_FILL_DESTINATION_CLEANUP_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if fail_path.as_deref() == Some(path) {
            fail_path.take();
            return Err(Error::storage("injected fill destination cleanup failure"));
        }
    }
    remove_file_if_present(path)
}

fn next_generation(key: &str, length: u64) -> u64 {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let mut digest = Sha256::new();
    digest.update(key.as_bytes());
    digest.update(length.to_le_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
            .to_le_bytes(),
    );
    digest.update(SEQUENCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    u64::from_le_bytes(digest.finalize()[..8].try_into().unwrap())
}

fn next_distinct_generation(key: &str, length: u64, prior: Option<u64>) -> u64 {
    loop {
        let generation = next_generation(key, length);
        if Some(generation) != prior {
            return generation;
        }
    }
}

fn segment_filename(id: &str) -> String {
    format!("segment-{}", sha256_hex(id))
}

fn prevalidate_segments(inner: &Inner, segments: &[StagedSegment]) -> Result<ValidatedSegments> {
    if segments.is_empty() {
        return Err(Error::storage("segmented object must contain a segment"));
    }
    let staging_device =
        ovcc_platform::file_identity(&ovcc_platform::open_directory(&inner.staging_dir)?)?.device;
    let mut by_id = HashMap::<String, ValidatedSource>::new();
    let mut entries = Vec::with_capacity(segments.len());
    let mut unique_sources = Vec::new();
    let mut source_parents = HashSet::new();
    let mut total_length = 0u64;

    for segment in segments {
        if segment.id.is_empty() {
            return Err(Error::storage("segment id must not be empty"));
        }
        if segment.id.len() > u32::MAX as usize {
            return Err(Error::storage("segment id is too long"));
        }
        if let Some(existing) = by_id.get(&segment.id) {
            if existing.path != segment.path || existing.size != segment.size {
                return Err(Error::storage(
                    "repeated segment id has inconsistent path or size",
                ));
            }
            entries.push(ManifestEntry {
                id: segment.id.clone(),
                filename: existing.filename.clone(),
                size: existing.size,
            });
        } else {
            let ovcc_platform::Stat { metadata, identity } =
                ovcc_platform::stat_nofollow(&segment.path)?;
            if !metadata.file_type().is_file() {
                return Err(Error::storage("staged segment is not a regular file"));
            }
            if metadata.len() != segment.size {
                return Err(Error::storage("staged segment size changed"));
            }
            if identity.device != staging_device {
                return Err(Error::storage(
                    "staged segment is not on the cache filesystem",
                ));
            }
            let parent = segment
                .path
                .parent()
                .ok_or_else(|| Error::storage("staged segment has no parent"))?
                .to_path_buf();
            source_parents.insert(parent);
            let source = ValidatedSource {
                id: segment.id.clone(),
                path: segment.path.clone(),
                filename: segment_filename(&segment.id),
                size: segment.size,
                identity,
            };
            entries.push(ManifestEntry {
                id: source.id.clone(),
                filename: source.filename.clone(),
                size: source.size,
            });
            by_id.insert(segment.id.clone(), source.clone());
            unique_sources.push(source);
        }
        total_length = total_length
            .checked_add(segment.size)
            .ok_or_else(|| Error::storage("segmented object length overflow"))?;
    }

    Ok(ValidatedSegments {
        entries,
        unique_sources,
        source_parents,
        total_length,
    })
}

fn rollback_moved_sources(staging_path: &Path, moved: &[ValidatedSource]) {
    for source in moved.iter().rev() {
        let staged = staging_path.join(&source.filename);
        // Never overwrite a newer upload that recreated this source path. A
        // conflict leaves the old fragment private for normal staging cleanup.
        let _ = ovcc_platform::rename_noreplace(&staged, &source.path);
    }
    for source in moved {
        if let Some(parent) = source.path.parent() {
            let _ = sync_directory(parent);
        }
    }
}

fn encode_manifest(manifest: &SegmentManifest) -> Result<Vec<u8>> {
    let key = manifest.key.as_bytes();
    if manifest.entries.len() > MAX_MANIFEST_SEGMENTS {
        return Err(Error::storage("too many segments"));
    }
    let count =
        u32::try_from(manifest.entries.len()).map_err(|_| Error::storage("too many segments"))?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MANIFEST_MAGIC);
    bytes.extend_from_slice(&MANIFEST_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&manifest.generation.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(key.len())
            .map_err(|_| Error::storage("manifest key too long"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(&count.to_le_bytes());
    bytes.extend_from_slice(&manifest.total_length.to_le_bytes());
    bytes.extend_from_slice(key);
    for entry in &manifest.entries {
        let id = entry.id.as_bytes();
        let filename = entry.filename.as_bytes();
        bytes.extend_from_slice(
            &u32::try_from(id.len())
                .map_err(|_| Error::storage("segment id too long"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(
            &u32::try_from(filename.len())
                .map_err(|_| Error::storage("segment filename too long"))?
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&entry.size.to_le_bytes());
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(filename);
    }
    if bytes.len() as u64 + MANIFEST_DIGEST_LEN as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::storage("segmented manifest is too large"));
    }
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest);
    Ok(bytes)
}

fn write_manifest(directory: &Path, manifest: &SegmentManifest) -> Result<()> {
    let bytes = encode_manifest(manifest)?;
    let path = directory.join(MANIFEST_FILENAME);
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    let value = bytes
        .get(*cursor..*cursor + 4)
        .ok_or_else(|| Error::storage("manifest is truncated"))?;
    *cursor += 4;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    let value = bytes
        .get(*cursor..*cursor + 8)
        .ok_or_else(|| Error::storage("manifest is truncated"))?;
    *cursor += 8;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

fn take_string(bytes: &[u8], cursor: &mut usize, length: usize) -> Result<String> {
    let value = bytes
        .get(*cursor..*cursor + length)
        .ok_or_else(|| Error::storage("manifest is truncated"))?;
    *cursor += length;
    String::from_utf8(value.to_vec()).map_err(|_| Error::storage("manifest text is not UTF-8"))
}

fn decode_manifest(bytes: &[u8]) -> Result<SegmentManifest> {
    if bytes.len() < MANIFEST_FIXED_LEN + MANIFEST_DIGEST_LEN {
        return Err(Error::storage("manifest is truncated"));
    }
    let payload_len = bytes.len() - MANIFEST_DIGEST_LEN;
    if Sha256::digest(&bytes[..payload_len]).as_slice() != &bytes[payload_len..] {
        return Err(Error::storage("manifest digest mismatch"));
    }
    if &bytes[..8] != MANIFEST_MAGIC {
        return Err(Error::storage("manifest magic mismatch"));
    }
    let mut cursor = 8;
    let version = take_u64(bytes, &mut cursor)?;
    if version != MANIFEST_FORMAT_VERSION {
        return Err(Error::storage("unsupported manifest version"));
    }
    let generation = take_u64(bytes, &mut cursor)?;
    let key_len = take_u32(bytes, &mut cursor)? as usize;
    let count = take_u32(bytes, &mut cursor)? as usize;
    let total_length = take_u64(bytes, &mut cursor)?;
    let key = take_string(bytes, &mut cursor, key_len)?;
    // Every encoded entry needs at least its three fixed-width fields. Reject
    // an impossible attacker-controlled count before reserving the vector.
    if count > MAX_MANIFEST_SEGMENTS || count > payload_len.saturating_sub(cursor) / (4 + 4 + 8) {
        return Err(Error::storage("unsupported manifest segment count"));
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(count)
        .map_err(|_| Error::storage("manifest segment allocation failed"))?;
    let mut computed_total = 0u64;
    for _ in 0..count {
        let id_len = take_u32(bytes, &mut cursor)? as usize;
        let filename_len = take_u32(bytes, &mut cursor)? as usize;
        let size = take_u64(bytes, &mut cursor)?;
        let id = take_string(bytes, &mut cursor, id_len)?;
        let filename = take_string(bytes, &mut cursor, filename_len)?;
        if id.is_empty() || filename != segment_filename(&id) {
            return Err(Error::storage("manifest segment identity mismatch"));
        }
        computed_total = computed_total
            .checked_add(size)
            .ok_or_else(|| Error::storage("manifest total length overflow"))?;
        entries.push(ManifestEntry { id, filename, size });
    }
    if cursor != payload_len || computed_total != total_length || entries.is_empty() {
        return Err(Error::storage("manifest shape or length mismatch"));
    }
    Ok(SegmentManifest {
        key,
        generation,
        total_length,
        entries,
    })
}

fn open_regular_at(directory: &File, filename: &str, expected_size: u64) -> Result<File> {
    if filename.contains(['/', '\\']) || filename == "." || filename == ".." {
        return Err(Error::storage("invalid segment filename"));
    }
    let file = ovcc_platform::open_regular_at(directory, filename)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(Error::storage("segment type or size mismatch"));
    }
    Ok(file)
}

fn validate_segmented_directory(
    directory: &File,
    expected_key: &str,
    expected_generation: u64,
    expected_total: Option<u64>,
) -> Result<SegmentManifest> {
    if !directory.metadata()?.file_type().is_dir() {
        return Err(Error::storage("segmented object is not a directory"));
    }
    let manifest_file = ovcc_platform::open_regular_at(directory, MANIFEST_FILENAME)?;
    let manifest_metadata = manifest_file.metadata()?;
    if !manifest_metadata.file_type().is_file() {
        return Err(Error::storage("directory entry is not a regular file"));
    }
    let length = manifest_metadata.len();
    if length > MAX_MANIFEST_BYTES {
        return Err(Error::storage("segmented manifest is too large"));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    manifest_file
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let manifest = decode_manifest(&bytes)?;
    if manifest.key != expected_key
        || manifest.generation != expected_generation
        || expected_total.is_some_and(|total| total != manifest.total_length)
    {
        return Err(Error::storage("manifest does not match metadata"));
    }
    let mut checked = HashSet::new();
    for entry in &manifest.entries {
        if checked.insert(entry.filename.clone()) {
            drop(open_regular_at(directory, &entry.filename, entry.size)?);
        }
    }
    Ok(manifest)
}

fn raw_file_validates(file: &File, record: &MetadataRecord) -> Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.is_file() && metadata.len() == record.size_bytes)
}

fn object_validates(path: &Path, record: &MetadataRecord) -> bool {
    match record.layout {
        ObjectLayout::Contiguous => false,
        ObjectLayout::Raw => {
            File::open(path).is_ok_and(|file| raw_file_validates(&file, record).unwrap_or(false))
        }
        ObjectLayout::Segmented => ovcc_platform::open_directory(path)
            .and_then(|directory| {
                validate_segmented_directory(
                    &directory,
                    &record.key,
                    record.generation,
                    Some(record.size_bytes),
                )
                .map_err(|error| io::Error::other(error.to_string()))
            })
            .is_ok(),
    }
}

fn object_len(path: &Path, record: &MetadataRecord) -> Result<u64> {
    match record.layout {
        ObjectLayout::Contiguous => Err(Error::storage("retired object layout")),
        ObjectLayout::Raw => Ok(fs::metadata(path)?.len()),
        ObjectLayout::Segmented => {
            let directory = ovcc_platform::open_directory(path)?;
            Ok(
                validate_segmented_directory(&directory, &record.key, record.generation, None)?
                    .total_length,
            )
        }
    }
}

// --- Sweeps ----------------------------------------------------------------

/// Deletes every leftover staging entry; returns removed file and directory counts.
fn sweep_staging(inner: &Inner) -> Result<(usize, usize)> {
    let mut files = 0;
    let mut directories = 0;
    let active = inner
        .active_staging
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let entries = match fs::read_dir(&inner.staging_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if active.contains(&entry.path()) {
            continue;
        }
        if inner.generation_is_pinned(ovcc_platform::stat_nofollow(&entry.path())?.identity) {
            continue;
        }
        if entry.file_type()?.is_dir() {
            directories += 1;
        } else {
            files += 1;
        }
        remove_object_path(&entry.path())?;
    }
    Ok((files, directories))
}

/// Deletes every data file under `objects/` not in `protected`; returns the count.
fn sweep_orphan_objects(
    inner: &Inner,
    protected: &std::collections::HashSet<PathBuf>,
) -> Result<(usize, usize)> {
    let mut deleted_files = 0;
    let mut deleted_directories = 0;
    let shards = match fs::read_dir(&inner.objects_dir) {
        Ok(shards) => shards,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(err) => return Err(err.into()),
    };
    for shard in shards {
        let shard = shard?;
        if !shard.file_type()?.is_dir() {
            continue;
        }
        for object in fs::read_dir(shard.path())? {
            let object = object?;
            let path = object.path();
            if !protected.contains(&path) {
                if inner.generation_is_pinned(ovcc_platform::stat_nofollow(&path)?.identity) {
                    continue;
                }
                if object.file_type()?.is_dir() {
                    fs::remove_dir_all(&path)?;
                    deleted_directories += 1;
                } else {
                    fs::remove_file(&path)?;
                    deleted_files += 1;
                }
            }
        }
    }
    Ok((deleted_files, deleted_directories))
}

// --- Encoding helpers ------------------------------------------------------

/// Encodes a record as `[schema_version: u16 LE][postcard body]`.
/// Keep the envelope and explicit decoder dispatch for future schema migrations.
fn encode_record(record: &MetadataRecord) -> Result<Vec<u8>> {
    let mut out = METADATA_SCHEMA_VERSION.to_le_bytes().to_vec();
    let body = postcard::to_allocvec(record)
        .map_err(|err| Error::storage(format!("metadata encode: {err}")))?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decodes a version-tagged metadata value.
///
/// `Ok(None)` means the value was written by a schema version this build does
/// not know; the caller treats it as absent so a rollback or format bump never
/// mis-decodes a foreign record. Recovery purges those records as disposable
/// cache state. `Err` is reserved for a truncated tag or a corrupt body of a
/// known version. Retired versions no longer have conversion arms; add a
/// versioned record conversion here when a future schema supports its predecessor.
fn decode_record(bytes: &[u8]) -> Result<Option<MetadataRecord>> {
    #[cfg(test)]
    RECORD_DECODES.with(|count| count.set(count.get() + 1));
    let Some((tag, body)) = bytes.split_first_chunk::<2>() else {
        return Err(Error::storage("metadata: truncated schema version tag"));
    };
    match u16::from_le_bytes(*tag) {
        9 => postcard::from_bytes::<MetadataRecord>(body)
            .and_then(|record| {
                if record.layout == ObjectLayout::Contiguous {
                    Err(postcard::Error::DeserializeBadEnum)
                } else {
                    Ok(record)
                }
            })
            .map(Some)
            .map_err(|err| Error::storage(format!("metadata decode (v9): {err}"))),
        _ => Ok(None),
    }
}

/// Lowercase hex of `sha256(key)` — the content-address basis for the on-disk
/// path. Keyed on the cache key (not the bytes) so the path is known up front.
fn sha256_hex_bytes(key: &str) -> [u8; 64] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(key.as_bytes());
    let mut out = [0; 64];
    for (byte, pair) in digest.iter().zip(out.chunks_exact_mut(2)) {
        pair[0] = HEX[(byte >> 4) as usize];
        pair[1] = HEX[(byte & 0x0f) as usize];
    }
    out
}

fn sha256_hex(key: &str) -> String {
    String::from_utf8(sha256_hex_bytes(key).to_vec()).expect("hex is ASCII")
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn open_metadata_db(path: &Path) -> Result<Database> {
    Database::create(path).map_err(map_redb)
}

fn map_redb(err: impl std::fmt::Display) -> Error {
    Error::storage(err.to_string())
}

fn with_cleanup_error(primary: Error, cleanup: Result<()>) -> Error {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => Error::storage(format!("{primary}; cleanup failed: {cleanup}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ShortVectoredWriter {
        bytes: Vec<u8>,
        interrupt: bool,
        budget: usize,
        fail_after: Option<usize>,
        failure_views: Vec<Vec<u8>>,
    }

    impl Write for ShortVectoredWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            unreachable!("test requires vectored writes")
        }

        fn write_vectored(&mut self, chunks: &[IoSlice<'_>]) -> io::Result<usize> {
            if std::mem::take(&mut self.interrupt) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self
                .fail_after
                .is_some_and(|limit| self.bytes.len() >= limit)
            {
                self.failure_views = chunks.iter().map(|chunk| chunk.to_vec()).collect();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected vectored failure",
                ));
            }
            let mut written = 0;
            for chunk in chunks {
                let count = chunk.len().min(self.budget - written);
                self.bytes.extend_from_slice(&chunk[..count]);
                written += count;
                if written == self.budget {
                    break;
                }
            }
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn disk_write_vectored_handles_interrupts_partial_slices_and_empty_chunks() -> io::Result<()> {
        // Arrange.
        let mut writer = ShortVectoredWriter {
            bytes: Vec::new(),
            interrupt: true,
            budget: 7,
            ..Default::default()
        };
        let mut chunks = [
            IoSlice::new(b""),
            IoSlice::new(b"abc"),
            IoSlice::new(b""),
            IoSlice::new(b"defghijkl"),
            IoSlice::new(b"mn"),
            IoSlice::new(b""),
        ];
        // Act.
        let written = write_all_vectored(&mut writer, &mut chunks)?;
        // Assert.
        assert_eq!(written, 14);
        assert_eq!(writer.bytes, b"abcdefghijklmn");
        Ok(())
    }

    #[test]
    fn disk_write_vectored_rejects_zero_progress() {
        // Arrange.
        let mut writer = ShortVectoredWriter {
            bytes: Vec::new(),
            interrupt: false,
            budget: 0,
            ..Default::default()
        };
        let mut chunks = [IoSlice::new(b"data")];
        // Act.
        let result = write_all_vectored(&mut writer, &mut chunks);
        // Assert.
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::WriteZero));
    }

    #[test]
    fn disk_write_vectored_preserves_advanced_slices_on_later_error() -> io::Result<()> {
        // Arrange.
        let mut writer = ShortVectoredWriter {
            budget: 5,
            fail_after: Some(5),
            ..Default::default()
        };
        let mut chunks = [
            IoSlice::new(b"abc"),
            IoSlice::new(b"defgh"),
            IoSlice::new(b"ij"),
        ];
        // Act.
        let result = write_all_vectored(&mut writer, &mut chunks);
        // Assert.
        let error = result
            .err()
            .ok_or_else(|| io::Error::other("write failure was swallowed"))?;
        assert_eq!(writer.bytes, b"abcde");
        assert_eq!(writer.failure_views, [b"fgh".to_vec(), b"ij".to_vec()]);
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "injected vectored failure");
        Ok(())
    }

    #[test]
    fn disk_write_vectored_commits_more_than_one_os_vector_batch() -> Result<()> {
        // Arrange.
        let directory = tempfile::tempdir()?;
        let storage = DiskStorage::open(directory.path())?;
        let bytes: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let mut chunks: Vec<_> = bytes.chunks(1).map(IoSlice::new).collect();
        let mut writer = storage.begin_fill("vectored", StoredMeta::default())?;
        // Act.
        writer.write_chunks(&mut chunks)?;
        let lease = writer.commit()?;
        // Assert.
        assert_eq!(lease.meta.size, bytes.len() as u64);
        assert_eq!(read_body(&lease), bytes);
        Ok(())
    }

    /// Positional exact reads for tests on either platform backend.
    trait ReadExactAt {
        fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<()>;
    }

    impl ReadExactAt for File {
        fn read_exact_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<()> {
            ovcc_platform::read_exact_at(self, buffer, offset)
        }
    }

    const BEGIN_FILL_FAULTS: [FaultPoint; 1] = [FaultPoint::BeginStagingCreate];
    const COMMIT_FAULTS: [FaultPoint; 7] = [
        FaultPoint::CommitFlush,
        FaultPoint::CommitSyncAll,
        FaultPoint::CommitSizePublication,
        FaultPoint::CommitObjectDirectoryCreate,
        FaultPoint::CommitRename,
        FaultPoint::CommitFinalReopen,
        FaultPoint::CommitMetadataPublication,
    ];

    fn open_temp() -> (tempfile::TempDir, DiskStorage) {
        let temp = tempfile::tempdir().unwrap();
        let storage = DiskStorage::open(temp.path()).unwrap();
        (temp, storage)
    }

    /// A fully-populated record so the round-trip and golden tests exercise every
    /// field (all `Option`s `Some`, all counters non-zero).
    pub(super) fn full_record() -> MetadataRecord {
        MetadataRecord {
            logical_key: None,
            physical_keys: Vec::new(),
            key: "obj://sample".to_string(),
            state: CacheState::Cached,
            layout: ObjectLayout::Raw,
            generation: 0,
            provenance: CacheProvenance::PeerCopy,
            max_idle_secs: Some(600),
            status: 206,
            etag: Some("\"v1\"".to_string()),
            last_modified: Some("Wed, 01 Jul 2026 00:00:00 GMT".to_string()),
            content_type: Some("application/octet-stream".to_string()),
            headers: vec![("Cache-Control".to_string(), "max-age=60".to_string())],
            representation_length: Some(2),
            expires_at: Some(1_700_000_300),
            versionable: true,
            content_range: Some("bytes 0-1/2".to_string()),
            size_bytes: 2,
            last_access: 1_700_000_000,
            hit_count: 7,
            fetch_cost: 42,
            fetch_duration: 99,
        }
    }

    #[test]
    fn metadata_encode_decode_round_trips() -> Result<()> {
        // Arrange.
        let record = full_record();

        // Act.
        let encoded = encode_record(&record)?;
        let Some(decoded) = decode_record(&encoded)? else {
            return Err(Error::storage("current record was ignored"));
        };

        // Assert.
        assert_eq!(decoded, record);
        assert!(decoded.versionable);
        Ok(())
    }

    #[test]
    fn metadata_value_carries_the_schema_version_prefix() -> Result<()> {
        // Frozen before retiring compatibility, using the unchanged v9 encoder.
        for (layout, golden) in [
            (
                ObjectLayout::Raw,
                include_bytes!("../tests/fixtures/current/metadata-v9-raw.bin").as_slice(),
            ),
            (
                ObjectLayout::Segmented,
                include_bytes!("../tests/fixtures/current/metadata-v9-segmented.bin").as_slice(),
            ),
        ] {
            let mut record = full_record();
            record.layout = layout;
            assert_eq!(encode_record(&record)?, golden);
            assert_eq!(decode_record(golden)?, Some(record));
            assert_eq!(&golden[..2], &METADATA_SCHEMA_VERSION.to_le_bytes());
        }
        Ok(())
    }

    #[test]
    fn metadata_decode_rejects_corrupt_current_records() -> Result<()> {
        let mut bytes = encode_record(&full_record())?;
        bytes.pop();
        assert!(decode_record(&bytes).is_err());
        // Discriminator 0 stays reserved; removing it would renumber live layouts.
        let mut retired = full_record();
        retired.layout = ObjectLayout::Contiguous;
        assert!(decode_record(&encode_record(&retired)?).is_err());
        // Freeze the retired range; future supported predecessors must not be
        // rejected merely because the current schema advances.
        for version in 1u16..=8 {
            bytes[..2].copy_from_slice(&version.to_le_bytes());
            assert_eq!(decode_record(&bytes)?, None);
        }
        Ok(())
    }

    #[test]
    fn metadata_from_an_unknown_future_version_is_ignored() {
        // A record written by a newer engine then read after
        // a rollback decodes to `None`, so the caller treats it as absent rather
        // than mis-decoding a forward layout.
        let mut forward = METADATA_SCHEMA_VERSION
            .saturating_add(1)
            .to_le_bytes()
            .to_vec();
        forward.extend_from_slice(&postcard::to_allocvec(&full_record()).unwrap());
        assert!(decode_record(&forward).unwrap().is_none());
    }

    #[test]
    fn metadata_with_a_truncated_version_tag_errors() {
        // Fewer than two bytes cannot carry the version tag.
        assert!(decode_record(&[0x01]).is_err());
        assert!(decode_record(&[]).is_err());
    }

    #[test]
    fn disk_recover_metadata_only_purges_residue()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        for orphan in [false, true] {
            // Arrange. Recovery must discard metadata-only residue in the current format.
            let (temp, storage) = open_temp();
            write_object(&storage, "obj://keep", None, &[b"cached bytes"]);
            let key = "obj://sample";
            let mut record = full_record();
            record.state = CacheState::MetadataOnly;
            let bytes = encode_record(&record)?;
            let txn = storage.inner.db.begin_write()?;
            {
                let mut table = txn.open_table(METADATA)?;
                table.insert(key, bytes.as_slice())?;
            }
            txn.commit()?;
            let path = storage.inner.object_paths(key).1;
            if orphan {
                fs::create_dir_all(path.parent().ok_or("missing object parent")?)?;
                fs::write(&path, b"old eviction interrupted before unlink")?;
            }
            drop(storage);

            // Act.
            let reopened = DiskStorage::open(temp.path())?;

            // Assert.
            assert!(reopened.inner.get_record(key)?.is_none(), "orphan {orphan}");
            assert!(matches!(reopened.lookup(key)?, Lookup::Miss));
            assert!(!path.exists());
            assert_eq!(
                read_body(&expect_hit(&reopened, "obj://keep")),
                b"cached bytes"
            );
        }
        Ok(())
    }

    #[test]
    fn disk_recover_metadata_only_batches_preserve_cached_and_active_fills()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Arrange.
        let (_temp, storage) = open_temp();
        write_object(&storage, "obj://keep", None, &[b"keep"]);
        let lease = expect_hit(&storage, "obj://keep");
        let mut writer = storage.begin_fill("obj://active", StoredMeta::default())?;
        writer.write_chunk(b"active")?;
        let count = RECOVERY_DELETE_BATCH * 2 + 1;
        let txn = storage.inner.db.begin_write()?;
        {
            let mut table = txn.open_table(METADATA)?;
            for index in 0..count {
                let mut record = full_record();
                record.key = format!("obj://evicted-{index}");
                record.state = CacheState::MetadataOnly;
                table.insert(record.key.as_str(), encode_record(&record)?.as_slice())?;
            }
        }
        txn.commit()?;

        // Act.
        let report = storage.recover_on_open()?;

        // Assert.
        assert_eq!(report.metadata_only, count);
        assert_eq!(report.removed_records, count);
        assert_eq!(report.cached, 1);
        assert_eq!(report.skipped_active_fills, 1);
        assert_eq!(storage.inner.all_records()?.len(), 2);
        assert_eq!(read_body(&lease), b"keep");
        assert_eq!(read_body(&writer.commit()?), b"active");
        Ok(())
    }

    fn write_object(storage: &DiskStorage, key: &str, etag: Option<&str>, chunks: &[&[u8]]) {
        write_object_with_provenance(storage, key, etag, chunks, CacheProvenance::Origin);
    }

    fn write_object_with_provenance(
        storage: &DiskStorage,
        key: &str,
        etag: Option<&str>,
        chunks: &[&[u8]],
        provenance: CacheProvenance,
    ) {
        let meta = StoredMeta {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance,
            max_idle_secs: None,
            status: 200,
            etag: etag.map(str::to_string),
            last_modified: None,
            content_type: None,
            headers: Vec::new(),
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: None,
        };
        let mut writer = storage.begin_fill(key, meta).unwrap();
        for chunk in chunks {
            writer.write_chunk(chunk).unwrap();
        }
        let lease = writer.commit().unwrap();
        let expected: Vec<u8> = chunks.concat();
        assert_eq!(lease.meta.size, expected.len() as u64);
    }

    /// Reads the body of a lease, honoring the public `body_offset` contract.
    fn read_body(lease: &Lease) -> Vec<u8> {
        let mut file = lease.file.try_clone().unwrap();
        file.seek(SeekFrom::Start(lease.body_offset)).unwrap();
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        buf
    }

    fn expect_hit(storage: &DiskStorage, key: &str) -> Lease {
        match storage.lookup(key).unwrap() {
            Lookup::Hit(lease) => lease,
            other => panic!("expected Hit for {key}, got {other:?}"),
        }
    }

    /// The committed generation storage currently holds for `key`, supplying
    /// the exact-generation argument to `evict_data`.
    fn current_revision(storage: &DiskStorage, key: &str) -> u64 {
        storage
            .inner
            .get_record(key)
            .unwrap()
            .expect("record for eviction revision")
            .generation
    }

    fn replacement_meta() -> StoredMeta {
        StoredMeta {
            etag: Some("\"replacement\"".to_owned()),
            headers: vec![("X-Replacement".to_owned(), "true".to_owned())],
            ..StoredMeta::default()
        }
    }

    fn seed_metadata_only(storage: &DiskStorage, key: &str) -> MetadataRecord {
        write_object(storage, key, Some("\"original\""), &[b"original body"]);
        // Missing-file reconciliation still retains refill history.
        fs::remove_file(storage.inner.object_paths(key).1).unwrap();
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::SoftMiss(_)));
        let record = storage.inner.get_record(key).unwrap().unwrap();
        assert_eq!(record.state, CacheState::MetadataOnly);
        record
    }

    fn assert_fill_files_removed(
        temp: &tempfile::TempDir,
        storage: &DiskStorage,
        key: &str,
        fault: FaultPoint,
    ) {
        let staging_count = fs::read_dir(temp.path().join("staging")).unwrap().count();
        assert_eq!(staging_count, 0, "staging survived {fault:?}");
        let (_, destination) = storage.inner.object_paths(key);
        assert!(
            !destination.exists(),
            "destination survived failed commit at {fault:?}"
        );
    }

    // --- Round-trip + basic surface ---------------------------------------

    #[test]
    fn put_commit_lookup_round_trips_bytes_size_and_etag() {
        let (_t, storage) = open_temp();
        let body: &[&[u8]] = &[b"hello, ", b"streaming ", b"world"];
        let expected: Vec<u8> = body.concat();
        write_object(&storage, "obj://a", Some("\"etag-1\""), body);

        let lease = expect_hit(&storage, "obj://a");
        assert_eq!(lease.meta.size, expected.len() as u64);
        assert_eq!(lease.meta.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(read_body(&lease), expected);
    }

    #[test]
    fn provenance_tracks_replacement_and_abort_restores_prior_history() {
        // Arrange.
        let (_temp, storage) = open_temp();
        write_object_with_provenance(
            &storage,
            "obj://source",
            Some("\"peer\""),
            &[b"peer"],
            CacheProvenance::PeerCopy,
        );

        // Act.
        let mut failed_origin = storage
            .begin_fill(
                "obj://source",
                StoredMeta {
                    provenance: CacheProvenance::Origin,
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        failed_origin.write_chunk(b"incomplete").unwrap();
        failed_origin.abort();
        let restored = storage.inner.get_record("obj://source").unwrap().unwrap();
        write_object(&storage, "obj://source", Some("\"origin\""), &[b"origin"]);
        let replaced = expect_hit(&storage, "obj://source");

        // Assert.
        assert_eq!(restored.state, CacheState::MetadataOnly);
        assert_eq!(restored.provenance, CacheProvenance::PeerCopy);
        assert_eq!(replaced.meta.provenance, CacheProvenance::Origin);
        assert_eq!(read_body(&replaced), b"origin");
    }

    #[test]
    fn in_progress_read_sees_exact_appends_and_survives_commit_rename() {
        let (_temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill("obj://tail-commit", StoredMeta::default())
            .unwrap();
        let reader = writer.in_progress_read().unwrap();

        writer.write_chunk(b"first").unwrap();
        let mut first = [0u8; 5];
        reader
            .file
            .read_exact_at(&mut first, reader.body_offset)
            .unwrap();
        assert_eq!(&first, b"first");

        writer.write_chunk(b"-second").unwrap();
        let mut second = [0u8; 7];
        reader
            .file
            .read_exact_at(&mut second, reader.body_offset + 5)
            .unwrap();
        assert_eq!(&second, b"-second");

        let lease = writer.commit().unwrap();
        assert!(lease.path.exists());
        let mut complete = [0u8; 12];
        reader
            .file
            .read_exact_at(&mut complete, reader.body_offset)
            .unwrap();
        assert_eq!(&complete, b"first-second");
    }

    #[test]
    fn in_progress_read_survives_abort_unlink() {
        let (temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill("obj://tail-abort", StoredMeta::default())
            .unwrap();
        let reader = writer.in_progress_read().unwrap();
        writer.write_chunk(b"private bytes").unwrap();

        writer.abort();

        assert_eq!(
            fs::read_dir(temp.path().join("staging")).unwrap().count(),
            0
        );
        assert!(matches!(
            storage.lookup("obj://tail-abort").unwrap(),
            Lookup::Miss
        ));
        let mut body = [0u8; 13];
        reader
            .file
            .read_exact_at(&mut body, reader.body_offset)
            .unwrap();
        assert_eq!(&body, b"private bytes");
    }

    #[test]
    fn peek_reports_committed_metadata_without_access_history_mutation() {
        // Arrange.
        let (_temp, storage) = open_temp();
        write_object(&storage, "obj://peek", Some("\"etag-1\""), &[b"body"]);
        let before = storage.inner.get_record("obj://peek").unwrap().unwrap();

        // Act.
        let meta = storage.peek("obj://peek").unwrap();
        let after = storage.inner.get_record("obj://peek").unwrap().unwrap();
        let pending = storage.inner.lock_pending().get("obj://peek").copied();

        // Assert.
        assert_eq!(
            meta.and_then(|meta| meta.etag),
            Some("\"etag-1\"".to_owned())
        );
        assert_eq!(after.hit_count, before.hit_count);
        assert_eq!(after.last_access, before.last_access);
        assert!(pending.is_none());
    }

    #[test]
    fn peek_treats_fetch_in_progress_as_absent() {
        // Arrange.
        let (_temp, storage) = open_temp();
        let writer = storage
            .begin_fill(
                "obj://filling",
                StoredMeta {
                    etag: Some("\"etag-1\"".to_owned()),
                    ..StoredMeta::default()
                },
            )
            .unwrap();

        // Act.
        let observed = storage.peek("obj://filling").unwrap();

        // Assert.
        assert_eq!(observed, None);
        writer.abort();
    }

    #[test]
    fn expires_at_round_trips_through_fill_and_lookup() {
        let (_t, storage) = open_temp();
        let meta = StoredMeta {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: None,
            status: 200,
            etag: None,
            last_modified: None,
            content_type: None,
            headers: Vec::new(),
            representation_length: None,
            expires_at: Some(1_700_000_042),
            versionable: false,
            content_range: None,
        };
        let mut writer = storage.begin_fill("obj://ttl", meta).unwrap();
        writer.write_chunk(b"bursty-body").unwrap();
        let lease = writer.commit().unwrap();
        assert_eq!(lease.meta.expires_at, Some(1_700_000_042));

        // The expiry is durable in the record and projected on a fresh lookup.
        let lease = expect_hit(&storage, "obj://ttl");
        assert_eq!(lease.meta.expires_at, Some(1_700_000_042));
    }

    #[test]
    fn disk_storage_head_representation_length_round_trips_independently_from_body() -> Result<()> {
        // Arrange.
        let (_temp, storage) = open_temp();
        let meta = StoredMeta {
            representation_length: Some(1_329_006_803),
            ..StoredMeta::default()
        };

        // Act.
        let writer = storage.begin_fill("obj://head-snapshot", meta)?;
        let committed = writer.commit()?;
        let looked_up = match storage.lookup("obj://head-snapshot")? {
            Lookup::Hit(lease) => lease,
            other => {
                return Err(Error::storage(format!(
                    "HEAD snapshot did not survive lookup: {other:?}"
                )));
            }
        };

        // Assert.
        assert_eq!(committed.meta.size, 0);
        assert_eq!(committed.meta.representation_length, Some(1_329_006_803));
        assert_eq!(looked_up.meta.size, 0);
        assert_eq!(looked_up.meta.representation_length, Some(1_329_006_803));
        Ok(())
    }

    #[test]
    fn disk_storage_refresh_freshness_exact_generation_updates_expiry() {
        // Arrange.
        let (_temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill(
                "obj://revalidate",
                StoredMeta {
                    etag: Some("\"etag-1\"".to_owned()),
                    last_modified: Some("Mon, 24 Aug 2026 12:00:00 GMT".to_owned()),
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"unchanged body").unwrap();
        writer.commit().unwrap();
        let expected = storage.peek("obj://revalidate").unwrap().unwrap();
        let before = storage
            .inner
            .get_record("obj://revalidate")
            .unwrap()
            .unwrap();

        // Act.
        let refreshed = storage
            .refresh_freshness("obj://revalidate", &expected, 200)
            .unwrap();

        // Assert.
        assert!(refreshed);
        let after = storage
            .inner
            .get_record("obj://revalidate")
            .unwrap()
            .unwrap();
        let mut expected_record = before;
        expected_record.expires_at = Some(200);
        assert_eq!(after, expected_record);
        let lease = expect_hit(&storage, "obj://revalidate");
        assert_eq!(lease.meta.expires_at, Some(200));
        assert_eq!(read_body(&lease), b"unchanged body");
    }

    #[test]
    fn disk_storage_refresh_freshness_changed_or_unserveable_generation_rejected() {
        // Arrange.
        let (_temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill(
                "obj://revalidate-race",
                StoredMeta {
                    etag: Some("\"etag-1\"".to_owned()),
                    expires_at: Some(100),
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        writer.commit().unwrap();
        let stale = storage.peek("obj://revalidate-race").unwrap().unwrap();
        let mut writer = storage
            .begin_fill("obj://revalidate-race", stale.clone())
            .unwrap();
        writer.write_chunk(b"next").unwrap();
        writer.commit().unwrap();

        // Act.
        let refreshed = storage
            .refresh_freshness("obj://revalidate-race", &stale, 200)
            .unwrap();

        // Assert.
        assert!(!refreshed);
        let replacement = storage.peek("obj://revalidate-race").unwrap().unwrap();
        assert_ne!(replacement.revision, stale.revision);
        let mut same_projected_metadata = replacement.clone();
        same_projected_metadata.revision = stale.revision;
        assert_eq!(same_projected_metadata, stale);
        assert_eq!(
            read_body(&expect_hit(&storage, "obj://revalidate-race")),
            b"next"
        );

        // Arrange.
        assert!(
            storage
                .evict_data("obj://revalidate-race", replacement.revision)
                .unwrap()
        );

        // Act.
        let refreshed = storage
            .refresh_freshness("obj://revalidate-race", &replacement, 300)
            .unwrap();

        // Assert.
        assert!(!refreshed);
    }

    #[test]
    fn content_range_round_trips_for_partial_entries() {
        let (_t, storage) = open_temp();
        let meta = StoredMeta {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: None,
            status: 206,
            etag: None,
            last_modified: None,
            content_type: None,
            headers: Vec::new(),
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: Some("bytes 0-1023/4096".to_owned()),
        };
        let mut writer = storage.begin_fill("obj://partial", meta).unwrap();
        writer.write_chunk(&[0u8; 1024]).unwrap();
        let lease = writer.commit().unwrap();
        assert_eq!(
            lease.meta.content_range.as_deref(),
            Some("bytes 0-1023/4096")
        );

        let lease = expect_hit(&storage, "obj://partial");
        assert_eq!(
            lease.meta.content_range.as_deref(),
            Some("bytes 0-1023/4096")
        );
        assert_eq!(lease.meta.size, 1024);
    }

    #[test]
    fn headers_round_trip_through_fill_and_lookup() {
        let (_t, storage) = open_temp();
        let meta = StoredMeta {
            logical_key: None,
            size: 0,
            revision: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: None,
            status: 200,
            etag: None,
            last_modified: None,
            content_type: None,
            headers: vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (
                    "X-Conan-Server-Capabilities".to_owned(),
                    "revisions".to_owned(),
                ),
            ],
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: None,
        };
        let mut writer = storage.begin_fill("obj://hdrs", meta).unwrap();
        writer.write_chunk(b"{}").unwrap();
        let _ = writer.commit().unwrap();

        let lease = expect_hit(&storage, "obj://hdrs");
        assert_eq!(
            lease.meta.headers,
            vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                (
                    "X-Conan-Server-Capabilities".to_owned(),
                    "revisions".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn status_round_trips_through_fill_and_lookup() {
        // Arrange.
        let (_t, storage) = open_temp();
        let meta = StoredMeta {
            status: 404,
            headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
            ..StoredMeta::default()
        };

        // Act.
        let mut writer = storage.begin_fill("obj://bursty-not-found", meta).unwrap();
        writer.write_chunk(br#"{"error":"missing"}"#).unwrap();
        let committed = writer.commit().unwrap();
        let looked_up = expect_hit(&storage, "obj://bursty-not-found");

        // Assert.
        assert_eq!(committed.meta.status, 404);
        assert_eq!(looked_up.meta.status, 404);
        assert_eq!(read_body(&looked_up), br#"{"error":"missing"}"#);
        assert_eq!(
            looked_up.meta.headers,
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
    }

    fn sample_record(key: &str) -> MetadataRecord {
        MetadataRecord {
            logical_key: None,
            physical_keys: Vec::new(),
            key: key.to_owned(),
            state: CacheState::Cached,
            layout: ObjectLayout::Raw,
            generation: 0,
            provenance: CacheProvenance::Origin,
            max_idle_secs: None,
            status: 200,
            etag: None,
            last_modified: None,
            content_type: None,
            headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
            representation_length: None,
            expires_at: None,
            versionable: false,
            content_range: None,
            size_bytes: 1,
            last_access: 0,
            hit_count: 0,
            fetch_cost: 0,
            fetch_duration: 0,
        }
    }

    #[test]
    fn record_encoding_round_trips_and_rejects_foreign_versions() {
        let rec = sample_record("obj://v");
        // Current-version bytes decode back to the same record.
        assert_eq!(
            decode_record(&encode_record(&rec).unwrap()).unwrap(),
            Some(rec.clone())
        );

        // A different leading version is discarded, not decoded.
        let mut foreign = encode_record(&rec).unwrap();
        foreign[0] ^= 0xff;
        assert_eq!(decode_record(&foreign).unwrap(), None);

        // Too-short bytes are rejected rather than panicking.
        assert!(decode_record(&[0x01]).is_err());
    }

    #[test]
    fn recovery_discards_a_stale_format_record_and_its_file() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://stale", Some("v"), &[b"body"]);

        // Rewrite the stored value with a truncated current-schema blob; recovery
        // treats corrupt known-version metadata as disposable cache state.
        {
            let txn = storage.inner.db.begin_write().unwrap();
            {
                let mut table = txn.open_table(METADATA).unwrap();
                table.insert("obj://stale", [1u8].as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        // Recovery deletes it + sweeps the orphan file.
        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.discarded_stale_format, 1);
        assert!(matches!(
            storage.lookup("obj://stale").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn rollback_style_recovery_reclaims_unknown_segmented_generation() {
        let (temp, storage) = open_temp();
        let key = "obj://future-segmented";
        let segment = stage(temp.path(), "future", b"future-bytes");
        drop(
            storage
                .adopt_segmented(key, StoredMeta::default(), &[segment])
                .unwrap(),
        );
        let segmented = storage.inner.segmented_object_path(key);
        assert!(segmented.exists());

        let mut future = METADATA_SCHEMA_VERSION
            .saturating_add(1)
            .to_le_bytes()
            .to_vec();
        future.extend_from_slice(&postcard::to_allocvec(&full_record()).unwrap());
        {
            let txn = storage.inner.db.begin_write().unwrap();
            {
                let mut table = txn.open_table(METADATA).unwrap();
                table.insert(key, future.as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.discarded_stale_format, 1);
        assert_eq!(report.deleted_orphan_directories, 1);
        assert!(!segmented.exists());
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::Miss));
    }

    #[test]
    fn committed_entries_survive_reopen() {
        let temp = tempfile::tempdir().unwrap();
        {
            let storage = DiskStorage::open(temp.path()).unwrap();
            write_object(&storage, "obj://persist", Some("e"), &[b"durable bytes"]);
        }
        let storage = DiskStorage::open(temp.path()).unwrap();
        let lease = expect_hit(&storage, "obj://persist");
        assert_eq!(read_body(&lease), b"durable bytes");
        assert_eq!(lease.meta.etag.as_deref(), Some("e"));
    }

    #[test]
    fn lookup_miss_returns_miss() {
        let (_t, storage) = open_temp();
        assert!(matches!(
            storage.lookup("obj://never").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn abort_discards_staging_and_leaves_no_record() {
        let (temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill("obj://aborted", StoredMeta::default())
            .unwrap();
        writer.write_chunk(b"partial").unwrap();
        writer.abort();

        // Cold-fill abort -> Absent (no record).
        assert!(matches!(
            storage.lookup("obj://aborted").unwrap(),
            Lookup::Miss
        ));
        let staging = temp.path().join("staging");
        let leftovers: Vec<_> = fs::read_dir(&staging)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "staging not cleaned: {leftovers:?}");
    }

    #[test]
    fn disk_begin_fill_cleanup_restores_absent_after_each_failure() {
        for fault in BEGIN_FILL_FAULTS {
            // Arrange.
            let (temp, storage) = open_temp();
            let key = format!("obj://begin-cold-{fault:?}");
            inject_fault(fault);

            // Act.
            let result = storage.begin_fill(&key, replacement_meta());

            // Assert.
            assert!(result.is_err(), "{fault:?} did not fail");
            assert_eq!(
                storage.inner.get_record(&key).unwrap(),
                None,
                "{fault:?} left metadata"
            );
            assert_fill_files_removed(&temp, &storage, &key, fault);
        }
    }

    #[test]
    fn disk_begin_fill_cleanup_restores_metadata_only_after_each_failure() {
        for fault in BEGIN_FILL_FAULTS {
            // Arrange.
            let (temp, storage) = open_temp();
            let key = format!("obj://begin-refill-{fault:?}");
            let prior = seed_metadata_only(&storage, &key);
            inject_fault(fault);

            // Act.
            let result = storage.begin_fill(&key, replacement_meta());

            // Assert.
            assert!(result.is_err(), "{fault:?} did not fail");
            assert_eq!(
                storage.inner.get_record(&key).unwrap(),
                Some(prior),
                "{fault:?} did not restore prior metadata"
            );
            assert_fill_files_removed(&temp, &storage, &key, fault);
        }
    }

    #[test]
    fn disk_commit_cleanup_restores_absent_after_each_failure() {
        for fault in COMMIT_FAULTS {
            // Arrange.
            let (temp, storage) = open_temp();
            let key = format!("obj://commit-cold-{fault:?}");
            let mut writer = storage.begin_fill(&key, replacement_meta()).unwrap();
            writer.write_chunk(b"replacement body").unwrap();
            let reader = writer.in_progress_read().unwrap();
            inject_fault(fault);

            // Act.
            let result = writer.commit();

            // Assert.
            assert!(result.is_err(), "{fault:?} did not fail");
            assert_eq!(
                storage.inner.get_record(&key).unwrap(),
                None,
                "{fault:?} left metadata"
            );
            assert_fill_files_removed(&temp, &storage, &key, fault);
            let mut body = [0u8; 16];
            reader
                .file
                .read_exact_at(&mut body, reader.body_offset)
                .unwrap();
            assert_eq!(&body, b"replacement body");
        }
    }

    #[test]
    fn disk_commit_cleanup_restores_metadata_only_after_each_failure() {
        for fault in COMMIT_FAULTS {
            // Arrange.
            let (temp, storage) = open_temp();
            let key = format!("obj://commit-refill-{fault:?}");
            let prior = seed_metadata_only(&storage, &key);
            let mut writer = storage.begin_fill(&key, replacement_meta()).unwrap();
            writer.write_chunk(b"replacement body").unwrap();
            inject_fault(fault);

            // Act.
            let result = writer.commit();

            // Assert.
            assert!(result.is_err(), "{fault:?} did not fail");
            assert_eq!(
                storage.inner.get_record(&key).unwrap(),
                Some(prior),
                "{fault:?} did not restore prior metadata"
            );
            assert_fill_files_removed(&temp, &storage, &key, fault);
        }
    }

    #[test]
    fn disk_commit_cleanup_retry_preserves_restored_state() {
        // Arrange.
        let _cleanup_test = CLEANUP_TEST_LOCK.lock().unwrap();
        let (_temp, storage) = open_temp();
        let key = "obj://cleanup-retry-after-restore";
        let prior = seed_metadata_only(&storage, key);
        let mut writer = storage.begin_fill(key, replacement_meta()).unwrap();
        writer.write_chunk(b"replacement body").unwrap();
        let writer = writer.as_any_mut().downcast_mut::<DiskWriter>().unwrap();
        inject_fault(FaultPoint::CommitMetadataPublication);
        assert!(writer.commit_inner().is_err());
        let destination = writer.published_path.clone().unwrap();
        *FAIL_FILL_DESTINATION_CLEANUP_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(destination.clone());

        // Act.
        let first = writer.cleanup_uncommitted();

        // Assert.
        assert!(first.is_err());
        assert!(writer.state_restored);
        assert_eq!(writer.revoked, Some(false));
        assert_eq!(storage.inner.get_record(key).unwrap(), Some(prior.clone()));
        assert!(destination.exists());

        // Act.
        let second = writer.cleanup_uncommitted();

        // Assert.
        assert!(second.is_ok());
        assert!(writer.record.is_none());
        assert_eq!(storage.inner.get_record(key).unwrap(), Some(prior));
        assert!(!destination.exists());
    }

    #[test]
    fn disk_commit_cleanup_retry_preserves_replacement_destination() {
        // Arrange.
        let _cleanup_test = CLEANUP_TEST_LOCK.lock().unwrap();
        let (_temp, storage) = open_temp();
        let key = "obj://cleanup-retry-with-replacement";
        seed_metadata_only(&storage, key);
        let mut writer = storage.begin_fill(key, replacement_meta()).unwrap();
        writer.write_chunk(b"failed body").unwrap();
        let writer = writer.as_any_mut().downcast_mut::<DiskWriter>().unwrap();
        inject_fault(FaultPoint::CommitMetadataPublication);
        assert!(writer.commit_inner().is_err());
        let destination = writer.published_path.clone().unwrap();
        *FAIL_FILL_DESTINATION_CLEANUP_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(destination);
        assert!(writer.cleanup_uncommitted().is_err());

        let mut replacement = storage.begin_fill(key, replacement_meta()).unwrap();
        replacement.write_chunk(b"replacement body").unwrap();
        drop(replacement.commit().unwrap());

        // Act.
        let retry = writer.cleanup_uncommitted();

        // Assert.
        assert!(retry.is_ok());
        assert_eq!(read_body(&expect_hit(&storage, key)), b"replacement body");
    }

    #[test]
    fn put_overwrites_existing_key() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://k", Some("v1"), &[b"first"]);
        write_object(&storage, "obj://k", Some("v2"), &[b"second-longer"]);

        let lease = expect_hit(&storage, "obj://k");
        assert_eq!(read_body(&lease), b"second-longer");
        assert_eq!(lease.meta.etag.as_deref(), Some("v2"));
    }

    // --- evict_data / remove ----------------------------------------------

    #[test]
    fn disk_evict_metadata_removes_record_and_stays_absent_after_restart() -> Result<()> {
        // Arrange.
        let (temp, storage) = open_temp();
        let key = "obj://evict-metadata";
        write_object(&storage, key, Some("v"), &[b"bytes"]);
        let lease = expect_hit(&storage, key);
        let path = lease.path.clone();

        // Act.
        assert!(storage.evict_data(key, lease.meta.revision)?);

        // Assert.
        assert!(storage.inner.get_record(key)?.is_none());
        assert!(matches!(storage.lookup(key)?, Lookup::Miss));
        assert!(!path.exists());
        assert_eq!(read_body(&lease), b"bytes");
        assert!(!storage.evict_data(key, lease.meta.revision)?);
        // A late hit-history bump from an existing reader cannot recreate metadata.
        storage.inner.record_hit(key);
        storage.inner.flush_pending()?;
        assert!(storage.inner.get_record(key)?.is_none());
        drop(lease);
        drop(storage);

        // Act.
        let reopened = DiskStorage::open(temp.path())?;

        // Assert.
        assert!(reopened.inner.get_record(key)?.is_none());
        assert!(matches!(reopened.lookup(key)?, Lookup::Miss));
        assert!(reopened.eviction_candidates()?.is_empty());
        Ok(())
    }

    #[test]
    fn evict_data_skips_fetch_in_progress() {
        // Arrange. An open fill leaves the record outside `Cached`.
        let (_t, storage) = open_temp();
        let writer = storage
            .begin_fill("obj://f", StoredMeta::default())
            .unwrap();
        let revision = current_revision(&storage, "obj://f");

        // Act.
        let evicted = storage.evict_data("obj://f", revision).unwrap();

        // Assert. An in-flight fetch survives per I17, and the refusal is
        // reported so policy does not account for a reclamation that never
        // happened.
        assert!(!evicted);
        let rec = storage.inner.get_record("obj://f").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::FetchInProgress);

        writer.abort();
    }

    #[test]
    fn disk_evict_generation_rejects_replacement() {
        // Arrange. An eviction decision observes generation one before the
        // replacement commits, which is a legal linearization of the race, so
        // reproducing it needs no barrier.
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://replaced", Some("v1"), &[b"first"]);
        let stale = storage.peek("obj://replaced").unwrap().unwrap();
        write_object(&storage, "obj://replaced", Some("v2"), &[b"second"]);
        let replacement = storage.peek("obj://replaced").unwrap().unwrap();
        assert_ne!(replacement.revision, stale.revision);

        // Act.
        let evicted = storage
            .evict_data("obj://replaced", stale.revision)
            .unwrap();

        // Assert. The superseded decision reclaims nothing and the
        // replacement still serves.
        assert!(!evicted);
        let lease = expect_hit(&storage, "obj://replaced");
        assert_eq!(read_body(&lease), b"second");
        assert_eq!(lease.meta.etag.as_deref(), Some("v2"));
        drop(lease);

        // Act.
        let evicted = storage
            .evict_data("obj://replaced", replacement.revision)
            .unwrap();

        // Assert. Naming the current generation removes its data and metadata.
        assert!(evicted);
        assert!(matches!(
            storage.lookup("obj://replaced").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn remove_deletes_data_and_record_then_misses() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://gone", None, &[b"bye"]);
        let path = expect_hit(&storage, "obj://gone").path.clone();
        assert!(path.exists());

        storage.remove("obj://gone").unwrap();
        assert!(!path.exists());
        assert!(matches!(
            storage.lookup("obj://gone").unwrap(),
            Lookup::Miss
        ));
        // Removing an absent key is tolerated.
        storage.remove("obj://gone").unwrap();
    }

    #[test]
    fn disk_remove_open_writer_commit_is_revoked() {
        // Arrange.
        let (temp, storage) = open_temp();
        let key = "obj://remove-during-fill";
        let mut writer = storage.begin_fill(key, StoredMeta::default()).unwrap();
        writer.write_chunk(b"fetched before the purge").unwrap();

        // Act.
        // Removal lands while the fill is still open, the ordering that lets a
        // later commit resurrect the key.
        storage.remove(key).unwrap();

        // Assert.
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::Miss));

        // Act.
        let Err(revoked) = writer.commit() else {
            panic!("a removed key accepted the prior writer's commit")
        };

        // Assert.
        assert!(revoked.is_fill_revoked(), "{revoked}");
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::Miss));
        assert!(storage.inner.get_record(key).unwrap().is_none());
        assert!(!storage.inner.object_paths(key).1.exists());
        assert_eq!(
            fs::read_dir(temp.path().join("staging")).unwrap().count(),
            0,
            "revoked fill leaked staging"
        );
    }

    #[test]
    fn disk_remove_open_writer_abort_keeps_key_absent() {
        // Arrange.
        // History makes this a replacement fill, which an abort would otherwise
        // restore to `MetadataOnly`.
        let (_t, storage) = open_temp();
        let key = "obj://remove-during-replacement";
        write_object(&storage, key, Some("\"original\""), &[b"original body"]);
        let mut writer = storage.begin_fill(key, replacement_meta()).unwrap();
        writer.write_chunk(b"replacement body").unwrap();

        // Act.
        storage.remove(key).unwrap();
        writer.abort();

        // Assert.
        assert!(
            storage.inner.get_record(key).unwrap().is_none(),
            "abort resurrected metadata for a removed key"
        );
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::Miss));
    }

    #[test]
    fn disk_remove_dropped_writer_keeps_key_absent() {
        // Arrange.
        let (temp, storage) = open_temp();
        let key = "obj://remove-during-drop";
        write_object(&storage, key, Some("\"original\""), &[b"original body"]);
        let mut writer = storage.begin_fill(key, replacement_meta()).unwrap();
        writer.write_chunk(b"replacement body").unwrap();

        // Act.
        // The Drop safety net owns the revocation, since neither commit nor
        // abort runs.
        storage.remove(key).unwrap();
        drop(writer);

        // Assert.
        assert!(storage.inner.get_record(key).unwrap().is_none());
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::Miss));
        assert_eq!(
            fs::read_dir(temp.path().join("staging")).unwrap().count(),
            0,
            "dropped revoked fill leaked staging"
        );
    }

    #[test]
    fn disk_commit_superseded_writer_is_revoked() {
        // Arrange.
        // The ovcc-cache per-key permit normally prevents this overlap, and
        // storage must not lean on that for correctness.
        let (_t, storage) = open_temp();
        let key = "obj://superseded-fill";
        let mut stale = storage.begin_fill(key, StoredMeta::default()).unwrap();
        stale.write_chunk(b"stale").unwrap();
        let mut fresh = storage.begin_fill(key, replacement_meta()).unwrap();
        fresh.write_chunk(b"fresh").unwrap();

        // Act.
        let Err(revoked) = stale.commit() else {
            panic!("a superseded writer published over the newer fill")
        };

        // Assert.
        assert!(revoked.is_fill_revoked(), "{revoked}");

        // Act.
        let lease = fresh.commit().unwrap();

        // Assert.
        assert_eq!(read_body(&lease), b"fresh");
        assert_eq!(read_body(&expect_hit(&storage, key)), b"fresh");
    }

    #[test]
    fn disk_commit_writer_revocation_survives_successor_abort() {
        // Arrange.
        let (_t, storage) = open_temp();
        let key = "obj://superseded-fill-abort";
        write_object(&storage, key, Some("\"original\""), &[b"original body"]);
        let mut stale = storage.begin_fill(key, replacement_meta()).unwrap();
        stale.write_chunk(b"stale").unwrap();
        let fresh = storage.begin_fill(key, replacement_meta()).unwrap();

        // Act.
        // The newer fill restores the superseded generation as metadata-only,
        // which must not grant its old writer authority again.
        fresh.abort();
        let Err(revoked) = stale.commit() else {
            panic!("an aborted successor restored an older writer's authority")
        };

        // Assert.
        assert!(revoked.is_fill_revoked(), "{revoked}");
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::SoftMiss(_)));
        assert!(!storage.inner.object_paths(key).1.exists());
    }

    #[test]
    fn disk_recover_live_replacement_fill_stays_committable() {
        // Arrange.
        let (_t, storage) = open_temp();
        let key = "obj://recover-during-replacement";
        write_object(&storage, key, Some("\"original\""), &[b"original body"]);
        let mut writer = storage.begin_fill(key, replacement_meta()).unwrap();
        writer.write_chunk(b"replacement body").unwrap();

        // Act.
        // Recovery also runs at runtime, and a fill that has not failed keeps
        // its record whether or not the key carried history. Reaping it would
        // revoke the writer; the cold-fill case is covered by
        // `runtime_recovery_preserves_active_writer_and_segmented_lease`.
        let report = storage.recover_on_open().unwrap();

        // Assert.
        assert_eq!(report.skipped_active_fills, 1);
        assert_eq!(report.reconciled_to_metadata_only, 0);

        // Act.
        let lease = writer.commit().unwrap();

        // Assert.
        assert_eq!(read_body(&lease), b"replacement body");
        assert_eq!(read_body(&expect_hit(&storage, key)), b"replacement body");
    }

    #[test]
    fn disk_recover_active_fill_matches_exact_generation() {
        // Arrange.
        let (_t, storage) = open_temp();
        let key = "obj://recover-exact-live-fill";
        write_object(&storage, key, Some("\"original\""), &[b"original body"]);
        let stale = storage.begin_fill(key, replacement_meta()).unwrap();
        let abandoned = storage.begin_fill(key, replacement_meta()).unwrap();
        abandoned.simulate_crash();

        // Act.
        // The stale writer remains live, but it must not protect the abandoned
        // successor's generation from recovery.
        let report = storage.recover_on_open().unwrap();

        // Assert.
        assert_eq!(report.skipped_active_fills, 0);
        assert_eq!(report.reconciled_to_metadata_only, 1);
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::SoftMiss(_)));

        // Act.
        drop(stale);

        // Assert.
        assert!(matches!(storage.lookup(key).unwrap(), Lookup::SoftMiss(_)));
    }

    // --- I9 serve path reconciles a missing file --------------------------

    #[test]
    fn cached_with_missing_file_reconciles_to_soft_miss() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://vanish", Some("v"), &[b"data"]);
        // Simulates out-of-band damage by deleting the data file but leaving the Cached record.
        let (_, path) = storage.inner.object_paths("obj://vanish");
        fs::remove_file(&path).unwrap();

        match storage.lookup("obj://vanish").unwrap() {
            Lookup::SoftMiss(_) => {}
            other => panic!("expected SoftMiss after I9, got {other:?}"),
        }
        let rec = storage.inner.get_record("obj://vanish").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::MetadataOnly);
    }

    // --- Startup classification B1-B4 -------------------------------------

    #[test]
    fn b1_absent_stays_absent() {
        let (_t, storage) = open_temp();
        let report = storage.recover_on_open().unwrap();
        assert_eq!(report, RecoveryReport::default());
    }

    #[test]
    fn b2_metadata_and_data_present_stays_cached() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://b2", Some("v"), &[b"bytes"]);

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.cached, 1);
        assert!(matches!(
            storage.lookup("obj://b2").unwrap(),
            Lookup::Hit(_)
        ));
    }

    #[test]
    fn b3_metadata_without_data_becomes_metadata_only() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://b3", Some("v"), &[b"bytes"]);
        let (_, path) = storage.inner.object_paths("obj://b3");
        fs::remove_file(&path).unwrap(); // metadata=yes, data=no

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.reconciled_to_metadata_only, 1);
        let rec = storage.inner.get_record("obj://b3").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::MetadataOnly);
    }

    #[test]
    fn b4_data_without_metadata_is_orphan_deleted() {
        let (_t, storage) = open_temp();
        // Hand-build a data file with NO record (metadata=no, data=yes).
        let (_, path) = storage.inner.object_paths("obj://b4");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        file.write_all(b"orphan").unwrap();
        drop(file);

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_files, 1);
        assert!(!path.exists());
        assert!(matches!(storage.lookup("obj://b4").unwrap(), Lookup::Miss));
    }

    // --- Crash recovery C1-C5 ---------------------------------------------

    #[test]
    fn c2_crash_mid_download_leaves_only_tmp_which_is_swept() {
        let (temp, storage) = open_temp();
        // Simulates a tmp file in staging, with no record and no object.
        let tmp = temp.path().join("staging").join("99999-0-0.tmp");
        fs::write(&tmp, b"half a body").unwrap();
        let directory = temp.path().join("staging").join("segmented-abandoned");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("fragment"), b"half a fragment").unwrap();

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.swept_staging, 1);
        assert_eq!(report.swept_staging_directories, 1);
        assert!(!tmp.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn c3_crash_mid_eviction_file_deleted_record_remains() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://c3", Some("v"), &[b"bytes"]);
        // Crash window where the data file is gone but the record is still Cached.
        let (_, path) = storage.inner.object_paths("obj://c3");
        fs::remove_file(&path).unwrap();

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.reconciled_to_metadata_only, 1);
        let rec = storage.inner.get_record("obj://c3").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::MetadataOnly);
    }

    #[test]
    fn c4_crash_mid_metadata_deletion_orphan_file_cleaned() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://c4", Some("v"), &[b"bytes"]);
        // Crash window where the record is deleted but the file remains.
        let (_, path) = storage.inner.object_paths("obj://c4");
        storage.inner.delete_record("obj://c4").unwrap();
        assert!(path.exists());

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_files, 1);
        assert!(!path.exists());
    }

    #[test]
    fn c5_crash_during_cold_fetch_clears_to_absent() {
        let (_t, storage) = open_temp();
        // Seed a FetchInProgress record with no file and no history (cold fill).
        storage
            .inner
            .put_record(&MetadataRecord {
                logical_key: None,
                physical_keys: Vec::new(),
                key: "obj://c5cold".to_string(),
                state: CacheState::FetchInProgress,
                layout: ObjectLayout::Raw,
                generation: 0,
                provenance: CacheProvenance::Origin,
                max_idle_secs: None,
                status: 200,
                etag: None,
                last_modified: None,
                content_type: None,
                headers: Vec::new(),
                representation_length: None,
                expires_at: None,
                versionable: false,
                content_range: None,
                size_bytes: 0,
                last_access: now_unix(),
                hit_count: 0,
                fetch_cost: 0,
                fetch_duration: 0,
            })
            .unwrap();

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.removed_records, 1);
        assert!(matches!(
            storage.lookup("obj://c5cold").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn c5_crash_during_refill_keeps_metadata_only() {
        let (_t, storage) = open_temp();
        // Seed a FetchInProgress record WITH history (a failed refill).
        storage
            .inner
            .put_record(&MetadataRecord {
                logical_key: None,
                physical_keys: Vec::new(),
                key: "obj://c5refill".to_string(),
                state: CacheState::FetchInProgress,
                layout: ObjectLayout::Raw,
                generation: 0,
                provenance: CacheProvenance::PeerCopy,
                max_idle_secs: None,
                status: 200,
                etag: Some("v".to_string()),
                last_modified: None,
                content_type: None,
                headers: Vec::new(),
                representation_length: None,
                expires_at: None,
                versionable: false,
                content_range: None,
                size_bytes: 10,
                last_access: now_unix(),
                hit_count: 3,
                fetch_cost: 0,
                fetch_duration: 0,
            })
            .unwrap();

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.reconciled_to_metadata_only, 1);
        let rec = storage.inner.get_record("obj://c5refill").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::MetadataOnly);
        assert_eq!(rec.hit_count, 3);
    }

    #[test]
    fn disk_recover_replacement_crash_before_publish_never_relabels_old_bytes() {
        let cases = [
            (
                "etag",
                StoredMeta {
                    provenance: CacheProvenance::PeerCopy,
                    etag: Some("new-etag".to_owned()),
                    ..StoredMeta::default()
                },
            ),
            (
                "last-modified",
                StoredMeta {
                    last_modified: Some("Mon, 24 Aug 2026 12:00:00 GMT".to_owned()),
                    ..StoredMeta::default()
                },
            ),
            (
                "no-validator",
                StoredMeta {
                    provenance: CacheProvenance::PeerCopy,
                    ..StoredMeta::default()
                },
            ),
        ];

        for (case, replacement) in cases {
            let temp = tempfile::tempdir().unwrap();
            let storage = DiskStorage::open(temp.path()).unwrap();
            let key = format!("obj://replacement-crash-{case}");
            write_object(&storage, &key, Some("old-etag"), &[b"old body"]);
            let old_lease = expect_hit(&storage, &key);
            let old_path = old_lease.path.clone();

            let mut writer = storage.begin_fill(&key, replacement.clone()).unwrap();
            assert!(!old_path.exists(), "old public generation survived {case}");
            assert_eq!(read_body(&old_lease), b"old body");
            writer.write_chunk(b"partial replacement").unwrap();
            writer.simulate_crash();
            drop(old_lease);
            drop(storage);

            let recovered = DiskStorage::open(temp.path()).unwrap();
            match recovered.lookup(&key).unwrap() {
                Lookup::SoftMiss(meta) => {
                    assert_eq!(meta.etag, replacement.etag, "{case}");
                    assert_eq!(meta.last_modified, replacement.last_modified, "{case}");
                    assert_eq!(meta.provenance, replacement.provenance, "{case}");
                }
                other => panic!("replacement crash served bytes for {case}: {other:?}"),
            }
            assert_eq!(recovered.peek(&key).unwrap(), None, "{case}");
            assert!(!old_path.exists(), "old bytes reappeared for {case}");
            assert_eq!(
                fs::read_dir(temp.path().join("staging")).unwrap().count(),
                0
            );
        }
    }

    #[test]
    fn disk_recover_replacement_crash_after_publish_promotes_new_bytes() {
        let (_temp, storage) = open_temp();
        let key = "obj://promote-replacement";
        write_object(&storage, key, Some("old-etag"), &[b"old bytes"]);
        let mut record = storage.inner.get_record(key).unwrap().unwrap();
        let (_, path) = storage.inner.object_paths(key);
        storage.inner.retire_path(&path).unwrap();

        record.state = CacheState::FetchInProgress;
        record.provenance = CacheProvenance::PeerCopy;
        record.etag = Some("new-etag".to_owned());
        record.size_bytes = b"new committed bytes".len() as u64;
        storage.inner.put_record(&record).unwrap();

        // Leaves the durable state from a crash after replacement publication
        // but before the final Cached transaction.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = File::create(&path).unwrap();
        file.write_all(b"new committed bytes").unwrap();
        drop(file);

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.promoted_to_cached, 1);
        let lease = expect_hit(&storage, key);
        assert_eq!(read_body(&lease), b"new committed bytes");
        assert_eq!(lease.meta.size, b"new committed bytes".len() as u64);
        assert_eq!(lease.meta.etag.as_deref(), Some("new-etag"));
        assert_eq!(lease.meta.provenance, CacheProvenance::PeerCopy);
    }

    // --- Coalesced access-history -----------------------------------------

    #[test]
    fn hits_coalesce_and_flush_before_state_mutation() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://hot", Some("v"), &[b"bytes"]);
        // Several hits accumulate in memory (below the flush threshold).
        for _ in 0..5 {
            let _ = expect_hit(&storage, "obj://hot");
        }
        // A state mutation flushes pending history first, so it is persisted.
        let writer = storage
            .begin_fill("obj://other", StoredMeta::default())
            .unwrap();
        writer.abort();
        let rec = storage.inner.get_record("obj://hot").unwrap().unwrap();
        assert_eq!(rec.state, CacheState::Cached);
        assert_eq!(rec.hit_count, 5);
    }

    #[test]
    fn coalesced_history_persists_across_reopen() {
        let temp = tempfile::tempdir().unwrap();
        {
            let storage = DiskStorage::open(temp.path()).unwrap();
            write_object(&storage, "obj://h", Some("v"), &[b"bytes"]);
            for _ in 0..3 {
                let _ = expect_hit(&storage, "obj://h");
            }
            // Clean shutdown flushes pending history (best-effort) on drop.
        }
        let storage = DiskStorage::open(temp.path()).unwrap();
        let rec = storage.inner.get_record("obj://h").unwrap().unwrap();
        assert_eq!(rec.hit_count, 3);
        assert_eq!(rec.state, CacheState::Cached);
    }

    #[test]
    fn pending_bump_does_not_resurrect_removed_record() {
        let (_t, storage) = open_temp();
        write_object(&storage, "obj://rm", None, &[b"bytes"]);
        let _ = expect_hit(&storage, "obj://rm"); // pending bump in memory
        storage.remove("obj://rm").unwrap();
        // A subsequent flush must discard the orphaned bump (no resurrection).
        storage.inner.flush_pending().unwrap();
        assert!(storage.inner.get_record("obj://rm").unwrap().is_none());
        assert!(matches!(storage.lookup("obj://rm").unwrap(), Lookup::Miss));
    }

    pub(super) fn stage(root: &Path, id: &str, bytes: &[u8]) -> StagedSegment {
        let uploads = root.join("uploads");
        fs::create_dir_all(&uploads).unwrap();
        let path = uploads.join(segment_filename(id));
        fs::write(&path, bytes).unwrap();
        StagedSegment::new(id, path, bytes.len() as u64)
    }

    fn read_segmented(lease: &Lease) -> Vec<u8> {
        assert_eq!(lease.layout(), LeaseLayout::Segmented);
        let mut bytes = Vec::new();
        for descriptor in lease.segments().unwrap() {
            lease
                .open_segment(&descriptor)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
        }
        bytes
    }

    fn wait_until_removed(path: &Path) {
        for _ in 0..100 {
            if !path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("cleanup did not remove {}", path.display());
    }

    #[test]
    fn segmented_manifest_rejects_corruption_future_versions_and_impossible_counts() {
        fn refresh_digest(bytes: &mut [u8]) {
            let payload_len = bytes.len() - MANIFEST_DIGEST_LEN;
            let digest = Sha256::digest(&bytes[..payload_len]);
            bytes[payload_len..].copy_from_slice(&digest);
        }

        let id = "block";
        let manifest = SegmentManifest {
            key: "obj://manifest".to_owned(),
            generation: 7,
            total_length: 5,
            entries: vec![ManifestEntry {
                id: id.to_owned(),
                filename: segment_filename(id),
                size: 5,
            }],
        };
        let encoded = encode_manifest(&manifest).unwrap();
        assert_eq!(decode_manifest(&encoded).unwrap(), manifest);

        let mut too_many = manifest;
        too_many.entries = vec![too_many.entries[0].clone(); MAX_MANIFEST_SEGMENTS + 1];
        assert!(encode_manifest(&too_many).is_err());

        let mut corrupt = encoded.clone();
        corrupt[MANIFEST_FIXED_LEN] ^= 1;
        assert!(decode_manifest(&corrupt).is_err());

        let mut future = encoded.clone();
        future[8..16].copy_from_slice(&(MANIFEST_FORMAT_VERSION + 1).to_le_bytes());
        refresh_digest(&mut future);
        assert!(decode_manifest(&future).is_err());

        let mut impossible_count = encoded;
        impossible_count[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        refresh_digest(&mut impossible_count);
        assert!(decode_manifest(&impossible_count).is_err());
    }

    #[test]
    fn segmented_lookup_rejects_corrupt_manifest_and_missing_segment() {
        for fault in ["corrupt-manifest", "missing-segment"] {
            let (temp, storage) = open_temp();
            let key = format!("obj://{fault}");
            let segment = stage(temp.path(), "block", b"bytes");
            drop(
                storage
                    .adopt_segmented(&key, StoredMeta::default(), &[segment])
                    .unwrap(),
            );
            let directory = storage.inner.segmented_object_path(&key);

            if fault == "corrupt-manifest" {
                let manifest_path = directory.join(MANIFEST_FILENAME);
                let mut bytes = fs::read(&manifest_path).unwrap();
                bytes[MANIFEST_FIXED_LEN] ^= 1;
                fs::write(manifest_path, bytes).unwrap();
            } else {
                fs::remove_file(directory.join(segment_filename("block"))).unwrap();
            }

            assert!(matches!(storage.lookup(&key).unwrap(), Lookup::SoftMiss(_)));
            assert!(!directory.exists());
        }
    }

    #[cfg(target_os = "linux")]
    const FIFO_KEY: &str = "obj://fifo-manifest";

    /// A committed segmented object whose manifest was replaced by a FIFO
    /// with no writer, which blocks any open that lacks `O_NONBLOCK`.
    #[cfg(target_os = "linux")]
    fn storage_with_fifo_manifest() -> (tempfile::TempDir, DiskStorage) {
        let (temp, storage) = open_temp();
        let segment = stage(temp.path(), "block", b"bytes");
        drop(
            storage
                .adopt_segmented(FIFO_KEY, StoredMeta::default(), &[segment])
                .unwrap(),
        );
        let manifest_path = storage
            .inner
            .segmented_object_path(FIFO_KEY)
            .join(MANIFEST_FILENAME);
        fs::remove_file(&manifest_path).unwrap();
        ovcc_platform::create_fifo(&manifest_path).unwrap();
        (temp, storage)
    }

    /// Runs `probe` off the test thread and fails instead of hanging if it
    /// blocks.
    #[cfg(target_os = "linux")]
    fn without_blocking<T: Send + 'static>(
        what: &str,
        probe: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(probe());
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{what} must not block on a FIFO manifest"))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn segmented_fifo_manifest_is_rejected_by_live_peek_and_lookup() {
        let (_temp, storage) = storage_with_fifo_manifest();
        // The record is still Cached, so both calls open the live FIFO.
        let (peeked, missed) = without_blocking("peek and lookup", move || {
            let peeked = storage.peek(FIFO_KEY).unwrap();
            let looked_up = storage.lookup(FIFO_KEY).unwrap();
            (peeked, matches!(looked_up, Lookup::SoftMiss(_)))
        });
        assert!(peeked.is_none());
        assert!(missed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn segmented_fifo_manifest_is_rejected_by_recovery() {
        let (temp, storage) = storage_with_fifo_manifest();
        drop(storage);
        let root = temp.path().to_path_buf();
        let peeked = without_blocking("recovery", move || {
            DiskStorage::open(&root).unwrap().peek(FIFO_KEY).unwrap()
        });
        assert!(peeked.is_none());
    }

    #[test]
    fn segmented_lookup_rejects_bad_length_and_incomplete_manifests() {
        for fault in ["bad-length", "incomplete"] {
            let (temp, storage) = open_temp();
            let key = format!("obj://{fault}");
            let first = stage(temp.path(), "first", b"A");
            let second = stage(temp.path(), "second", b"B");
            drop(
                storage
                    .adopt_segmented(&key, StoredMeta::default(), &[first, second])
                    .unwrap(),
            );
            let directory = storage.inner.segmented_object_path(&key);
            let manifest_path = directory.join(MANIFEST_FILENAME);
            let mut manifest = decode_manifest(&fs::read(&manifest_path).unwrap()).unwrap();
            if fault == "bad-length" {
                manifest.total_length -= 1;
            } else {
                manifest.entries.pop();
                manifest.total_length = 1;
            }
            fs::write(&manifest_path, encode_manifest(&manifest).unwrap()).unwrap();

            assert!(matches!(storage.lookup(&key).unwrap(), Lookup::SoftMiss(_)));
            assert!(!directory.exists());
        }
    }

    #[test]
    fn segmented_adoption_preserves_order_duplicates_and_empty_blocks() {
        let (temp, storage) = open_temp();
        let first = stage(temp.path(), "first", b"abc");
        let empty = stage(temp.path(), "empty", b"");
        let last = stage(temp.path(), "last", b"XYZ");
        let lease = storage
            .adopt_segmented(
                "obj://segmented",
                StoredMeta::default(),
                &[first.clone(), empty, first, last],
            )
            .unwrap();

        assert_eq!(lease.meta.size, 9);
        assert_eq!(lease.segments().unwrap().len(), 4);
        assert_eq!(read_segmented(&lease), b"abcabcXYZ");
        assert!(lease.path.join(MANIFEST_FILENAME).is_file());
    }

    #[test]
    fn segmented_prevalidation_missing_block_mutates_nothing() {
        let (temp, storage) = open_temp();
        let existing = stage(temp.path(), "existing", b"kept");
        let missing = StagedSegment::new("missing", temp.path().join("uploads/missing"), 7);
        assert!(
            storage
                .adopt_segmented(
                    "obj://incomplete",
                    StoredMeta::default(),
                    &[existing.clone(), missing],
                )
                .is_err()
        );
        assert!(existing.path.exists());
        assert!(
            storage
                .inner
                .get_record("obj://incomplete")
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            storage.lookup("obj://incomplete").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn segmented_layout_can_be_replaced_by_raw_fill() {
        let (temp, storage) = open_temp();
        let key = "obj://segmented-to-raw";
        let segment = stage(temp.path(), "block", b"segmented");
        drop(
            storage
                .adopt_segmented(key, StoredMeta::default(), &[segment])
                .unwrap(),
        );
        let contiguous = storage.inner.object_paths(key).1;
        let segmented = storage.inner.segmented_object_path(key);
        assert!(!contiguous.exists());
        assert!(segmented.is_dir());

        // A refill after metadata removal must retire the segmented sibling
        // before publishing a raw body at the same key.
        storage.inner.delete_record(key).unwrap();
        let mut writer = storage.begin_fill(key, StoredMeta::default()).unwrap();
        writer.write_chunk(b"raw refill").unwrap();
        writer.commit().unwrap();
        assert!(contiguous.is_file());
        assert!(!segmented.exists());

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_directories, 0);
        assert!(!segmented.exists());
    }

    #[test]
    fn segmented_durability_preparation_does_not_block_unrelated_lookup() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (temp, storage) = open_temp();
        write_object(&storage, "obj://existing", None, &[b"hit"]);
        let storage = Arc::new(storage);
        let segment = stage(temp.path(), "new", b"new");
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *SEGMENT_PREPARE_BARRIERS
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some((
            "obj://new".to_owned(),
            Arc::clone(&entered),
            Arc::clone(&release),
        ));

        let adopting_storage = Arc::clone(&storage);
        let adoption = std::thread::spawn(move || {
            adopting_storage
                .adopt_segmented("obj://new", StoredMeta::default(), &[segment])
                .unwrap()
        });
        entered.wait();

        let lookup_storage = Arc::clone(&storage);
        let (tx, rx) = mpsc::channel();
        let lookup = std::thread::spawn(move || {
            tx.send(lookup_storage.lookup("obj://existing")).unwrap();
        });
        let lookup_result = rx.recv_timeout(Duration::from_secs(1));
        release.wait();
        lookup.join().unwrap();
        drop(adoption.join().unwrap());

        assert!(matches!(lookup_result.unwrap().unwrap(), Lookup::Hit(_)));
    }

    #[test]
    fn segmented_adoption_rejects_same_size_source_replacement() {
        let (temp, storage) = open_temp();
        let storage = Arc::new(storage);
        let segment = stage(temp.path(), "identity", b"old");
        let source_path = segment.path.clone();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *SEGMENT_PREPARE_BARRIERS
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some((
            "obj://identity-race".to_owned(),
            Arc::clone(&entered),
            Arc::clone(&release),
        ));

        let adopting_storage = Arc::clone(&storage);
        let adoption = std::thread::spawn(move || {
            adopting_storage.adopt_segmented(
                "obj://identity-race",
                StoredMeta::default(),
                &[segment],
            )
        });
        entered.wait();
        let replacement = source_path.with_extension("replacement");
        fs::write(&replacement, b"new").unwrap();
        fs::rename(replacement, &source_path).unwrap();
        release.wait();

        assert!(adoption.join().unwrap().is_err());
        assert_eq!(fs::read(&source_path).unwrap(), b"new");
        assert!(
            storage
                .inner
                .get_record("obj://identity-race")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn segmented_entry_survives_restart_and_orphans_are_swept() {
        let temp = tempfile::tempdir().unwrap();
        {
            let storage = DiskStorage::open(temp.path()).unwrap();
            let segment = stage(temp.path(), "restart", b"durable");
            drop(
                storage
                    .adopt_segmented("obj://restart", StoredMeta::default(), &[segment])
                    .unwrap(),
            );
        }
        let storage = DiskStorage::open(temp.path()).unwrap();
        assert_eq!(
            read_segmented(&expect_hit(&storage, "obj://restart")),
            b"durable"
        );

        let orphan = storage
            .inner
            .segmented_object_path("obj://orphan-directory");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("fragment"), b"orphan").unwrap();
        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_directories, 1);
        assert!(!orphan.exists());
    }

    #[test]
    fn segmented_crash_after_directory_publish_promotes_on_recovery() {
        let (temp, storage) = open_temp();
        let key = "obj://segmented-crash-window";
        let source = stage(temp.path(), "crash", b"published");
        let validated = prevalidate_segments(&storage.inner, &[source]).unwrap();
        let generation = next_generation(key, validated.total_length);
        let destination = storage.inner.segmented_object_path(key);
        fs::create_dir_all(&destination).unwrap();
        for source in &validated.unique_sources {
            fs::rename(&source.path, destination.join(&source.filename)).unwrap();
        }
        write_manifest(
            &destination,
            &SegmentManifest {
                key: key.to_owned(),
                generation,
                total_length: validated.total_length,
                entries: validated.entries,
            },
        )
        .unwrap();
        let mut record = sample_record(key);
        record.state = CacheState::FetchInProgress;
        record.layout = ObjectLayout::Segmented;
        record.generation = generation;
        record.size_bytes = validated.total_length;
        storage.inner.put_record(&record).unwrap();

        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.promoted_to_cached, 1);
        assert_eq!(read_segmented(&expect_hit(&storage, key)), b"published");
    }

    #[test]
    fn segmented_lease_pins_generation_across_eviction() {
        let (temp, storage) = open_temp();
        let segment = stage(temp.path(), "pinned", b"still-readable");
        let lease = storage
            .adopt_segmented("obj://pinned", StoredMeta::default(), &[segment])
            .unwrap();
        assert!(
            storage
                .evict_data("obj://pinned", lease.meta.revision)
                .unwrap()
        );
        assert!(storage.inner.get_record("obj://pinned").unwrap().is_none());
        assert_eq!(read_segmented(&lease), b"still-readable");
        assert!(matches!(
            storage.lookup("obj://pinned").unwrap(),
            Lookup::Miss
        ));
        let deferred = storage.inner.lock_deferred().front().unwrap().path.clone();
        assert!(deferred.exists());
        drop(lease);
        storage.eviction_candidates().unwrap();
        assert!(!deferred.exists());
    }

    #[test]
    fn segmented_lease_keeps_owner_alive_and_reclaims_after_storage_drop() {
        let _cleanup_test = CLEANUP_TEST_LOCK.lock().unwrap();
        let (temp, storage) = open_temp();
        let root = temp.path().to_path_buf();
        let segment = stage(temp.path(), "owner", b"leased-after-drop");
        let lease = storage
            .adopt_segmented("obj://owner", StoredMeta::default(), &[segment])
            .unwrap();
        assert!(
            storage
                .evict_data("obj://owner", lease.meta.revision)
                .unwrap()
        );
        let deferred = storage.inner.lock_deferred().front().unwrap().path.clone();
        drop(storage);

        assert_eq!(read_segmented(&lease), b"leased-after-drop");
        assert!(deferred.exists());
        let (sender, receiver) = std::sync::mpsc::channel();
        *CLEANUP_THREAD_REPORTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(CleanupThreadReporter {
            root: root.clone(),
            sender,
        });
        drop(lease);
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        *CLEANUP_THREAD_REPORTER.get().unwrap().lock().unwrap() = None;
        assert!(!deferred.exists());

        let reopened = DiskStorage::open(&root).unwrap();
        assert!(matches!(
            reopened.lookup("obj://owner").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn remove_invalidates_target_before_unrelated_cleanup_failure() {
        let (temp, storage) = open_temp();
        let key = "obj://fail-closed-remove";
        write_object(&storage, key, None, &[b"stale"]);
        let target = storage.inner.object_paths(key).1;

        let unrelated = temp.path().join("staging/unrelated-deferred");
        fs::write(&unrelated, b"unrelated").unwrap();
        let identity = ovcc_platform::stat_nofollow(&unrelated).unwrap().identity;
        storage.inner.defer_cleanup(unrelated.clone(), identity);
        *FAIL_DEFERRED_CLEANUP_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(unrelated);

        let error = storage.remove(key).unwrap_err();
        assert!(error.to_string().contains("injected deferred cleanup"));
        assert!(target.exists(), "physical target cleanup was not reached");
        assert!(
            matches!(storage.lookup(key).unwrap(), Lookup::Miss),
            "durable target invalidation must precede unrelated cleanup"
        );
    }

    #[test]
    fn rollback_does_not_clobber_recreated_source() {
        let temp = tempfile::tempdir().unwrap();
        let source_path = temp.path().join("source");
        let staging = temp.path().join("staging");
        fs::create_dir_all(&staging).unwrap();
        let source = ValidatedSource {
            id: "block".to_owned(),
            path: source_path.clone(),
            filename: "fragment".to_owned(),
            size: 3,
            identity: ObjectIdentity { device: 0, file: 0 },
        };
        fs::write(staging.join(&source.filename), b"old").unwrap();
        fs::write(&source_path, b"new").unwrap();

        rollback_moved_sources(&staging, std::slice::from_ref(&source));

        assert_eq!(fs::read(&source_path).unwrap(), b"new");
        assert_eq!(fs::read(staging.join(&source.filename)).unwrap(), b"old");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_lease_drop_queues_cleanup_off_runtime_thread() {
        let _cleanup_test = CLEANUP_TEST_LOCK.lock().unwrap();
        let (temp, storage) = open_temp();
        let segment = stage(temp.path(), "async-drop", b"leased");
        let lease = storage
            .adopt_segmented("obj://async-drop", StoredMeta::default(), &[segment])
            .unwrap();
        assert!(
            storage
                .evict_data("obj://async-drop", lease.meta.revision)
                .unwrap()
        );
        let deferred = storage.inner.lock_deferred().front().unwrap().path.clone();
        let dropping_thread = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::channel();
        *CLEANUP_THREAD_REPORTER
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(CleanupThreadReporter {
            root: temp.path().to_path_buf(),
            sender,
        });

        drop(lease);
        let cleanup_thread = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        *CLEANUP_THREAD_REPORTER.get().unwrap().lock().unwrap() = None;
        assert_ne!(cleanup_thread, dropping_thread);
        wait_until_removed(&deferred);
    }

    #[test]
    fn deferred_cleanup_failure_preserves_queue_for_retry() {
        let (temp, storage) = open_temp();
        let paths = ["pinned", "failed", "later"].map(|name| {
            let path = temp.path().join("staging").join(name);
            fs::write(&path, name.as_bytes()).unwrap();
            let identity = ovcc_platform::stat_nofollow(&path).unwrap().identity;
            storage.inner.defer_cleanup(path.clone(), identity);
            (path, identity)
        });
        storage.inner.pin_generation(paths[0].1);
        *FAIL_DEFERRED_CLEANUP_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(paths[1].0.clone());

        assert!(storage.inner.drain_deferred().is_err());
        let queued = storage
            .inner
            .lock_deferred()
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            queued,
            paths
                .iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>()
        );

        assert!(storage.inner.release_generation(paths[0].1));
        assert_eq!(storage.inner.drain_deferred().unwrap(), 3);
        assert!(paths.iter().all(|(path, _)| !path.exists()));
    }

    #[test]
    fn recovery_preserves_pinned_public_orphan_after_retire_failure() {
        let (temp, storage) = open_temp();
        let key = "obj://pinned-public-orphan";
        let segment = stage(temp.path(), "public-orphan", b"still-readable");
        let lease = storage
            .adopt_segmented(key, StoredMeta::default(), &[segment])
            .unwrap();
        let public = storage.inner.segmented_object_path(key);
        *storage.inner.fail_retire_key.lock().unwrap() = Some(key.to_owned());

        assert!(storage.remove(key).is_err());
        assert!(public.exists());
        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_directories, 0);
        assert_eq!(read_segmented(&lease), b"still-readable");

        drop(lease);
        let report = storage.recover_on_open().unwrap();
        assert_eq!(report.deleted_orphan_directories, 1);
        assert!(!public.exists());
    }

    #[test]
    fn segmented_lookup_pins_before_concurrent_replacement_can_retire() {
        let (temp, storage) = open_temp();
        let key = "obj://lookup-pin-race";
        let initial = stage(temp.path(), "old-generation", b"old");
        drop(
            storage
                .adopt_segmented(key, StoredMeta::default(), &[initial])
                .unwrap(),
        );
        let replacement = stage(temp.path(), "new-generation", b"new");
        let storage = Arc::new(storage);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *LOOKUP_PIN_BARRIERS
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some((key.to_owned(), Arc::clone(&entered), Arc::clone(&release)));

        let lookup_storage = Arc::clone(&storage);
        let lookup = std::thread::spawn(move || expect_hit(&lookup_storage, key));
        entered.wait();

        let replacement_started = Arc::new(std::sync::Barrier::new(2));
        let replacement_storage = Arc::clone(&storage);
        let replacement_signal = Arc::clone(&replacement_started);
        let replace = std::thread::spawn(move || {
            replacement_signal.wait();
            replacement_storage
                .adopt_segmented(key, StoredMeta::default(), &[replacement])
                .unwrap()
        });
        replacement_started.wait();
        release.wait();

        let old_lease = lookup.join().unwrap();
        drop(replace.join().unwrap());
        assert_eq!(read_segmented(&old_lease), b"old");
        assert_eq!(read_segmented(&expect_hit(&storage, key)), b"new");
    }

    #[test]
    fn storage_get_large_metadata_remains_a_hit() {
        let (_root, storage) = open_temp();
        let key = "obj://large-metadata";
        let value = "x".repeat(128 * 1024);
        let mut writer = storage
            .begin_fill(
                key,
                StoredMeta {
                    headers: vec![("x-metadata".to_owned(), value.clone())],
                    ..StoredMeta::default()
                },
            )
            .unwrap();
        writer.write_chunk(b"body").unwrap();
        drop(writer.commit().unwrap());
        for _ in 0..2 {
            let lease = expect_hit(&storage, key);
            assert_eq!(lease.meta.headers[0].1, value);
            assert_eq!(read_body(&lease), b"body");
        }
    }

    #[test]
    fn storage_get_pending_history_keeps_latest_sample_and_saturates() {
        let mut pending = PendingBump {
            hits: u64::MAX - 1,
            last_access: 200,
        };
        pending.record_hit(100);
        assert_eq!(pending.last_access, 200);
        assert_eq!(pending.hits, u64::MAX);
        pending.record_hit(300);
        assert_eq!(pending.last_access, 300);
        assert_eq!(pending.hits, u64::MAX);
    }

    #[test]
    fn storage_get_hit_decodes_unchanged_record_once() {
        let (_root, storage) = open_temp();
        let key = "obj://single-decode";
        write_object(&storage, key, Some("etag"), &[b"body"]);
        RECORD_DECODES.with(|count| count.set(0));
        drop(expect_hit(&storage, key));
        assert_eq!(RECORD_DECODES.with(|count| count.get()), 1);
    }

    #[test]
    fn storage_get_snapshot_preserves_identity_checks() {
        let (_root, storage) = open_temp();
        let key = "obj://snapshot-identity";
        write_object(&storage, key, Some("etag"), &[b"body"]);
        let mut snapshot = LookupRecordSnapshot::new();
        let record = storage
            .inner
            .get_lookup_record(key, &mut snapshot)
            .unwrap()
            .unwrap();
        for (generation, layout, expected) in [
            (record.generation, Some(ObjectLayout::Raw), true),
            (record.generation, None, true),
            (record.generation + 1, None, false),
            (record.generation, Some(ObjectLayout::Segmented), false),
        ] {
            assert_eq!(
                storage
                    .inner
                    .lookup_record_is_current(key, &snapshot, generation, layout)
                    .unwrap(),
                expected
            );
        }
        let mut changed = record;
        changed.state = CacheState::MetadataOnly;
        storage.inner.put_record(&changed).unwrap();
        assert!(
            !storage
                .inner
                .lookup_record_is_current(key, &snapshot, changed.generation, None)
                .unwrap()
        );
        storage.inner.delete_record(key).unwrap();
        assert!(
            !storage
                .inner
                .lookup_record_is_current(key, &snapshot, changed.generation, None)
                .unwrap()
        );
    }

    #[test]
    fn storage_get_object_paths_preserve_sha256_layouts() {
        let (_root, storage) = open_temp();
        for key in [
            "",
            "abc",
            "obj://blob/東京/é",
            "obj://many/parts?sig=0123456789",
        ] {
            let expected: String = Sha256::digest(key.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(sha256_hex(key), expected);
            let relative = format!("{}/{}.raw", &expected[..2], &expected[2..]);
            let raw = storage.inner.objects_dir.join(&relative);
            assert_eq!(storage.inner.object_paths(key), (relative, raw.clone()));
            assert_eq!(storage.inner.object_path(key, ObjectLayout::Raw), raw);
            assert_eq!(
                storage.inner.segmented_object_path(key),
                raw.with_extension("segments")
            );
        }
    }

    static LOOKUP_RECORD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn storage_get_hit_survives_history_flush_between_record_reads() {
        let _serial = LOOKUP_RECORD_TEST_LOCK.lock().unwrap();
        let (_temp, storage) = open_temp();
        let key = "obj://lookup-history-race";
        write_object(&storage, key, Some("\"old\""), &[b"old-body"]);
        let before = storage.inner.get_record(key).unwrap().unwrap();
        drop(expect_hit(&storage, key));

        let storage = Arc::new(storage);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        *LOOKUP_RECORD_BARRIERS
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some((key.to_owned(), Arc::clone(&entered), Arc::clone(&release)));
        let reader = Arc::clone(&storage);
        let lookup = std::thread::spawn(move || reader.lookup(key).unwrap());
        entered.wait();
        storage.inner.flush_pending().unwrap();
        let flushed = storage.inner.get_record(key).unwrap().unwrap();
        release.wait();

        assert!(flushed.hit_count > before.hit_count);
        assert_eq!(flushed.generation, before.generation);
        let Lookup::Hit(lease) = lookup.join().unwrap() else {
            panic!("history-only rewrite must remain a HIT");
        };
        assert_eq!(lease.meta.revision, before.generation);
        assert_eq!(read_body(&lease), b"old-body");
    }

    #[test]
    fn contiguous_lookup_rejects_a_generation_published_after_its_record_read() {
        let _serial = LOOKUP_RECORD_TEST_LOCK.lock().unwrap();
        // The same-length replacement matches the old body and etag lengths, so
        // its metadata and file length are identical and only the generation
        // re-read can reject it. The longer replacement also covers the
        // truncating variant.
        for (case, body) in [
            ("same-length", &b"new-body"[..]),
            ("longer", &b"new-body-and-more"[..]),
        ] {
            // Arrange.
            let (_temp, storage) = open_temp();
            let key = "obj://lookup-generation-race";
            write_object(&storage, key, Some("\"old\""), &[b"old-body"]);

            let storage = Arc::new(storage);
            let entered = Arc::new(std::sync::Barrier::new(2));
            let release = Arc::new(std::sync::Barrier::new(2));
            *LOOKUP_RECORD_BARRIERS
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap() = Some((key.to_owned(), Arc::clone(&entered), Arc::clone(&release)));

            let reader = Arc::clone(&storage);
            let lookup = std::thread::spawn(move || reader.lookup(key).unwrap());
            // The reader has read the Cached record and is parked before its
            // open, so the replacement lands inside the window under test.
            entered.wait();

            // Act.
            let mut writer = storage
                .begin_fill(
                    key,
                    StoredMeta {
                        etag: Some("\"new\"".to_owned()),
                        ..StoredMeta::default()
                    },
                )
                .unwrap();
            writer.write_chunk(body).unwrap();
            drop(writer.commit().unwrap());
            release.wait();
            let raced = lookup.join().unwrap();

            // Assert.
            match raced {
                Lookup::Hit(lease) => panic!(
                    "{case}: lookup mixed metadata {:?} with body {:?}",
                    lease.meta.etag,
                    String::from_utf8_lossy(&read_body(&lease))
                ),
                Lookup::SoftMiss(meta) => {
                    assert_eq!(meta.etag.as_deref(), Some("\"old\""), "{case}")
                }
                Lookup::Miss => panic!("{case}: history must survive as a soft miss"),
            }

            // Act.
            // Declining to serve repairs nothing, so the replacement must still
            // serve its own metadata and bytes.
            let fresh = expect_hit(&storage, key);

            // Assert.
            assert_eq!(fresh.meta.etag.as_deref(), Some("\"new\""), "{case}");
            assert_eq!(read_body(&fresh), body, "{case}");
        }
    }

    #[test]
    fn exchanged_generation_is_queued_before_fsync_failure() {
        let (temp, storage) = open_temp();
        let key = "obj://exchange-fsync-failure";
        let initial = stage(temp.path(), "before-fsync-failure", b"old");
        drop(
            storage
                .adopt_segmented(key, StoredMeta::default(), &[initial])
                .unwrap(),
        );
        let replacement = stage(temp.path(), "after-fsync-failure", b"new");
        *FAIL_PUBLISH_SYNC_PATH
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap() = Some(storage.inner.segmented_object_path(key));

        let error = storage
            .adopt_segmented(key, StoredMeta::default(), &[replacement])
            .unwrap_err();
        assert!(error.to_string().contains("injected post-exchange"));
        let deferred = storage
            .inner
            .lock_deferred()
            .front()
            .expect("old generation queued before failure")
            .path
            .clone();
        assert!(deferred.exists());

        storage.recover_on_open().unwrap();
        assert!(!deferred.exists());
        assert_eq!(read_segmented(&expect_hit(&storage, key)), b"new");
    }

    #[test]
    fn cross_layout_replacement_retires_inactive_sibling() {
        let (temp, storage) = open_temp();
        let key = "obj://cross-layout";
        write_object(&storage, key, None, &[b"contiguous"]);
        let contiguous = storage.inner.object_paths(key).1;
        let segmented = storage.inner.segmented_object_path(key);
        assert!(contiguous.exists());

        let segment = stage(temp.path(), "segmented", b"segmented");
        drop(
            storage
                .adopt_segmented(key, StoredMeta::default(), &[segment])
                .unwrap(),
        );
        assert!(!contiguous.exists());
        assert!(segmented.exists());
        assert_eq!(read_segmented(&expect_hit(&storage, key)), b"segmented");
        assert_eq!(
            storage.eviction_candidates().unwrap()[0].size_bytes,
            b"segmented".len() as u64
        );

        write_object(&storage, key, None, &[b"contiguous-again"]);
        assert!(contiguous.exists());
        assert!(!segmented.exists());
        assert_eq!(read_body(&expect_hit(&storage, key)), b"contiguous-again");
        assert_eq!(
            storage.eviction_candidates().unwrap()[0].size_bytes,
            b"contiguous-again".len() as u64
        );

        storage.remove(key).unwrap();
        assert!(!contiguous.exists());
        assert!(!segmented.exists());
    }

    #[test]
    fn general_remove_retirement_faults_are_store_scoped() -> Result<()> {
        // Arrange. Independent test stores may arm a fault for the same key.
        let (_first_root, first) = open_temp();
        let (_second_root, second) = open_temp();
        let key = "obj://store-scoped-retirement-fault";
        for storage in [&first, &second] {
            write_object(storage, key, None, &[b"body"]);
            *storage
                .inner
                .fail_retire_key
                .lock()
                .map_err(|_| Error::storage("fault lock poisoned"))? = Some(key.to_owned());
        }
        // Act.
        let outcomes = [first.remove(key), second.remove(key)];
        // Assert.
        assert!(
            outcomes.iter().all(Result::is_err),
            "one storage instance consumed another instance's fault"
        );
        Ok(())
    }

    #[test]
    fn retirement_failure_cannot_leave_old_generation_serveable() {
        let (_temp, storage) = open_temp();
        write_object(&storage, "obj://stale-fail-closed", None, &[b"stale"]);
        *storage.inner.fail_retire_key.lock().unwrap() = Some("obj://stale-fail-closed".to_owned());
        assert!(storage.remove("obj://stale-fail-closed").is_err());
        assert!(matches!(
            storage.lookup("obj://stale-fail-closed").unwrap(),
            Lookup::Miss
        ));
        storage.recover_on_open().unwrap();
        assert!(matches!(
            storage.lookup("obj://stale-fail-closed").unwrap(),
            Lookup::Miss
        ));
    }

    #[test]
    fn runtime_recovery_preserves_active_writer_and_segmented_lease() {
        let (temp, storage) = open_temp();
        let mut writer = storage
            .begin_fill("obj://active-writer", StoredMeta::default())
            .unwrap();
        writer.write_chunk(b"in progress").unwrap();

        let segment = stage(temp.path(), "active-lease", b"leased");
        let lease = storage
            .adopt_segmented("obj://active-lease", StoredMeta::default(), &[segment])
            .unwrap();
        assert!(
            storage
                .evict_data("obj://active-lease", lease.meta.revision)
                .unwrap()
        );
        let deferred = storage.inner.lock_deferred().front().unwrap().path.clone();

        storage.recover_on_open().unwrap();
        assert_eq!(read_segmented(&lease), b"leased");
        assert!(deferred.exists());
        drop(writer.commit().unwrap());

        drop(lease);
        storage.recover_on_open().unwrap();
        assert!(!deferred.exists());
    }

    /// Counts descriptors open under `root`, so other tests running in parallel
    /// in this binary cannot move the count.
    #[cfg(target_os = "linux")]
    fn descriptors_under(root: &Path) -> usize {
        fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
            .filter(|target| target.starts_with(root))
            .count()
    }

    // Counts descriptors through procfs; the bound itself is platform-neutral.
    #[cfg(target_os = "linux")]
    #[test]
    fn segmented_1969_descriptor_lease_keeps_fd_usage_bounded() {
        let (temp, storage) = open_temp();
        let root = temp.path().canonicalize().unwrap();
        let mut segments = Vec::with_capacity(1_969);
        for index in 0..1_969 {
            segments.push(stage(
                temp.path(),
                &format!("segment-{index:04}"),
                &[index as u8],
            ));
        }
        let before = descriptors_under(&root);
        let lease = storage
            .adopt_segmented("obj://1969", StoredMeta::default(), &segments)
            .unwrap();
        let after = descriptors_under(&root);
        assert_eq!(lease.segments().unwrap().len(), 1_969);
        assert!(
            after <= before + 2,
            "descriptor delta was {}",
            after - before
        );
        let first = lease.segments().unwrap().next().unwrap();
        let opened = lease.open_segment(&first).unwrap();
        let with_one = descriptors_under(&root);
        assert!(with_one <= after + 1);
        drop(opened);
    }

    #[test]
    fn ovcc_storage_model_scenarios_conform_to_production_actions() {
        use serde_json::Value;
        use std::collections::{HashMap, HashSet};

        const KEY: &str = "model";
        // One shortest executable replay per production action plus one replay
        // per declared environment fault witness. These are generated scenario
        // IDs, not hand-authored transitions; every edge is replayed from Init.
        const REPLAY_SCENARIOS: &[u64] = &[
            15, 42, 268, 270, 387, 998, 1013, 1037, 1042, 1057, 1065, 1078, 1082, 1098, 1139, 1412,
            1666,
        ];

        struct Replay {
            root: tempfile::TempDir,
            storage: Option<DiskStorage>,
            writer: Option<Box<dyn StoreWrite>>,
            private_segmented: PathBuf,
            generation: u64,
            lease: Option<Lease>,
            opened_segments: usize,
        }

        impl Replay {
            fn new() -> Self {
                let root = tempfile::tempdir().unwrap();
                let storage = DiskStorage::open(root.path()).unwrap();
                let private_segmented = root.path().join("staging/model-replay.segments");
                Self {
                    root,
                    storage: Some(storage),
                    writer: None,
                    private_segmented,
                    generation: 41,
                    lease: None,
                    opened_segments: 0,
                }
            }

            fn storage(&self) -> &DiskStorage {
                self.storage.as_ref().unwrap()
            }

            fn begin_segmented(&mut self) {
                let storage = self.storage();
                let mut record = sample_record(KEY);
                record.state = CacheState::FetchInProgress;
                record.layout = ObjectLayout::Segmented;
                record.generation = self.generation;
                record.size_bytes = 2;
                record.hit_count = 0;
                storage.inner.put_record(&record).unwrap();
                let _ = remove_object_path(&self.private_segmented);
                fs::create_dir_all(&self.private_segmented).unwrap();
            }

            fn segment_path(&self, id: &str) -> PathBuf {
                let private = self.private_segmented.join(segment_filename(id));
                if private.exists() {
                    private
                } else {
                    self.storage()
                        .inner
                        .segmented_object_path(KEY)
                        .join(segment_filename(id))
                }
            }

            fn publish_manifest(&self, expected: &Value) {
                let entries = expected["manifest_entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| ManifestEntry {
                        id: id.as_str().unwrap().to_owned(),
                        filename: segment_filename(id.as_str().unwrap()),
                        size: 1,
                    })
                    .collect();
                write_manifest(
                    &self.private_segmented,
                    &SegmentManifest {
                        key: KEY.to_owned(),
                        generation: self.generation,
                        total_length: expected["manifest_total"].as_u64().unwrap(),
                        entries,
                    },
                )
                .unwrap();
            }

            fn execute(&mut self, action: &Value, expected: &Value) {
                let name = action["name"].as_str().unwrap();
                match name {
                    "BeginContiguousFill" => {
                        let provenance = match action["args"][0].as_str().unwrap() {
                            "Origin" => CacheProvenance::Origin,
                            "PeerCopy" => CacheProvenance::PeerCopy,
                            other => panic!("unknown modeled provenance {other}"),
                        };
                        self.writer = Some(
                            self.storage()
                                .begin_fill(
                                    KEY,
                                    StoredMeta {
                                        provenance,
                                        ..StoredMeta::default()
                                    },
                                )
                                .unwrap(),
                        );
                    }
                    "PublishContiguous" => {
                        self.writer.as_mut().unwrap().write_chunk(b"cc").unwrap();
                    }
                    "BeginSegmentedFill" => self.begin_segmented(),
                    "PublishSegment" => {
                        let id = action["args"][0].as_str().unwrap();
                        fs::write(self.private_segmented.join(segment_filename(id)), b"x").unwrap();
                    }
                    "PublishManifest"
                    | "PublishBadLengthManifest"
                    | "PublishIncompleteManifest" => self.publish_manifest(expected),
                    "AdoptSegmented" => {
                        let destination = self.storage().inner.segmented_object_path(KEY);
                        ensure_object_parent(&self.storage().inner.objects_dir, &destination)
                            .unwrap();
                        fs::rename(&self.private_segmented, destination).unwrap();
                    }
                    "CommitRecord" => {
                        let writer = self.writer.take().expect("contiguous replay writer");
                        drop(writer.commit().unwrap());
                    }
                    "InvalidateContiguous" => {
                        if let Some(writer) = self.writer.take() {
                            drop(writer.commit().unwrap());
                            let mut record = self.storage().inner.get_record(KEY).unwrap().unwrap();
                            record.state = CacheState::FetchInProgress;
                            self.storage().inner.put_record(&record).unwrap();
                        }
                        let path = self.storage().inner.object_paths(KEY).1;
                        File::create(path).unwrap();
                    }
                    "DeleteSegment" => {
                        let id = action["args"][0].as_str().unwrap();
                        fs::remove_file(self.segment_path(id)).unwrap();
                    }
                    "InvalidateManifest" => {
                        let directory = if self.private_segmented.exists() {
                            self.private_segmented.clone()
                        } else {
                            self.storage().inner.segmented_object_path(KEY)
                        };
                        let path = directory.join(MANIFEST_FILENAME);
                        let mut bytes = fs::read(&path).unwrap();
                        bytes[0] ^= 1;
                        fs::write(path, bytes).unwrap();
                    }
                    "ServeContiguous" => {
                        let lookup = self.storage().lookup(KEY).unwrap();
                        match expected["served_kind"].as_str().unwrap() {
                            "Contiguous" => {
                                let Lookup::Hit(lease) = lookup else {
                                    panic!("valid contiguous replay did not hit")
                                };
                                assert_eq!(read_body(&lease), b"cc");
                            }
                            "None" => assert!(!matches!(lookup, Lookup::Hit(_))),
                            other => panic!("unexpected contiguous observation {other}"),
                        }
                    }
                    "OpenSegmentedLease" => {
                        let lookup = self.storage().lookup(KEY).unwrap();
                        if expected["lease_active"].as_bool().unwrap() {
                            let Lookup::Hit(lease) = lookup else {
                                panic!("valid segmented replay did not hit")
                            };
                            self.lease = Some(lease);
                        } else {
                            assert!(!matches!(lookup, Lookup::Hit(_)));
                        }
                    }
                    "OpenNextSegment" => {
                        let lease = self.lease.as_ref().unwrap();
                        let descriptor =
                            lease.segments().unwrap().nth(self.opened_segments).unwrap();
                        assert_eq!(
                            lease
                                .open_segment(&descriptor)
                                .unwrap()
                                .metadata()
                                .unwrap()
                                .len(),
                            1
                        );
                        self.opened_segments += 1;
                    }
                    "CloseSegmentedLease" => {
                        drop(self.lease.take());
                        self.opened_segments = 0;
                    }
                    "Evict" => {
                        // The model enables `Evict` only from `Cached`, so the
                        // observed generation is the one the action retires.
                        let revision = self
                            .storage()
                            .inner
                            .get_record(KEY)
                            .unwrap()
                            .map(|record| record.generation)
                            .unwrap_or_default();
                        assert!(
                            self.storage().evict_data(KEY, revision).unwrap(),
                            "modeled Evict must retire the observed generation"
                        );
                    }
                    "Remove" => {
                        // The model disables `CommitRecord` outside
                        // `FetchInProgress`, so removal has to be final for a
                        // writer opened before it. The adapter keeps that
                        // writer alive across the action and asserts the
                        // production refusal rather than laundering the
                        // discrepancy with an abort.
                        self.storage().remove(KEY).unwrap();
                        if let Some(writer) = self.writer.take() {
                            let Err(revoked) = writer.commit() else {
                                panic!("a removed key accepted a prior writer's commit")
                            };
                            assert!(revoked.is_fill_revoked(), "{revoked}");
                        }
                    }
                    "CleanupOrphan" => {
                        // The model separates orphan-file cleanup from metadata recovery.
                        sweep_orphan_objects(&self.storage().inner, &HashSet::new()).unwrap();
                    }
                    "Recover" => {
                        self.storage().recover_on_open().unwrap();
                    }
                    "Crash" => {
                        drop(self.lease.take());
                        if let Some(writer) = self.writer.take() {
                            writer.simulate_crash();
                        }
                        drop(self.storage.take());
                        self.storage = Some(DiskStorage::open(self.root.path()).unwrap());
                    }
                    "ChangeSchemaVersion" | "ChangeDataVersion" => {
                        panic!("selected replay unexpectedly requires a version-switch driver")
                    }
                    other => panic!("missing replay adapter for {other}"),
                }
                self.assert_record_projection(expected, name);
            }

            fn assert_record_projection(&self, expected: &Value, action: &str) {
                let record = self.storage().inner.get_record(KEY).unwrap();
                match expected["state"].as_str().unwrap() {
                    "Absent" => assert!(record.is_none(), "{action}: expected Absent"),
                    "Cached" => assert_eq!(
                        record.as_ref().map(|record| record.state),
                        Some(CacheState::Cached),
                        "{action}: expected Cached"
                    ),
                    "MetadataOnly" => assert_eq!(
                        record.as_ref().map(|record| record.state),
                        Some(CacheState::MetadataOnly),
                        "{action}: expected MetadataOnly"
                    ),
                    "FetchInProgress" => assert_eq!(
                        record.as_ref().map(|record| record.state),
                        Some(CacheState::FetchInProgress),
                        "{action}: expected FetchInProgress"
                    ),
                    other => panic!("unknown modeled state {other}"),
                }
                if let Some(record) = record {
                    let expected_provenance = match expected["record_source"].as_str().unwrap() {
                        "Origin" => CacheProvenance::Origin,
                        "PeerCopy" => CacheProvenance::PeerCopy,
                        other => panic!("unknown modeled provenance {other}"),
                    };
                    assert_eq!(
                        record.provenance, expected_provenance,
                        "{action}: provenance projection"
                    );
                }
                if action == "BeginContiguousFill" {
                    assert!(
                        !self.storage().inner.object_paths(KEY).1.exists()
                            && !self.storage().inner.segmented_object_path(KEY).exists(),
                        "replacement begin retained a public generation"
                    );
                }
            }
        }

        let scenarios: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-storage-state-machine/fixtures/OvccStorage.scenarios.json"
        ))
        .unwrap();
        let graph: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-storage-state-machine/fixtures/OvccStorage.graph.json"
        ))
        .unwrap();
        let mapping: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-storage-state-machine/action-map.json"
        ))
        .unwrap();
        let mapped_actions = mapping["actions"].as_object().unwrap();
        assert_eq!(
            scenarios["mapped_edge_count"], scenarios["raw_edge_count"],
            "every raw model edge must remain mapped"
        );
        let edges = graph["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| (edge["id"].as_u64().unwrap(), edge))
            .collect::<HashMap<_, _>>();
        let nodes = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| (node["id"].as_u64().unwrap(), &node["state"]))
            .collect::<HashMap<_, _>>();
        let selected = scenarios["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|scenario| REPLAY_SCENARIOS.contains(&scenario["id"].as_u64().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(selected.len(), REPLAY_SCENARIOS.len());
        let mut covered_production = HashSet::new();
        let mut consumed_witnesses = HashSet::new();
        for scenario in selected {
            let mut replay = Replay::new();
            let mut node = graph["initial"].as_u64().unwrap();
            let trace = scenario["trace_edge_ids"].as_array().unwrap();
            let mut executed = Vec::with_capacity(trace.len());
            for edge_id in trace {
                let edge_id = edge_id.as_u64().unwrap();
                let edge = edges[&edge_id];
                assert_eq!(edge["from"].as_u64().unwrap(), node, "trace reordered");
                let action = &edge["action"];
                let action_name = action["name"].as_str().unwrap();
                let mode = mapped_actions[action_name]["mode"].as_str().unwrap();
                if mode == "production" {
                    covered_production.insert(action_name.to_owned());
                }
                node = edge["to"].as_u64().unwrap();
                replay.execute(action, nodes[&node]);
                executed.push(edge_id);
            }
            assert_eq!(
                executed,
                trace
                    .iter()
                    .map(|id| id.as_u64().unwrap())
                    .collect::<Vec<_>>(),
                "an intermediate edge was skipped or reordered"
            );
            assert_eq!(
                edges[executed.last().unwrap()]["action"],
                scenario["action"],
                "trace must terminate at its representative action"
            );
            if let Some(witness) = scenario["fault_witness"].as_str() {
                consumed_witnesses.insert(witness.to_owned());
            }
        }
        assert_eq!(
            covered_production,
            HashSet::from_iter(
                [
                    "BeginContiguousFill",
                    "CommitRecord",
                    "ServeContiguous",
                    "OpenSegmentedLease",
                    "OpenNextSegment",
                    "CloseSegmentedLease",
                    "Evict",
                    "Remove",
                    "CleanupOrphan",
                    "Recover",
                ]
                .map(str::to_owned)
            )
        );
        assert_eq!(
            consumed_witnesses,
            HashSet::from_iter(
                [
                    "manifest-total-minus-one",
                    "manifest-omits-last-segment",
                    "truncated-contiguous-payload",
                    "delete-manifest-listed-segment",
                    "corrupt-manifest-envelope",
                    "restart-same-volume",
                ]
                .map(str::to_owned)
            )
        );
    }
}
