// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use ovdc_bytes::keys;
use ovdc_trace::{
    label,
    metrics::{timer, Measure},
};
use rocksdb::{ReadOptions, WriteBatchWithTransaction};

use crate::{
    adapter, environment::rocks_adapter::rdb_perf, metadata::Metadata, now_ms, AnyEntry,
    BlobProvider, Cursor, Entry, Error, MutCursor,
};

use super::{
    cache::Cache,
    merge,
    metrics::{self},
    value::{self, higher_order_key, stored_kind_into_entry, Invalid, StoredKind},
    write_semaphore, Rocks, RocksDb, DEFAULT_CF,
};

/// How many times a GC write backs off before the key is left for the next
/// pass.
///
/// Deliberately small. If the engine is stalling, hammering one key does not
/// help -- the scan visits thousands of them, so a long per-key wait turns into
/// a scan that never finishes, and the scan is what relieves the stall.
pub(crate) const MAX_BUSY_RETRIES: usize = 3;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum DeepestLevel {
    Tier1,
    Tier2,
}

#[repr(transparent)]
pub struct RocksCursor {
    db: Rocks,
}

impl Cursor for RocksCursor {
    #[tracing::instrument(level = "trace", skip(self))]
    fn get(&self, key: ovdc_bytes::keys::Compound) -> Result<AnyEntry, crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        get_key_as_entry(
            &self.db.inner,
            &self.db.cache,
            &cf,
            &key,
            DeepestLevel::Tier2,
        )
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn get_non_blocking(&self, key: keys::Compound) -> Result<AnyEntry, crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        get_key_as_entry(
            &self.db.inner,
            &self.db.cache,
            &cf,
            &key,
            DeepestLevel::Tier1,
        )
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn exists(&self, key: keys::Compound) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        get_key_as_entry(
            &self.db.inner,
            &self.db.cache,
            &cf,
            &key,
            DeepestLevel::Tier2,
        )
        .map(|_| ())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn exists_non_blocking(&self, key: keys::Compound) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        get_key_as_entry(
            &self.db.inner,
            &self.db.cache,
            &cf,
            &key,
            DeepestLevel::Tier1,
        )
        .map(|_| ())
    }
}

impl MutCursor for RocksCursor {
    #[tracing::instrument(level = "trace", skip(self))]
    fn delete(&self, key: keys::Compound) -> Result<(), crate::Error> {
        if !write_semaphore::try_write(&self.db.writers) {
            return Err(crate::Error::Busy);
        }
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        let result = delete_key(&self.db.inner, &self.db.cache, &cf, &key);

        write_semaphore::decr_writer(&self.db.writers);

        result
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn update(&self, key: keys::Compound, mut_meta: adapter::Update) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        update_many(&self.db.inner, &cf, std::iter::once((key, mut_meta)))
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn update_many(&self, iter: Vec<(keys::Compound, crate::Update)>) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        update_many(&self.db.inner, &cf, iter.into_iter())
    }

