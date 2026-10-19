// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{ops::Deref, sync::Arc};

use ovdc_bytes::keys;
use ovdc_trace::metrics::Measure;

use crate::{
    environment::rocks_adapter::{value, DEFAULT_CF, WRITE_NO_SLOWDOWN_OR_WAL},
    metadata::Metadata,
    AnyEntry, Blob, BlobContext, Entry, MultiPartGuard, FLAG_DB_BLOB_ENABLE,
};

use super::{
    metrics::{self},
    multi_part,
    value::stored_kind_into_entry,
    write_semaphore, Rocks,
};

/// The highest part number this backend can store.
///
/// Unlike the log backend, the part number here is a single byte of the key's
/// reserved field (`multi_part::key_at`), so 255 is the ceiling of the on-disk
/// layout rather than of a counter -- widening the counter would not help.
///
/// The limit is 254 rather than 255 so that `multi_part::key_range`, which is
/// end-exclusive at part 255, covers every part that can exist. Allowing part
/// 255 would leave one part per blob that delete-range never reclaims.
const MAX_PARTS: u8 = 254;

fn family(blobs: bool) -> &'static str {
    match blobs {
        false => crate::DEFAULT_CF,
        true => crate::BLOBS_CF,
    }
}

#[repr(transparent)]
pub struct Blobs(Rocks);

