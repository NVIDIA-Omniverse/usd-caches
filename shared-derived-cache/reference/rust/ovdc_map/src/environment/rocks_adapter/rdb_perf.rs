// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use ovdc_trace::metrics::Measure;

use super::metrics;

pub fn rdb_thread_perf_set() -> rocksdb::PerfContext {
    rocksdb::perf::set_perf_stats(rocksdb::PerfStatsLevel::EnableTime);
    let mut cx = rocksdb::PerfContext::default();
    cx.reset();
    cx
}

pub fn rdb_thread_perf_end() {
    rocksdb::perf::set_perf_stats(rocksdb::PerfStatsLevel::Disable);
}

#[derive(Default, Debug)]
pub struct RdbWriteTime {
    /// Time spent writing to WAL
    pub wal: Duration,
    /// Time spent writing to memtable
    pub memtable: Duration,
    /// Time spent waiting for a delay or throttle to clear.
    pub delay: Duration,
    /// Time spent switching memtable/wal and scheduling flushes & compactions.
    pub scheduling_flushes_compactions: Duration,
    /// Time spent on writing a record, excluding the wal, memtable, delay, and scheduling.
    pub pre_and_post: Duration,
    /// Time spent waiting for other threads writing to the database.
    pub write_wait: Duration,
    /// Time spent waiting to acquire the global write lock in RDB.
    pub mutex_acquire: Duration,
}

impl From<&rocksdb::PerfContext> for RdbWriteTime {
    fn from(value: &rocksdb::PerfContext) -> Self {
        let mut time = RdbWriteTime::default();

        // The counters are already reported by `report_put_io_perf` on every put;
        // `report` does not reset them, so measuring here too would double count.
        for (key, value) in iter_report(&value.report(true)) {
            let key = match key {
                "write_wal_time" => &mut time.wal,
                "write_memtable_time" => &mut time.memtable,
                "write_delay_time" => &mut time.delay,
                "write_scheduling_flushes_compactions_time" => {
                    &mut time.scheduling_flushes_compactions
                }
                "write_pre_and_post_process_time" => &mut time.pre_and_post,
                "write_thread_wait_nanos" => &mut time.write_wait,
                "db_mutex_lock_nanos" => &mut time.mutex_acquire,
                _ => continue,
            };
            *key = Duration::from_nanos(value);
        }

        time
    }
}

pub fn iter_report(value: &str) -> impl Iterator<Item = (&str, u64)> {
    value.split(',').filter_map(|y| {
        let mut split = y.trim().split(" = ");
        Some((split.next()?, (split.next()?).parse::<u64>().ok()?))
    })
}

pub fn report_retains_io_perf(perf: &mut rocksdb::perf::PerfContext) {
    for (key, value) in iter_report(&perf.report(true)) {
        metrics::RDB_RETAINS_PERF.measure(value, &ovdc_trace::label!("name": key.to_string()));
    }
    perf.reset();
}

pub fn report_get_io_perf(value: &str) {
    for (key, value) in iter_report(value) {
        metrics::RDB_GET_PERF.measure(value, &ovdc_trace::label!("name": key.to_string()));
    }
}

pub fn report_put_io_perf(value: &str) {
    for (key, value) in iter_report(value) {
        metrics::RDB_PUT_PERF.measure(value, &ovdc_trace::label!("name": key.to_string()));
    }
}
