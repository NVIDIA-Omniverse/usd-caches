// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_bytes::keys;

use crate::{
    metadata, multi_mode,
    tests::{temp_db, temp_db_flags},
    BlobProvider, Change, Database, Entry, Update, FLAG_DB_BLOB_ENABLE, FLAG_DB_LOG_ENABLE,
};

use super::Rocks;

multi_mode! {
    test(
        write_read,
        write_delete_read,
        abandon,
        existing_is_immutable,
        eventual_removal_abandon,
        access_time_only_moves_forward,
        read_back_through_get,
        blob_survives_a_keep_everything_gc_pass
    ) =>
    {
        default = { temp_db() },
        blob = { temp_db_flags(FLAG_DB_BLOB_ENABLE) },
        log = { temp_db_flags(FLAG_DB_LOG_ENABLE) }
    }
}

/// Dropping the last handle must leave no thread behind that can still reach
/// RocksDB.
///
/// The monitor thread re-`upgrade`s its `Weak<RocksDb>` every tick, so if it
/// outlives the last `Rocks` it can become the final owner and run `~DBImpl`
/// itself. Nothing joined it, so that landed during process exit, racing C++
/// static destruction: `~DBImpl` unlocks the LOCK file through
/// `PosixFileSystem::UnlockFile`, which takes a file-scope `static port::Mutex`,
/// and locking that after it has been destroyed aborts the process with
/// `pthread lock: Invalid argument`. The test that was running had already
/// reported `ok`, so it read as a spurious SIGABRT in CI.
///
/// Both assertions are timing-free. The monitor thread owns an `Arc<Monitor>`
/// for as long as it runs, so a strong count back at one is proof it has
/// actually exited rather than proof that this check was lucky.
#[test]
fn test_dropping_the_last_handle_joins_the_monitor_thread() {
    use std::sync::Arc;

    let (_dir, db) = temp_db();
    let monitor = db.monitor.clone();
    let weak_db = Arc::downgrade(&db.inner);

    // A surviving clone must keep everything open -- the join belongs to the
    // last handle, not to every one of them.
    let clone = db.clone();
    drop(clone);
    assert!(
        weak_db.upgrade().is_some(),
        "dropping one of two handles closed the database"
    );

    drop(db);

    assert_eq!(
        Arc::strong_count(&monitor),
        1,
        "the monitor thread was still running after the last handle was dropped"
    );
    assert!(
        weak_db.upgrade().is_none(),
        "something still owns the database after the last handle was dropped"
    );
}

/// A blob must be readable the way a client actually reads one: `get` the key,
/// take the blob id out of the returned entry, and stream with that.
///
/// Every other blob test streams with the id `begin_write` handed back, so none
/// of them ever reads the id out of what was stored. `done` recorded the tag its
/// records were written under in place of the blob's own id, and the parts are
/// keyed by the id -- so `get` returned a handle pointing at a prefix holding
/// nothing, and every multi-part value read as missing. This is the path
/// `recall_multi_part` in the get service takes.
fn read_back_through_get(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(9_100));
    let metadata = metadata::Metadata::default();

    let written: Vec<u8> = (0..24u8).collect();

    let (mut cx, _tag) = db
        .blobs()
        .begin_write(key, metadata, written.len() as u64)
        .unwrap();
    for chunk in written.chunks(8) {
        assert!(db.blobs().schedule(&mut cx, chunk));
    }
    db.blobs().done(&mut cx).unwrap();
    drop(cx);

    let entry = db.open().get(key).expect("the blob should be readable");
    let crate::Value::Blob { size, tag, .. } = entry.value() else {
        panic!("a multi-part value should read back as a blob");
    };
    assert_eq!(*size, written.len() as u64);

    // Deliberately the id from the entry, not the one `begin_write` returned.
    let mut read_cx = db.blobs().begin_read(key, *tag);
    let mut rebuilt = vec![];
    while (rebuilt.len() as u64) < *size {
        db.blobs()
            .poll_read(&mut read_cx)
            .expect("a part was not where the entry said it would be");
        rebuilt.extend_from_slice(db.blobs().next(&mut read_cx).unwrap());
    }

    assert_eq!(rebuilt, written, "the blob did not read back intact");
}

