// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{atomic::AtomicU64, Arc};

use crate::{
    environment::Rocks, metadata::Metadata, BlobProvider, Change, Database, Entry, Update, Value,
    FLAG_DB_BLOB_ENABLE, FLAG_DB_LOG_ENABLE,
};
use bytes::{BufMut, BytesMut};
use ovdc_bytes::keys::{self, Compound};

pub const CFG: &str = include_str!("../../ovdc/src/config.toml");

pub fn default_cfg() -> ovdc_settings::Cfg {
    ovdc_settings::load_document_with(CFG, |_| {})
}

pub fn temp_db_flags(flags: u8) -> (tempfile::TempDir, Rocks) {
    let tempdir = tempfile::tempdir().unwrap();
    let map = Rocks::new(
        tempdir.path().to_string_lossy().as_ref(),
        32_000_000,
        flags,
        &default_cfg(),
    )
    .unwrap();
    (tempdir, map)
}

pub fn temp_db() -> (tempfile::TempDir, Rocks) {
    temp_db_flags(0)
}

/// A database with a shallow LSM tree, so a test can reach the bottommost level.
///
/// The compaction filter only acts at the bottom, which with the shipped
/// `num_levels = 7` means a store has to grow through six levels before the
/// filter is offered anything -- far more data than a test writes. Lowering the
/// count moves the bottom within reach, so two compactions are enough to put a
/// row there and then feed it to the filter.
pub fn temp_db_with_levels(flags: u8, num_levels: i64) -> (tempfile::TempDir, Rocks) {
    let tempdir = tempfile::tempdir().unwrap();
    let cfg = ovdc_settings::load_document_with(CFG, |doc| {
        let engine = doc["engine"].as_table_mut().expect("[engine] section");
        engine["cf"]["num_levels"] = toml_edit::value(num_levels);

        // Dynamic sizing keeps the tree's occupied levels packed against the
        // bottom, which decides where a compaction's output lands. Fixing the
        // levels makes the placement -- and so which compaction the filter sees
        // -- follow from the compactions the test runs rather than from how
        // much data happens to be present.
        engine["cf"]["level_compaction_dynamic_level_bytes"] = toml_edit::value(false);
    });

    let map = Rocks::new(
        tempdir.path().to_string_lossy().as_ref(),
        32_000_000,
        flags,
        &cfg,
    )
    .unwrap();
    (tempdir, map)
}

/// A shallow tree whose files are rewritten once they reach `seconds` old.
///
/// `periodic_compaction_seconds` is what drives rows down to the bottommost
/// level on a store that is not growing, and so the only thing that gets an
/// emptied row in front of the compaction filter when write pressure never
/// does. The shipped value is eight hours; a test needs one it can wait out.
pub fn temp_db_compacting_every(
    flags: u8,
    num_levels: i64,
    seconds: i64,
) -> (tempfile::TempDir, Rocks) {
    let tempdir = tempfile::tempdir().unwrap();
    let cfg = ovdc_settings::load_document_with(CFG, |doc| {
        let engine = doc["engine"].as_table_mut().expect("[engine] section");
        engine["cf"]["num_levels"] = toml_edit::value(num_levels);
        engine["cf"]["level_compaction_dynamic_level_bytes"] = toml_edit::value(false);
        engine["cf"]["periodic_compaction_seconds"] = toml_edit::value(seconds);
    });

    let map = Rocks::new(
        tempdir.path().to_string_lossy().as_ref(),
        32_000_000,
        flags,
        &cfg,
    )
    .unwrap();
    (tempdir, map)
}

