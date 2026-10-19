// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ops::Bound::{Excluded, Unbounded};

use ovcc_cachekey::{CacheKey, LogicalKey, PhysicalKey, PhysicalKeyParts};

use super::*;

const PHYSICAL_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("physical_keys_v1");
const KEY_BUILD: TableDefinition<&str, u8> = TableDefinition::new("physical_keys_build_v1");
const BACKFILL_BATCH: usize = 128;

/// Local identity policy. Implementations own the format and permitted optional
/// descriptors; storage owns publication, revision checks, and body lifetime.
/// These callbacks must be deterministic, bounded, and perform no network I/O.
pub trait KeyPolicy: Send + Sync {
    /// Rejects unknown policy identifiers and unauthorized optional descriptors.
    fn validate(&self, logical: &LogicalKey) -> Result<()>;
    /// Preserves the backend's existing physical-key format.
    fn render(&self, parts: PhysicalKeyParts<'_>) -> CacheKey;
    /// Strict reconstruction for legacy records. Return None if information is
    /// missing or ambiguous. Storage independently verifies the full round trip.
    fn recover(&self, _key: &str, _meta: &StoredMeta) -> Option<LogicalKey> {
        None
    }
}

/// A match is not authorization or representation validation. Callers retain
/// their existing checks before serving; the mechanism adds no origin probes.
#[derive(Debug)]
pub struct LogicalMatch<T> {
    pub owner_key: String,
    pub logical_key: Option<LogicalKey>,
    /// Union of omissions on the requested and owning physical-key expansion.
    pub omitted_parts: Vec<String>,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PhysicalOwner {
    key: String,
    revision: u64,
}

fn expand(policy: &dyn KeyPolicy, logical: &LogicalKey) -> Result<Vec<PhysicalKey>> {
    logical
        .validate()
        .map_err(|e| Error::storage(e.to_string()))?;
    policy.validate(logical)?;
    logical
        .physical_keys(|parts| policy.render(parts))
        .map_err(|e| Error::storage(e.to_string()))
}

fn decode_owner(bytes: &[u8]) -> Result<PhysicalOwner> {
    postcard::from_bytes(bytes).map_err(|e| Error::storage(format!("physical-key decode: {e}")))
}

impl Inner {
    pub(super) fn reset_physical_keys(&self) -> Result<()> {
        self.key_index_ready.store(false, Ordering::Release);
        if self.key_policy.is_none() {
            return Ok(());
        }
        let txn = self.db.begin_write().map_err(map_redb)?;
        txn.delete_table(PHYSICAL_KEYS).map_err(map_redb)?;
        txn.open_table(PHYSICAL_KEYS).map_err(map_redb)?;
        txn.open_table(KEY_BUILD)
            .map_err(map_redb)?
            .insert("complete", 0)
            .map_err(map_redb)?;
        txn.commit().map_err(map_redb)?;
        Ok(())
    }
    pub(super) fn validate_key_admission(&self, key: &str, meta: &StoredMeta) -> Result<()> {
        if let Some(logical) = &meta.logical_key {
            logical
                .validate()
                .map_err(|e| Error::storage(e.to_string()))?;
            if let Some(policy) = self.key_policy.as_deref() {
                let keys = expand(policy, logical)?;
                if keys.first().is_none_or(|full| full.key.as_str() != key) {
                    return Err(Error::storage(
                        "logical key does not render the admission key",
                    ));
                }
            }
        }
        Ok(())
    }
    pub(super) fn prepare_key_record(&self, record: &MetadataRecord) -> Result<MetadataRecord> {
        let mut record = record.clone();
        record.physical_keys.clear();
        let Some(policy) = self.key_policy.as_deref() else {
            return Ok(record);
        };
        if record.state != CacheState::Cached {
            return Ok(record);
        }
        if record.logical_key.is_none() {
            record.logical_key = policy.recover(&record.key, &DiskStorage::meta_of(&record));
        }
        let Some(logical) = &record.logical_key else {
            return Ok(record);
        };
        // Unknown legacy policy is retained for a future capable process, but
        // cannot acquire alternate identities under the current policy.
        let Ok(keys) = expand(policy, logical) else {
            return Ok(record);
        };
        if keys
            .first()
            .is_none_or(|full| full.key.as_str() != record.key)
        {
            return Ok(record);
        }
        record.physical_keys = keys.into_iter().map(|k| k.key.into_string()).collect();
        Ok(record)
    }

    pub(super) fn remove_physical_keys(
        &self,
        txn: &redb::WriteTransaction,
        record: &MetadataRecord,
    ) -> Result<()> {
        if record.physical_keys.is_empty() {
            return Ok(());
        }
        let mut table = txn.open_table(PHYSICAL_KEYS).map_err(map_redb)?;
        for key in &record.physical_keys {
            let owner = table
                .get(key.as_str())
                .map_err(map_redb)?
                .and_then(|v| decode_owner(v.value()).ok());
            if owner.is_some_and(|o| o.key == record.key && o.revision == record.generation) {
                table.remove(key.as_str()).map_err(map_redb)?;
            }
        }
        Ok(())
    }

