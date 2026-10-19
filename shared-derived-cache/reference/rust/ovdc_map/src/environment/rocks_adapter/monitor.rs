// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Weak,
    },
    time::{Duration, Instant},
};

use rocksdb::FlushOptions;

use ovdc_trace::metrics::Measure;

use super::metrics;
use crate::PropertyKind;

/// The RocksDB property listing every SST and blob file, with its size.
pub(crate) const PROP_SSTABLES: &str = "rocksdb.sstables";

/// Unwraps one `rocksdb.sstables` read, reporting rather than panicking.
///
/// `Ok(None)` means RocksDB does not recognise the property name, which is a
/// programming error rather than a transient one -- but it is still not worth a
/// process abort from a metrics thread, so it is reported the same way.
fn sstables(result: Result<Option<String>, rocksdb::Error>, cf: &str) -> Option<String> {
    match result {
        Ok(Some(value)) => Some(value),
        Ok(None) => {
            report(
                &format!("rocksdb does not know the property {PROP_SSTABLES}"),
                cf,
            );
            None
        }
        Err(error) => {
            report(&format!("failed to read {PROP_SSTABLES}: {error}"), cf);
            None
        }
    }
}

fn report(reason: &str, cf: &str) {
    metrics::ERROR.measure(1, &ovdc_trace::label!("kind": "monitor_collect_failed"));
    tracing::error!("{reason} on column family {cf}; capacity stats are stale this cycle");
}

#[derive(Default, Clone, Copy, Debug)]
#[allow(unused)]
pub struct Status {
    pub size_mem_tables: u64,
    pub size_sst_and_blob_files: u64,
    pub size_other_files: u64,
    pub is_write_stopped: bool,
    pub is_write_stalled: bool,
}

pub struct Monitor {
    stats: parking_lot::Mutex<Status>,
    thread: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
    condvar: parking_lot::Condvar,
    hint: AtomicBool,
    path: PathBuf,
    max: u64,
}

impl Monitor {
    pub fn new(path: PathBuf, max: u64) -> Self {
        Self {
            path,
            max,
            stats: Default::default(),
            thread: Default::default(),
            condvar: Default::default(),
            hint: Default::default(),
        }
    }

    pub fn current(&self) -> Status {
        *self.stats.lock()
    }

    pub fn is_writes_stopped(&self) -> bool {
        self.hint.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        let task = self.thread.lock().take();
        self.condvar.notify_all();
        if let Some(task) = task {
            let _ = task.join();
        }
    }

    /// Refreshes the cached [`Status`].
    ///
    /// This runs once a second on a background thread, so it must not panic: the
    /// release profile sets `panic = 'abort'`, and even without that a dead
    /// monitor freezes [`Monitor::current`] at whatever it last saw, leaving GC's
    /// capacity trigger and the write-stop hint permanently stale.
    ///
    /// A failed read is therefore skipped rather than substituted with zero --
    /// reporting "no files on disk" would tell GC it has nothing to reclaim.
    pub(crate) fn collect(&self, rocks: &super::RocksDb) {
        let Some(files_input) = sstables(rocks.property_value(PROP_SSTABLES), "default") else {
            return;
        };

        // An absent blobs column family and a failed read of one both used to
        // fold into `None` here, which contributes zero bytes. Only the first is
        // "nothing on disk": in log and blob modes the blobs family holds nearly
        // all of it, so treating a failed read as empty under-reports capacity
        // by almost everything, `is_over_max` goes false, and GC stops firing on
        // a disk that is filling. The default family above answers that case by
        // skipping the cycle; this one has to as well.
        let files_input_blobs_cf = match rocks.cf_handle(crate::BLOBS_CF) {
            Some(cf) => {
                let Some(listing) =
                    sstables(rocks.property_value_cf(&cf, PROP_SSTABLES), crate::BLOBS_CF)
                else {
                    return;
                };
                Some(listing)
            }
            None => None,
        };
        let files_input_blobs_cf = files_input_blobs_cf
            .as_ref()
            .map(|y| super::stats::iter_files(y));

        let files = super::stats::iter_files(&files_input)
            .chain(files_input_blobs_cf.into_iter().flatten());

        let size_sst_and_blob_files: u64 = files.map(|y| y.size as u64).sum();
        let size_other_files = non_sst_or_blob_calc(&self.path);

        let size_mem_tables = rocks
            .property_int_value(super::stats::PROP_ROCKSDB_SIZE_ALL_MEM_TABLES.name())
            .ok()
            .unwrap_or_default()
            .unwrap_or_default();

        let is_write_stopped = rocks
            .property_int_value(super::stats::PROP_ROCKSDB_IS_WRITE_STOPPED.name())
            .ok()
            .unwrap_or_default()
            .unwrap_or_default()
            == 1;
        let is_write_stalled = rocks
            .property_int_value(super::stats::PROP_ROCKSDB_ACTUAL_DELAYED_WRITE_RATE.name())
            .ok()
            .unwrap_or_default()
            .unwrap_or_default()
            != 0;

        let stats = Status {
            size_mem_tables,
            size_sst_and_blob_files,
            size_other_files,
            is_write_stopped,
            is_write_stalled,
        };

        let is_over_max = self.max < (size_sst_and_blob_files + size_other_files + size_mem_tables);

        *self.stats.lock() = stats;
        self.hint.store(
            is_over_max || is_write_stopped || is_write_stalled,
            Ordering::Release,
        );
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.thread.lock().take();
        self.condvar.notify_all();
    }
}