/// A database that reports back-pressure once `budget` bytes of memtable are
/// live, so a test can reach the stall paths on demand.
///
/// RocksDB's L0 and pending-compaction stall causes are all gated on
/// `!disable_auto_compactions`, so turning compaction off to let files pile up
/// suppresses those stalls rather than provoking them. The write buffer manager
/// is the usable lever: its threshold is a function of bytes held in memtables,
/// not of when a flush happens to run, so a test built on it is deterministic.
pub fn temp_db_stalling_at(flags: u8, budget: u64) -> (tempfile::TempDir, Rocks) {
    let tempdir = tempfile::tempdir().unwrap();
    let cfg = ovdc_settings::load_document_with(CFG, |doc| {
        let engine = doc["engine"].as_table_mut().expect("[engine] section");
        engine["useWriteBufferManager"] = toml_edit::value(true);
        engine["writeBufferManagerSize"] = toml_edit::value(budget as i64);
        engine["writeBufferManagerAllowStall"] = toml_edit::value(true);

        // The manager counts memtable arena allocations rather than payload
        // bytes, and an arena block is an eighth of write_buffer_size. Left at
        // the shipped 64M, the first block of the first memtable dwarfs any
        // budget small enough to reach in a test, so the database would arrive
        // already stalled and a test could not set anything up. The blobs
        // column family is configured from the generated defaults rather than
        // from `cf`, so it needs saying twice.
        engine["cf"]["write_buffer_size"] = toml_edit::value(64 * 1024i64);
        engine["blobsCf"]["write_buffer_size"] = toml_edit::value(64 * 1024i64);
    });

    let map = Rocks::new(
        tempdir.path().to_string_lossy().as_ref(),
        32_000_000,
        flags,
        &cfg,
    )
    .unwrap();
    (tempdir, map)
}

/// The stall knobs have to actually reach RocksDB, or every test built on them
/// would pass by never provoking anything.
#[test]
fn test_write_buffer_manager_stall_surfaces_as_busy() {
    let (_dir, map) = temp_db_stalling_at(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE, 64 * 1024);

    let value = [3u8; 4096];
    let mut busy = None;

    for i in 0..512u64 {
        let key = Compound::new(Default::default(), keys::Key::from_u64_be(i));
        let cursor = map.open();
        match cursor.put(
            key,
            Metadata {
                ttl_ms: None,
                access_time_secs: 0,
                created_secs: 0,
            },
            &value,
        ) {
            Ok(()) => continue,
            Err(crate::Error::Busy) => {
                busy = Some(i);
                break;
            }
            Err(e) => panic!("unexpected error at write {i}: {e:?}"),
        }
    }

    assert!(
        busy.is_some(),
        "writing 2MB against a 64KB memtable budget never reported Busy; \
         the write buffer manager settings are not reaching RocksDB"
    );
}

/// Writes until the engine reports back-pressure, so a test starts from a
/// stalled database rather than assuming one.
pub fn fill_until_busy(map: &Rocks, from: u64) {
    let value = [5u8; 4096];
    for i in from..from + 512 {
        let key = Compound::new(Default::default(), keys::Key::from_u64_be(i));
        let cursor = map.open();
        match cursor.put(
            key,
            Metadata {
                ttl_ms: None,
                access_time_secs: 0,
                created_secs: 0,
            },
            &value,
        ) {
            Ok(()) => continue,
            Err(crate::Error::Busy) => return,
            Err(e) => panic!("unexpected error while filling: {e:?}"),
        }
    }
    panic!("never reached back-pressure");
}

/// Runs a rejecting `retain` with a deadline.
///
/// What these tests guard is the *bound* on GC's Busy retry, and losing it does
/// not produce a wrong answer -- it produces no answer. Called directly, the
/// test would hang, which in CI is a job timeout with no failing test name and
/// reads as a slow runner rather than as the regression it is. The scan runs on
/// its own thread so the deadline can fail the test instead.
fn retain_within(map: &Rocks, deadline: std::time::Duration) {
    let (tx, rx) = std::sync::mpsc::channel();

    let scan = map.clone();
    std::thread::spawn(move || {
        // A send error means the deadline already passed and nobody is
        // listening, which the receiver has reported.
        let _ = tx.send(scan.open().retain(Box::new(|_| false)));
    });

    rx.recv_timeout(deadline)
        .expect("retain never returned under back-pressure; the Busy retry is unbounded again")
        .expect("retain reported an error under back-pressure");
}