/// A blob nobody asked to remove must survive garbage collection.
///
/// Every other blob test reads back without a GC pass in between, so none of
/// them saw that `stale_blobs` retired a blob the moment it completed. `done`
/// writes a DROP to retire what the key held before, and that DROP is tagged
/// later than the id the blob was given when its upload began -- so comparing
/// the id against the DROP marked every completed blob stale, and the first
/// `retain` after a write reclaimed the parts of the value it had just stored.
///
/// The predicate keeps everything, so anything missing afterwards was removed
/// by GC deciding on its own that the blob was dead.
fn blob_survives_a_keep_everything_gc_pass(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(9_101));
    let metadata = metadata::Metadata::default();

    let written: Vec<u8> = (0..24u8).collect();

    let (mut cx, _tag) = db
        .blobs()
        .begin_write(key, metadata, written.len() as u64)
        .unwrap();
    for chunk in written.chunks(8) {
        assert!(db.blobs().schedule(&mut cx, chunk));
    }
    db.blobs().done(&mut cx).unwrap();
    drop(cx);

    db.open()
        .get(key)
        .expect("the blob should be readable before any gc pass");

    db.open().retain(Box::new(|_| true)).unwrap();

    let entry = db
        .open()
        .get(key)
        .expect("a gc pass that keeps every key must not remove the blob");
    let crate::Value::Blob { size, tag, .. } = entry.value() else {
        panic!("a multi-part value should read back as a blob");
    };
    assert_eq!(*size, written.len() as u64);

    let mut read_cx = db.blobs().begin_read(key, *tag);
    let mut rebuilt = vec![];
    while (rebuilt.len() as u64) < *size {
        db.blobs()
            .poll_read(&mut read_cx)
            .expect("gc reclaimed a part of a blob it was told to keep");
        rebuilt.extend_from_slice(db.blobs().next(&mut read_cx).unwrap());
    }

    assert_eq!(
        rebuilt, written,
        "the blob did not read back intact after gc"
    );
}

fn write_read(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    let metadata = metadata::Metadata::default();

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3]));
    assert!(db.blobs().schedule(&mut cx, &[4, 5, 6, 7]));
    db.blobs().done(&mut cx).unwrap();

    let mut cx = db.blobs().begin_read(key, tag);

    db.blobs().poll_read(&mut cx).unwrap();
    let value_0 = db.blobs().next(&mut cx).unwrap().to_vec();

    if value_0.len() == 8 {
        assert_eq!(&value_0, &[0, 1, 2, 3, 4, 5, 6, 7]);
        return;
    }

    db.blobs().poll_read(&mut cx).unwrap();
    let value_1 = db.blobs().next(&mut cx).unwrap().to_vec();

    let value = {
        let mut value = vec![];
        value.extend_from_slice(&value_0);
        value.extend_from_slice(&value_1);
        value
    };

    assert_eq!(&value, &[0u8, 1, 2, 3, 4, 5, 6, 7]);

    assert_eq!(
        db.blobs().poll_read(&mut cx).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    )
}

fn write_delete_read(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    let metadata = metadata::Metadata::default();

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 4).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3]));
    db.blobs().done(&mut cx).unwrap();

    db.blobs().remove(key, tag).unwrap();

    let mut cx = db.blobs().begin_read(key, tag);
    assert_eq!(
        db.blobs().poll_read(&mut cx).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

fn abandon(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    let metadata = metadata::Metadata::default();

    let (mut cx, _tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3]));

    assert!(db.blobs().begin_write(key, metadata, 8).is_none());

    drop(cx);

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3, 4, 5, 6, 7]));
    db.blobs().done(&mut cx).unwrap();

    let mut cx = db.blobs().begin_read(key, tag);
    assert!(db.blobs().poll_read(&mut cx).is_ok());

    assert_eq!(
        db.blobs().next(&mut cx),
        Some(&(&[0u8, 1, 2, 3, 4, 5, 6, 7])[..])
    );
}

fn existing_is_immutable(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    let metadata = metadata::Metadata::default();

    let (mut cx, _tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3, 4, 5, 6, 7]));

    assert!(db.blobs().begin_write(key, metadata, 8).is_none());
}