    #[tracing::instrument(level = "trace", skip(self, metadata, value))]
    fn put(
        &self,
        key: keys::Compound,
        metadata: Metadata,
        value: &[u8],
    ) -> Result<(), crate::Error> {
        if metadata.is_expired(now_ms()) {
            return Ok(());
        }

        if !write_semaphore::try_write(&self.db.writers) {
            return Err(crate::Error::Busy);
        }

        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        let result = put_key_as_kind(
            &self.db.inner,
            &self.db.cache,
            &cf,
            &key,
            StoredKind::KeyValue(key, metadata, value.into()),
            0,
        );

        write_semaphore::decr_writer(&self.db.writers);

        result
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn retain(
        &self,
        with: Box<dyn Fn(&dyn Entry) -> bool + Send + Sync + 'static>,
    ) -> Result<(), crate::adapter::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();

        let mut perf = rdb_perf::rdb_thread_perf_set();

        let mut iter = iterator(&self.db.inner, &cf);
        iter.seek_to_first();

        let mut scanned = 0;
        let mut deleted = 0;

        // Iterate all entries
        loop {
            if !iter.valid() {
                break;
            }
            scanned += 1;

            let Some(key) = iter
                .key()
                .and_then(|y| Some(keys::Compound::new_exact(y.try_into().ok()?)))
            else {
                tracing::error!(key = ?iter.key(), "key is not valid");
                iter.next();
                continue;
            };

            let (kind, invalid) = value::entry(key, iter.value().unwrap());

            let mut delete = false;
            let mut del_parts = None;

            // Tombstones are unreadable rows: either the empty placeholder the
            // merge operator must return for a key that did not exist, or a
            // value that failed to parse. Neither holds recoverable data, and
            // nothing else reclaims them, so collect them here.
            if invalid.contains(Invalid::EXPIRED) || invalid.contains(Invalid::TOMBSTONE) {
                delete = true;
                del_parts = match &kind {
                    StoredKind::KeyValue(..) => None,
                    StoredKind::Part { random, .. } => Some(*random),
                    StoredKind::PartPreamble { random, .. } => Some(*random),
                };
            }

            let next = match kind {
                StoredKind::Part { key, .. } => {
                    iter.seek(higher_order_key(&key));
                    continue;
                }
                StoredKind::PartPreamble { key, random, .. } => {
                    del_parts = Some(random);
                    let next = stored_kind_into_entry(key, kind);
                    iter.seek(higher_order_key(&key));
                    next
                }
                StoredKind::KeyValue(key, ..) => {
                    let next = stored_kind_into_entry(key, kind);
                    iter.next();
                    next
                }
            };

            if delete || (invalid.is_empty() && !with(&next as &dyn Entry)) {
                deleted += 1;

                // Back off and retry rather than dropping the key -- but
                // bounded. `remove` and `delete` both write with `no_slowdown`,
                // so a stall answers `Busy` on every attempt, and this scan is
                // what relieves the pressure causing it: spinning here would
                // wedge GC exactly when it is needed. Giving up costs one pass,
                // since the next scan sees the same key and repeats the work.
                //
                // The parts go first, and the row only once they are gone. A
                // bounded retry can stop between the two, so the order picks
                // which leftover that produces -- and only one of the two ever
                // heals.
                //
                // Parts with no row are the ones to avoid: this scan walks rows,
                // and a bare `StoredKind::Part` is stepped over by the
                // `seek(higher_order_key(..))` above, so once the row naming
                // them is gone nothing visits them again and the disk they hold
                // is lost for the life of the store -- at exactly the moment GC
                // is running because that disk is scarce.
                //
                // A row whose parts are gone is the better leftover. It reads as
                // present and cannot produce its bytes, which is a real fault,
                // but the row is already condemned: the next scan reaches the
                // same verdict and finishes the delete, so the window closes on
                // its own. `log::cursor::retain` orders it the same way, for the
                // same reason.
                let mut parts_deleted = del_parts.is_none();

                for attempt in 0..MAX_BUSY_RETRIES {
                    let last = attempt + 1 == MAX_BUSY_RETRIES;

                    if let Some(tag) = del_parts.filter(|_| !parts_deleted) {
                        match self.db.blobs().remove(key, tag) {
                            Err(crate::Error::Busy) if last => {
                                tracing::error!(
                                    "gave up removing the blob for {key} after \
                                     {MAX_BUSY_RETRIES} attempts, leaving the row for a later pass"
                                );
                                break;
                            }
                            Err(crate::Error::Busy) => {
                                std::thread::sleep(Duration::from_millis(10));
                                continue;
                            }
                            Err(e) => {
                                // Not back-pressure, so waiting will not clear
                                // it. Leaving the row keeps the pair together
                                // for a later pass rather than stranding the
                                // parts behind a row that is already gone.
                                tracing::error!("failed to remove blob for {key}: {e}");
                                break;
                            }
                            Ok(()) => parts_deleted = true,
                        }
                    }

                    match self.delete(key) {
                        Err(crate::Error::Busy) if last => {
                            tracing::error!(
                                "gave up removing key {key} after {MAX_BUSY_RETRIES} attempts"
                            );
                            break;
                        }
                        Err(crate::Error::Busy) => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        other => {
                            if other.is_err() {
                                ovdc_trace::drop!(other, "could not remove key {key}");
                            }
                            break;
                        }
                    }
                }
            }

            if scanned % 16 == 0 {
                iter = iterator(&self.db.inner, &cf);
                iter.seek(key);
                if iter.key().map(|y| y == key.as_slice()).unwrap_or_default() {
                    iter.next();
                }

                rdb_perf::report_retains_io_perf(&mut perf);
            }
        }
        drop(iter);

        rdb_perf::report_retains_io_perf(&mut perf);

        if deleted > 0 {
            tracing::info!(scanned, deleted, "removed");

            // Make sure the deletion markers are flushed.
            self.db.inner.flush()?;
            self.db
                .inner
                .compact_range_cf::<&[u8], &[u8]>(&cf, None, None);
        }

        rdb_perf::report_retains_io_perf(&mut perf);

        /*
        The cache is allowed to serve any version of data for a key, so it is OK to not invalidate.
        This may not always be true.
        if let Some(cache) = self.db.cache.as_ref() {
            cache.invalidate_all();
        }
        */

        Ok(())
    }
}