    pub(super) fn update_physical_keys(
        &self,
        txn: &redb::WriteTransaction,
        record: &MetadataRecord,
    ) -> Result<()> {
        let metadata = txn.open_table(METADATA).map_err(map_redb)?;
        let previous = metadata
            .get(record.key.as_str())
            .map_err(map_redb)?
            .map(|v| decode_record(v.value()))
            .transpose()?
            .flatten();
        if let Some(previous) = previous {
            self.remove_physical_keys(txn, &previous)?;
        }
        if record.state != CacheState::Cached || record.physical_keys.is_empty() {
            return Ok(());
        }
        let mut table = txn.open_table(PHYSICAL_KEYS).map_err(map_redb)?;
        let encoded = postcard::to_allocvec(&PhysicalOwner {
            key: record.key.clone(),
            revision: record.generation,
        })
        .map_err(|e| Error::storage(format!("physical-key encode: {e}")))?;
        for physical in &record.physical_keys {
            // Direct ownership also covers old exact-only records with no index.
            if physical != &record.key {
                let direct = metadata
                    .get(physical.as_str())
                    .map_err(map_redb)?
                    .map(|v| decode_record(v.value()))
                    .transpose()?
                    .flatten();
                if direct.is_some_and(|r| r.state == CacheState::Cached) {
                    continue;
                }
            }
            let owner = table
                .get(physical.as_str())
                .map_err(map_redb)?
                .and_then(|v| decode_owner(v.value()).ok());
            if physical == &record.key
                || owner
                    .as_ref()
                    .is_none_or(|o| o.key != *physical && o.key <= record.key)
            {
                table
                    .insert(physical.as_str(), encoded.as_slice())
                    .map_err(map_redb)?;
            }
        }
        Ok(())
    }
}

impl DiskStorage {
    pub(super) fn rebuild_physical_keys(&self) -> Result<()> {
        self.inner.key_index_ready.store(false, Ordering::Release);
        if self.inner.key_policy.is_none() {
            return Ok(());
        }
        // Rebuild on every enabled open, including policy upgrades. Only the
        // derived index is cleared. A durable marker records partial builds.
        self.inner.reset_physical_keys()?;

        let mut after = None::<String>;
        loop {
            let records = {
                let txn = self.inner.db.begin_read().map_err(map_redb)?;
                let table = match txn.open_table(METADATA) {
                    Ok(table) => table,
                    Err(redb::TableError::TableDoesNotExist(_)) => break,
                    Err(e) => return Err(map_redb(e)),
                };
                let bounds = (after.as_deref().map_or(Unbounded, Excluded), Unbounded);
                let mut records = Vec::with_capacity(BACKFILL_BATCH);
                for entry in table
                    .range::<&str>(bounds)
                    .map_err(map_redb)?
                    .take(BACKFILL_BATCH)
                {
                    let (key, value) = entry.map_err(map_redb)?;
                    records.push((key.value().to_owned(), decode_record(value.value())?));
                }
                records
            };
            if records.is_empty() {
                break;
            }
            let txn = self.inner.db.begin_write().map_err(map_redb)?;
            for (key, record) in records {
                after = Some(key);
                let Some(record) = record else {
                    continue;
                };
                if record.state != CacheState::Cached {
                    continue;
                }
                let record = self.inner.prepare_key_record(&record)?;
                self.inner.update_physical_keys(&txn, &record)?;
                let encoded = encode_record(&record)?;
                txn.open_table(METADATA)
                    .map_err(map_redb)?
                    .insert(record.key.as_str(), encoded.as_slice())
                    .map_err(map_redb)?;
            }
            txn.commit().map_err(map_redb)?;
            #[cfg(test)]
            fail_if_injected(FaultPoint::BackfillBatch)?;
        }
        let txn = self.inner.db.begin_write().map_err(map_redb)?;
        txn.open_table(KEY_BUILD)
            .map_err(map_redb)?
            .insert("complete", 1)
            .map_err(map_redb)?;
        txn.commit().map_err(map_redb)?;
        self.inner.key_index_ready.store(true, Ordering::Release);
        Ok(())
    }

    fn logical_candidate(&self, logical: &LogicalKey) -> Result<Option<LogicalMatch<StoredMeta>>> {
        let Some(policy) = self.inner.key_policy.as_deref() else {
            return Ok(None);
        };
        if !self.inner.key_index_ready.load(Ordering::Acquire) {
            return Ok(None);
        }
        let keys = expand(policy, logical)?;
        for physical in keys {
            // A physical key's exact owner takes precedence over index claims.
            let direct = self.peek(physical.key.as_str())?;
            let (owner, meta) = if let Some(meta) = direct {
                (physical.key.as_str().to_owned(), meta)
            } else {
                let owner = {
                    let txn = self.inner.db.begin_read().map_err(map_redb)?;
                    let table = txn.open_table(PHYSICAL_KEYS).map_err(map_redb)?;
                    table
                        .get(physical.key.as_str())
                        .map_err(map_redb)?
                        .and_then(|v| decode_owner(v.value()).ok())
                };
                let Some(owner) = owner else {
                    continue;
                };
                let Some(meta) = self.peek(&owner.key)? else {
                    continue;
                };
                if meta.revision != owner.revision {
                    continue;
                }
                (owner.key, meta)
            };
            let mut omitted_parts = physical.omitted_parts;
            let full_match = omitted_parts.is_empty() && owner == physical.key.as_str();
            // Even an exact rendered string is not permission to weaken a
            // recorded required identity if a legacy renderer is ambiguous.
            if meta
                .logical_key
                .as_ref()
                .is_some_and(|stored| !logical.same_required_parts(stored))
            {
                continue;
            }
            if !full_match {
                let Some(stored) = &meta.logical_key else {
                    continue;
                };
                if !logical.same_required_parts(stored) {
                    continue;
                }
                let Ok(owner_keys) = expand(policy, stored) else {
                    continue;
                };
                let Some(owner_match) = owner_keys.iter().find(|p| p.key == physical.key) else {
                    continue;
                };
                omitted_parts.extend(owner_match.omitted_parts.iter().cloned());
                omitted_parts.sort_unstable();
                omitted_parts.dedup();
            }
            return Ok(Some(LogicalMatch {
                owner_key: owner,
                logical_key: meta.logical_key.clone(),
                omitted_parts,
                value: meta,
            }));
            // Resolution chooses one physical mapping. Compatibility rejection
            // belongs to the caller; it must not silently search losing entries.
        }
        Ok(None)
    }