impl Deref for Blobs {
    type Target = Rocks;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Blob for Blobs {
    fn begin_read(&self, key: keys::Compound, tag: u128) -> BlobContext {
        Box::new(BlobReader {
            index: 1,
            read_index: 0,
            key,
            value: None,
            tag,
        })
    }

    fn begin_write(
        &self,
        key: keys::Compound,
        metadata: Metadata,
        size: u64,
    ) -> Option<(BlobContext, u128)> {
        tracing::trace!(?key, size, "begin writing blob");
        metrics::BLOB_BEGIN.measure(1, &[]);

        let weak = Arc::downgrade(&self.inner);
        let cache = self.cache.clone();

        let is_blobs_cf = self.flags & FLAG_DB_BLOB_ENABLE != 0;

        let guard = MultiPartGuard::new(
            key,
            &self.multi_part_in_progress,
            move |is_flag_set, key| {
                if is_flag_set {
                    tracing::trace!("key {key:?} was fully written and will not be removed");
                    return;
                }
                metrics::BLOB_ABANDON.measure(1, &[]);

                tracing::warn!("key {key:?} was NOT fully written and WILL be removed");

                let Some(db) = weak.upgrade() else {
                    tracing::error!("unable to fully clean key {key:?}");
                    return;
                };

                ovdc_trace::drop!(
                        multi_part::delete_blob_values(&db, &db.cf_handle(family(is_blobs_cf)).unwrap(), key),
                        "failed to remove the key range after a failed multi part upload for key {key:?}"
                    );

                ovdc_trace::drop!(
                    db.delete_opt(key, &WRITE_NO_SLOWDOWN_OR_WAL),
                    "failed to remove the key index {key:?}"
                );

                for i in 0..255 {
                    cache.invalidate(&multi_part::key_at(key, i));
                }
            },
        )?;

        let default_cf = self.inner.cf_handle(DEFAULT_CF).unwrap();

        if self.inner.key_may_exist_cf(&default_cf, key) {
            match self.inner.get_cf(&default_cf, key) {
                Ok(Some(value)) => {
                    let is_fully_written = value::parse(&value)
                        .map(|y| {
                            // IFF the value exists and is valid.
                            y.invalid().is_empty()
                        })
                        .unwrap_or_default();

                    if is_fully_written {
                        metrics::BLOB_DENY.measure(1, &[]);
                        tracing::info!("multi part value already exists for {:?}", key);
                        guard.assert_complete();
                        return None;
                    }

                    tracing::error!("multi part value is not complete for {:?}", key);
                    // Note that the exiting value is deleted when the guard is dropped or is
                    // overwritten, correcting this issue.
                }
                Err(e) if e.kind() == rocksdb::ErrorKind::NotFound => {}
                e @ Err(..) => {
                    metrics::BLOB_DENY.measure(1, &[]);
                    ovdc_trace::drop!(e, "failed to look up {key}");
                    // Forces the value to not be deleted.
                    guard.assert_complete();
                    return None;
                }
                Ok(None) => {}
            }
        }

        let random = rand::random();

        Some((
            Box::new(BlobWriter {
                random,
                index: 1,
                total_size: size,
                key,
                metadata,
                guard,
            }),
            random,
        ))
    }

    fn max_parts(&self) -> u32 {
        MAX_PARTS as u32
    }

    fn remove(&self, key: keys::Compound, _tag: u128) -> Result<(), crate::Error> {
        let is_blobs_cf = self.flags & FLAG_DB_BLOB_ENABLE != 0;
        let cf = self.inner.cf_handle(family(is_blobs_cf)).unwrap();
        ovdc_trace::err!(
            multi_part::delete_blob_values(&self.inner, &cf, key),
            "failed to remove full range of blob keys"
        )
        .map_err(Into::into)
    }
}

struct BlobReader {
    index: u8,
    #[allow(unused)]
    tag: u128,
    read_index: u8,
    key: keys::Compound,
    value: Option<AnyEntry>,
}

impl crate::Read for Blobs {
    fn poll_read(&self, cx: &mut BlobContext) -> Result<(), std::io::Error> {
        let cx = cx.downcast_mut::<BlobReader>().unwrap();
        let is_blobs_cf = self.flags & FLAG_DB_BLOB_ENABLE != 0;

        let key = multi_part::key_at(cx.key, cx.index);
        let cf = self.inner.cf_handle(family(is_blobs_cf)).unwrap();

        // First try the cache...
        metrics::QUERY.measure(1, &ovdc_trace::label!("level": 0));
        if let Some(value) = self.cache.get(&key) {
            metrics::BYTES_RETURNED.measure(value.value().len(), &ovdc_trace::label!("level": 0));

            cx.value = Some(value);
            cx.index += 1;
            return Ok(());
        }
        metrics::MISS.measure(1, &ovdc_trace::label!("level": 0));
        metrics::QUERY.measure(1, &ovdc_trace::label!("level": 2));

        match ovdc_trace::err!(
            self.inner.get_cf(&cf, key.as_slice()),
            "failed to read blob part"
        ) {
            Ok(Some(value)) => {
                let (kind, _) = value::entry(key, &value);
                let entry = stored_kind_into_entry(key, kind);
                self.cache.insert(key, entry.clone());

                metrics::BYTES_RETURNED
                    .measure(value.len() as u64, &ovdc_trace::label!("level": 2));

                cx.value = Some(entry);
                cx.index += 1;
            }
            Ok(None) => {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            Err(e) if e.kind() == rocksdb::ErrorKind::NotFound => {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            _ => {
                return Err(std::io::ErrorKind::Other.into());
            }
        }

        Ok(())
    }

    fn next<'a>(&self, cx: &'a mut BlobContext) -> Option<&'a [u8]> {
        let cx = cx.downcast_mut::<BlobReader>().unwrap();

        if cx.read_index == cx.index {
            cx.value.take();
        }
        cx.read_index = cx.index;

        cx.value.as_ref().and_then(|y| y.value().slice())
    }
}

struct BlobWriter {
    index: u8,
    random: u128,
    total_size: u64,
    key: keys::Compound,
    metadata: Metadata,
    guard: Arc<MultiPartGuard>,
}

impl crate::Write for Blobs {
    fn schedule(&self, cx: &mut BlobContext, from: &[u8]) -> bool {
        let cx = cx.downcast_mut::<BlobWriter>().unwrap();

        // Refuse rather than wrap. The part number is one byte of the key, so
        // incrementing past the limit writes over an earlier part of this same
        // blob -- and part 0 is the blob index, so the wrap destroys the record
        // that describes it. Callers should have checked `max_parts` before
        // starting; this is the backstop for the ones that did not.
        if cx.index > MAX_PARTS {
            tracing::error!(
                key = ?cx.key,
                "blob needs more than {MAX_PARTS} parts and cannot be stored by this backend"
            );
            return false;
        }

        if !write_semaphore::try_write(&self.writers) {
            return false;
        }

        if cx.index == 1 {
            let default_cf_handle = self.inner.cf_handle(DEFAULT_CF).unwrap();
            if multi_part::put_blob_index(
                &self.inner,
                &default_cf_handle,
                cx.key,
                cx.metadata,
                cx.total_size,
                cx.random,
            )
            .is_err()
            {
                write_semaphore::decr_writer(&self.writers);
                return false;
            }
            self.cache.invalidate(&cx.key);
        }

        let cf_handle = self
            .inner
            .cf_handle(family(self.flags & FLAG_DB_BLOB_ENABLE != 0))
            .unwrap();

        let key = multi_part::key_at(cx.key, cx.index);
        let result = multi_part::put_blob_value_segment(
            &self.inner,
            &cf_handle,
            key,
            from,
            cx.index,
            cx.random,
        )
        .is_ok();

        self.cache.invalidate(&key);

        if result {
            cx.index += 1;
        }

        write_semaphore::decr_writer(&self.writers);

        result
    }

    fn done(&self, cx: &mut BlobContext) -> Result<(), crate::Error> {
        let cx = cx.downcast_mut::<BlobWriter>().unwrap();
        let default_cf_handle = self.inner.cf_handle(DEFAULT_CF).unwrap();

        // `set_filled` writes with no_slowdown, so this reports a stall rather
        // than blocking -- and a stall is likely here, since `done` runs as the
        // last chunk of a large upload lands.
        multi_part::set_filled(&self.inner, &default_cf_handle, cx.key, cx.metadata)?;
        self.cache.invalidate(&cx.key);

        // Only once the write has landed. A guard marked complete never cleans
        // up, so marking it earlier would leave the parts on disk with nothing
        // pointing at them if the write had failed.
        cx.guard.assert_complete();

        Ok(())
    }
}