pub(crate) fn iterator<'a>(
    db: &'a RocksDb,
    cf: &impl rocksdb::AsColumnFamilyRef,
) -> rocksdb::DBRawIteratorWithThreadMode<'a, RocksDb> {
    let mut options = ReadOptions::default();
    // Don't fill the block cache with nonsense
    options.fill_cache(false);
    // Allow rocks to async read data
    options.set_async_io(true);

    db.raw_iterator_cf_opt(cf, options)
}

/// The write options for the calling thread.
///
/// `no_slowdown` is set either way, so a write that would stall returns
/// `Incomplete` -- mapped to [`crate::Error::Busy`] -- instead of blocking the
/// caller. The WAL is disabled unless the caller asked for higher durability
/// via [`crate::thread_local_flags`].
pub(super) fn thread_write_options() -> &'static rocksdb::WriteOptions {
    match crate::get_thread_local_flags() & crate::FLAGS_HIGHER_DURABILITY != 0 {
        false => &super::WRITE_NO_SLOWDOWN_OR_WAL,
        true => &super::WRITE_NO_SLOWDOWN,
    }
}

fn raw_value_into_entry(key: keys::Compound, value: &[u8]) -> Result<AnyEntry, Error> {
    let (inner, invalid) = value::entry(key, value);

    if invalid.contains(Invalid::UNFILLED_BLOB) {
        tracing::trace!(?key, "cannot return blob incomplete");
        return Err(crate::Error::WriteInProgress);
    }

    if !invalid.is_empty() {
        tracing::trace!(?key, "cannot return: invalid");
        return Err(crate::Error::NotFound);
    }

    let entry = stored_kind_into_entry(key, inner);
    tracing::trace!(?key, "good value");

    Ok(entry)
}

pub(crate) fn get_key_as_entry(
    db: &RocksDb,
    cache: &Cache,
    cf: &impl rocksdb::AsColumnFamilyRef,
    key: &keys::Compound,
    level: DeepestLevel,
) -> Result<AnyEntry, Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_READ.clone());

    tracing::trace!(?key, "get");

    // First try the cache...
    if let Some(value) = cache.get(key) {
        tracing::trace!(?key, "using cached key");
        return Ok(value);
    }

    let value = get_key(db, cf, key, level)?;

    let entry = match raw_value_into_entry(*key, &value) {
        Ok(entry) => entry,
        Err(err) => {
            tracing::trace!(?key, "cannot return: {err:?}");
            return Err(err);
        }
    };

    cache.insert(*key, entry.clone());

    Ok(entry)
}

label! {
    static LABELS_LEVEL0 = ["level": 0; 1];
    static LABELS_LEVEL1 = ["level": 1; 1];
    static LABELS_LEVEL2 = ["level": 2; 1];
}