/// The legacy backend refuses a blob past its part limit instead of wrapping
/// onto an earlier part.
///
/// Its part number is one byte of the key, so 255 is the ceiling of the on-disk
/// layout. Incrementing past it used to wrap -- silently in release, where
/// overflow checks are off -- and part 0 is the blob index, so the wrap
/// overwrote the record describing the blob.
#[test]
fn test_blob_table_refuses_a_blob_past_its_part_limit() {
    let (_dir, db) = temp_db_flags(FLAG_DB_BLOB_ENABLE);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));
    let metadata = metadata::Metadata::default();

    let limit = db.blobs().max_parts();
    let (mut cx, tag) = db
        .blobs()
        .begin_write(key, metadata, limit as u64 + 8)
        .unwrap();

    for part in 1..=limit {
        assert!(
            db.blobs().schedule(&mut cx, &[part as u8]),
            "part {part} of {limit} was refused"
        );
    }

    assert!(
        !db.blobs().schedule(&mut cx, &[0xFF]),
        "backend accepted a part past its limit and would have wrapped"
    );

    // The blob index still describes a blob rather than raw part bytes.
    db.blobs().done(&mut cx).unwrap();
    let mut read = db.blobs().begin_read(key, tag);
    assert!(
        db.blobs().poll_read(&mut read).is_ok(),
        "the blob index was overwritten"
    );
}

/// The log backend numbers parts with a `u32` field of the part key, so it is
/// not bounded by a byte the way the legacy one is.
#[test]
fn test_log_part_limit_is_not_a_byte() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE);
    assert!(
        db.blobs().max_parts() > u16::MAX as u32,
        "log parts are still bounded like a byte"
    );
}

/// A blob write that fails because the engine is stalling must still release
/// the process-wide write permit.
///
/// `Blobs::schedule` takes the permit, and the branch handling a failed
/// `put_blob_index` returned without releasing it. That write uses no_slowdown,
/// so the failure is a stall -- the exact condition the permit exists to signal
/// -- and with MAX_WRITERS at 1 a single leak left every later write reporting
/// Busy forever, long after the stall had cleared.
///
/// Asserts the counter rather than trying a later write: while the engine is
/// still stalling both a leaked permit and the stall itself produce Busy, and
/// clearing the stall first would only test the clearing.
#[test]
fn test_blob_schedule_releases_the_write_permit_when_stalled() {
    let (_dir, db) = crate::tests::temp_db_stalling_at(FLAG_DB_BLOB_ENABLE, 64 * 1024);
    crate::tests::fill_until_busy(&db, 0);

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(9_000));
    let metadata = metadata::Metadata::default();

    // Whether the blob index write fails is RocksDB's business; releasing the
    // permit when it does is ours.
    if let Some((mut cx, _tag)) = db.blobs().begin_write(key, metadata, 4096) {
        let _ = db.blobs().schedule(&mut cx, &[1u8; 4096]);
    }

    assert_eq!(
        db.writers.load(std::sync::atomic::Ordering::Acquire),
        0,
        "the failed blob write left the process-wide write permit held"
    );
}

/// Completing a blob reports back-pressure rather than aborting, and leaves
/// nothing behind when it fails.
///
/// `done` writes the record that makes the parts readable. The legacy backend
/// used to `unwrap` that write -- a stall aborts the process under
/// `panic = 'abort'`, and in an unwinding build the panic dropped the context
/// with the guard unsatisfied, deleting the blob it had just finished writing.
/// The log backend discarded the error instead and answered success, leaving
/// parts on disk that nothing referred to.
///
/// Either way the caller must be able to tell.
#[test]
fn test_blob_done_reports_back_pressure_instead_of_aborting() {
    for flags in [
        FLAG_DB_BLOB_ENABLE,
        FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE,
    ] {
        let (_dir, db) = crate::tests::temp_db_stalling_at(flags, 64 * 1024);
        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(7_000));
        let metadata = metadata::Metadata::default();

        let Some((mut cx, _tag)) = db.blobs().begin_write(key, metadata, 8) else {
            panic!("flags {flags}: could not start a blob write on a fresh database")
        };
        let _ = db.blobs().schedule(&mut cx, &[1, 2, 3, 4, 5, 6, 7, 8]);

        crate::tests::fill_until_busy(&db, 0);

        // Whether this particular call is refused is RocksDB's business. What
        // matters is that it comes back with an answer, and that the answer is
        // true: either back-pressure, or a blob that really is readable.
        match db.blobs().done(&mut cx) {
            Err(crate::Error::Busy) => {}
            Err(e) => panic!("flags {flags}: unexpected error completing blob: {e:?}"),
            Ok(()) => {
                // Asserted through the public read, not through `poll_read`.
                // `poll_read` fetches part keys straight out of the blobs column
                // family and never consults the record `done` writes, so part 1
                // -- already written by `schedule` above -- made it answer `Ok`
                // whether or not `done` did anything. That is exactly the
                // regression named in this test's doc comment, so the old
                // assertion could not see it.
                //
                // `get` goes through the record instead: in log mode
                // `rollup::latest` publishes a blob value only from a
                // `Completed` record, and the legacy backend needs the preamble
                // row `done` writes.
                let entry = db.open().get(key).unwrap_or_else(|e| {
                    panic!(
                        "flags {flags}: done reported success but the key is not readable: {e:?}"
                    )
                });

                assert!(
                    matches!(entry.value(), crate::Value::Blob { .. }),
                    "flags {flags}: done reported success but the key does not hold a blob"
                );
            }
        }
    }
}