/// Joins the monitor thread when the last [`Rocks`](super::Rocks) handle goes
/// away, so that no thread can still be inside RocksDB once `main` returns.
///
/// Without this the monitor thread outlives every handle to the database. It
/// re-`upgrade`s its [`Weak`] on each tick, so it can become the *last* owner of
/// the `Arc<RocksDb>` and then run `~DBImpl` itself -- during the process exit
/// phase, concurrently with C++ static destruction. `~DBImpl` releases the LOCK
/// file through `PosixFileSystem::UnlockFile`, which takes a file-scope
/// `static port::Mutex`; once that static has been destroyed the lock returns
/// `EINVAL`, and RocksDB's `PthreadCall` turns that into
/// `pthread lock: Invalid argument` followed by `abort()`. The test that was
/// running has already reported `ok` by then, so it surfaces as a spurious
/// SIGABRT attributed to whichever test happened to be last.
///
/// [`Monitor::drop`] cannot do this job: the monitor thread owns an
/// `Arc<Monitor>`, so the refcount cannot reach zero while the thread is alive.
/// This guard is deliberately *not* held by the thread, which both breaks that
/// cycle and guarantees the join never runs on the monitor thread itself.
pub struct MonitorGuard(Arc<Monitor>);

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

pub fn spawn(monitor: Arc<Monitor>, rocks: Arc<super::RocksDb>) -> MonitorGuard {
    let rocks_weak = Arc::downgrade(&rocks);
    let mt = monitor.clone();

    monitor.collect(&rocks);

    let thread = std::thread::spawn(move || {
        background(rocks_weak, mt);
    });
    *monitor.thread.lock() = Some(thread);

    MonitorGuard(monitor)
}

fn background(rocks: Weak<super::RocksDb>, monitor: Arc<Monitor>) {
    let mut last_flush_called = Instant::now();
    loop {
        let Some(rocks) = rocks.upgrade() else {
            return;
        };

        monitor.collect(&rocks);

        if last_flush_called.elapsed() > Duration::from_secs(5) {
            let mut opts = FlushOptions::new();
            opts.set_wait(false);
            let _ = rocks.flush_opt(&opts);
            last_flush_called = Instant::now();
        }

        let mut guard = monitor.thread.lock();
        if guard.is_none()
            || !monitor
                .condvar
                .wait_for(&mut guard, Duration::from_secs(1))
                .timed_out()
        {
            return;
        }
    }
}

/// Sums everything in the database directory that is not an SST or blob file --
/// the WAL, the MANIFEST, and so on -- which the `rocksdb.sstables` property
/// does not account for.
///
/// Directory reads race with RocksDB's own file churn: an entry can be unlinked
/// between the listing and the `stat`. Every such error is skipped, because the
/// caller runs on the monitor thread where a panic aborts the process.
fn non_sst_or_blob_calc(path: &std::path::Path) -> u64 {
    let paths = match std::fs::read_dir(path) {
        Ok(paths) => paths,
        Err(error) => {
            report(&format!("failed to list {}: {error}", path.display()), "-");
            return 0;
        }
    };

    let mut total_size = 0;

    for path in paths {
        let Ok(path) = path else {
            continue;
        };
        let path = path.path();

        if matches!(path.extension(), Some(y) if y == "blob" || y == "sst") {
            continue;
        }

        let Ok(path_metadata) = std::fs::metadata(path) else {
            continue;
        };
        let size = path_metadata.size();

        total_size += size;
    }

    total_size
}