pub(crate) fn get_key(
    db: &RocksDb,
    cf: &impl rocksdb::AsColumnFamilyRef,
    key: &keys::Compound,
    level: DeepestLevel,
) -> Result<Vec<u8>, Error> {
    tracing::trace!(?key, "get");

    let (_t, labels, options) = match level {
        DeepestLevel::Tier1 => {
            let mut options = ReadOptions::default();
            options.set_read_tier(rocksdb::ReadTier::BlockCache);
            let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_READ_TIER1.clone());
            (_t, &*LABELS_LEVEL1, options)
        }
        DeepestLevel::Tier2 => {
            let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_READ_TIER2.clone());
            (_t, &*LABELS_LEVEL2, ReadOptions::default())
        }
    };

    let perf = rdb_perf::rdb_thread_perf_set();

    metrics::QUERY.measure(1, labels);
    let value = db
        .get_cf_opt(cf, key.as_slice(), &options)?
        .ok_or_else(|| {
            tracing::trace!(?key, "value is None");
            metrics::MISS.measure(1, labels);
            crate::Error::NotFound
        })?;

    metrics::GET_BYTES.measure(value.len() as _, labels);
    metrics::ANSWER.measure(1, labels);
    rdb_perf::rdb_thread_perf_end();
    rdb_perf::report_get_io_perf(&perf.report(true));

    tracing::trace!(?key, "value found");

    Ok(value)
}

fn put_key_as_kind(
    db: &RocksDb,
    cache: &Cache,
    cf: &impl rocksdb::AsColumnFamilyRef,
    key: &keys::Compound,
    kind: StoredKind,
    flags: u16,
) -> Result<(), Error> {
    let value = value::format(flags, &kind);
    put_key(db, cf, key, &value)?;

    cache.insert(*key, stored_kind_into_entry(*key, kind));

    Ok(())
}

fn put_key(
    db: &RocksDb,
    cf: &impl rocksdb::AsColumnFamilyRef,
    key: &keys::Compound,
    value: &[u8],
) -> Result<(), Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_WRITE.clone());
    tracing::trace!(?key, "put");

    let perf = rdb_perf::rdb_thread_perf_set();

    let opts = thread_write_options();
    db.put_cf_opt(cf, key.as_slice(), value, opts)?;

    rdb_perf::rdb_thread_perf_end();
    rdb_perf::report_put_io_perf(&perf.report(true));
    let elapsed = _t.stop();

    if elapsed.as_millis() > 1 {
        let w_perf = rdb_perf::RdbWriteTime::from(&perf);
        if (w_perf.write_wait + w_perf.mutex_acquire).as_millis() > 1 {
            tracing::warn!(
                write_wait = ?{ w_perf.write_wait },
                mutex_acquire = ?{ w_perf.mutex_acquire },
                "write thread contended for more than 1ms"
            );
        }

        if w_perf.delay.as_nanos() > 0 {
            tracing::warn!(
                delay = ?{ w_perf.delay },
                "rocksdb has delayed writes"
            );
        }

        tracing::debug!(?w_perf, "time");
    }

    tracing::trace!(?key, "put OK");

    metrics::PUT.measure(1, &[]);
    metrics::PUT_BYTES.measure(value.len() as u64, &[]);
    metrics::PUT_BYTE_HIST.measure(value.len() as u64, &[]);

    Ok(())
}

fn delete_key(
    db: &RocksDb,
    cache: &Cache,
    cf: &impl rocksdb::AsColumnFamilyRef,
    key: &keys::Compound,
) -> Result<(), Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_DELETE.clone());

    tracing::trace!(?key, "delete");

    let opts = thread_write_options();
    db.delete_cf_opt(cf, key.as_slice(), opts)?;
    cache.invalidate(key);

    Ok(())
}