/// Abandoning a blob write over an existing value must leave that value alone.
///
/// The cleanup used to delete the whole row. A key's value in log mode is a
/// merge log, so that took with it anything written before the upload started --
/// a failed write removing the value it was replacing.
#[test]
fn test_log_abandoned_upload_keeps_the_existing_value() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(5_000));
    let metadata = metadata::Metadata::default();

    db.open().put(key, metadata, b"before").unwrap();

    let (mut cx, _tag) = db.blobs().begin_write(key, metadata, 4096).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[9u8; 64]));
    drop(cx);

    let entry = db
        .open()
        .get(key)
        .expect("the abandoned upload destroyed the value it was replacing");
    assert_eq!(entry.value().slice().unwrap(), b"before");
}

/// The guard reclaims an abandoned upload's parts itself, up front.
///
/// The delete covers the whole part range the upload could have reached rather
/// than the parts it is known to have written, because an abandoned upload
/// never recorded how far it got.
///
/// It is best effort, and it writes nothing to say the parts are gone. Only the
/// code that has just deleted them may claim that: `eval_drops` retires a blob
/// id on sight of a `Deleted` record, so one that outlived its delete would
/// strand the parts where nothing revisits them -- and the delete and the record
/// live in different column families, which flush independently, so a crash can
/// keep either one alone.
///
/// The `Created` record therefore stays, and GC retires the blob by age, running
/// the same range delete again. The second half is that pass.
#[test]
fn test_log_abandoned_upload_parts_are_reclaimed() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(5_200));
    let metadata = metadata::Metadata::default();

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 4096).unwrap();
    // Several parts, so this pins the range delete rather than the removal of a
    // single part key.
    for _ in 0..4 {
        assert!(db.blobs().schedule(&mut cx, &[9u8; 64]));
    }

    assert_eq!(
        blobs_cf_rows(&db),
        4,
        "the parts should be on disk before the upload is abandoned"
    );

    drop(cx);

    assert_eq!(
        blobs_cf_rows(&db),
        0,
        "the guard left the abandoned upload's parts on disk"
    );

    // The blob must still be recorded as existing. Writing `Deleted` here would
    // be a claim the guard cannot back -- it is a separate write, in a separate
    // column family, and a crash keeping it while losing the delete above puts
    // the parts beyond the reach of anything that could retry. It is also not
    // observable directly: `Deleted` cancels the `Created` it refers to and both
    // are collapsed away, so its absence shows up as the `Created` surviving.
    let cf = db.inner.cf_handle("default").unwrap();
    let stored = db.inner.get_cf(&cf, key).unwrap().unwrap_or_default();
    let mut log = stored.as_slice();
    let mut created = false;
    while let Some(next) = super::log::bits::_next(&mut log) {
        if next.kind == super::log::models::KIND_BLOB {
            let change = crate::serde::deserialize::<super::log::models::BlobChange>(next.value)
                .expect("a blob record should decode");
            created |= matches!(
                change,
                super::log::models::BlobChange::Created { id } if id == tag
            );
        }
    }
    assert!(
        created,
        "the guard retired the blob record; only the pass that deletes the \
         parts may say they are gone"
    );

    // The `Created` record is still there, so GC has to retire the row -- and it
    // must, or every later pass re-reclaims the same already-empty range.
    //
    // Age is the only thing that retires a lone `Created`: before the grace it
    // reads as an upload still running, and `retain` correctly leaves it be. So
    // the wait is what puts the row in front of the branch under test, not
    // padding. `ABANDONED_UPLOAD_GRACE` is 100ms under `cfg(test)`.
    std::thread::sleep(std::time::Duration::from_millis(150));

    db.open().retain(Box::new(|_| true)).unwrap();

    // Asserted against the stored row, not through `get`. A row holding only a
    // `Created` record has no metadata, and `get_key_as_entry` answers NotFound
    // for any row without it -- so `get(key).is_err()` was already true the
    // instant the upload was abandoned and stayed true whether this scan retired
    // the row, left it untouched, or was deleted outright.
    let stored = db.inner.get_cf(&cf, key).unwrap().unwrap_or_default();
    assert!(
        super::log::rollup::is_fully_dropped(&stored),
        "GC left the abandoned upload's row behind: {} bytes still hold records",
        stored.len()
    );
}

