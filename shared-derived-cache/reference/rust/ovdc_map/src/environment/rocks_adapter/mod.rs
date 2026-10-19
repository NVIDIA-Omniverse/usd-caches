// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
};

mod adapt;
pub mod blobs;
mod cache;
mod config;
pub mod log;
mod merge;
mod metrics;
mod monitor;
mod multi_part;
mod rdb_perf;
pub mod stats;
mod value;
mod write_semaphore;

#[cfg(test)]
mod tests;

use ovdc_bytes::keys;
use ovdc_settings::Cfg;
use ovdc_trace::metrics::Measure;
use rocksdb::{DBWithThreadMode, ErrorKind, MultiThreaded, WriteOptions};
use write_semaphore::WriterCount;

use crate::{Database, GuardMap, PropertyId, PropertyValue, DEFAULT_CF, FLAG_DB_LOG_ENABLE};

use self::adapt::RocksCursor;

static WRITE_NO_SLOWDOWN: LazyLock<WriteOptions> = LazyLock::new(|| {
    let mut opts = WriteOptions::default();
    opts.set_no_slowdown(true);

    opts
});

static WRITE_NO_SLOWDOWN_OR_WAL: LazyLock<WriteOptions> = LazyLock::new(|| {
    let mut opts = WriteOptions::default();
    opts.set_no_slowdown(true);
    opts.disable_wal(true);

    opts
});

type RocksDb = DBWithThreadMode<MultiThreaded>;

/// A Rocks DB [Database] implementation.
#[derive(Clone)]
pub struct Rocks {
    /// Declared first so it drops first: the monitor thread is joined before
    /// the `Arc<RocksDb>` below is released, which keeps `~DBImpl` on the
    /// thread that dropped the last handle instead of letting it land on the
    /// monitor thread during process exit. See [`monitor::MonitorGuard`].
    ///
    /// Held for its `Drop` alone, so nothing ever reads it.
    #[allow(dead_code)]
    monitor_guard: Arc<monitor::MonitorGuard>,
    inner: Arc<RocksDb>,
    cache: cache::Cache,
    multi_part_in_progress: Arc<GuardMap>,
    monitor: Arc<monitor::Monitor>,
    writers: WriterCount,
    flags: u8,
}

impl Database for Rocks {
    fn new(path: &str, db_size_limit: usize, flags: u8, cfg: &Cfg) -> Result<Self, crate::Error> {
        let db_size_limit = db_size_limit as u64;

        let (options, cfs, cache) = config::fs_write_and_load_settings(cfg, flags);

        let inner = Arc::new(ovdc_trace::err!(
            RocksDb::open_cf_descriptors(&options, path, cfs.into_iter()),
            "failed to open rocksdb database"
        )?);

        let monitor = Arc::new(monitor::Monitor::new(path.into(), db_size_limit));
        let monitor_guard = Arc::new(monitor::spawn(monitor.clone(), inner.clone()));

        Ok(Rocks {
            monitor_guard,
            inner,
            cache,
            multi_part_in_progress: Default::default(),
            monitor,
            writers: Default::default(),
            flags,
        })
    }

    fn open(&self) -> &dyn crate::Cursor {
        match self.flags & FLAG_DB_LOG_ENABLE != 0 {
            false => unsafe { &*(self as *const Rocks as *const RocksCursor) },
            true => unsafe { &*(self as *const Rocks as *const log::RocksCursor) },
        }
    }

    fn stat(&self) -> Result<crate::Stat, crate::Error> {
        let all = self.monitor.current();
        let content = all.size_sst_and_blob_files;
        let metadata = all.size_other_files;
        Ok(crate::Stat {
            content,
            metadata,
            total: content + metadata,
        })
    }

    fn close(&self) -> Result<(), crate::Error> {
        self.monitor.close();
        tracing::trace!("close...");
        self.inner.flush()?;
        self.inner.flush_wal(true)?;
        self.inner.cancel_all_background_work(true);
        tracing::trace!("closed");
        Ok(())
    }

    fn properties(&self) -> HashMap<PropertyId, PropertyValue> {
        stats::stat_map(self)
    }

    fn can_write_hint(&self) -> bool {
        !self.monitor.is_writes_stopped()
    }
}

impl Rocks {
    /// Force flush internal database.
    #[allow(unused)]
    pub(crate) fn flush(&self) {
        self.inner.flush().unwrap();
    }

    /// Re-sample the stats [`stat`](Database::stat) reads, instead of waiting
    /// for the monitor's one-second tick.
    ///
    /// The properties underneath are queried live, so after a completed
    /// [`flush`](Self::flush) this observes exactly what the next tick would.
    #[cfg(test)]
    pub(crate) fn refresh_stat(&self) {
        self.monitor.collect(&self.inner);
    }
}

impl From<rocksdb::Error> for crate::Error {
    fn from(value: rocksdb::Error) -> Self {
        let error_name = match value.kind() {
            ErrorKind::TryAgain => {
                metrics::ERROR.measure(1, &ovdc_trace::label!("kind": "rocks_try_again"));
                return Self::Busy;
            }
            ErrorKind::Incomplete => {
                metrics::PUT_DELAY.measure(1, &[]);
                metrics::ERROR.measure(1, &ovdc_trace::label!("kind": "rocks_incomplete"));
                return Self::Busy;
            }
            ErrorKind::NotFound => return Self::NotFound,
            ErrorKind::Corruption => "rocks_corruption",
            ErrorKind::NotSupported => "rocks_not_supported",
            ErrorKind::InvalidArgument => "rocks_invalid_argument",
            ErrorKind::IOError => "rocks_io_error",
            ErrorKind::MergeInProgress => "rocks_merge_in_progress",
            ErrorKind::ShutdownInProgress => "rocks_shutdown_in_progress",
            ErrorKind::TimedOut => "rocks_timed_out",
            ErrorKind::Aborted => "rocks_aborted",
            ErrorKind::Busy => "rocks_busy",
            ErrorKind::Expired => "rocks_expired",
            ErrorKind::CompactionTooLarge => "rocks_compaction_too_large",
            ErrorKind::ColumnFamilyDropped => "rocks_column_family_dropped",
            ErrorKind::Unknown => "rocks_unknown",
        };

        metrics::ERROR.measure(1, &ovdc_trace::label!("kind": error_name));
        Self::Engine(Box::new(value))
    }
}

fn incr_part_number(key: &mut keys::Compound) {
    let mut reserved = key.reserved();
    reserved[0] = reserved[0].saturating_add(1);
    key.set_reserved(reserved);
}
