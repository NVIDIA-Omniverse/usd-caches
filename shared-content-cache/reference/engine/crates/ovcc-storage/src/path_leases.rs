// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! In-memory deadline grants over immutable, generation-specific local paths.

use super::*;

const DEADLINES: TableDefinition<&str, u64> = TableDefinition::new("local_path_deadlines_v1");
pub const MAX_PATH_LEASE_MILLIS: u64 = 300_000;

pub trait LeaseClock: Send + Sync {
    fn now_millis(&self) -> u64;
}

pub struct SystemLeaseClock;
impl LeaseClock for SystemLeaseClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[derive(Clone)]
struct Entry {
    meta: Arc<StoredMeta>,
    name: Option<String>,
    path: Option<PathBuf>,
}

#[derive(Default)]
pub(super) struct Catalog {
    current: HashMap<String, Entry>,
    deadlines: HashMap<String, u64>,
    dirty: bool,
}

/// A backend-selected candidate, not yet a granted lease.
#[derive(Clone)]
pub struct PathCandidate {
    pub meta: Arc<StoredMeta>,
    source: LocalPathSource,
    name: Option<String>,
    path: Option<PathBuf>,
}

impl PathCandidate {
    pub fn is_supported(&self) -> bool {
        self.path.is_some()
    }

    /// Recreates a cleaned-up alias and grants only the originally selected generation.
    pub fn reprepare_and_grant(&self, duration_millis: u64) -> Result<Option<PathLeaseGrant>> {
        let Some(inner) = self.source.inner.upgrade() else {
            return Ok(None);
        };
        let store = DiskStorage { inner };
        let _operation = store.inner.write_operations();
        let LocalPathLookup::Hit(candidate) = store.prepare_local_path_locked(&self.source.key)?
        else {
            return Ok(None);
        };
        if candidate.meta.revision != self.meta.revision {
            return Ok(None);
        }
        candidate.grant(duration_millis)
    }

    /// Extends only a still-current candidate, without file I/O or a durable write.
    pub fn grant(&self, duration_millis: u64) -> Result<Option<PathLeaseGrant>> {
        if duration_millis == 0 || duration_millis > MAX_PATH_LEASE_MILLIS {
            return Err(Error::storage("invalid path lease duration"));
        }
        let Some(inner) = self.source.inner.upgrade() else {
            return Ok(None);
        };
        let now = inner.lease_clock.now_millis();
        let requested = now
            .checked_add(duration_millis)
            .ok_or_else(|| Error::storage("lease deadline overflow"))?;
        let mut catalog = inner.path_leases.lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = catalog.current.get(&self.source.key) else {
            return Ok(None);
        };
        if current.meta.revision != self.meta.revision || current.name != self.name {
            return Ok(None);
        }
        let (Some(name), Some(path)) = (&self.name, &self.path) else {
            return Ok(None);
        };
        let deadline = catalog.deadlines.entry(name.clone()).or_default();
        *deadline = (*deadline).max(requested);
        let deadline_unix_millis = *deadline;
        catalog.dirty = true;
        Ok(Some(PathLeaseGrant {
            path: path.clone(),
            deadline_unix_millis,
        }))
    }
}

#[derive(Debug)]
pub struct PathLeaseGrant {
    pub path: PathBuf,
    pub deadline_unix_millis: u64,
}

#[derive(Clone)]
pub struct LocalPathSource {
    inner: Weak<Inner>,
    key: String,
}

impl LocalPathSource {
    pub fn prepare(&self) -> Result<LocalPathLookup> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| Error::storage("cache closed"))?;
        DiskStorage { inner }.prepare_local_path(&self.key)
    }
}

pub enum LocalPathLookup {
    Hit(PathCandidate),
    SoftMiss(StoredMeta),
    Miss,
}

fn name(key: &str, generation: u64) -> String {
    format!("{}-{generation:016x}.raw", sha256_hex(key))
}