/// GC reclaims an upload that nothing cleaned up.
///
/// The guard normally clears an abandoned upload, but it cannot when the write
/// is refused under a stall, and it never runs at all if the process dies
/// holding the upload. What is left is a `Created` record: not stale, because
/// nothing newer completed and no drop marker was written, and skipped by the
/// orphan-row branch because the row still has a blob record. Nothing reclaimed
/// it, so the parts stayed on disk for good.
#[test]
fn test_log_gc_reclaims_an_upload_nobody_cleaned_up() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(5_100));
    let metadata = metadata::Metadata::default();

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 4096).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[9u8; 64]));

    // Exactly what a process dying mid-upload leaves behind: the record and its
    // parts on disk, and no guard that will ever run. Leaking the context skips
    // the cleanup; releasing the key by hand is what the dead process's map
    // would no longer be holding.
    std::mem::forget(cx);
    db.multi_part_in_progress.remove(&key);

    let _ = tag;

    // Against the column family rather than through `poll_read`, which answers
    // from the row cache -- and `remove` does not invalidate it, so a read would
    // report the part present after the bytes were gone.
    assert!(
        blobs_cf_rows(&db) > 0,
        "the part should be on disk before GC runs"
    );

    std::thread::sleep(std::time::Duration::from_millis(150));
    db.open().retain(Box::new(|_| true)).unwrap();

    assert_eq!(
        blobs_cf_rows(&db),
        0,
        "the abandoned upload's parts were never reclaimed"
    );
}

/// Per-column-family properties must be reported for every family, not just
/// `default`.
///
/// Memtable and compaction state is computed from one family's own state, so
/// reading `default` alone said nothing about `blobs` -- which is where the
/// large values land, and so the family most likely to stall.
#[test]
fn test_per_cf_properties_cover_every_column_family() {
    let (_dir, db) = temp_db_flags(FLAG_DB_BLOB_ENABLE);
    let props = db.properties();

    for cf in [crate::DEFAULT_CF, crate::BLOBS_CF] {
        assert!(
            props.contains_key(&crate::PropertyId::column_family(
                "rocksdb_size_all_mem_tables",
                cf
            )),
            "no memtable size reported for the {cf} column family"
        );
        assert!(
            props.contains_key(&crate::PropertyId::column_family(
                "__rocksdb_stalls_total_stop",
                cf
            )),
            "no write-stall stats reported for the {cf} column family"
        );
    }

    // Database-wide values must not be duplicated per family: the handler
    // ignores the column family, so a `cf`-scoped copy would be invented detail.
    assert!(
        props.contains_key(&crate::PropertyId::database("rocksdb_is_write_stopped")),
        "the shared write controller should be reported once, database-wide"
    );
    assert!(
        !props.contains_key(&crate::PropertyId::column_family(
            "rocksdb_is_write_stopped",
            crate::BLOBS_CF
        )),
        "a database-wide property was reported per column family"
    );
}

/// Without the blobs column family there is nothing to fan out to, and the
/// default family must still report.
#[test]
fn test_per_cf_properties_without_a_blobs_column_family() {
    let (_dir, db) = temp_db();
    let props = db.properties();

    assert!(props.contains_key(&crate::PropertyId::column_family(
        "rocksdb_size_all_mem_tables",
        crate::DEFAULT_CF
    )));
    assert!(!props.contains_key(&crate::PropertyId::column_family(
        "rocksdb_size_all_mem_tables",
        crate::BLOBS_CF
    )));
}

/// Rows in the `blobs` column family, where part data lives.
fn blobs_cf_rows(db: &Rocks) -> usize {
    let cf = db.inner.cf_handle(crate::BLOBS_CF).unwrap();
    let mut iter = db.inner.raw_iterator_cf(&cf);
    iter.seek_to_first();

    let mut rows = 0;
    while iter.valid() {
        rows += 1;
        iter.next();
    }
    rows
}

