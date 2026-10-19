// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{sync::Arc, time::Duration};

use ovdc_bytes::keys;
use ovdc_trace::metrics::{timer, Measure};
use rocksdb::WriteBatch;

use crate::{
    adapter,
    environment::{
        rocks_adapter::{
            adapt::{self, DeepestLevel},
            cache::Cache,
            log::{self},
            metrics::{self},
            rdb_perf, RocksDb, DEFAULT_CF,
        },
        Rocks,
    },
    metadata::Metadata,
    now_ms, AnyEntry, BlobProvider, Change, Cursor, Entry, Error, MutCursor, Value, BLOBS_CF,
};

use super::{
    bits::{self, common},
    models::{self, KIND_ACCESS, KIND_DROP, KIND_METADATA},
    rollup, snowflake,
};

use adapt::MAX_BUSY_RETRIES;

/// How long an unfinished upload is left alone before GC reclaims it.
///
/// Only reached when the guard could not clean up -- refused under a stall, or
/// the process died holding the upload -- so it never needs to be tight. It is
/// also the only thing separating an abandoned upload from one still running:
/// both leave nothing but a `Created` record, so an upload that outlives this
/// has its parts reclaimed underneath it. Set well above the longest upload the
/// deployment expects, not merely above the request timeout.
#[cfg(not(test))]
const ABANDONED_UPLOAD_GRACE: Duration = Duration::from_secs(60 * 60 * 8);

/// Shortened under test so the reclamation path can be exercised without
/// waiting hours. Nothing else protects a live upload, so a test that holds one
/// open across a `retain` for longer than this will have its parts reclaimed.
#[cfg(test)]
const ABANDONED_UPLOAD_GRACE: Duration = Duration::from_millis(100);

/// Writes the records one `update` turns into, under a single tag.
///
/// Access time and ttl travel separately: access time is what a read changes,
/// and as an [`models::Access`] record any number of them fold into one, while a
/// ttl change is rare and stays a metadata change. A request setting both emits
/// both, which is why they share a tag -- the reader treats records under one
/// tag as a single write.
fn write_update(update: adapter::Update, tag: u128, buff: &mut bits::ArrayBuf) {
    if let Change::Set(latest) = update.access_time {
        bits::write(KIND_ACCESS, tag, &models::Access { latest }, buff);
    }

    if let Change::Set(_) = update.ttl_ms {
        bits::write(
            KIND_METADATA,
            tag,
            &models::Metadata::Change(models::Modifications {
                ttl_ms: update.ttl_ms,
            }),
            buff,
        );
    }
}

#[repr(transparent)]
pub struct RocksCursor {
    pub(super) db: Rocks,
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
            adapt::DeepestLevel::Tier2,
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
            adapt::DeepestLevel::Tier1,
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
            adapt::DeepestLevel::Tier2,
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
            adapt::DeepestLevel::Tier1,
        )
        .map(|_| ())
    }
}

impl MutCursor for RocksCursor {
    #[tracing::instrument(level = "trace", skip(self))]
    fn delete(&self, key: keys::Compound) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();

        let mut buff = bits::ArrayBuf::new();
        bits::write(KIND_DROP, snowflake::next(), &models::DropBefore, &mut buff);
        self.db
            .inner
            .merge_cf_opt(&cf, key, buff.as_slice(), adapt::thread_write_options())?;
        self.db.cache.invalidate(&key);

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn update(&self, key: keys::Compound, mut_meta: adapter::Update) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        let mut buff = bits::ArrayBuf::new();
        write_update(mut_meta, snowflake::next(), &mut buff);

        self.db
            .inner
            .merge_cf_opt(&cf, key, buff.as_slice(), adapt::thread_write_options())?;
        self.db.cache.invalidate(&key);

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn update_many(&self, iter: Vec<(keys::Compound, crate::Update)>) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();

        let mut batch = WriteBatch::default();
        let tag = snowflake::next();

        for (key, mut_meta) in &iter {
            let mut buff = bits::ArrayBuf::new();
            write_update(*mut_meta, tag, &mut buff);
            batch.merge_cf(&cf, key, buff.as_slice());
        }

        self.db
            .inner
            .write_opt(batch, adapt::thread_write_options())?;