fn valid_name(value: &str) -> bool {
    value.len() == 85
        && value.as_bytes()[64] == b'-'
        && value.ends_with(".raw")
        && value.as_bytes()[..64]
            .iter()
            .chain(value.as_bytes()[65..81].iter())
            .all(|b| b.is_ascii_hexdigit())
}

impl DiskStorage {
    pub fn local_path_source(&self, key: &str) -> LocalPathSource {
        LocalPathSource {
            inner: Arc::downgrade(&self.inner),
            key: key.to_owned(),
        }
    }

    /// Reads the warm metadata snapshot only; authorization remains with the proxy.
    pub fn cached_local_path(&self, key: &str) -> Option<PathCandidate> {
        let catalog = self
            .inner
            .path_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = catalog.current.get(key)?;
        let candidate = PathCandidate {
            meta: entry.meta.clone(),
            source: self.local_path_source(key),
            name: entry.name.clone(),
            path: entry.path.clone(),
        };
        drop(catalog);
        let mut pending = self.inner.lock_pending();
        let bump = pending.entry(key.to_owned()).or_default();
        bump.hits = bump.hits.saturating_add(1);
        bump.last_access = now_unix();
        Some(candidate)
    }

    /// Prepares a stable alias once per generation on the blocking storage path.
    pub fn prepare_local_path(&self, key: &str) -> Result<LocalPathLookup> {
        let _operation = self.inner.write_operations();
        self.prepare_local_path_locked(key)
    }