/// An upload that never finished must not take the key with it.
///
/// The guard's cleanup normally erases an abandoned upload, but that cleanup is
/// non-blocking by design, so under a stall it is refused -- and nothing retries
/// it. `begin_write` used to count the surviving `Created` record as a blob that
/// had been written, so the key could never be written again, and could not be
/// read either. GC does not clear it: the record reads as `Created` rather than
/// `Stale`, so `retain` leaves it alone.
#[test]
fn test_log_unfinished_upload_does_not_brick_the_key() {
    let (_dir, db) =
        crate::tests::temp_db_stalling_at(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE, 64 * 1024);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(6_000));
    let metadata = metadata::Metadata::default();

    let (mut cx, _tag) = db
        .blobs()
        .begin_write(key, metadata, 8)
        .expect("first write should be admitted");
    let _ = db.blobs().schedule(&mut cx, &[1, 2, 3, 4, 5, 6, 7, 8]);

    // Stall, so both completing the upload and cleaning up after it are refused
    // -- which is what leaves the record on disk with nobody to remove it.
    crate::tests::fill_until_busy(&db, 0);
    let _ = db.blobs().done(&mut cx);
    drop(cx);

    assert!(
        db.blobs().begin_write(key, metadata, 8).is_some(),
        "an unfinished upload left the key permanently unwritable"
    );
}

/// Deleting a blob key must retire its blob, so GC reclaims the parts and the
/// key can be written again.
///
/// The blob id orders a key's blobs against the DROP records that retire them.
/// While it was `rand::random()` it could not be compared against a snowflake
/// tag at all, so a deleted blob was never marked Stale: its parts were never
/// reclaimed, and `begin_write` kept seeing a live blob and refused the key
/// forever.
#[test]
fn test_log_deleted_blob_is_reclaimed() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE);
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));
    let metadata = metadata::Metadata::default();

    let (mut cx, tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3, 4, 5, 6, 7]));
    db.blobs().done(&mut cx).unwrap();

    // `done` no longer consumes the context, so the write guard is held until
    // the caller lets go. Release it before asking to write the key again.
    drop(cx);

    db.open().delete(key).unwrap();

    // A GC pass retiring nothing of its own: the delete is what retires the
    // blob, and retain is what acts on it.
    db.open().retain(Box::new(|_| true)).unwrap();

    let mut read = db.blobs().begin_read(key, tag);
    assert_eq!(
        db.blobs().poll_read(&mut read).unwrap_err().kind(),
        std::io::ErrorKind::NotFound,
        "parts of the deleted blob were not reclaimed"
    );

    assert!(
        db.blobs().begin_write(key, metadata, 8).is_some(),
        "key was left permanently un-writable by the delete"
    );
}

/// An update carrying a time older than the stored one must not move the access
/// time backwards.
///
/// `LruWriter::compact` reads the clock once and then loops with retries, and
/// two shards flush independently, so an update written later can carry an
/// earlier time than one already applied. GC evicts on this field.
fn access_time_only_moves_forward(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(7_000));
    let metadata = metadata::Metadata {
        access_time_secs: 500,
        ..metadata::Metadata::default()
    };

    db.open().put(key, metadata, b"value").unwrap();

    for time in [1000, 200] {
        db.open()
            .update(
                key,
                Update {
                    access_time: Change::Set(time),
                    ttl_ms: Change::No,
                },
            )
            .unwrap();
    }

    // Read the way GC does, by scanning the column family. `get` would answer
    // from the row cache, which `put` populated and `update` deliberately does
    // not invalidate (see the note in `adapt.rs`) -- and GC is the consumer that
    // actually acts on this field.
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let found = seen.clone();
    db.open()
        .retain(Box::new(move |entry| {
            if entry.key() == key {
                found.store(
                    entry.metadata().access_time_secs,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            true
        }))
        .unwrap();

    assert_eq!(
        seen.load(std::sync::atomic::Ordering::Relaxed),
        1000,
        "a stale update moved the access time backwards"
    );
}

/// `update` on a key that was never written, which is what the LRU compaction
/// does for every key that missed.
fn update_missing_key(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    db.open()
        .update(
            key,
            Update {
                access_time: Change::Set(1234),
                ttl_ms: Change::No,
            },
        )
        .unwrap();
}

/// (row count, total value bytes) in the default column family.
fn default_cf_rows(db: &Rocks) -> (usize, usize) {
    let cf = db.inner.cf_handle("default").unwrap();
    let mut iter = db.inner.raw_iterator_cf(&cf);
    iter.seek_to_first();

    let (mut rows, mut bytes) = (0, 0);
    while iter.valid() {
        rows += 1;
        bytes += iter.value().map(|y| y.len()).unwrap_or_default();
        iter.next();
    }
    (rows, bytes)
}

/// The legacy merge operator has to return *something* for a key that does not
/// exist, so it stores an empty placeholder. That row is unreadable, and it is
/// classified TOMBSTONE rather than EXPIRED, so GC used to skip it forever --
/// one permanent row per key that was ever missed.
#[test]
fn test_default_orphan_update_row_is_collected() {
    let (_dir, db) = temp_db();

    update_missing_key(&db);
    assert_eq!(default_cf_rows(&db).0, 1, "placeholder row was not created");

    // Keep everything the predicate is offered; only unreadable rows should go.
    db.open().retain(Box::new(|_| true)).unwrap();

    assert_eq!(
        default_cf_rows(&db).0,
        0,
        "placeholder row was not collected"
    );
}

/// The log backend leaks the same rows by a different route: the metadata
/// Change record has no Set to apply to, so rollup yields no value and no
/// metadata, `is_expired` is false, and no DROP was ever written.
///
/// Collapsing discards such a record on sight -- a note with no value to
/// describe can never apply to anything -- so the key never accumulates value
/// bytes at all, rather than carrying the record until GC.
///
/// The empty row it leaves is the compaction filter's job. Two compactions,
/// because the first only materialises the merge and the filter is offered
/// nothing until the second; two levels, so the bottom is in reach.
#[test]
fn test_log_orphan_update_record_is_discarded() {
    let (_dir, db) = crate::tests::temp_db_with_levels(FLAG_DB_LOG_ENABLE, 2);
    let cf = db.inner.cf_handle("default").unwrap();

    update_missing_key(&db);
    assert_eq!(
        default_cf_rows(&db).1,
        0,
        "an update on a key that does not exist left a record behind"
    );

    db.open().retain(Box::new(|_| true)).unwrap();
    db.inner.flush_cf(&cf).unwrap();
    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);

    assert_eq!(
        default_cf_rows(&db).0,
        0,
        "a row describing a key that was never written survived compaction"
    );
}