fn update_many(
    db: &RocksDb,
    cf: &impl rocksdb::AsColumnFamilyRef,
    iter: impl Iterator<Item = (keys::Compound, crate::Update)>,
) -> Result<(), crate::Error> {
    let mut batch = WriteBatchWithTransaction::<false>::default();

    tracing::trace!("update many");

    for (key, mut_meta) in iter {
        tracing::trace!(?key, "update many");

        batch.merge_cf(
            cf,
            key.as_slice(),
            merge::Merge::UpdateMetadata(mut_meta).to_vec(),
        );

        metrics::METADATA_UPDATE.measure(1, &[]);
    }

    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_MERGE.clone());

    let opts = thread_write_options();
    db.write_opt(batch, opts)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use ovdc_bytes::keys;

    use crate::{
        environment::rocks_adapter::value::{StoredKind, ValueKind},
        metadata::Metadata,
        tests::temp_db,
        Entry, Value,
    };

    #[test]
    fn test_simple_put_get() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let value = &[1, 2, 3];

        super::put_key(&db.inner, &cf, &key, &value[..]).expect("unable to put new value in map");

        let get_value = super::get_key(&db.inner, &cf, &key, super::DeepestLevel::Tier2)
            .expect("unable to get value from map");
        assert_eq!(get_value, value);
    }

    #[test]
    fn test_simple_put_delete() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let value = &[1, 2, 3];

        super::put_key(&db.inner, &cf, &key, &value[..]).expect("unable to put new value in map");

        super::delete_key(&db.inner, &db.cache, &cf, &key)
            .expect("unable to delete value from map");

        assert!(super::get_key(&db.inner, &cf, &key, super::DeepestLevel::Tier2).is_err());
    }

    #[test]
    fn test_put_get_key_value() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let value = &[1, 2, 3];

        super::put_key_as_kind(
            &db.inner,
            &db.cache,
            &cf,
            &key,
            StoredKind::KeyValue(key, Metadata::default(), ValueKind::Slice(&value[..])),
            0,
        )
        .expect("unable to put new value in map");

        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier1)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Vec(..)));

        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier2)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Vec(..)));
    }

    #[test]
    fn test_put_get_part_preamble() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));

        super::put_key_as_kind(
            &db.inner,
            &db.cache,
            &cf,
            &key,
            StoredKind::PartPreamble {
                key,
                metadata: Metadata::default(),
                size: 123,
                parts: 0,
                random: 123,
            },
            0,
        )
        .expect("unable to put new value in map");

        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier2)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Blob { size: 123, .. }));
    }

    #[test]
    fn test_put_get_part() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));

        super::put_key_as_kind(
            &db.inner,
            &db.cache,
            &cf,
            &key,
            StoredKind::Part {
                key,
                value: ValueKind::Slice(&[3, 4]),
                random: 123,
            },
            0,
        )
        .expect("unable to put new value in map");

        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier2)
                .expect("unable to get value from map");
        assert!(matches!(
            entry.value(),
            Value::Blob {
                value: Some(..),
                ..
            }
        ));
    }

    #[test]
    fn test_entry_is_cached_after_get() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let value = &[1, 2, 3];

        super::put_key_as_kind(
            &db.inner,
            &db.cache,
            &cf,
            &key,
            StoredKind::KeyValue(key, Metadata::default(), ValueKind::Slice(&value[..])),
            0,
        )
        .expect("unable to put new value in map");

        assert!(db.cache.get(&key).is_some());

        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier1)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Vec(..)));

        db.cache.invalidate(&key);
        assert!(db.cache.get(&key).is_none());

        assert!(super::get_key_as_entry(
            &db.inner,
            &db.cache,
            &cf,
            &key,
            super::DeepestLevel::Tier1
        )
        .is_ok());
        assert!(db.cache.get(&key).is_some());

        db.cache.invalidate(&key);
        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier1)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Vec(..)));

        db.cache.invalidate(&key);
        let entry =
            super::get_key_as_entry(&db.inner, &db.cache, &cf, &key, super::DeepestLevel::Tier1)
                .expect("unable to get value from map");
        assert!(matches!(entry.value(), Value::Vec(..)));
    }
}