    fn prepare_local_path_locked(&self, key: &str) -> Result<LocalPathLookup> {
        if let Some(candidate) = self.cached_local_path(key) {
            return Ok(LocalPathLookup::Hit(candidate));
        }
        let Some(record) = self.inner.get_record(key)? else {
            return Ok(LocalPathLookup::Miss);
        };
        if record.state != CacheState::Cached {
            return Ok(LocalPathLookup::SoftMiss(Self::meta_of(&record)));
        }
        let meta = Arc::new(Self::meta_of(&record));
        let (lease_name, path) = if record.layout == ObjectLayout::Raw {
            let original = self.inner.record_path(&record);
            let file_metadata = match fs::symlink_metadata(&original) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.reconcile_to_metadata_only(&record)?;
                    return Ok(LocalPathLookup::SoftMiss(Self::meta_of(&record)));
                }
                Err(error) => return Err(error.into()),
            };
            if !file_metadata.is_file() || file_metadata.len() != meta.size {
                self.reconcile_to_metadata_only(&record)?;
                self.inner.retire_path(&original)?;
                return Ok(LocalPathLookup::SoftMiss(Self::meta_of(&record)));
            }
            let directory = self.inner.root.join("local-leases");
            fs::create_dir_all(&directory)?;
            let lease_name = name(key, record.generation);
            let path = fs::canonicalize(directory)?.join(&lease_name);
            match fs::hard_link(&original, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if ovcc_platform::stat_nofollow(&path)?.identity
                        != ovcc_platform::stat_nofollow(&original)?.identity
                    {
                        return Err(Error::storage("local lease generation path collision"));
                    }
                }
                Err(error) => return Err(error.into()),
            }
            (Some(lease_name), Some(path))
        } else {
            (None, None)
        };
        let mut catalog = self
            .inner
            .path_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(name) = &lease_name {
            catalog.deadlines.entry(name.clone()).or_default();
        }
        catalog.current.insert(
            key.to_owned(),
            Entry {
                meta: meta.clone(),
                name: lease_name.clone(),
                path: path.clone(),
            },
        );
        Ok(LocalPathLookup::Hit(PathCandidate {
            meta,
            source: self.local_path_source(key),
            name: lease_name,
            path,
        }))
    }

    /// Batches persistence and reclaims expired aliases independently of clients.
    pub fn maintain_local_leases(&self) -> Result<()> {
        let _operation = self.inner.write_operations();
        self.inner.persist_local_deadlines()?;
        let now = self.inner.lease_clock.now_millis();
        let expired = {
            let catalog = self
                .inner
                .path_leases
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            catalog
                .deadlines
                .iter()
                .filter(|(_, expiry)| **expiry <= now)
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>()
        };
        for name in expired {
            let should_remove = {
                let mut catalog = self
                    .inner
                    .path_leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if catalog
                    .deadlines
                    .get(&name)
                    .is_some_and(|expiry| *expiry <= now)
                {
                    catalog
                        .current
                        .retain(|_, entry| entry.name.as_deref() != Some(&name));
                    catalog.deadlines.remove(&name);
                    catalog.dirty = true;
                    true
                } else {
                    false
                }
            };
            if should_remove {
                let path = self.inner.root.join("local-leases").join(&name);
                if let Err(error) = remove_file_if_present(&path) {
                    self.inner
                        .path_leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .deadlines
                        .insert(name, now);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

impl Inner {
    pub(super) fn invalidate_local_path(&self, key: &str) {
        self.path_leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .remove(key);
    }

    /// Eviction and extension serialize on this same lock, after revision validation.
    pub(super) fn local_path_blocks_eviction(&self, key: &str, generation: u64) -> bool {
        let mut catalog = self.path_leases.lock().unwrap_or_else(|e| e.into_inner());
        if catalog
            .deadlines
            .get(&name(key, generation))
            .is_some_and(|d| *d > self.lease_clock.now_millis())
        {
            return true;
        }
        catalog.current.remove(key);
        false
    }

    fn take_local_deadline_snapshot(&self) -> Option<Vec<(String, u64)>> {
        let mut catalog = self.path_leases.lock().unwrap_or_else(|e| e.into_inner());
        if !catalog.dirty {
            return None;
        }
        catalog.dirty = false;
        Some(
            catalog
                .deadlines
                .iter()
                .map(|(name, expiry)| (name.clone(), *expiry))
                .collect(),
        )
    }

    pub(super) fn persist_local_deadlines(&self) -> Result<()> {
        let Some(snapshot) = self.take_local_deadline_snapshot() else {
            return Ok(());
        };
        let result = self.persist_local_snapshot(snapshot);
        if result.is_err() {
            self.path_leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .dirty = true;
        }
        result
    }

    fn persist_local_snapshot(&self, snapshot: Vec<(String, u64)>) -> Result<()> {
        let directory = self.root.join("local-leases");
        if !directory.exists() {
            return Ok(());
        }
        // Publishes directory entries before making their deadlines durable.
        sync_directory(&directory)?;
        let now = self.lease_clock.now_millis();
        let txn = self.db.begin_write().map_err(map_redb)?;
        {
            let mut table = txn.open_table(DEADLINES).map_err(map_redb)?;
            let mut expired = Vec::new();
            for row in table.iter().map_err(map_redb)? {
                let (name, expiry) = row.map_err(map_redb)?;
                if expiry.value() <= now {
                    expired.push(name.value().to_owned());
                }
            }
            for name in expired {
                table.remove(name.as_str()).map_err(map_redb)?;
            }
            for (name, expiry) in snapshot {
                if expiry > now {
                    let previous = table
                        .get(name.as_str())
                        .map_err(map_redb)?
                        .map(|v| v.value())
                        .unwrap_or(0);
                    table
                        .insert(name.as_str(), previous.max(expiry))
                        .map_err(map_redb)?;
                }
            }
        }
        txn.commit().map_err(map_redb)?;
        Ok(())
    }
}

pub(super) fn recover(storage: &DiskStorage) -> Result<()> {
    let directory = storage.inner.root.join("local-leases");
    let now = storage.inner.lease_clock.now_millis();
    let txn = storage.inner.db.begin_read().map_err(map_redb)?;
    let mut retained = HashMap::new();
    let mut discarded = Vec::new();
    match txn.open_table(DEADLINES) {
        Ok(table) => {
            for row in table.iter().map_err(map_redb)? {
                let (name, expiry) = row.map_err(map_redb)?;
                if valid_name(name.value()) && expiry.value() > now {
                    let present = match fs::symlink_metadata(directory.join(name.value())) {
                        Ok(metadata) => metadata.is_file(),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                        Err(error) => return Err(error.into()),
                    };
                    if present {
                        retained.insert(name.value().to_owned(), expiry.value());
                        continue;
                    }
                }
                discarded.push(name.value().to_owned());
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(error) => return Err(Error::storage(error.to_string())),
    }
    drop(txn);
    if !discarded.is_empty() {
        let txn = storage.inner.db.begin_write().map_err(map_redb)?;
        {
            let mut table = txn.open_table(DEADLINES).map_err(map_redb)?;
            for name in discarded {
                table.remove(name.as_str()).map_err(map_redb)?;
            }
        }
        txn.commit().map_err(map_redb)?;
    }
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if let Some(entries) = entries {
        for file in entries {
            let file = file?;
            if let Some(name) = file.file_name().to_str()
                && valid_name(name)
                && !retained.contains_key(name)
                && remove_file_if_present(&file.path()).is_err()
            {
                // Windows can retain an expired mapped file; maintenance retries it.
                retained.insert(name.to_owned(), now);
            }
        }
    }
    storage
        .inner
        .path_leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .deadlines = retained;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    mod model;

    #[derive(Default)]
    struct GateClock {
        gate: Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
    }

    impl LeaseClock for GateClock {
        fn now_millis(&self) -> u64 {
            let gate = self.gate.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some((entered, resume)) = gate {
                let _ = entered.send(());
                let _ = resume.recv();
            }
            0
        }
    }

    #[derive(Default)]
    struct Clock(AtomicU64);
    impl LeaseClock for Clock {
        fn now_millis(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    impl Clock {
        fn set(&self, now: u64) {
            self.0.store(now, Ordering::SeqCst);
        }
    }

    fn populate(store: &DiskStorage, value: &[u8]) -> Result<()> {
        let mut writer = store.begin_fill(
            "object",
            StoredMeta {
                expires_at: Some(i64::MAX),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(value)?;
        drop(writer.commit()?);
        Ok(())
    }

    fn candidate(store: &DiskStorage) -> Result<PathCandidate> {
        match store.prepare_local_path("object")? {
            LocalPathLookup::Hit(candidate) => Ok(candidate),
            _ => Err(Error::storage("expected path candidate")),
        }
    }

    #[test]
    fn general_get_path_lease_cleanup_before_grant() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"original")?;
        let selected = candidate(&store)?;

        // Act.
        store.maintain_local_leases()?;
        assert!(selected.grant(20)?.is_none());
        let grant = selected
            .reprepare_and_grant(20)?
            .ok_or_else(|| Error::storage("grant denied after alias cleanup"))?;
        store.maintain_local_leases()?;

        // Assert.
        assert_eq!(fs::read(&grant.path)?, b"original");
        assert_eq!(grant.deadline_unix_millis, 20);
        clock.set(20);
        store.maintain_local_leases()?;
        assert!(!grant.path.exists());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_reprepare_rejects_replacement() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let store = DiskStorage::open(root.path())?;
        populate(&store, b"original")?;
        let selected = candidate(&store)?;
        store.maintain_local_leases()?;
        // Act.
        populate(&store, b"replacement")?;

        // Assert.
        assert!(selected.reprepare_and_grant(20)?.is_none());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_reprepare_rejects_eviction() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let store = DiskStorage::open(root.path())?;
        populate(&store, b"original")?;
        let selected = candidate(&store)?;
        // Act.
        assert!(store.evict_data("object", selected.meta.revision)?);

        // Assert.
        assert!(selected.reprepare_and_grant(20)?.is_none());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_recovery_holds_lock_through_grant()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Arrange. The injected clock pauses the real grant before its catalog update.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(GateClock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"original")?;
        let selected = candidate(&store)?;
        store.maintain_local_leases()?;
        let (entered, at_grant) = std::sync::mpsc::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        *clock.gate.lock().map_err(|_| "gate poisoned")? = Some((entered, paused));

        std::thread::scope(
            |scope| -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let unblock = resume;
                // Act.
                let recovery = scope.spawn(|| selected.reprepare_and_grant(20));
                at_grant.recv_timeout(std::time::Duration::from_secs(10))?;
                let (contended, observed) = std::sync::mpsc::channel();
                let cleanup_store = &store;
                let cleanup = scope.spawn(move || {
                    let locked = matches!(
                        cleanup_store.inner.operations.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    );
                    let _ = contended.send(locked);
                    cleanup_store.maintain_local_leases()
                });
                let locked = observed.recv_timeout(std::time::Duration::from_secs(10))?;
                unblock.send(())?;
                let recovered = recovery.join().map_err(|_| "recovery panicked")?;
                cleanup.join().map_err(|_| "cleanup panicked")??;
                // Assert.
                assert!(locked, "recovery released the mutation lock before grant");
                let grant = recovered?.ok_or("grant denied")?;
                assert_eq!(fs::read(grant.path)?, b"original");
                assert_eq!(grant.deadline_unix_millis, 20);
                Ok(())
            },
        )
    }

    #[test]
    fn general_get_path_lease_expiry_blocks_stale_eviction() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"usd")?;
        let selected = store.eviction_candidates()?.remove(0);
        let candidate = candidate(&store)?;
        // Act.
        let grant = candidate
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        let early = store.evict_data("object", selected.revision)?;
        clock.set(20);
        let expired = store.evict_data("object", selected.revision)?;
        store.maintain_local_leases()?;
        // Assert.
        assert!(!early);
        assert!(expired);
        assert!(!grant.path.exists());
        assert!(candidate.grant(20)?.is_none());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_renewal_never_shortens_deadline() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"usd")?;
        let candidate = candidate(&store)?;
        // Act.
        let first = candidate
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        let shorter = candidate
            .grant(10)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        clock.set(15);
        let renewed = candidate
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        // Assert.
        assert_eq!(first.deadline_unix_millis, 20);
        assert_eq!(shorter.deadline_unix_millis, 20);
        assert_eq!(renewed.deadline_unix_millis, 35);
        assert_eq!(first.path, renewed.path);
        Ok(())
    }

    #[test]
    fn general_get_path_lease_replacement_preserves_promised_bytes() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"original")?;
        let old = candidate(&store)?;
        let grant = old
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        // Act.
        populate(&store, b"replacement")?;
        let current = candidate(&store)?
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        // Assert.
        assert_ne!(grant.path, current.path);
        assert_eq!(fs::read(&grant.path)?, b"original");
        assert_eq!(fs::read(&current.path)?, b"replacement");
        assert!(old.grant(20)?.is_none());
        clock.set(20);
        store.maintain_local_leases()?;
        assert!(!grant.path.exists());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_restart_restores_only_persisted_deadline() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"usd")?;
        let current = candidate(&store)?;
        let persisted = current
            .grant(10)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        store.maintain_local_leases()?;
        let volatile = current
            .grant(20)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        // Act.
        drop(store);
        clock.set(9);
        let reopened = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        reopened.maintain_local_leases()?;
        // Assert.
        assert_eq!(volatile.deadline_unix_millis, 20);
        assert!(persisted.path.exists());
        clock.set(10);
        reopened.maintain_local_leases()?;
        assert!(!persisted.path.exists());
        Ok(())
    }

    #[test]
    fn general_get_path_lease_client_death_needs_no_release() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(Clock::default());
        let store = DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        populate(&store, b"usd")?;
        let grant = candidate(&store)?
            .grant(10)?
            .ok_or_else(|| Error::storage("grant denied"))?;
        let path = grant.path.clone();
        // Act.
        drop(grant);
        clock.set(9);
        store.maintain_local_leases()?;
        // Assert.
        assert!(path.exists());
        clock.set(10);
        store.maintain_local_leases()?;
        assert!(!path.exists());
        Ok(())
    }
}