/// A value whose metadata did not survive is collected, not fatal.
///
/// Record decoding is deliberately tolerant -- a record that fails to validate
/// is skipped rather than aborting the process from a background thread -- so a
/// value can be left with no metadata describing it. `get` already answers
/// `NotFound` for that row. GC used to reach it and `unwrap`, which under the
/// release profile's `panic = 'abort'` takes the process down on the GC thread,
/// on exactly the pass meant to reclaim the row.
///
/// The row is written by hand because nothing in the API can produce it: every
/// write path emits metadata alongside the value.
#[test]
fn test_log_a_value_without_metadata_is_collected() {
    let (_dir, db) = temp_db_flags(FLAG_DB_LOG_ENABLE);
    let cf = db.inner.cf_handle("default").unwrap();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(4_242));

    let mut record = Vec::new();
    super::log::bits::write(
        super::log::models::KIND_INLINE_VALUE,
        1,
        &super::log::models::InlineValue {
            inner: vec![9, 9, 9, 9],
        },
        &mut record,
    );
    db.inner.merge_cf(&cf, key, &record).unwrap();

    assert_eq!(
        default_cf_rows(&db).0,
        1,
        "the row should have been written"
    );
    assert!(
        db.open().get(key).is_err(),
        "a value with no metadata should not be readable"
    );

    // The pass that used to abort here.
    db.open().retain(Box::new(|_| true)).unwrap();

    let stored = db.inner.get_cf(&cf, key).unwrap().unwrap_or_default();
    assert!(
        super::log::rollup::is_fully_dropped(&stored),
        "an unreadable row survived the pass that should have collected it"
    );
}

/// A row holding nothing but its own drop record is removed, but only once it
/// reaches the bottom.
///
/// The filter is what reclaims rows the merge operator emptied -- a collapsed
/// log is a zero-length value, not a deletion, so nothing else takes them. It
/// acts only at the bottommost level, because removing a key anywhere higher
/// drops it with no tombstone and an older version below would surface again.
///
/// Two compactions are needed, and the reason is not the level. RocksDB does
/// not offer merge results to a compaction filter, and in log mode every write
/// is a merge -- so the first compaction only materialises the collapsed log as
/// an ordinary value, without the filter ever seeing it. The second is the first
/// pass that can offer it, and by then it is at the bottom.
///
/// Which means row counts alone cannot show that the level test does anything;
/// `config::tests` asserts that part directly against `drops_at_level`.
///
/// Two levels, because with the shipped seven a store has to fill six of them
/// before a compaction reads the bottom at all -- far more data than a test
/// writes, and worth remembering when reasoning about when this runs in
/// production.
#[test]
fn test_log_compaction_filter_removes_a_dropped_row_at_the_bottom() {
    let (_dir, db) = crate::tests::temp_db_with_levels(FLAG_DB_LOG_ENABLE, 2);
    let cf = db.inner.cf_handle("default").unwrap();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));
    db.open()
        .put(key, metadata::Metadata::default(), &[1u8; 32])
        .unwrap();
    db.open().delete(key).unwrap();

    assert_eq!(
        default_cf_rows(&db).0,
        1,
        "a deleted key should still hold its drop record"
    );

    db.inner.flush_cf(&cf).unwrap();
    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);

    assert_eq!(
        default_cf_rows(&db).0,
        1,
        "the first compaction only materialises the merge; nothing is filtered yet"
    );

    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);

    assert_eq!(
        default_cf_rows(&db).0,
        0,
        "a fully dropped row survived a compaction at the bottommost level"
    );
}