/// GC must survive running while the engine is stalling -- which is when it
/// runs, since that is when space needs reclaiming.
///
/// Every write `retain` makes can now return Busy rather than blocking, and it
/// used to assert on one of them and unwrap another. Under `panic = 'abort'`
/// either would have taken the process down on a background thread.
/// Deliberately no set-up beyond filling: anything written before the fill
/// competes with the same budget, so whether it lands depends on how promptly a
/// flush happens to run, and the test turns flaky under load.
#[test]
fn test_retain_survives_back_pressure() {
    let (_dir, map) = temp_db_stalling_at(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE, 256 * 1024);

    fill_until_busy(&map, 0);

    // Reject everything, so the scan writes a drop marker for every key it sees
    // and takes the retry path rather than idling. It has to come back: the
    // retries are bounded precisely because this scan is what relieves the
    // stall it is running under.
    retain_within(&map, std::time::Duration::from_secs(60));
}

/// The same, for the backend that does not use the log.
///
/// It reclaims a key by removing the blob parts and then deleting the row, and
/// both write with `no_slowdown`, so a stall answers Busy on every attempt.
/// That retry was unbounded, so the scan parked on the first key it tried to
/// collect and never came back -- wedging GC exactly when it is the thing that
/// relieves the stall.
#[test]
fn test_blob_retain_survives_back_pressure() {
    let (_dir, map) = temp_db_stalling_at(FLAG_DB_BLOB_ENABLE, 256 * 1024);

    fill_until_busy(&map, 0);

    retain_within(&map, std::time::Duration::from_secs(60));
}

/// Generates tests for multiple db operating modes.
#[macro_export]
macro_rules! multi_mode {
    {@
        $name:ident =>
        {
            $($db_name:ident = $db_expr:expr),*
        }
    } => {
        pastey::paste!{
        $(
        #[test]
        fn [< test_ $db_name _ $name >]() {
            let (_dir, db) = $db_expr;
            $name(&db);
        }
        )*
        }
    };
    {
        test($($name:ident),*)
        =>
        $tokens:tt
    } => {
        $(
            multi_mode!{@ $name => $tokens}
        )*
    };
}

multi_mode! {
    test(
        that_stat_increases,
        that_metadata_is_udpdated,
        map_that_put_value_is_get_value,
        put_honours_durability_flags,
        expired_keys_not_served,
        retain_deletes,
        multi_part,
        that_finished_stream_is_read_in_compaction,
        multi_part_rewrite,
        mp_keys_get_stuck,
        cursor_write_and_read,
        delete_value,
        change_value,
        value_expired
    ) =>
    {
        default = { temp_db() },
        blob = { temp_db_flags(FLAG_DB_BLOB_ENABLE) },
        log = { temp_db_flags(FLAG_DB_LOG_ENABLE | FLAG_DB_BLOB_ENABLE) }
    }
}

fn that_stat_increases(map: &Rocks) {
    let mut ids = (0..).map(|y| keys::Compound::new(Default::default(), keys::Key::from_u64_be(y)));
    let mut last = 0;
    for _ in 0..3 {
        for key in ids.by_ref().take(32) {
            let mut seed = key.as_ref().iter().fold(0xcbf29ce484222325, |acc, byte| {
                (acc ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
            let value: Vec<_> = (0..8 * 1024)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 56) as u8
                })
                .collect();
            let cursor = map.open();
            cursor
                .put(
                    key,
                    Metadata {
                        ttl_ms: None,
                        access_time_secs: 0,
                        created_secs: 0,
                    },
                    &value,
                )
                .unwrap();
        }
        map.flush();

        // `stat()` reports a snapshot the monitor refreshes on a one-second
        // tick. The properties underneath are read live and `flush()` blocks
        // until the SST lands, so re-sample directly rather than sleeping for
        // the tick to come around.
        map.refresh_stat();

        // Compare only the SST/blob `content` term: a flush writes new data to
        // disk there, whereas the `metadata`/WAL term is non-monotonic (a flush
        // can recycle WAL files, so the total can stay flat even as data grows).
        //
        // Propagate a stat error rather than defaulting to 0: a failed read and
        // a database that did not grow are different bugs, and collapsing them
        // makes the assertion message unable to tell them apart.
        let next = map.stat().expect("stat() after flush").content;
        assert!(last < next, "content did not grow: {last} -> {next}");
        last = next;
    }
}

