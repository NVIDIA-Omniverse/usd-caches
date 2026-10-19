// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{ops::Deref, sync::Arc};

use bytes::BufMut;
use ovdc_bytes::keys::{self, Compound};
use ovdc_trace::metrics::{timer, Measure};
use rocksdb::{AsColumnFamilyRef, WriteBatch};

use crate::{
    environment::{
        rocks_adapter::{
            adapt,
            log::{self, models, rollup, snowflake},
            metrics::{self},
            write_semaphore, DEFAULT_CF, WRITE_NO_SLOWDOWN, WRITE_NO_SLOWDOWN_OR_WAL,
        },
        Rocks,
    },
    metadata::Metadata,
    AnyEntry, Blob, BlobContext, Entry, MultiPartGuard, Value,
};

use super::bits;

#[repr(transparent)]
pub struct Blobs(pub Rocks);

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

        // The blob id orders a key's blobs against each other and against the
        // DROP records that retire them, so it has to come from the same clock
        // as every other log tag. A random id compares meaninglessly against a
        // snowflake and leaves deleted blobs unreclaimable.
        let tag = snowflake::next();

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

                let cf = db.cf_handle(crate::BLOBS_CF).unwrap();
                let (from, to) = key_range_for(&key, tag);

                // Reclaim the parts here rather than leaving them all for GC:
                // the upload is known to be over, and holding disk until the
                // next pass buys nothing. The range covers every part the
                // upload could have reached, which is what an abandoned one
                // needs -- it never recorded how far it got.
                //
                // Best effort, and non-blocking: this runs when the guard
                // drops, on whatever thread abandoned the upload, so waiting
                // out a stall here parks a request thread indefinitely.
                //
                // The `Created` record is left standing either way, and nothing
                // is written to say the parts are gone. It is the only thing
                // keeping them reachable, and `Deleted` asserts they are already
                // off disk -- `eval_drops` retires the blob id on sight of one,
                // so a `Deleted` that outlived its delete would strand the parts
                // permanently. The two writes cannot be made to fail together:
                // they are in different column families, so they flush
                // independently and a crash can keep one and lose the other.
                //
                // So only the code that has just deleted the parts says they are
                // deleted. GC does that -- it reclaims the blob once the grace
                // period has passed, running the same range delete again, which
                // is a no-op when this one worked. The cost is a `Created`
                // record living until then, not the parts.
                if let Err(e) = db.delete_range_cf_opt(
                    &cf,
                    from.as_slice(),
                    to.as_slice(),
                    &WRITE_NO_SLOWDOWN_OR_WAL,
                ) {
                    tracing::warn!(
                        "could not remove the parts of the abandoned upload for key {key:?}, \
                         leaving them for GC: {e}"
                    );
                }

                cache.invalidate(&key);
            },
        )?;

        let default_cf = self.inner.cf_handle(DEFAULT_CF).unwrap();

        if self.inner.key_may_exist_cf(&default_cf, key) {
            match self.inner.get_cf(&default_cf, key) {
                Ok(Some(value)) => {
                    // Only a blob that finished blocks a new one. A `Created`
                    // record means an upload started and never got there --
                    // abandoned, or interrupted -- and counting that as written
                    // left the key unreadable and unwritable for good, since
                    // nothing else clears such a record either.
                    //
                    // A live upload is a different case and is already handled:
                    // `MultiPartGuard::new` above refuses a key another writer
                    // currently holds. This check is only about what is on disk.
                    let has_completed_blob = rollup::latest(&value).blob_value.is_some();

                    if has_completed_blob {
                        metrics::BLOB_DENY.measure(1, &[]);
                        tracing::info!("multi part value already exists for {:?}", key);
                        guard.assert_complete();
                        return None;
                    }

                    tracing::warn!("replacing an unfinished multi part value for {key:?}");
                    // The unfinished write's records are retired by this one:
                    // its blob id is newer, so the older id sorts below
                    // `latest_complete` once this blob completes and GC reclaims
                    // the parts left behind.
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

        Some((
            Box::new(BlobWriter {
                tag,
                index: 1,
                total_size: size,
                key,
                metadata,
                guard,
            }),
            tag,
        ))
    }

    fn max_parts(&self) -> u32 {
        // The part number is a u32 field of the part key, and `key_range_for`
        // bounds the range at `!0`, so the last usable part is one below that.
        u32::MAX - 1
    }

    fn remove(&self, key: keys::Compound, tag: u128) -> Result<(), crate::Error> {
        let cf = self.inner.cf_handle(crate::BLOBS_CF).unwrap();
        let (from, to) = key_range_for(&key, tag);
        self.inner
            .delete_range_cf_opt(&cf, from.as_slice(), to.as_slice(), &WRITE_NO_SLOWDOWN)
            .map_err(Into::into)
    }
}

struct BlobReader {
    index: u32,
    #[allow(unused)]
    tag: u128,
    read_index: u32,
    key: keys::Compound,
    value: Option<AnyEntry>,
}

impl crate::Read for Blobs {
    fn poll_read(&self, cx: &mut BlobContext) -> Result<(), std::io::Error> {
        let cx = cx.downcast_mut::<BlobReader>().unwrap();

        let key = key_for(&cx.key, cx.tag, cx.index);
        let cf = self.inner.cf_handle(crate::BLOBS_CF).unwrap();

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
                let len = value.len();
                let entry = AnyEntry {
                    key,
                    metadata: Default::default(),
                    value: Value::Vec(value.into()),
                };
                self.cache.insert(key, entry.clone());

                metrics::BYTES_RETURNED.measure(len as u64, &ovdc_trace::label!("level": 2));

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
    index: u32,
    tag: u128,
    total_size: u64,
    key: keys::Compound,
    metadata: Metadata,
    guard: Arc<MultiPartGuard>,
}

impl crate::Write for Blobs {
    fn schedule(&self, cx: &mut BlobContext, from: &[u8]) -> bool {
        let cx = cx.downcast_mut::<BlobWriter>().unwrap();

        if !write_semaphore::try_write(&self.writers) {
            return false;
        }

        let mut write_batch = WriteBatch::default();

        if cx.index == 1 {
            let mut writer = bits::ArrayBuf::new();
            bits::write(
                models::KIND_BLOB,
                snowflake::next(),
                &models::BlobChange::Created { id: cx.tag },
                &mut writer,
            );
            let default_cf_handle = self.inner.cf_handle(DEFAULT_CF).unwrap();
            write_batch.merge_cf(&default_cf_handle, cx.key, writer.as_slice());
        }

        let blobs_cf_handle = self.inner.cf_handle(crate::BLOBS_CF).unwrap();
        let key = key_for(&cx.key, cx.tag, cx.index);

        put_blob(
            &mut write_batch,
            &blobs_cf_handle,
            key_for(&cx.key, cx.tag, cx.index),
            from,
        );

        let result = self
            .0
            .inner
            .write_opt(write_batch, &WRITE_NO_SLOWDOWN_OR_WAL)
            .is_ok();

        self.cache.invalidate(&cx.key);
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

        let tag = snowflake::next();

        let mut writer = bits::ArrayBuf::new();
        bits::write(
            models::KIND_DROP,
            tag,
            &log::models::DropBefore,
            &mut writer,
        );
        bits::write(
            models::KIND_METADATA,
            tag,
            &log::models::Metadata::Set(cx.metadata),
            &mut writer,
        );
        bits::write(
            models::KIND_BLOB,
            snowflake::next(),
            &models::BlobChange::Completed {
                // The blob's own id, not the tag this record is written under.
                // It is what the parts are keyed by, so a reader given anything
                // else looks for them under a prefix that holds nothing, and GC
                // reclaims an empty range while the real parts stay on disk.
                id: cx.tag,
                size: cx.total_size,
                // `index` is the part number the next chunk would take, and
                // parts are numbered from one, so it trails by one.
                parts: cx.index - 1,
            },
            &mut writer,
        );

        // This record is what makes the blob readable -- `rollup` only sets a
        // blob value from a Completed record -- so the result cannot be
        // discarded. Non-blocking, like every other write on this path.
        self.0.inner.merge_cf_opt(
            &default_cf_handle,
            cx.key,
            writer.as_slice(),
            adapt::thread_write_options(),
        )?;

        self.cache.invalidate(&cx.key);

        // Only once the write has landed: marking the guard complete stops the
        // cleanup that would otherwise reclaim the parts.
        cx.guard.assert_complete();

        Ok(())
    }
}

fn key_for(for_key: &Compound, tag: u128, part: u32) -> keys::Compound {
    let mut buffer = [0u8; keys::USER_KEY_SIZE];

    let mut buf = &mut buffer[..];
    buf.put_u64(for_key.hash());
    buf.put_u128(tag);
    buf.put_u32(part);

    keys::Compound::new(for_key.into_parts().0, keys::Key::new(&buffer).unwrap())
}

fn key_range_for(for_key: &Compound, tag: u128) -> (keys::Compound, keys::Compound) {
    (key_for(for_key, tag, 0), key_for(for_key, tag, !0))
}

fn put_blob(
    batch: &mut WriteBatch,
    cf: &impl AsColumnFamilyRef,
    key: keys::Compound,
    value: &[u8],
) {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_WRITE.clone());

    batch.put_cf(cf, key, value);
    metrics::PUT.measure(1, &[]);
    metrics::PUT_BYTE_HIST.measure(value.len() as u64, &[]);
}