/// Time alone reclaims an emptied row, with no manual compaction.
///
/// The other filter tests drive placement by compacting on demand, which proves
/// the filter works but not that anything ever calls it. In production nothing
/// does: `retain` compacts only when it deleted something, and a store that is
/// not growing produces no compactions of its own -- so the row a key leaves
/// behind when it is evicted would sit at rest forever.
/// `periodic_compaction_seconds` is what walks it down to the bottom, and this
/// is the whole reclamation path end to end.
///
/// The writes in the loop are the poke: RocksDB decides what to compact when a
/// flush gives it a reason to look, so a completely idle database re-examines
/// nothing. They are to other keys, and the assertion is about this one.
#[test]
fn test_log_periodic_compaction_reclaims_an_emptied_row() {
    let (_dir, db) = crate::tests::temp_db_compacting_every(FLAG_DB_LOG_ENABLE, 2, 1);
    let cf = db.inner.cf_handle("default").unwrap();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(77));
    db.open()
        .put(key, metadata::Metadata::default(), &[1u8; 32])
        .unwrap();
    db.open().delete(key).unwrap();
    db.inner.flush_cf(&cf).unwrap();

    assert!(
        db.inner.get_cf(&cf, key).unwrap().is_some(),
        "the deleted key should still hold a row before anything reclaims it"
    );

    // Generous: the file has to age past a second and then be picked. It
    // normally goes within five, and a cap that fails is better than a test
    // that waits forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut poke = 1_000u64;

    while db.inner.get_cf(&cf, key).unwrap().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "periodic compaction never reclaimed the emptied row; nothing else \
             will, so rows accumulate for the life of the store"
        );

        std::thread::sleep(std::time::Duration::from_millis(500));

        let other = keys::Compound::new(Default::default(), keys::Key::from_u64_be(poke));
        db.open()
            .put(other, metadata::Metadata::default(), &[2u8; 32])
            .unwrap();
        db.inner.flush_cf(&cf).unwrap();
        poke += 1;
    }
}

/// The filter must only take rows that hold nothing a reader wants.
///
/// It runs on every value at the bottom, so a mistake in `is_fully_dropped`
/// deletes live data during a routine compaction, with no tombstone and nothing
/// to say it happened. This is the same walk to the bottom as the test above,
/// against a key that was never deleted.
#[test]
fn test_log_compaction_filter_keeps_a_live_row_at_the_bottom() {
    let (_dir, db) = crate::tests::temp_db_with_levels(FLAG_DB_LOG_ENABLE, 2);
    let cf = db.inner.cf_handle("default").unwrap();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));
    let written = [7u8; 32];
    db.open()
        .put(key, metadata::Metadata::default(), &written)
        .unwrap();

    db.inner.flush_cf(&cf).unwrap();
    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);
    db.inner.compact_range_cf(&cf, None::<&[u8]>, None::<&[u8]>);

    assert_eq!(
        default_cf_rows(&db).0,
        1,
        "the filter removed a row that still holds a value"
    );

    let entry = db
        .open()
        .get(key)
        .expect("a live key should survive compaction at the bottommost level");
    let crate::Value::Vec(value) = entry.value() else {
        panic!("the value should still read back inline");
    };
    assert_eq!(value.as_slice(), &written);
}

fn eventual_removal_abandon(db: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(1));

    let metadata = metadata::Metadata::default();

    let (mut cx, _tag) = db.blobs().begin_write(key, metadata, 8).unwrap();
    assert!(db.blobs().schedule(&mut cx, &[0, 1, 2, 3, 4, 5, 6, 7]));

    drop(cx);

    assert!(db.blobs().begin_write(key, metadata, 8).is_some());
}