fn that_metadata_is_udpdated(map: &Rocks) {
    let key = Compound::new(
        Default::default(),
        keys::Key::new(&0u64.to_ne_bytes()).unwrap(),
    );

    {
        let cursor = map.open();
        cursor
            .put(
                key,
                Metadata {
                    ttl_ms: None,
                    access_time_secs: 0,
                    created_secs: 0,
                },
                &[120u8; 32],
            )
            .unwrap();
    }

    {
        let cursor = map.open();
        cursor
            .update(
                key,
                Update {
                    access_time: Change::Set(1000),
                    ttl_ms: Change::No,
                },
            )
            .unwrap();
    }

    map.close().unwrap();

    /*
     * This does not work
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    ovdc_trace::Builder::new(ovdc_trace::LogFormat::Human).init();

    let inner = {
        let map = Rocks::new(
            tempdir.path().to_string_lossy().as_ref(),
            32_000_000,
            &Default::default(),
        )
        .unwrap();

        let inner = Arc::downgrade(&map.inner);

        let cursor = map.open().unwrap();
        let next = cursor.get(key).unwrap();

        assert_eq!(next.metadata().access_time_secs, 1000);

        map.close().unwrap();

        inner
    };

    let metrics = tokio::runtime::Handle::current().metrics();


    for _ in 0..60 {
    let num_idle = metrics.num_idle_blocking_threads();
    let num_blocking = metrics.num_blocking_threads();
    let num_active_tasks = metrics.active_tasks_count();

    let strong_count = inner.strong_count();
    let weak_count = inner.weak_count();

    println!("end {num_idle} {num_blocking} {num_active_tasks}, {strong_count} {weak_count}");
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    */
}

fn map_that_put_value_is_get_value(map: &Rocks) {
    let key = Compound::new(
        Default::default(),
        keys::Key::new(&0u64.to_ne_bytes()).unwrap(),
    );

    {
        let mut bytes = BytesMut::new();
        bytes.put_slice(&[1u8; 120]);

        let cursor = map.open();
        cursor
            .put(
                key,
                Metadata {
                    ttl_ms: None,
                    access_time_secs: 0,
                    created_secs: 0,
                },
                &bytes[..],
            )
            .unwrap();
    }

    let cursor = map.open();
    let entry = cursor.get(key).unwrap();
    assert_eq!(entry.value().slice().unwrap(), &[1u8; 120]);
}

/// A write must round-trip whichever durability the caller asked for.
///
/// The log backend used to write with default options and never consult
/// `thread_write_options`, so its low-durability path -- the one that disables
/// the WAL -- was never exercised at all.
fn put_honours_durability_flags(map: &Rocks) {
    for (i, flags) in [0u8, crate::FLAGS_HIGHER_DURABILITY]
        .into_iter()
        .enumerate()
    {
        let key = Compound::new(Default::default(), keys::Key::from_u64_be(900 + i as u64));
        let value = [7u8; 128];

        {
            let _guard = crate::thread_local_flags(flags);
            let cursor = map.open();
            cursor
                .put(
                    key,
                    Metadata {
                        ttl_ms: None,
                        access_time_secs: 0,
                        created_secs: 0,
                    },
                    &value,
                )
                .unwrap();
        }

        let cursor = map.open();
        let entry = cursor
            .get(key)
            .unwrap_or_else(|e| panic!("value written with flags {flags} was not readable: {e:?}"));
        assert_eq!(entry.value().slice().unwrap(), &value[..], "flags {flags}");
    }
}