    /// Side-effect-free full-first lookup, including owner/revision validation.
    pub fn peek_logical(&self, logical: &LogicalKey) -> Result<Option<LogicalMatch<StoredMeta>>> {
        self.logical_candidate(logical)
    }

    /// Opens the owning body once. A replacement between observation and open
    /// becomes a MISS; an already acquired old lease remains valid as usual.
    pub fn lookup_logical(&self, logical: &LogicalKey) -> Result<Option<LogicalMatch<Lease>>> {
        let Some(matched) = self.peek_logical(logical)? else {
            return Ok(None);
        };
        self.open_logical_match(matched)
    }

    fn open_logical_match(
        &self,
        matched: LogicalMatch<StoredMeta>,
    ) -> Result<Option<LogicalMatch<Lease>>> {
        let mut snapshot = LookupRecordSnapshot::new();
        let Lookup::Hit(lease) = self.lookup_with_snapshot(&matched.owner_key, &mut snapshot)?
        else {
            return Ok(None);
        };
        if lease.meta.revision != matched.value.revision {
            return Ok(None);
        }
        // Exact `lookup` now rejects a replacement landing between its own
        // record read and open, so this recheck is a second guard against new
        // bytes inheriting the old revision rather than the only one. Later
        // retirement stays safe because the acquired file/directory lease
        // already owns its bytes.
        if !self.inner.lookup_record_is_current(
            &matched.owner_key,
            &snapshot,
            lease.meta.revision,
            None,
        )? {
            return Ok(None);
        }
        if lease.segments().is_none()
            && lease.file.metadata()?.len() != lease.body_offset.saturating_add(lease.meta.size)
        {
            return Ok(None);
        }
        Ok(Some(LogicalMatch {
            owner_key: matched.owner_key,
            logical_key: matched.logical_key,
            omitted_parts: matched.omitted_parts,
            value: lease,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovcc_cachekey::KeyPart;

    struct TestPolicy;

    impl KeyPolicy for TestPolicy {
        fn validate(&self, logical: &LogicalKey) -> Result<()> {
            let parts = logical.parts();
            if logical.policy() != "test.v1"
                || parts.len() != 3
                || parts[0].name != "object"
                || parts[0].optional
                || parts[1].name != "tenant"
                || parts[1].optional
                || parts[2].name != "variant"
                || !parts[2].optional
            {
                return Err(Error::storage("unsupported test key policy"));
            }
            Ok(())
        }

        fn render(&self, parts: PhysicalKeyParts<'_>) -> CacheKey {
            CacheKey::from_raw(format!(
                "{}|{}|{}",
                parts.get("tenant").flatten().unwrap_or(""),
                parts.get("object").flatten().unwrap_or(""),
                parts.get("variant").flatten().unwrap_or("")
            ))
        }

        fn recover(&self, key: &str, _meta: &StoredMeta) -> Option<LogicalKey> {
            let parts: Vec<_> = key.split('|').collect();
            if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() {
                return None;
            }
            logical(
                parts[0],
                parts[1],
                (!parts[2].is_empty()).then_some(parts[2]),
            )
            .ok()
        }
    }

    fn logical(tenant: &str, object: &str, variant: Option<&str>) -> Result<LogicalKey> {
        LogicalKey::new(
            "test.v1".into(),
            vec![
                KeyPart {
                    name: "tenant".into(),
                    value: Some(tenant.into()),
                    optional: false,
                },
                KeyPart {
                    name: "object".into(),
                    value: Some(object.into()),
                    optional: false,
                },
                KeyPart {
                    name: "variant".into(),
                    value: variant.map(str::to_owned),
                    optional: true,
                },
            ],
        )
        .map_err(|e| Error::storage(e.to_string()))
    }

    fn open(root: &Path) -> Result<DiskStorage> {
        DiskStorage::open_with_key_policy(root, Some(Arc::new(TestPolicy)))
    }

    fn fill(storage: &DiskStorage, variant: Option<&str>, body: &[u8]) -> Result<Lease> {
        let logical = logical("tenant", "object", variant)?;
        let key = expand(&TestPolicy, &logical)?.remove(0).key;
        let mut writer = storage.begin_fill(
            key.as_str(),
            StoredMeta {
                logical_key: Some(logical),
                etag: Some("\"v1\"".into()),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(body)?;
        writer.commit()
    }

    fn required<T>(value: Option<T>) -> Result<T> {
        value.ok_or_else(|| Error::storage("expected optional-key match"))
    }

    fn body(lease: &Lease) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if let Some(segments) = lease.segments() {
            for descriptor in segments {
                lease.open_segment(&descriptor)?.read_to_end(&mut bytes)?;
            }
        } else {
            let mut file = lease.file.try_clone()?;
            file.seek(SeekFrom::Start(lease.body_offset))?;
            file.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    }

    #[test]
    fn general_get_optional_keys_reuses_lookup_snapshot() -> Result<()> {
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        drop(fill(&storage, Some("A"), b"payload")?);
        let query = logical("tenant", "object", None)?;
        let matched = required(storage.peek_logical(&query)?)?;
        RECORD_DECODES.with(|count| count.set(0));
        let found = required(storage.open_logical_match(matched)?)?;
        assert_eq!(RECORD_DECODES.with(|count| count.get()), 1);
        assert_eq!(body(&found.value)?, b"payload");
        Ok(())
    }

    #[test]
    fn general_get_optional_keys_reuses_one_body_and_keeps_required_identity() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let seeded = fill(&storage, Some("A"), b"payload")?;
        let before = required(storage.inner.get_record("tenant|object|A")?)?;
        // Act.
        let peek = required(storage.peek_logical(&logical("tenant", "object", None)?)?)?;
        let after_peek = required(storage.inner.get_record("tenant|object|A")?)?;
        let found = required(storage.lookup_logical(&logical("tenant", "object", Some("B"))?)?)?;
        // Assert.
        assert_eq!(before, after_peek);
        assert_eq!(peek.owner_key, "tenant|object|A");
        assert_eq!(found.omitted_parts, ["variant"]);
        assert_eq!(
            ovcc_platform::file_identity(&found.value.file)?,
            ovcc_platform::file_identity(&seeded.file)?
        );
        assert_eq!(body(&found.value)?, b"payload");
        let accounting = storage.eviction_candidates()?;
        assert_eq!(accounting.len(), 1);
        assert_eq!(accounting[0].size_bytes, 7);
        assert_eq!(accounting[0].hit_count, 1);
        assert!(
            storage
                .lookup_logical(&logical("other", "object", None)?)?
                .is_none()
        );
        assert!(
            storage
                .lookup_logical(&logical("tenant", "other", None)?)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn general_commit_optional_keys_lexicographic_winner_in_both_orders() -> Result<()> {
        // Arrange.
        for order in [["A", "B"], ["B", "A"]] {
            let root = tempfile::tempdir()?;
            let storage = open(root.path())?;
            // Act.
            for version in order {
                fill(&storage, Some(version), version.as_bytes())?;
            }
            let winner = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
            // Assert.
            assert_eq!(winner.owner_key, "tenant|object|B");
            assert_eq!(body(&winner.value)?, b"B");
            assert!(matches!(storage.lookup("tenant|object|A")?, Lookup::Hit(_)));
            drop(winner);
            drop(storage);
            let recovered = open(root.path())?;
            assert_eq!(
                required(recovered.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
                "tenant|object|B"
            );
        }
        Ok(())
    }

    #[test]
    fn general_commit_optional_keys_direct_owner_precedes_omission() -> Result<()> {
        // Arrange.
        for direct_first in [false, true] {
            let root = tempfile::tempdir()?;
            let storage = open(root.path())?;
            // Act.
            if direct_first {
                fill(&storage, None, b"direct")?;
            }
            fill(&storage, Some("Z"), b"optional")?;
            if !direct_first {
                fill(&storage, None, b"direct")?;
            }
            let hit = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
            // Assert.
            assert!(hit.omitted_parts.is_empty());
            assert_eq!(body(&hit.value)?, b"direct");
        }
        Ok(())
    }

    #[test]
    fn general_evict_optional_keys_stale_cleanup_and_held_lease() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        fill(&storage, Some("A"), b"old")?;
        let old_record = required(storage.inner.get_record("tenant|object|A")?)?;
        let held = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
        // Act.
        fill(&storage, Some("A"), b"new")?;
        let txn = storage.inner.db.begin_write().map_err(map_redb)?;
        storage.inner.remove_physical_keys(&txn, &old_record)?;
        txn.commit().map_err(map_redb)?;
        let replacement = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
        assert!(storage.evict_data("tenant|object|A", replacement.value.meta.revision)?);
        // Assert.
        assert!(storage.inner.get_record("tenant|object|A")?.is_none());
        assert_eq!(body(&held.value)?, b"old");
        assert_eq!(body(&replacement.value)?, b"new");
        assert!(
            storage
                .peek_logical(&logical("tenant", "object", None)?)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn general_abort_optional_keys_incomplete_entries_never_compete() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        fill(&storage, Some("A"), b"committed")?;
        let mut writer = storage.begin_fill(
            "tenant|object|Z",
            StoredMeta {
                logical_key: Some(logical("tenant", "object", Some("Z"))?),
                ..StoredMeta::default()
            },
        )?;
        // Act.
        writer.write_chunk(b"incomplete")?;
        let during = required(storage.peek_logical(&logical("tenant", "object", None)?)?)?;
        writer.abort();
        // Assert.
        assert_eq!(during.owner_key, "tenant|object|A");
        assert_eq!(
            required(storage.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|A"
        );
        Ok(())
    }

    #[test]
    fn general_get_optional_keys_segmented_body_survives_eviction() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let fragment = root.path().join("fragment");
        fs::write(&fragment, b"segment")?;
        let seeded = storage.adopt_segmented(
            "tenant|object|A",
            StoredMeta {
                logical_key: Some(logical("tenant", "object", Some("A"))?),
                ..StoredMeta::default()
            },
            &[StagedSegment::new("part", &fragment, 7)],
        )?;
        // Act.
        let found = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
        storage.remove("tenant|object|A")?;
        // Assert.
        assert_eq!(
            ovcc_platform::file_identity(&found.value.file)?,
            ovcc_platform::file_identity(&seeded.file)?
        );
        assert_eq!(body(&found.value)?, b"segment");
        Ok(())
    }

    #[test]
    fn general_recover_optional_keys_disabled_and_unknown_keys_stay_exact_only() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = DiskStorage::open(root.path())?;
        fill(&storage, Some("A"), b"stored")?;
        let mut unknown = storage.begin_fill("unrecoverable", StoredMeta::default())?;
        unknown.write_chunk(b"other")?;
        unknown.commit()?;
        // Act.
        assert!(
            storage
                .peek_logical(&logical("tenant", "object", None)?)?
                .is_none()
        );
        drop(storage);
        let enabled = open(root.path())?;
        let found = required(enabled.peek_logical(&logical("tenant", "object", None)?)?)?;
        drop(enabled);
        let disabled = DiskStorage::open(root.path())?;
        // Assert.
        assert_eq!(found.owner_key, "tenant|object|A");
        assert!(matches!(disabled.lookup("unrecoverable")?, Lookup::Hit(_)));
        assert!(
            disabled
                .peek_logical(&logical("tenant", "object", None)?)?
                .is_none()
        );
        assert!(matches!(
            disabled.lookup("tenant|object|A")?,
            Lookup::Hit(_)
        ));
        Ok(())
    }

    #[test]
    fn general_recover_optional_keys_interrupted_backfill_restarts_before_lookup() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        for object in 0..(BACKFILL_BATCH + 1) {
            let object = format!("object-{object:03}");
            let key = format!("tenant|{object}|A");
            let mut writer = storage.begin_fill(
                &key,
                StoredMeta {
                    logical_key: Some(logical("tenant", &object, Some("A"))?),
                    ..StoredMeta::default()
                },
            )?;
            writer.write_chunk(b"persisted")?;
            writer.commit()?;
        }
        inject_fault(FaultPoint::BackfillBatch);
        // Act.
        let interrupted = storage.recover_on_open();
        // Assert.
        assert!(interrupted.is_err());
        assert!(INJECTED_FAULT.with(|fault| fault.get().is_none()));
        assert!(!storage.inner.key_index_ready.load(Ordering::Acquire));
        assert!(
            storage
                .peek_logical(&logical("tenant", "object-000", None)?)?
                .is_none()
        );
        assert_eq!(storage.eviction_candidates()?.len(), BACKFILL_BATCH + 1);
        {
            let txn = storage.inner.db.begin_read().map_err(map_redb)?;
            assert_eq!(
                txn.open_table(KEY_BUILD)
                    .map_err(map_redb)?
                    .get("complete")
                    .map_err(map_redb)?
                    .map(|v| v.value()),
                Some(0)
            );
        }
        // Arrange.
        drop(storage);
        // Act.
        let recovered = open(root.path())?;
        let first = required(recovered.peek_logical(&logical("tenant", "object-000", None)?)?)?;
        let last = required(recovered.peek_logical(&logical("tenant", "object-128", None)?)?)?;
        recovered.recover_on_open()?;
        // Assert.
        assert!(recovered.inner.key_index_ready.load(Ordering::Acquire));
        assert_eq!(first.owner_key, "tenant|object-000|A");
        assert_eq!(last.owner_key, "tenant|object-128|A");
        assert_eq!(
            required(recovered.peek_logical(&logical("tenant", "object-128", None)?)?)?
                .value
                .revision,
            last.value.revision
        );
        Ok(())
    }

    #[test]
    fn general_commit_optional_keys_failed_publication_does_not_register() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        fill(&storage, Some("A"), b"valid")?;
        let mut writer = storage.begin_fill(
            "tenant|object|Z",
            StoredMeta {
                logical_key: Some(logical("tenant", "object", Some("Z"))?),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"failed")?;
        inject_fault(FaultPoint::CommitMetadataPublication);
        // Act.
        let failed = writer.commit();
        // Assert.
        assert!(failed.is_err());
        assert!(INJECTED_FAULT.with(|fault| fault.get().is_none()));
        assert_eq!(
            required(storage.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|A"
        );
        storage.recover_on_open()?;
        assert_eq!(
            required(storage.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|A"
        );
        Ok(())
    }

    #[test]
    fn general_commit_optional_keys_index_failure_rolls_back_metadata_and_mappings() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        fill(&storage, Some("A"), b"valid")?;
        let mut writer = storage.begin_fill(
            "tenant|object|Z",
            StoredMeta {
                logical_key: Some(logical("tenant", "object", Some("Z"))?),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"uncommitted")?;
        inject_fault(FaultPoint::PhysicalKeyRegistration);
        // Act.
        let failed = writer.commit();
        // Assert.
        assert!(failed.is_err());
        assert!(INJECTED_FAULT.with(|fault| fault.get().is_none()));
        assert!(storage.peek("tenant|object|Z")?.is_none());
        assert_eq!(
            required(storage.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|A"
        );
        drop(storage);
        let reopened = open(root.path())?;
        assert_eq!(
            required(reopened.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|A"
        );
        Ok(())
    }

    #[test]
    fn general_commit_optional_keys_concurrent_registration_keeps_greatest_owner() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let mut writers = Vec::new();
        for version in ["A", "Z"] {
            let key = format!("tenant|object|{version}");
            let mut writer = storage.begin_fill(
                &key,
                StoredMeta {
                    logical_key: Some(logical("tenant", "object", Some(version))?),
                    ..StoredMeta::default()
                },
            )?;
            writer.write_chunk(version.as_bytes())?;
            writers.push(writer);
        }
        let barrier = Arc::new(std::sync::Barrier::new(2));
        // Act.
        let handles: Vec<_> = writers
            .into_iter()
            .map(|writer| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    writer.commit().map(drop)
                })
            })
            .collect();
        for handle in handles {
            handle
                .join()
                .map_err(|_| Error::storage("commit thread panicked"))??;
        }
        // Assert.
        assert_eq!(
            required(storage.peek_logical(&logical("tenant", "object", None)?)?)?.owner_key,
            "tenant|object|Z"
        );
        assert_eq!(storage.eviction_candidates()?.len(), 2);
        Ok(())
    }

    #[test]
    fn general_recover_optional_keys_current_record_backfills() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        // Exact-only admission is still supported; enabling a policy on reopen
        // reconstructs its logical identity and derived mappings.
        let storage = DiskStorage::open(root.path())?;
        let mut writer = storage.begin_fill(
            "tenant|object|A",
            StoredMeta {
                etag: Some("\"current-v1\"".into()),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"current body")?;
        let lease = writer.commit()?;
        let path = lease.path.clone();
        let revision = lease.meta.revision;
        let inode = ovcc_platform::file_identity(&lease.file)?;
        drop(lease);
        drop(storage);
        // Act.
        let recovered = open(root.path())?;
        let hit = required(recovered.lookup_logical(&logical("tenant", "object", None)?)?)?;
        let record = required(recovered.inner.get_record("tenant|object|A")?)?;
        // Assert.
        assert_eq!(hit.owner_key, "tenant|object|A");
        assert_eq!(body(&hit.value)?, b"current body");
        assert_eq!(hit.value.path, path);
        assert_eq!(hit.value.meta.revision, revision);
        assert_eq!(ovcc_platform::file_identity(&hit.value.file)?, inode);
        assert_eq!(hit.value.body_offset, 0);
        assert_eq!(fs::read(&hit.value.path)?, b"current body");
        assert_eq!(record.physical_keys, ["tenant|object|A", "tenant|object|"]);
        assert_eq!(hit.value.meta.etag.as_deref(), Some("\"current-v1\""));
        let txn = recovered.inner.db.begin_read().map_err(map_redb)?;
        let table = txn.open_table(METADATA).map_err(map_redb)?;
        let bytes = table
            .get("tenant|object|A")
            .map_err(map_redb)?
            .ok_or_else(|| Error::storage("missing recovered record"))?;
        assert_eq!(&bytes.value()[..2], &[9, 0]);
        Ok(())
    }

    #[test]
    fn general_recover_optional_keys_missing_body_cannot_gain_mapping() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let lease = fill(&storage, Some("A"), b"body")?;
        fs::remove_file(&lease.path)?;
        drop(lease);
        drop(storage);
        // Act.
        let recovered = open(root.path())?;
        // Assert.
        assert!(
            recovered
                .peek_logical(&logical("tenant", "object", None)?)?
                .is_none()
        );
        assert!(recovered.eviction_candidates()?.is_empty());
        Ok(())
    }

    #[test]
    fn general_recover_optional_keys_current_segmented_record_keeps_fragments() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = DiskStorage::open(root.path())?;
        let first = crate::tests::stage(root.path(), "a", b"first");
        let second = crate::tests::stage(root.path(), "b", b"second");
        let lease =
            storage.adopt_segmented("tenant|object|A", StoredMeta::default(), &[first, second])?;
        let revision = lease.meta.revision;
        drop(lease);
        let directory = storage.inner.segmented_object_path("tenant|object|A");
        let files = fs::read_dir(&directory)?
            .map(|entry| {
                let path = entry?.path();
                Ok((
                    path.clone(),
                    fs::read(&path)?,
                    ovcc_platform::stat_nofollow(&path)?.identity,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        drop(storage);
        // Act.
        let recovered = open(root.path())?;
        let hit = required(recovered.lookup_logical(&logical("tenant", "object", None)?)?)?;
        // Assert.
        assert_eq!(body(&hit.value)?, b"firstsecond");
        assert_eq!(
            hit.value.segments().map(|segments| segments.count()),
            Some(2)
        );
        assert_eq!(hit.value.meta.revision, revision);
        for (path, bytes, inode) in files {
            assert_eq!(ovcc_platform::stat_nofollow(&path)?.identity, inode);
            assert_eq!(fs::read(&path)?, bytes);
        }
        assert_eq!(recovered.eviction_candidates()?.len(), 1);
        Ok(())
    }

    #[test]
    fn general_get_optional_keys_required_parts_survive_rendered_collision() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let source = logical("tenant", "object|other", Some("A"))?;
        let request = logical("tenant|object", "other", Some("A"))?;
        let key = expand(&TestPolicy, &source)?.remove(0).key;
        assert_eq!(expand(&TestPolicy, &request)?[0].key, key);
        let mut writer = storage.begin_fill(
            key.as_str(),
            StoredMeta {
                logical_key: Some(source),
                ..StoredMeta::default()
            },
        )?;
        writer.write_chunk(b"private")?;
        writer.commit()?;
        // Act.
        let cross_identity = storage.lookup_logical(&request)?;
        // Assert.
        assert!(cross_identity.is_none());
        Ok(())
    }

    #[test]
    fn general_fill_optional_keys_invalid_policy_cannot_replace_entry() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let original = fill(&storage, Some("A"), b"original")?;
        let forged = LogicalKey::new(
            "test.v1".into(),
            vec![
                KeyPart {
                    name: "object".into(),
                    value: Some("object".into()),
                    optional: false,
                },
                KeyPart {
                    name: "tenant".into(),
                    value: Some("tenant".into()),
                    optional: true,
                },
                KeyPart {
                    name: "variant".into(),
                    value: Some("A".into()),
                    optional: true,
                },
            ],
        )
        .map_err(|e| Error::storage(e.to_string()))?;
        // Act.
        let attempt = storage.begin_fill(
            "tenant|object|A",
            StoredMeta {
                logical_key: Some(forged),
                ..StoredMeta::default()
            },
        );
        // Assert.
        assert!(attempt.is_err());
        let hit = required(storage.lookup_logical(&logical("tenant", "object", None)?)?)?;
        assert_eq!(hit.value.meta.revision, original.meta.revision);
        assert_eq!(body(&hit.value)?, b"original");
        Ok(())
    }

    #[test]
    fn general_fill_optional_keys_reconstructed_identity_matches_committed_lease() -> Result<()> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let storage = open(root.path())?;
        let mut writer = storage.begin_fill("tenant|object|A", StoredMeta::default())?;
        writer.write_chunk(b"body")?;
        // Act.
        let lease = writer.commit()?;
        let durable = required(storage.peek("tenant|object|A")?)?;
        // Assert.
        assert_eq!(lease.meta, durable);
        assert_eq!(
            lease.meta.logical_key,
            Some(logical("tenant", "object", Some("A"))?)
        );
        Ok(())
    }

    #[test]
    fn general_get_optional_keys_observation_to_open_races_become_misses() -> Result<()> {
        // Arrange.
        for replacement in [false, true] {
            let root = tempfile::tempdir()?;
            let storage = open(root.path())?;
            fill(&storage, Some("A"), b"old")?;
            let observed = required(storage.peek_logical(&logical("tenant", "object", None)?)?)?;
            // Act. Deterministically schedules retirement between the two
            // production stages of lookup_logical, without a sleep or fake DB.
            if replacement {
                fill(&storage, Some("A"), b"new")?;
            } else {
                assert!(storage.evict_data("tenant|object|A", observed.value.revision)?);
            }
            let stale = storage.open_logical_match(observed)?;
            // Assert.
            assert!(stale.is_none());
        }
        Ok(())
    }

    #[test]
    fn general_model_optional_keys_generated_traces_conform_to_storage()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use serde_json::Value;
        type ModelResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
        struct Replay {
            root: tempfile::TempDir,
            storage: Option<DiskStorage>,
            writers: Vec<Option<Box<dyn StoreWrite>>>,
            revisions: [[u64; 3]; 2],
            retired: Option<MetadataRecord>,
        }
        impl Replay {
            fn storage(&self) -> ModelResult<&DiskStorage> {
                self.storage
                    .as_ref()
                    .ok_or_else(|| "model process is stopped".into())
            }
            fn key(owner: usize) -> &'static str {
                if owner == 0 {
                    "tenant|object|A"
                } else {
                    "tenant|object|B"
                }
            }
            fn execute(&mut self, action: &Value, expected: &Value) -> ModelResult<()> {
                let name = action["name"].as_str().ok_or("missing action name")?;
                let owner = action["args"][0].as_u64().unwrap_or(1) as usize - 1;
                match name {
                    "Begin" => {
                        self.retired = self.storage()?.inner.get_record(Self::key(owner))?;
                        let variant = if owner == 0 { "A" } else { "B" };
                        self.writers[owner] = Some(self.storage()?.begin_fill(
                            Self::key(owner),
                            StoredMeta {
                                logical_key: Some(logical("tenant", "object", Some(variant))?),
                                ..StoredMeta::default()
                            },
                        )?);
                        let record = required(self.storage()?.inner.get_record(Self::key(owner))?)?;
                        let model_revision = expected["revision"][owner]
                            .as_u64()
                            .ok_or("missing revision")?
                            as usize;
                        self.revisions[owner][model_revision] = record.generation;
                    }
                    "Publish" => {
                        // Model-only sub-phase: bytes are staged here. Actual
                        // durable publication remains inside production commit.
                        self.writers[owner]
                            .as_mut()
                            .ok_or("missing writer")?
                            .write_chunk(b"x")?;
                    }
                    "Commit" => {
                        self.writers[owner]
                            .take()
                            .ok_or("missing writer")?
                            .commit()?;
                    }
                    "Abort" => {
                        self.writers[owner].take().ok_or("missing writer")?.abort();
                    }
                    "Evict" => {
                        self.retired = self.storage()?.inner.get_record(Self::key(owner))?;
                        let revision = self
                            .retired
                            .as_ref()
                            .map(|record| record.generation)
                            .unwrap_or_default();
                        self.storage()?.evict_data(Self::key(owner), revision)?;
                    }
                    "StaleCleanup" => {
                        if let Some(record) = self.retired.take() {
                            let txn = self.storage()?.inner.db.begin_write()?;
                            self.storage()?.inner.remove_physical_keys(&txn, &record)?;
                            txn.commit()?;
                        }
                    }
                    "Restart" => {
                        for writer in &mut self.writers {
                            if let Some(writer) = writer.take() {
                                writer.simulate_crash();
                            }
                        }
                        drop(self.storage.take());
                    }
                    "StartRecovery" | "RecoverEntry" => {
                        // Public open executes all recovery phases together.
                        // Intermediate batch faults have a separate real witness.
                    }
                    "Ready" => {
                        self.storage = Some(open(self.root.path())?);
                        self.retired = None;
                    }
                    "Lookup" => {
                        let args = action["args"].as_array().ok_or("missing lookup args")?;
                        let mut served = (0, 0);
                        if args[1] == true && args[2] == true {
                            let tenant = if args[3] == true { "tenant" } else { "other" };
                            let variant = match owner {
                                0 => Some("A"),
                                1 => Some("B"),
                                _ => None,
                            };
                            if let Some(hit) = self
                                .storage()?
                                .lookup_logical(&logical(tenant, "object", variant)?)?
                            {
                                served.0 = if hit.owner_key == Self::key(0) { 1 } else { 2 };
                                served.1 = hit.value.meta.revision;
                                assert_eq!(body(&hit.value)?, b"x");
                            }
                        }
                        let expected_owner = expected["served"][0]
                            .as_u64()
                            .ok_or("missing served owner")?
                            as usize;
                        assert_eq!(served.0, expected_owner);
                        if expected_owner > 0 {
                            let rev = expected["served"][1]
                                .as_u64()
                                .ok_or("missing served revision")?
                                as usize;
                            assert_eq!(served.1, self.revisions[expected_owner - 1][rev]);
                        }
                    }
                    _ => return Err(format!("unmapped action {name}").into()),
                }
                if expected["online"] == false || expected["rebuilding"] == true {
                    return Ok(());
                }
                let storage = self.storage()?;
                for owner in 0..2 {
                    let record = storage.inner.get_record(Self::key(owner))?;
                    let actual = match record.as_ref().map(|r| r.state) {
                        Some(CacheState::Cached) => "Cached",
                        Some(CacheState::FetchInProgress) => "Filling",
                        _ => "Absent", // History-only metadata is outside this model.
                    };
                    assert_eq!(actual, expected["state"][owner], "after {name}");
                }
                let txn = storage.inner.db.begin_read()?;
                let table = txn.open_table(PHYSICAL_KEYS)?;
                for (i, key) in [Self::key(0), Self::key(1), "tenant|object|"]
                    .into_iter()
                    .enumerate()
                {
                    let owner = table
                        .get(key)?
                        .map(|v| decode_owner(v.value()))
                        .transpose()?;
                    let expected_owner = expected["index"][i][0]
                        .as_u64()
                        .ok_or("missing index owner")?
                        as usize;
                    match owner {
                        None => assert_eq!(expected_owner, 0, "after {name}: {key}"),
                        Some(owner) => {
                            assert!(expected_owner > 0, "unexpected mapping after {name}");
                            assert_eq!(owner.key, Self::key(expected_owner - 1));
                            let rev = expected["index"][i][1]
                                .as_u64()
                                .ok_or("missing index revision")?
                                as usize;
                            assert_eq!(owner.revision, self.revisions[expected_owner - 1][rev]);
                        }
                    }
                }
                Ok(())
            }
        }

        // Arrange. The owning storage skill permits transition-covering replay
        // separately from complete raw-edge accounting. Every chosen scenario
        // starts at Init; source states are never synthesized.
        let graph: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-storage-state-machine/fixtures/OptionalKeyParts.graph.json"
        ))?;
        let scenarios: Value = serde_json::from_str(include_str!(
            "../skills/ovcc-storage-state-machine/fixtures/OptionalKeyParts.scenarios.json"
        ))?;
        let nodes: HashMap<_, _> = graph["nodes"]
            .as_array()
            .ok_or("nodes")?
            .iter()
            .map(|n| (n["id"].as_u64(), &n["state"]))
            .collect();
        let edges: HashMap<_, _> = graph["edges"]
            .as_array()
            .ok_or("edges")?
            .iter()
            .map(|e| (e["id"].as_u64(), e))
            .collect();
        let mut covered = HashSet::new();
        for action_name in [
            "Begin",
            "Commit",
            "Abort",
            "Evict",
            "StaleCleanup",
            "Ready",
            "Lookup",
        ] {
            let selected = scenarios["scenarios"]
                .as_array()
                .ok_or("scenarios")?
                .iter()
                .find(|scenario| {
                    let Some(trace) = scenario["trace_edge_ids"].as_array() else {
                        return false;
                    };
                    let Some(last) = trace.last().and_then(|id| edges.get(&id.as_u64())) else {
                        return false;
                    };
                    if last["action"]["name"] != action_name {
                        return false;
                    }
                    let source = nodes[&last["from"].as_u64()];
                    let target = nodes[&last["to"].as_u64()];
                    if action_name == "Lookup" && target["served"][0] == 0 {
                        return false;
                    }
                    if action_name == "StaleCleanup" && source["retired"][1] == 0 {
                        return false;
                    }
                    // Backfill interruption is exercised by the consumed
                    // BackfillBatch witness, not by this public-API adapter.
                    trace
                        .iter()
                        .all(|id| edges[&id.as_u64()]["action"]["name"] != "Crash")
                })
                .ok_or_else(|| format!("no executable generated trace for {action_name}"))?;
            let root = tempfile::tempdir()?;
            let storage = open(root.path())?;
            let mut replay = Replay {
                root,
                storage: Some(storage),
                writers: vec![None, None],
                revisions: [[0; 3]; 2],
                retired: None,
            };
            let mut node = graph["initial"].as_u64();
            // Act.
            for id in selected["trace_edge_ids"].as_array().ok_or("trace")? {
                let edge = edges[&id.as_u64()];
                assert_eq!(node, edge["from"].as_u64(), "trace edge reordered");
                node = edge["to"].as_u64();
                replay.execute(&edge["action"], nodes[&node])?;
            }
            covered.insert(action_name);
        }
        // Assert. This is seven generated transition-covering traces, not a
        // claim that every mapped production representative was executed.
        assert_eq!(covered.len(), 7);
        assert_eq!(scenarios["mapped_edge_count"], scenarios["raw_edge_count"]);
        Ok(())
    }
}