        for (key, _) in iter {
            self.db.cache.invalidate(&key);
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, metadata, value))]
    fn put(
        &self,
        key: keys::Compound,
        metadata: Metadata,
        value: &[u8],
    ) -> Result<(), crate::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        let mut buff = bits::ArrayBuf::new();
        common::log_inline_value_change(metadata, value, &mut buff);
        self.db
            .inner
            .merge_cf_opt(&cf, key, buff.as_slice(), adapt::thread_write_options())?;
        self.db.cache.invalidate(&key);

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn retain(
        &self,
        with: Box<dyn Fn(&dyn Entry) -> bool + Send + Sync + 'static>,
    ) -> Result<(), crate::adapter::Error> {
        let cf = self.db.inner.cf_handle(DEFAULT_CF).unwrap();
        let blobs_cf = self.db.inner.cf_handle(BLOBS_CF).unwrap();

        let mut perf = rdb_perf::rdb_thread_perf_set();

        let mut iter = adapt::iterator(&self.db.inner, &cf);
        let tag_for_any_updates = log::snowflake::next();
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
            let value = iter.value().unwrap();

            let latest = rollup::latest(value);

            let mut update_buf = bits::ArrayBuf::new();

            for blob_id in rollup::stale_blobs(value, ABANDONED_UPLOAD_GRACE) {
                // Only record the removal once it has actually happened.
                // Writing Deleted for parts still on disk would orphan them
                // permanently: nothing revisits a blob the log says is gone.
                // Leaving the blob as it is means the next pass retries it,
                // which is the right response to backpressure -- and GC runs
                // precisely when the engine is under write pressure.
                match self.db.blobs().remove(key, blob_id) {
                    Ok(()) => bits::write(
                        models::KIND_BLOB,
                        tag_for_any_updates,
                        &models::BlobChange::Deleted { id: blob_id },
                        &mut update_buf,
                    ),
                    Err(e) => {
                        tracing::warn!("could not remove blob for {key}, will retry: {e}")
                    }
                }
            }

            let expired = latest.is_expired();
            let rollup::Latest {
                inline_value,
                blob_value,
                metadata,
                ..
            } = latest;

            // Metadata is what makes a value readable -- `get_key_as_entry`
            // answers `NotFound` without it, because everything a caller is
            // handed lives there. It can be missing even for a value that
            // decoded: record decoding is deliberately tolerant, skipping one
            // that does not validate rather than aborting the process from a
            // background thread, which can leave a value with nothing
            // describing it.
            //
            // So the two are the same row to a reader, and both are dropped: an
            // expired entry, and one no reader could ever return. Asking the
            // predicate about it is not possible -- there is no entry to pass --
            // and leaving it costs a row that nothing will collect later.
            let any_entry = match metadata.filter(|_| !expired) {
                None => {
                    // Only when there is something to suppress. A row already
                    // holding no value is empty, and a drop record would add
                    // bytes to it rather than reclaim any.
                    if expired || inline_value.is_some() || blob_value.is_some() {
                        bits::write(
                            models::KIND_DROP,
                            tag_for_any_updates,
                            &models::DropBefore,
                            &mut update_buf,
                        );
                    }
                    None
                }
                Some(metadata) => {
                    if let Some(inline_value) = inline_value {
                        Some(AnyEntry {
                            key,
                            value: Value::Vec(Arc::new(inline_value.inner)),
                            metadata,
                        })
                    } else if let Some((tag, size)) = blob_value {
                        Some(AnyEntry {
                            key,
                            value: Value::Blob {
                                size,
                                next: key,
                                tag,
                                value: None,
                            },
                            metadata,
                        })
                    } else {
                        None
                    }
                }
            };

            if let Some(entry) = any_entry {
                if !with(&entry) {
                    deleted += 1;

                    // Log the fact that the entry should be removed.
                    bits::write(
                        models::KIND_DROP,
                        tag_for_any_updates,
                        &models::DropBefore,
                        &mut update_buf,
                    );
                }
            }

            if !update_buf.as_slice().is_empty() {
                // Now that this write can stall rather than block, back off and
                // retry rather than dropping the update: it records blob parts
                // already removed above, so losing it would orphan them.
                //
                // Bounded, because this scan is what relieves the pressure that
                // causes the stall -- spinning here until it clears would wedge
                // GC exactly when it is needed. Giving up costs one pass: the
                // next scan sees the same key and repeats the work.
                for attempt in 0..MAX_BUSY_RETRIES {
                    match self
                        .db
                        .inner
                        .merge_cf_opt(
                            &cf,
                            key,
                            update_buf.as_slice(),
                            adapt::thread_write_options(),
                        )
                        .map_err(crate::Error::from)
                    {
                        Ok(()) => break,
                        Err(crate::Error::Busy) if attempt + 1 == MAX_BUSY_RETRIES => {
                            tracing::error!(
                                "gave up recording log updates for {key} after {MAX_BUSY_RETRIES} attempts"
                            );
                        }
                        Err(crate::Error::Busy) => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => {
                            tracing::error!("could not record log updates for {key}: {e}");
                            break;
                        }
                    }
                }
                self.db.cache.invalidate(&key);
            }

            if scanned % 16 == 0 {
                iter = adapt::iterator(&self.db.inner, &cf);
                iter.seek(key);
                if iter.key().map(|y| y == key.as_slice()).unwrap_or_default() {
                    iter.next();
                }

                rdb_perf::report_retains_io_perf(&mut perf);
            } else {
                iter.next();
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
            self.db
                .inner
                .compact_range_cf::<&[u8], &[u8]>(&blobs_cf, None, None);
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
    metrics::QUERY.measure(1, &ovdc_trace::label!("level": 0));
    if let Some(value) = cache.get(key) {
        tracing::trace!(?key, "using cached key");
        metrics::BYTES_RETURNED.measure(value.value().len(), &ovdc_trace::label!("level": 0));
        return Ok(value);
    }
    metrics::MISS.measure(1, &ovdc_trace::label!("level": 0));
    metrics::QUERY.measure(1, &ovdc_trace::label!("level": 2));

    let value = adapt::get_key(db, cf, key, level)?;
    metrics::BYTES_RETURNED.measure(value.len() as u64, &ovdc_trace::label!("level": 2));

    let rollup::Latest {
        inline_value,
        blob_value,
        metadata,
        ..
    } = rollup::latest(&value);

    let metadata = metadata.ok_or(Error::NotFound)?;

    if metadata.is_expired(now_ms()) {
        return Err(Error::NotFound);
    }

    let any_entry = if let Some(inline_value) = inline_value {
        AnyEntry {
            key: *key,
            value: Value::Vec(Arc::new(inline_value.inner)),
            metadata,
        }
    } else if let Some((tag, size)) = blob_value {
        AnyEntry {
            key: *key,
            value: Value::Blob {
                size,
                next: *key,
                tag,
                value: None,
            },
            metadata,
        }
    } else {
        tracing::error!("key data not recognized: {key:?}");
        return Err(Error::NotFound);
    };

    cache.insert(*key, any_entry.clone());

    Ok(any_entry)
}