fn expired_keys_not_served(map: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(0));

    {
        let mut bytes = BytesMut::new();
        bytes.put_slice(&[1u8; 1]);

        let cursor = map.open();
        cursor
            .put(
                key,
                Metadata {
                    ttl_ms: Some(1),
                    access_time_secs: 123,
                    created_secs: 0,
                },
                &bytes[..],
            )
            .unwrap();
    }

    let exists = {
        let cursor = map.open();
        cursor.get(key).is_ok()
    };
    assert!(!exists);
}

fn retain_deletes(map: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(0));

    {
        let mut bytes = BytesMut::new();
        bytes.put_slice(&[1u8; 1]);

        let cursor = map.open();
        cursor
            .put(
                key,
                Metadata {
                    ttl_ms: Some(1),
                    access_time_secs: 123,
                    created_secs: 0,
                },
                &bytes[..],
            )
            .unwrap();
    }

    map.open().retain(Box::new(|_| false)).unwrap();

    let exists = {
        let cursor = map.open();
        cursor.exists(key).is_ok()
    };
    assert!(!exists);
}

fn multi_part(map: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(0));
    let metadata = Metadata {
        ttl_ms: None,
        created_secs: 0,
        access_time_secs: 0,
    };

    let buff = [0u8; 66];

    let (mut w_blob, tag) = map.blobs().begin_write(key, metadata, 256).unwrap();

    assert!(map.blobs().schedule(&mut w_blob, &buff[..63]));
    assert!(map.blobs().schedule(&mut w_blob, &buff[..65]));
    assert!(map.blobs().schedule(&mut w_blob, &buff[..64]));
    assert!(map.blobs().schedule(&mut w_blob, &buff[..64]));
    map.blobs().done(&mut w_blob).unwrap();

    let mut cx = map.blobs().begin_read(key, tag);

    let mut rebuilt = vec![];
    loop {
        if map.blobs().poll_read(&mut cx).is_err() {
            break;
        }
        rebuilt.extend_from_slice(map.blobs().next(&mut cx).unwrap());
    }
    assert_eq!(rebuilt.len(), 256);
}

fn that_finished_stream_is_read_in_compaction(map: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(0));
    let metadata = Metadata {
        ttl_ms: None,
        created_secs: 0,
        access_time_secs: 0,
    };

    let buff = [0u8; 64];

    {
        let (mut w_ctx, _tag) = map.blobs().begin_write(key, metadata, 128).unwrap();
        map.blobs().schedule(&mut w_ctx, &buff[..64]);
        map.blobs().schedule(&mut w_ctx, &buff[..64]);
        map.blobs().done(&mut w_ctx).unwrap();
    }

    {
        let cursor = map.open();

        let counter = Arc::new(AtomicU64::new(0));

        let c0 = counter.clone();
        cursor
            .retain(Box::new(move |_entry| {
                c0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                false
            }))
            .unwrap();

        assert_eq!(counter.load(std::sync::atomic::Ordering::Acquire), 1);
    }

    {
        let cursor = map.open();
        assert!(cursor.get(key).is_err());
    }
}

fn multi_part_rewrite(map: &Rocks) {
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(0));

    {
        let (mut w_ctx, _tag) = map
            .blobs()
            .begin_write(
                key,
                Metadata {
                    ttl_ms: None,
                    created_secs: 0,
                    access_time_secs: 0,
                },
                6,
            )
            .unwrap();
        map.blobs().schedule(&mut w_ctx, &[3, 4]);
    }

    let cursor = map.open();
    assert!(cursor.get(key).is_err());
    assert!(cursor.get_non_blocking(key).is_err());

    let tag = {
        let (mut w_ctx, tag) = map
            .blobs()
            .begin_write(
                key,
                Metadata {
                    ttl_ms: None,
                    created_secs: 0,
                    access_time_secs: 0,
                },
                6,
            )
            .unwrap();

        map.blobs().schedule(&mut w_ctx, &[1, 2]);
        map.blobs().schedule(&mut w_ctx, &[3, 4]);
        map.blobs().schedule(&mut w_ctx, &[5, 6]);
        map.blobs().done(&mut w_ctx).unwrap();
        tag
    };

    let mut cx = map.blobs().begin_read(key, tag);

    let mut rebuilt = vec![];
    loop {
        if map.blobs().poll_read(&mut cx).is_err() {
            break;
        }
        rebuilt.extend_from_slice(map.blobs().next(&mut cx).unwrap());
    }
    assert_eq!(rebuilt.len(), 6);
}

fn mp_keys_get_stuck(map: &Rocks) {
    let mut threads = vec![];
    // TODO recheck this logic
    for _ in 0..3 {
        let db = map.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..1000 {
                std::thread::yield_now();
                std::thread::yield_now();
                'x: for i in 0..8 {
                    std::thread::yield_now();

                    let Some((mut w_ctx, _tag)) = db.blobs().begin_write(
                        keys::Compound::new(Default::default(), keys::Key::from_u64_be(i)),
                        Metadata::default(),
                        10,
                    ) else {
                        break;
                    };

                    for _ in 0..9 {
                        std::thread::yield_now();
                        if db.blobs().schedule(&mut w_ctx, &[0]) {
                            continue 'x;
                        }
                    }

                    db.blobs().done(&mut w_ctx).unwrap();
                }
            }
        });
        threads.push(handle);
    }

    for handle in threads {
        handle.join().unwrap();
    }

    for i in 0..8 {
        let cursor = map.open();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(i));

        let entry = match cursor.get(key) {
            Ok(entry) => entry,
            Err(crate::Error::NotFound) => continue,
            Err(e) => panic!("get failed with {e}"),
        };

        let Value::Blob { size, next, .. } = entry.value() else {
            panic!("invalid start");
        };

        assert_eq!(*size, 10);

        let mut next = *next;
        for _ in 0..9 {
            let entry = cursor.get(next).unwrap();
            next = entry.value().next_blob_key().unwrap();
        }
    }
}

fn cursor_write_and_read(db: &Rocks) {
    let cursor = db.open();
    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
    cursor.put(key, Default::default(), b"hello").unwrap();
    assert!(matches!(cursor.get(key).unwrap().value(), Value::Vec(x) if **x == b"hello"));
}

fn delete_value(db: &Rocks) {
    let cursor = db.open();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
    cursor.put(key, Default::default(), b"hello").unwrap();
    cursor.delete(key).unwrap();
    cursor
        .get(key)
        .expect_err("value must be removed after deletion");
}

fn change_value(db: &Rocks) {
    let cursor = db.open();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
    cursor.put(key, Default::default(), b"hello").unwrap();
    assert!(matches!(cursor.get(key).unwrap().value(), Value::Vec(x) if **x == b"hello"));

    cursor.delete(key).unwrap();

    cursor.put(key, Default::default(), b"world").unwrap();
    assert_eq!(
        cursor
            .get(key)
            .unwrap()
            .value()
            .slice()
            .map(std::str::from_utf8)
            .unwrap()
            .unwrap(),
        "world"
    );

    cursor.put(key, Default::default(), b"changed").unwrap();
    assert_eq!(
        cursor
            .get(key)
            .unwrap()
            .value()
            .slice()
            .map(std::str::from_utf8)
            .unwrap()
            .unwrap(),
        "changed"
    );
}

fn value_expired(db: &Rocks) {
    let cursor = db.open();

    let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
    cursor
        .put(
            key,
            crate::metadata::Metadata {
                ttl_ms: Some(1),
                created_secs: crate::now_secs() - 1,
                access_time_secs: 0,
            },
            b"hello",
        )
        .unwrap();
    cursor.get(key).expect_err("value must not be available");
}
