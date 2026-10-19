// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides stat and property collection for RocksDb.

use std::{collections::HashMap, marker::PhantomData, str::Lines};

use crate::{PropertyId, PropertyKind, PropertyValue, Sealed, BLOBS_CF, DEFAULT_CF};

use super::Rocks;

/// Column families whose per-family properties are exported separately.
///
/// `blobs` exists only under `FLAG_DB_BLOB_ENABLE`; `cf_handle` returning
/// `None` is how that is detected, so no flag check is needed here.
const EXPORTED_CFS: [&str; 2] = [DEFAULT_CF, BLOBS_CF];

/// A specific RocksDb property.
#[repr(transparent)]
pub struct RocksProperty<T>(&'static str, PhantomData<T>);

impl<T> Sealed for RocksProperty<T> {}

impl<T> PropertyKind for RocksProperty<T> {
    type Kind = T;

    #[inline(always)]
    fn name(&self) -> &'static str {
        self.0
    }
}

pub(crate) fn stat_map(db: &Rocks) -> HashMap<PropertyId, crate::PropertyValue> {
    let mut map: HashMap<PropertyId, crate::PropertyValue> = Default::default();

    let blobs_cf = db.inner.cf_handle(BLOBS_CF);

    if let Ok(Some(files)) = db.inner.property_value(super::monitor::PROP_SSTABLES) {
        let files_input_blobs_cf = blobs_cf.as_ref().and_then(|cf| {
            db.inner
                .property_value_cf(cf, super::monitor::PROP_SSTABLES)
                .ok()
                .flatten()
        });
        let files_input_blobs_cf = files_input_blobs_cf
            .as_ref()
            .map(|y| super::stats::iter_files(y));

        map_files(
            &mut map,
            super::stats::iter_files(&files).chain(files_input_blobs_cf.into_iter().flatten()),
        );
    }

    if let Ok(Some(input)) = db.inner.property_value("rocksdb.options-statistics") {
        optional_stats(&mut map, &input);
    }

    // Write stalls are per column family: `rocksdb.cf-write-stall-stats` reports
    // only the family it is asked about. Reading `default` alone reported zero
    // for every stall the blobs family caused -- which is where the large values
    // land, so it is the family most likely to stall.
    for cf_name in EXPORTED_CFS {
        let Some(stalls) = db.inner.cf_handle(cf_name).and_then(|cf| {
            db.inner
                .property_value_cf(&cf, "rocksdb.cf-write-stall-stats")
                .ok()
                .flatten()
        }) else {
            continue;
        };

        map_write_stalls(&mut map, &stalls, cf_name);
    }

    if let Ok(Some(input)) = db
        .inner
        .property_value("rocksdb.fast-block-cache-entry-stats")
    {
        let _ = parse_block_cache_stats(&mut map, &input);
    }

    rdb_properties(&mut map, db);

    map
}

pub(crate) enum FileKind {
    Sst(u8),
    Blob(u64),
}

pub(crate) struct FileStat {
    #[allow(unused)]
    pub id: u64,
    pub kind: FileKind,
    pub size: usize,
}

pub(crate) fn map_files(
    map: &mut HashMap<PropertyId, PropertyValue>,
    input: impl Iterator<Item = FileStat>,
) {
    let mut estimated_size_all = 0u64;
    let mut estimated_size_sst = 0u64;

    // Sized for the seven levels RocksDB uses by default. `engine.cf` is a
    // free-form map handed to RocksDB verbatim, so an operator can set
    // `num_levels` higher than this; levels past the end are folded into the
    // last bucket rather than indexed out of bounds. This runs on the
    // metrics-scrape path, where `panic = 'abort'` makes a bad index fatal.
    let mut estimated_size_sst_by_level = [0u64; 8];

    let mut estimated_size_blob = 0u64;
    let mut estimated_blob_entries = 0u64;

    let mut num_files = 0u64;
    let mut num_blobs = 0u64;
    let mut num_sst = 0u64;
    let mut num_sst_l0 = 0u64;

    for file in input {
        num_files += 1;

        match file.kind {
            FileKind::Sst(level) => {
                if level == 0 {
                    num_sst_l0 += 1;
                }

                num_sst += 1;
                let level = (level as usize).min(estimated_size_sst_by_level.len() - 1);
                estimated_size_sst_by_level[level] += file.size as u64;
                estimated_size_sst += file.size as u64;
                estimated_size_all += file.size as u64;
            }
            FileKind::Blob(entries) => {
                num_blobs += 1;

                estimated_size_blob += file.size as u64;
                estimated_size_all += file.size as u64;
                estimated_blob_entries += entries;
            }
        }
    }

    map.insert(
        PropertyId::database("__rocksdb_tables_sst_and_blob_num"),
        num_files.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_blobs_num"),
        num_blobs.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_sst_num"),
        num_sst.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_sst_l0_num"),
        num_sst_l0.into(),
    );

    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_and_blob_size"),
        estimated_size_all.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_size"),
        estimated_size_sst.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_blob_size"),
        estimated_size_blob.into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_blob_entries"),
        estimated_blob_entries.into(),
    );

    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l0"),
        estimated_size_sst_by_level[0].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l1"),
        estimated_size_sst_by_level[1].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l2"),
        estimated_size_sst_by_level[2].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l3"),
        estimated_size_sst_by_level[3].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l4"),
        estimated_size_sst_by_level[4].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l5"),
        estimated_size_sst_by_level[5].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l6"),
        estimated_size_sst_by_level[6].into(),
    );
    map.insert(
        PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l7"),
        estimated_size_sst_by_level[7].into(),
    );
}

pub(crate) fn iter_files(input: &str) -> impl Iterator<Item = FileStat> + '_ {
    let mut level = 0u8;
    let mut scan_sst_files = true;

    input.lines().filter_map(move |line| {
        if line.starts_with("--- level ") {
            level = level.saturating_add(1);
            return None;
        } else if line.starts_with("--- blob files") {
            // Blob info is included last.
            scan_sst_files = false;
            return None;
        }

        if !scan_sst_files {
            // A blob line looks like the following.
            // "blob_file_number: 195 total_blob_count: 224 total_blob_bytes: 60783700 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0"

            let mut words = line.split(' ');
            let blob_id_str = words.nth(1)?;
            let blob_id: u64 = blob_id_str.parse().ok()?;
            let blob_count_str = words.nth(1)?;
            let blob_count: u64 = blob_count_str.parse().ok()?;
            let blob_size_str = words.nth(1)?;
            let blob_size: usize = blob_size_str.parse().ok()?;

            return Some(FileStat {
                id: blob_id,
                kind: FileKind::Blob(blob_count),
                size: blob_size,
            });
        }

        // We only care about the first two numbers separated by a ':'
        // " 222:6097[7856 .. 7955]['0..."

        let name_and_size = line.trim().split('[').next()?;
        let mut name_and_size = name_and_size.split(':');

        let name_str = name_and_size.next()?;
        let size_str = name_and_size.next()?;

        Some(FileStat {
            id: name_str.parse().ok()?,
            // Every SST line follows a `--- level N ---` header, so `level` is
            // at least one by the time we get here. Guarded anyway rather than
            // underflowed, because this parses a free-form debug string.
            kind: FileKind::Sst(level.checked_sub(1)?),
            size: size_str.parse().ok()?,
        })
    })
}

fn compare_names(needle: &str, line: &str) -> bool {
    let mut parts = line.split(' ');
    let name = match parts.next() {
        Some(x) => x,
        _ => return false,
    };

    name[8..]
        .chars()
        .rev()
        .map(|y| match y {
            '.' => '_',
            x => x,
        })
        .eq(needle.chars().rev())
}

fn find_line_for_prop<'a>(name: &str, all: &'a str, lines: &mut Lines<'a>) -> Option<&'a str> {
    for line in lines.by_ref() {
        if compare_names(name, line) {
            return Some(line);
        }
    }

    // Just in case things are out of order, we have this secondary loop to protect
    // us.
    *lines = all.lines();
    lines.by_ref().find(|&line| compare_names(name, line))
}

/// Declares the engine properties collected on every scrape.
///
/// The `db_` sections are values RocksDb computes for the whole database: their
/// handlers ignore the column family entirely, so asking each family would
/// return the same number under a different label and invent detail that does
/// not exist. `rocksdb.is-write-stopped` and `rocksdb.actual-delayed-write-rate`
/// read the shared write controller; the block-cache sizes read the one `Cache`
/// object both families are configured with.
///
/// The `cf_` sections are computed from one family's own memtables and
/// compaction state, so they are read once per family in [`EXPORTED_CFS`] and
/// carry a `cf` label.
macro_rules! props {
(
    db_props: {
        $($prop:ident as $name:expr,)*
    };
    db_bools: {
        $($bool_prop:ident as $bool_name:expr,)*
    };
    cf_props: {
        $($cf_prop:ident as $cf_name:expr,)*
    };
    cf_bools: {
        $($cf_bool_prop:ident as $cf_bool_name:expr,)*
    };
) => {

    fn rdb_properties(map: &mut HashMap<PropertyId, PropertyValue>, db: &Rocks) {
        $(
        let id = PropertyId::database(stringify!($prop));
        if !map.contains_key(&id) {
            if let Some(value) = db.inner.property_value($name).ok().unwrap_or_default().and_then(|y| y.parse::<u64>().ok()) {
                map.insert(id, PropertyValue::Number(value));
            }
        }
        )*

        $(
        let id = PropertyId::database(stringify!($bool_prop));
        if !map.contains_key(&id) {
            if let Some(value) = db.inner.property_value($bool_name).ok().unwrap_or_default().and_then(|y| y.parse::<u64>().ok()) {
                map.insert(id, PropertyValue::Bool(value == 1));
            }
        }
        )*

        for cf_name in EXPORTED_CFS {
            let Some(cf) = db.inner.cf_handle(cf_name) else {
                continue;
            };

            $(
            let id = PropertyId::column_family(stringify!($cf_prop), cf_name);
            if !map.contains_key(&id) {
                if let Some(value) = db.inner.property_value_cf(&cf, $cf_name).ok().unwrap_or_default().and_then(|y| y.parse::<u64>().ok()) {
                    map.insert(id, PropertyValue::Number(value));
                }
            }
            )*

            $(
            let id = PropertyId::column_family(stringify!($cf_bool_prop), cf_name);
            if !map.contains_key(&id) {
                if let Some(value) = db.inner.property_value_cf(&cf, $cf_bool_name).ok().unwrap_or_default().and_then(|y| y.parse::<u64>().ok()) {
                    map.insert(id, PropertyValue::Bool(value == 1));
                }
            }
            )*
        }
    }

    $(
    pastey::paste! {
        /// A RocksDb property.
        #[allow(unused)]
        pub const [< PROP_ $prop:upper >]: RocksProperty<u64> = RocksProperty(stringify!($prop), PhantomData);
    }
    )*

    $(
    pastey::paste! {
        /// A RocksDb property, reported per column family.
        #[allow(unused)]
        pub const [< PROP_ $cf_prop:upper >]: RocksProperty<u64> = RocksProperty(stringify!($cf_prop), PhantomData);
    }
    )*

    $(
    pastey::paste! {
        /// A RocksDb property, reported per column family.
        #[allow(unused)]
        pub const [< PROP_ $cf_bool_prop:upper >]: RocksProperty<bool> = RocksProperty(stringify!($cf_bool_prop), PhantomData);
    }
    )*

    $(
    pastey::paste! {
        /// A RocksDb property.
        #[allow(unused)]
        pub const [< PROP_ $bool_prop:upper >]: RocksProperty<bool> = RocksProperty(stringify!($bool_prop), PhantomData);
    }
    )*
};
}

macro_rules! stats {
(
    count: {
        $($prop:ident,)*
    };
    histogram: {
        $($hist_prop:ident,)*
    };
) => {

    fn optional_stats(map: &mut HashMap<PropertyId, PropertyValue>, input: &str) -> Option<u16> {
        let mut count = 0;

        let input = input.trim();
        let mut lines = input.lines();

        $(
        let line_name = stringify!($prop);
        pastey::paste!{
        let name = stringify!([< __rocksdb_optional_ $prop >]);
        }
        if let Some(line) = find_line_for_prop(line_name, input, &mut lines) {
            map_insert(map, name, PropertyValue::Number(line.split(' ').last().unwrap().parse().ok().unwrap()));
            count += 1;
        }
        )*

        $(
        pastey::paste!{
        let name_p50 = stringify!([< __rocksdb_optional_ $hist_prop _hist_p50 >]);
        let name_p95 = stringify!([< __rocksdb_optional_ $hist_prop _hist_p95 >]);
        let name_p99 = stringify!([< __rocksdb_optional_ $hist_prop _hist_p99 >]);
        let name_events = stringify!([< __rocksdb_optional_ $hist_prop _hist_events >]);
        let name_sum = stringify!([< __rocksdb_optional_ $hist_prop _hist_sum >]);
        }
        if let Some(line) = find_line_for_prop(stringify!($hist_prop), input, &mut lines) {
            count += 1;
            let histogram = Histogram::from(line).unwrap();
            map_insert(map, name_p50, PropertyValue::Float(histogram.p_50));
            map_insert(map, name_p95, PropertyValue::Float(histogram.p_95));
            map_insert(map, name_p99, PropertyValue::Float(histogram.p_99));
            map_insert(map, name_events, PropertyValue::Number(histogram.events));
            map_insert(map, name_sum, PropertyValue::Number(histogram.sum));
        }
        )*

        Some(count)
    }


    $(
    pastey::paste! {
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $prop:upper >]: RocksProperty<u64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $prop >]), PhantomData);
    }
    )*

    $(
    pastey::paste! {
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $hist_prop:upper _HIST_P50>]: RocksProperty<f64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $hist_prop _hist_p50 >]), PhantomData);
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $hist_prop:upper _HIST_P95>]: RocksProperty<f64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $hist_prop _hist_p95 >]), PhantomData);
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $hist_prop:upper _HIST_P99>]: RocksProperty<f64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $hist_prop _hist_p99 >]), PhantomData);
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $hist_prop:upper _HIST_EVENTS>]: RocksProperty<u64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $hist_prop _hist_events >]), PhantomData);
        /// A computed RocksDb property from `rocksdb.options-statistics'.
        #[allow(unused, non_upper_case_globals)]
        pub const [< PROP__ROCKSDB_OPTIONAL_ $hist_prop:upper _HIST_SUM>]: RocksProperty<u64> =
            RocksProperty(stringify!([< __rocksdb_optional_ $hist_prop _hist_sum >]), PhantomData);
    }
    )*

};
}

props! {
    // Database-wide. Verified against the vendored RocksDb source: each of
    // these handlers takes `DBImpl*` and ignores the column family.
    db_props: {
        // db->NumRunningFlushes() / NumRunningCompactions().
        rocksdb_num_running_flushes as "rocksdb.num-running-flushes",
        rocksdb_num_running_compactions as "rocksdb.num-running-compactions",

        // The one Cache object both column families are configured with.
        rocksdb_block_cache_capacity as "rocksdb.block-cache-capacity",
        rocksdb_block_cache_usage as "rocksdb.block-cache-usage",
        rocksdb_block_cache_pinned_usage as "rocksdb.block-cache-pinned-usage",

        // db->write_controller(), which every family shares -- so this is
        // non-zero whichever family is responsible for the delay.
        rocksdb_actual_delayed_write_rate as "rocksdb.actual-delayed-write-rate",
    };
    db_bools: {
        // db->write_controller().IsStopped(). A stalling family takes its stop
        // token from that shared controller, so this reports true regardless of
        // which family caused it.
        rocksdb_is_write_stopped as "rocksdb.is-write-stopped",
    };
    // Per column family: computed from that family's own memtables and
    // compaction state, so `default` alone said nothing about `blobs`.
    cf_props: {
        rocksdb_num_immutable_mem_table as "rocksdb.num-immutable-mem-table",
        rocksdb_num_immutable_mem_table_flushed as "rocksdb.num-immutable-mem-table-flushed",

        rocksdb_background_errors as "rocksdb.background-errors",

        rocksdb_cur_size_active_mem_tables as "rocksdb.cur-size-active-mem-table",
        rocksdb_size_all_mem_tables as "rocksdb.size-all-mem-tables",
        rocksdb_num_entries_active_mem_table as "rocksdb.num-entries-active-mem-table",
        rocksdb_num_entries_imm_mem_tables as "rocksdb.num-entries-imm-mem-tables",
    };
    cf_bools: {
        rocksdb_mem_table_flush_pending as "rocksdb.mem-table-flush-pending",
        rocksdb_compaction_pending as "rocksdb.compaction-pending",
    };
}

stats! {
    count: {
        block_cache_miss,
        block_cache_hit,
        block_cache_bytes_read,
        block_cache_bytes_write,
        memtable_hit,
        memtable_miss,
        l0_hit,
        l1_hit,
        l2andup_hit,
        compaction_key_drop_new,
        compaction_key_drop_obsolete,
        compaction_key_drop_range_del,
        compaction_key_drop_user,
        compaction_range_del_drop_obsolete,
        compaction_optimized_del_drop_obsolete,
        compaction_cancelled,
        number_keys_written,
        number_keys_read,
        number_keys_updated,
        bytes_written,
        bytes_read,
        stall_micros,
        wal_bytes,
        compact_read_bytes,
        compact_write_bytes,
        flush_write_bytes,
        read_amp_estimate_useful_bytes,
        read_amp_total_read_bytes,

        blobdb_num_put,
        blobdb_num_write,
        blobdb_num_get,
        blobdb_bytes_written,
        blobdb_bytes_read,
        blobdb_blob_file_bytes_written,
        blobdb_blob_file_bytes_read,
        blobdb_blob_index_expired_count,
        blobdb_blob_index_expired_size,
        blobdb_blob_index_evicted_count,
        blobdb_blob_index_evicted_size,
        blobdb_gc_num_files,
        blobdb_gc_num_new_files,
        blobdb_gc_failures,
        blobdb_gc_num_keys_relocated,
        blobdb_gc_bytes_relocated,
        blobdb_fifo_num_files_evicted,
        blobdb_fifo_num_keys_evicted,
        blobdb_fifo_bytes_evicted,

        error_handler_bg_errro_count,
        error_handler_bg_io_errro_count,
        error_handler_bg_retryable_io_errro_count,
        error_handler_autoresume_count,
        error_handler_autoresume_retry_total_count,
        error_handler_autoresume_success_count,
    };
    histogram: {
        db_get_micros,
        db_write_micros,
        compaction_times_micros,
        compaction_times_cpu_micros,
    };
}

#[derive(Clone, Copy, Default, Debug)]
struct Histogram {
    pub p_50: f64,
    pub p_95: f64,
    pub p_99: f64,
    #[allow(unused)]
    pub p_100: f64,
    pub events: u64,
    pub sum: u64,
}

impl Histogram {
    fn from(line: &str) -> Option<Self> {
        let mut lines = line.trim().split(' ');

        Some(Histogram {
            p_50: lines.nth(3)?.parse().ok()?,
            p_95: lines.nth(2)?.parse().ok()?,
            p_99: lines.nth(2)?.parse().ok()?,
            p_100: lines.nth(2)?.parse().ok()?,
            events: lines.nth(2)?.parse().ok()?,
            sum: lines.nth(2)?.parse().ok()?,
        })
    }
}

pub(crate) fn map_write_stalls(
    map: &mut HashMap<PropertyId, PropertyValue>,
    input: &str,
    cf: &'static str,
) {
    // Skip the preamble, then take each value pair.
    let iter = input.trim()[21..].split(',').filter_map(|pair| {
        // Split the value pair by : so we can get the name and value.
        let mut pair_split = pair.split(':');

        let name = pair_split.next()?.trim();
        // Note that this causes the last `interval` recording to be lost. This is fine because
        // it doesn't make much sense to keep data about an arbitrary interval not controlled by
        // prometheus or grafana.
        let value: u64 = pair_split.next()?.trim().parse().ok()?;

        Some((name, value))
    });

    for (stall_name, count) in iter {
        let name = match stall_name {
            "cf-l0-file-count-limit-delays-with-ongoing-compaction" => {
                "__rocksdb_stalls_l0_file_count_limit_delays_with_ongoing_compaction"
            }
            "cf-l0-file-count-limit-stops-with-ongoing-compaction" => {
                "__rocksdb_stalls_l0_file_count_limit_stops_with_ongoing_compaction"
            }
            "write-buffer-manager-limit-stops" => {
                "__rocksdb_stalls_write_buffer_manager_limit_stops"
            }
            "l0-file-count-limit-delays" => "__rocksdb_stalls_delays_l0_file_count_limit",
            "l0-file-count-limit-stops" => "__rocksdb_stalls_stops_l0_file_count_limit",
            "memtable-limit-delays" => "__rocksdb_stalls_delays_memtable_limit",
            "memtable-limit-stops" => "__rocksdb_stalls_stops_memtable_limit",
            "pending-compaction-bytes-delays" => "__rocksdb_stalls_delays_pending_compaction_bytes",
            "pending-compaction-bytes-stops" => "__rocksdb_stalls_stops_pending_compaction_bytes",
            "total-delays" => "__rocksdb_stalls_total_delays",
            "total-stops" => "__rocksdb_stalls_total_stop",
            _ => continue,
        };

        map_insert_id(map, PropertyId::column_family(name, cf), count.into());
    }
}

/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_STOPS_L0_FILE_COUNT_LIMIT: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_stops_l0_file_count_limit", PhantomData);
/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_DELAYS_L0_FILE_COUNT_LIMIT: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_delays_l0_file_count_limit", PhantomData);

/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_STOPS_MEMTABLE_LIMIT: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_stops_memtable_limit", PhantomData);
/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_DELAYS_MEMTABLE_LIMIT: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_delays_memtable_limit", PhantomData);

/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_STOPS_PENDING_COMPACTION_BYTES: RocksProperty<u64> = RocksProperty(
    "__rocksdb_stalls_stops_pending_compaction_bytes",
    PhantomData,
);
/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_DELAYS_PENDING_COMPACTION_BYTES: RocksProperty<u64> = RocksProperty(
    "__rocksdb_stalls_delays_pending_compaction_bytes",
    PhantomData,
);

/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_TOTAL_STOP: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_total_stop", PhantomData);
/// A computed RocksDb property from `rocksdb.db-write-stall-stats'.
#[allow(unused, non_upper_case_globals)]
pub const PROP__ROCKSDB_STALLS_TOTAL_DELAYS: RocksProperty<u64> =
    RocksProperty("__rocksdb_stalls_total_delays", PhantomData);

#[derive(Default)]
struct Cost {
    count: u64,
    size: u64,
    portion: f64,
}

struct BlockCacheStats {
    capacity: u64,
    usage: u64,
    table_size: u64,
    occupancy: u64,
    collections: u64,

    data_cost: Cost,
    filter_cost: Cost,
    filter_meta_cost: Cost,
    index_cost: Cost,
    write_buffer_cost: Cost,
    other_cost: Cost,
}

fn parse_block_cache_stats(
    map: &mut HashMap<PropertyId, crate::PropertyValue>,
    input: &str,
) -> Option<()> {
    let parsed = parse_block_cache(input)?;

    map_insert(map, "__rocksdb_blockcache_capacity", parsed.capacity.into());
    map_insert(map, "__rocksdb_blockcache_usage", parsed.usage.into());
    map_insert(
        map,
        "__rocksdb_blockcache_occupancy",
        parsed.occupancy.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_table_size",
        parsed.table_size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_collections",
        parsed.collections.into(),
    );

    // Data
    map_insert(
        map,
        "__rocksdb_blockcache_cost_data_count",
        parsed.data_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_data_size",
        parsed.data_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_data_portion",
        (parsed.data_cost.portion as u64).into(),
    );

    // Filter
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_count",
        parsed.filter_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_size",
        parsed.filter_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_portion",
        (parsed.filter_cost.portion as u64).into(),
    );

    // Filter meta
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_meta_count",
        parsed.filter_meta_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_meta_size",
        parsed.filter_meta_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_filter_meta_portion",
        (parsed.filter_meta_cost.portion as u64).into(),
    );

    // Index
    map_insert(
        map,
        "__rocksdb_blockcache_cost_index_count",
        parsed.index_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_index_size",
        parsed.index_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_index_portion",
        (parsed.index_cost.portion as u64).into(),
    );

    // Write buffer
    map_insert(
        map,
        "__rocksdb_blockcache_cost_write_buffer_count",
        parsed.write_buffer_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_write_buffer_size",
        parsed.write_buffer_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_write_buffer_portion",
        (parsed.write_buffer_cost.portion as u64).into(),
    );

    // Other
    map_insert(
        map,
        "__rocksdb_blockcache_cost_other_count",
        parsed.other_cost.count.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_other_size",
        parsed.other_cost.size.into(),
    );
    map_insert(
        map,
        "__rocksdb_blockcache_cost_other_portion",
        (parsed.other_cost.portion as u64).into(),
    );

    Some(())
}

fn parse_block_cache(input: &str) -> Option<BlockCacheStats> {
    let offset = input.match_indices(' ').nth(2)?.0;
    let input = input[offset..].trim();

    let mut sections = input.split('\n');

    let first = sections.next()?.trim();

    let mut iter = first.split(' ');
    let capacity = iter.nth(1)?;
    let capacity_kind = iter.next()?;

    let capacity = capacity.parse::<f64>().ok()?;
    let capacity = conv_units(capacity_kind).map(|y| (y * capacity).ceil() as u64)?;

    let _seed = iter.nth(1)?;

    let usage = iter.nth(1)?;
    let usage_kind = iter.next()?;

    let usage = usage.parse::<f64>().ok()?;
    let usage = conv_units(usage_kind).map(|y| (y * usage).ceil() as u64)?;

    let table_size = iter.nth(1)?;
    let occupancy = iter.nth(1)?;
    let collections = iter.nth(1)?;

    let _last_copies = iter.nth(1)?;
    let _last_secs = iter.nth(1)?;
    let _secs_since = iter.nth(1)?;

    // Now parse the second line

    let second = sections.next()?;

    let mut iter = second.match_indices('(');
    iter.next();

    let mut cache_stats = BlockCacheStats {
        capacity,
        usage,
        table_size: table_size.parse().ok()?,
        occupancy: occupancy.parse().ok()?,
        collections: collections.parse().ok()?,

        data_cost: Default::default(),
        filter_cost: Default::default(),
        filter_meta_cost: Default::default(),
        index_cost: Default::default(),
        write_buffer_cost: Default::default(),
        other_cost: Default::default(),
    };

    for (i, _) in iter {
        let name = second[0..i].split(' ').next_back()?;

        let subslice = &second[i + 1..];
        let inner = &subslice[..subslice.match_indices(')').next()?.0];

        let mut iter = inner.split(',');

        let count = iter.next()?;

        let size = {
            let size = iter.next()?;
            let mut iter = size.split(' ');
            let num = iter.next()?;
            let unit = iter.next()?;

            let num = num.parse::<f64>().ok()?;
            conv_units(unit).map(|y| (y * num).ceil() as u64)?
        };

        let portion = iter.next()?;
        let portion = &portion[..portion.len() - 1];

        let portion = portion.parse::<f64>().ok()?;

        let cost = match name {
            "DataBlock" => &mut cache_stats.data_cost,
            "FilterBlock" => &mut cache_stats.filter_cost,
            "FilterMetaBlock" => &mut cache_stats.filter_meta_cost,
            "IndexBlock" => &mut cache_stats.index_cost,
            "WriteBuffer" => &mut cache_stats.write_buffer_cost,
            _ => &mut cache_stats.other_cost,
        };

        cost.count += count.parse::<u64>().ok()?;
        cost.portion += portion;
        cost.size += size;
    }

    Some(cache_stats)
}

fn conv_units(name: &str) -> Option<f64> {
    Some(match name {
        "TB" => 1_099_511_627_776.0,
        "GB" => 1_073_741_824.0,
        "MB" => 1_048_576.0,
        "KB" => 1024.0,
        _ => return None,
    })
}

/// Records a database-wide value.
fn map_insert(
    map: &mut HashMap<PropertyId, PropertyValue>,
    key: &'static str,
    value: PropertyValue,
) {
    map_insert_id(map, PropertyId::database(key), value);
}

/// Records a value under an explicit identity.
///
/// Repeat inserts accumulate, which is what folds the two column families'
/// file listings into one total in [`map_files`]. Per-family values must
/// therefore carry distinct ids, or they would be summed instead of reported
/// separately.
fn map_insert_id(
    map: &mut HashMap<PropertyId, PropertyValue>,
    id: PropertyId,
    value: PropertyValue,
) {
    map.entry(id)
        .and_modify(|y| match y {
            PropertyValue::Bool(x) => *x |= value.as_bool().unwrap(),
            PropertyValue::Number(x) => *x += value.as_u64().unwrap(),
            PropertyValue::Float(x) => *x += value.as_float().unwrap(),
            PropertyValue::String(_) => unimplemented!(),
        })
        .or_insert(value);
}

#[cfg(test)]
mod tests {

    use std::collections::HashMap;

    use crate::environment::rocks_metrics::optional_stats;
    use crate::{PropertyId, DEFAULT_CF};

    #[test]
    fn test_parse_block_cache_usage() {
        let example = "Block cache LRUCache@0x6000037cc018#19156 capacity: 7.45 GB seed: 851591437 usage: 29.75 MB table_size: 4160 occupancy: 2139 collections: 3 last_copies: 1 last_secs: 0.000519 secs_since: 0\nBlock cache entry stats(count,size,portion): DataBlock(2000,29.44 MB,0.385875%) FilterBlock(36,19.30 KB,0.00024705%) FilterMetaBlock(33,5.41 KB,6.93e-05%) IndexBlock(69,112.13 KB,0.00143531%) Misc(1,0.00 KB,0%)\n";
        super::parse_block_cache(example).unwrap();
    }

    #[test]
    fn test_parse_stats() {
        assert!(super::compare_names(
            "compaction_key_drop_range_del",
            "rocksdb.compaction.key.drop.range_del COUNT : 500"
        ));

        assert!(!super::compare_names(
            "block_cache_add",
            "rocksdb.block.cache.add.failures COUNT : 550",
        ));

        let value = r#"
rocksdb.block.cache.miss COUNT : 550
rocksdb.block.cache.hit COUNT : 1612
rocksdb.block.cache.add COUNT : 161
rocksdb.block.cache.add.failures COUNT : 0
rocksdb.block.cache.index.miss COUNT : 0
rocksdb.block.cache.index.hit COUNT : 0
rocksdb.block.cache.index.add COUNT : 0
rocksdb.block.cache.index.bytes.insert COUNT : 0
rocksdb.block.cache.filter.miss COUNT : 0
rocksdb.block.cache.filter.hit COUNT : 0
rocksdb.block.cache.filter.add COUNT : 0
rocksdb.block.cache.filter.bytes.insert COUNT : 0
rocksdb.block.cache.data.miss COUNT : 550
rocksdb.block.cache.data.hit COUNT : 1612
rocksdb.block.cache.data.add COUNT : 161
rocksdb.block.cache.data.bytes.insert COUNT : 2498017
rocksdb.block.cache.bytes.read COUNT : 17407739
rocksdb.block.cache.bytes.write COUNT : 2498017
rocksdb.bloom.filter.useful COUNT : 0
rocksdb.bloom.filter.full.positive COUNT : 0
rocksdb.bloom.filter.full.true.positive COUNT : 0
rocksdb.persistent.cache.hit COUNT : 0
rocksdb.persistent.cache.miss COUNT : 0
rocksdb.sim.block.cache.hit COUNT : 0
rocksdb.sim.block.cache.miss COUNT : 0
rocksdb.memtable.hit COUNT : 0
rocksdb.memtable.miss COUNT : 1892
rocksdb.l0.hit COUNT : 0
rocksdb.l1.hit COUNT : 0
rocksdb.l2andup.hit COUNT : 1892
rocksdb.compaction.key.drop.new COUNT : 0
rocksdb.compaction.key.drop.obsolete COUNT : 0
rocksdb.compaction.key.drop.range_del COUNT : 0
rocksdb.compaction.key.drop.user COUNT : 0
rocksdb.compaction.range_del.drop.obsolete COUNT : 0
rocksdb.compaction.optimized.del.drop.obsolete COUNT : 0
rocksdb.compaction.cancelled COUNT : 0
rocksdb.number.keys.written COUNT : 796
rocksdb.number.keys.read COUNT : 1892
rocksdb.number.keys.updated COUNT : 0
rocksdb.bytes.written COUNT : 54128
rocksdb.bytes.read COUNT : 471964174
rocksdb.number.db.seek COUNT : 300
rocksdb.number.db.next COUNT : 0
rocksdb.number.db.prev COUNT : 0
rocksdb.number.db.seek.found COUNT : 300
rocksdb.number.db.next.found COUNT : 0
rocksdb.number.db.prev.found COUNT : 0
rocksdb.db.iter.bytes.read COUNT : 76191414
rocksdb.no.file.opens COUNT : 31
rocksdb.no.file.errors COUNT : 0
rocksdb.stall.micros COUNT : 0
rocksdb.db.mutex.wait.micros COUNT : 0
rocksdb.number.multiget.get COUNT : 0
rocksdb.number.multiget.keys.read COUNT : 0
rocksdb.number.multiget.bytes.read COUNT : 0
rocksdb.number.merge.failures COUNT : 0
rocksdb.bloom.filter.prefix.checked COUNT : 0
rocksdb.bloom.filter.prefix.useful COUNT : 0
rocksdb.number.reseeks.iteration COUNT : 0
rocksdb.getupdatessince.calls COUNT : 0
rocksdb.wal.synced COUNT : 0
rocksdb.wal.bytes COUNT : 54128
rocksdb.write.self COUNT : 796
rocksdb.write.other COUNT : 0
rocksdb.write.wal COUNT : 796
rocksdb.compact.read.bytes COUNT : 1976200337
rocksdb.compact.write.bytes COUNT : 387397333
rocksdb.flush.write.bytes COUNT : 0
rocksdb.compact.read.marked.bytes COUNT : 0
rocksdb.compact.read.periodic.bytes COUNT : 0
rocksdb.compact.read.ttl.bytes COUNT : 0
rocksdb.compact.write.marked.bytes COUNT : 0
rocksdb.compact.write.periodic.bytes COUNT : 0
rocksdb.compact.write.ttl.bytes COUNT : 0
rocksdb.number.direct.load.table.properties COUNT : 0
rocksdb.number.superversion_acquires COUNT : 4
rocksdb.number.superversion_releases COUNT : 0
rocksdb.number.superversion_cleanups COUNT : 0
rocksdb.number.block.compressed COUNT : 0
rocksdb.number.block.decompressed COUNT : 0
rocksdb.number.block.not_compressed COUNT : 0
rocksdb.merge.operation.time.nanos COUNT : 30977694
rocksdb.filter.operation.time.nanos COUNT : 0
rocksdb.row.cache.hit COUNT : 1555
rocksdb.row.cache.miss COUNT : 1537
rocksdb.read.amp.estimate.useful.bytes COUNT : 0
rocksdb.read.amp.total.read.bytes COUNT : 0
rocksdb.number.rate_limiter.drains COUNT : 0
rocksdb.number.iter.skip COUNT : 0
rocksdb.blobdb.num.put COUNT : 0
rocksdb.blobdb.num.write COUNT : 0
rocksdb.blobdb.num.get COUNT : 0
rocksdb.blobdb.num.multiget COUNT : 0
rocksdb.blobdb.num.seek COUNT : 0
rocksdb.blobdb.num.next COUNT : 0
rocksdb.blobdb.num.prev COUNT : 0
rocksdb.blobdb.num.keys.written COUNT : 0
rocksdb.blobdb.num.keys.read COUNT : 0
rocksdb.blobdb.bytes.written COUNT : 0
rocksdb.blobdb.bytes.read COUNT : 0
rocksdb.blobdb.write.inlined COUNT : 0
rocksdb.blobdb.write.inlined.ttl COUNT : 0
rocksdb.blobdb.write.blob COUNT : 0
rocksdb.blobdb.write.blob.ttl COUNT : 0
rocksdb.blobdb.blob.file.bytes.written COUNT : 380959078
rocksdb.blobdb.blob.file.bytes.read COUNT : 2518384083
rocksdb.blobdb.blob.file.synced COUNT : 2
rocksdb.blobdb.blob.index.expired.count COUNT : 0
rocksdb.blobdb.blob.index.expired.size COUNT : 0
rocksdb.blobdb.blob.index.evicted.count COUNT : 0
rocksdb.blobdb.blob.index.evicted.size COUNT : 0
rocksdb.blobdb.gc.num.files COUNT : 0
rocksdb.blobdb.gc.num.new.files COUNT : 0
rocksdb.blobdb.gc.failures COUNT : 0
rocksdb.blobdb.gc.num.keys.relocated COUNT : 1171
rocksdb.blobdb.gc.bytes.relocated COUNT : 305687557
rocksdb.blobdb.fifo.num.files.evicted COUNT : 0
rocksdb.blobdb.fifo.num.keys.evicted COUNT : 0
rocksdb.blobdb.fifo.bytes.evicted COUNT : 0
rocksdb.txn.overhead.mutex.prepare COUNT : 0
rocksdb.txn.overhead.mutex.old.commit.map COUNT : 0
rocksdb.txn.overhead.duplicate.key COUNT : 0
rocksdb.txn.overhead.mutex.snapshot COUNT : 0
rocksdb.txn.get.tryagain COUNT : 0
rocksdb.number.multiget.keys.found COUNT : 0
rocksdb.num.iterator.created COUNT : 6
rocksdb.num.iterator.deleted COUNT : 6
rocksdb.block.cache.compression.dict.miss COUNT : 0
rocksdb.block.cache.compression.dict.hit COUNT : 0
rocksdb.block.cache.compression.dict.add COUNT : 0
rocksdb.block.cache.compression.dict.bytes.insert COUNT : 0
rocksdb.block.cache.add.redundant COUNT : 0
rocksdb.block.cache.index.add.redundant COUNT : 0
rocksdb.block.cache.filter.add.redundant COUNT : 0
rocksdb.block.cache.data.add.redundant COUNT : 0
rocksdb.block.cache.compression.dict.add.redundant COUNT : 0
rocksdb.files.marked.trash COUNT : 0
rocksdb.files.deleted.immediately COUNT : 10
rocksdb.error.handler.bg.errro.count COUNT : 0
rocksdb.error.handler.bg.io.errro.count COUNT : 0
rocksdb.error.handler.bg.retryable.io.errro.count COUNT : 0
rocksdb.error.handler.autoresume.count COUNT : 0
rocksdb.error.handler.autoresume.retry.total.count COUNT : 0
rocksdb.error.handler.autoresume.success.count COUNT : 0
rocksdb.memtable.payload.bytes.at.flush COUNT : 0
rocksdb.memtable.garbage.bytes.at.flush COUNT : 0
rocksdb.secondary.cache.hits COUNT : 0
rocksdb.verify_checksum.read.bytes COUNT : 0
rocksdb.backup.read.bytes COUNT : 0
rocksdb.backup.write.bytes COUNT : 0
rocksdb.remote.compact.read.bytes COUNT : 0
rocksdb.remote.compact.write.bytes COUNT : 0
rocksdb.hot.file.read.bytes COUNT : 0
rocksdb.warm.file.read.bytes COUNT : 0
rocksdb.cold.file.read.bytes COUNT : 0
rocksdb.hot.file.read.count COUNT : 0
rocksdb.warm.file.read.count COUNT : 0
rocksdb.cold.file.read.count COUNT : 0
rocksdb.last.level.read.bytes COUNT : 8882707
rocksdb.last.level.read.count COUNT : 536
rocksdb.non.last.level.read.bytes COUNT : 2518457860
rocksdb.non.last.level.read.count COUNT : 9593
rocksdb.block.checksum.compute.count COUNT : 562
rocksdb.multiget.coroutine.count COUNT : 0
rocksdb.blobdb.cache.miss COUNT : 0
rocksdb.blobdb.cache.hit COUNT : 0
rocksdb.blobdb.cache.add COUNT : 0
rocksdb.blobdb.cache.add.failures COUNT : 0
rocksdb.blobdb.cache.bytes.read COUNT : 0
rocksdb.blobdb.cache.bytes.write COUNT : 0
rocksdb.read.async.micros COUNT : 0
rocksdb.async.read.error.count COUNT : 0
rocksdb.secondary.cache.filter.hits COUNT : 0
rocksdb.secondary.cache.index.hits COUNT : 0
rocksdb.secondary.cache.data.hits COUNT : 0
rocksdb.table.open.prefetch.tail.miss COUNT : 0
rocksdb.table.open.prefetch.tail.hit COUNT : 0
rocksdb.db.get.micros P50 : 100.534521 P95 : 248.538071 P99 : 378.361644 P100 : 1541.000000 COUNT : 1892 SUM : 215688
rocksdb.db.write.micros P50 : 11.834171 P95 : 24.811429 P99 : 48.280000 P100 : 263.000000 COUNT : 796 SUM : 11264
rocksdb.compaction.times.micros P50 : 1016211.000000 P95 : 1016211.000000 P99 : 1016211.000000 P100 : 1016211.000000 COUNT : 1 SUM : 1016211
rocksdb.compaction.times.cpu_micros P50 : 718130.000000 P95 : 718130.000000 P99 : 718130.000000 P100 : 718130.000000 COUNT : 1 SUM : 718130
rocksdb.subcompaction.setup.times.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.table.sync.micros P50 : 100.000000 P95 : 100.000000 P99 : 100.000000 P100 : 100.000000 COUNT : 1 SUM : 100
rocksdb.compaction.outfile.sync.micros P50 : 916.000000 P95 : 916.000000 P99 : 916.000000 P100 : 916.000000 COUNT : 1 SUM : 916
rocksdb.wal.file.sync.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.manifest.file.sync.micros P50 : 34.000000 P95 : 52.000000 P99 : 52.000000 P100 : 52.000000 COUNT : 2 SUM : 83
rocksdb.table.open.io.micros P50 : 282.500000 P95 : 361.000000 P99 : 361.000000 P100 : 361.000000 COUNT : 6 SUM : 1615
rocksdb.db.multiget.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.read.block.compaction.micros P50 : 40.962766 P95 : 76.984211 P99 : 104.828421 P100 : 1568.000000 COUNT : 389 SUM : 18000
rocksdb.read.block.get.micros P50 : 71.312500 P95 : 191.200000 P99 : 244.640000 P100 : 273.000000 COUNT : 167 SUM : 13776
rocksdb.write.raw.block.micros P50 : 36.414773 P95 : 74.043981 P99 : 187.760000 P100 : 201.000000 COUNT : 389 SUM : 15507
rocksdb.numfiles.in.singlecompaction P50 : 5.000000 P95 : 5.000000 P99 : 5.000000 P100 : 5.000000 COUNT : 1 SUM : 5
rocksdb.db.seek.micros P50 : 225.555556 P95 : 700.833333 P99 : 1138.750000 P100 : 1207.000000 COUNT : 300 SUM : 79346
rocksdb.db.write.stall P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.sst.read.micros P50 : 2.875000 P95 : 17.318750 P99 : 41.917143 P100 : 1512.000000 COUNT : 574 SUM : 4670
rocksdb.num.subcompactions.scheduled P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.bytes.per.read P50 : 247437.722420 P95 : 499884.000000 P99 : 499884.000000 P100 : 499884.000000 COUNT : 1892 SUM : 471964174
rocksdb.bytes.per.write P50 : 68.000000 P95 : 68.000000 P99 : 68.000000 P100 : 68.000000 COUNT : 796 SUM : 54128
rocksdb.bytes.per.multiget P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.bytes.compressed P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.bytes.decompressed P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.compression.times.nanos P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.decompression.times.nanos P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.read.num.merge_operands P50 : 1.000000 P95 : 3.676000 P99 : 3.935200 P100 : 4.000000 COUNT : 1296 SUM : 2007
rocksdb.blobdb.key.size P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.value.size P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.write.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.get.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.multiget.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.seek.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.next.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.prev.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.blobdb.blob.file.write.micros P50 : 6.386885 P95 : 108.925824 P99 : 303.083333 P100 : 11796.000000 COUNT : 1455 SUM : 72666
rocksdb.blobdb.blob.file.read.micros P50 : 17.640173 P95 : 203.000000 P99 : 468.947368 P100 : 5187.000000 COUNT : 9555 SUM : 491608
rocksdb.blobdb.blob.file.sync.micros P50 : 580.000000 P95 : 1168.000000 P99 : 1168.000000 P100 : 1168.000000 COUNT : 2 SUM : 1724
rocksdb.blobdb.compression.micros P50 : 25.755796 P95 : 65.960106 P99 : 88.000000 P100 : 88.000000 COUNT : 1455 SUM : 40377
rocksdb.blobdb.decompression.micros P50 : 5.893232 P95 : 27.775120 P99 : 44.379444 P100 : 225.000000 COUNT : 9505 SUM : 84368
rocksdb.db.flush.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.sst.batch.size P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.num.index.and.filter.blocks.read.per.level P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.num.sst.read.per.level P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.error.handler.autoresume.retry.count P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.async.read.bytes P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.poll.wait.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.prefetched.bytes.discarded P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 5 SUM : 0
rocksdb.multiget.io.batch.size P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.num.level.read.per.multiget P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.async.prefetch.abort.micros P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
rocksdb.table.open.prefetch.tail.read.bytes P50 : 0.000000 P95 : 0.000000 P99 : 0.000000 P100 : 0.000000 COUNT : 0 SUM : 0
"#;

        let mut map = HashMap::default();

        optional_stats(&mut map, value).unwrap();
        println!("{map:?}");

        let hist = super::Histogram::from("rocksdb.blobdb.blob.file.sync.micros P50 : 580.000000 P95 : 1168.000000 P99 : 1168.000000 P100 : 1168.000000 COUNT : 2 SUM : 1724").unwrap();
        println!("{hist:?}");
    }

    #[test]
    fn test_parsing_sstables() {
        let prop_value = r#"
--- level 0 --- version# 4 ---
 222:6097[7856 .. 7955]['00000000000000000000000000000000000000000000000000000000000000000000000000000000' seq:7856, type:2 .. '00000000000000000000000000000063000000000000000000000000000000000000000000000000' seq:7955, type:2](0)
 217:6097[7756 .. 7855]['00000000000000000000000000000000000000000000000000000000000000000000000000000000' seq:7756, type:2 .. '00000000000000000000000000000063000000000000000000000000000000000000000000000000' seq:7855, type:2](0)
 212:7197[7656 .. 7755]['00000000000000000000000000000000000000000000000000000000000000000000000000000000' seq:7656, type:2 .. '00000000000000000000000000000063000000000000000000000000000000000000000000000000' seq:7755, type:2](0)
--- level 1 --- version# 4 ---
--- level 2 --- version# 4 ---
--- level 3 --- version# 4 ---
--- level 4 --- version# 4 ---
--- level 5 --- version# 4 ---
--- level 6 --- version# 4 ---
 208:6418622[0 .. 0]['00000000000000000000000000000000000000000000000000000000000000000000000000000000' seq:0, type:17 .. '000000000000000000000000000019FF000000000000000000000000000000000000000000000000' seq:0, type:17] blob_file:141(0)
--- blob files --- version# 4 ---
blob_file_number: 141 total_blob_count: 235 total_blob_bytes: 61998850 checksum_method:  checksum_value:  linked_ssts: { 208 } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 144 total_blob_count: 218 total_blob_bytes: 60576292 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 147 total_blob_count: 243 total_blob_bytes: 61140028 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 150 total_blob_count: 236 total_blob_bytes: 61110128 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 153 total_blob_count: 239 total_blob_bytes: 60946571 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 156 total_blob_count: 230 total_blob_bytes: 61237731 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 159 total_blob_count: 226 total_blob_bytes: 61025219 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 162 total_blob_count: 239 total_blob_bytes: 60384700 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 165 total_blob_count: 228 total_blob_bytes: 61108693 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 168 total_blob_count: 242 total_blob_bytes: 62111390 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 171 total_blob_count: 229 total_blob_bytes: 60886024 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 174 total_blob_count: 223 total_blob_bytes: 61870653 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 177 total_blob_count: 236 total_blob_bytes: 60768653 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 180 total_blob_count: 235 total_blob_bytes: 60424340 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 183 total_blob_count: 222 total_blob_bytes: 61624444 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 186 total_blob_count: 227 total_blob_bytes: 61070967 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 189 total_blob_count: 240 total_blob_bytes: 61153001 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 192 total_blob_count: 232 total_blob_bytes: 61861581 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 195 total_blob_count: 224 total_blob_bytes: 60783700 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 198 total_blob_count: 223 total_blob_bytes: 61646744 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 202 total_blob_count: 49 total_blob_bytes: 11445476 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 207 total_blob_count: 998 total_blob_bytes: 268684470 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0
blob_file_number: 209 total_blob_count: 596 total_blob_bytes: 160113972 checksum_method:  checksum_value:  linked_ssts: { } garbage_blob_count: 0 garbage_blob_bytes: 0"#;

        assert_eq!(super::iter_files(prop_value).count(), 27);
    }

    /// `engine.cf` is handed to RocksDB verbatim, so an operator can raise
    /// `num_levels` past the eight buckets `map_files` keeps. Files resident
    /// beyond the last bucket must fold into it rather than index out of
    /// bounds -- this is the metrics-scrape path, where a panic aborts the
    /// process.
    #[test]
    fn test_parsing_sstables_beyond_the_last_level_bucket() {
        let mut prop_value = String::new();
        for level in 0..12 {
            prop_value.push_str(&format!("--- level {level} --- version# 4 ---\n"));
            prop_value.push_str(" 222:100[7856 .. 7955]['00' seq:1, type:2](0)\n");
        }

        let mut map = Default::default();
        super::map_files(&mut map, super::iter_files(&prop_value));

        assert_eq!(
            map[&PropertyId::database("__rocksdb_tables_sst_num")].as_u64(),
            Some(12)
        );
        // Levels 0..=6 hold one file each; levels 7 through 11 all land in the
        // last bucket, so it reports five.
        assert_eq!(
            map[&PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l6")].as_u64(),
            Some(100)
        );
        assert_eq!(
            map[&PropertyId::database("__rocksdb_tables_estimated_sst_level_size_l7")].as_u64(),
            Some(500)
        );
    }

    /// An SST line before any `--- level` header would underflow the level
    /// counter. RocksDB does not emit that shape, but this parses a free-form
    /// debug string, so the line is skipped rather than trusted.
    #[test]
    fn test_parsing_sstables_without_a_level_header() {
        let prop_value = " 222:100[7856 .. 7955]['00' seq:1, type:2](0)\n";
        assert_eq!(super::iter_files(prop_value).count(), 0);
    }

    #[test]
    fn test_that_parsing_write_stalls_works() {
        let value = "Write Stall (count): cf-l0-file-count-limit-delays-with-ongoing-compaction: 0, cf-l0-file-count-limit-stops-with-ongoing-compaction: 0, l0-file-count-limit-delays: 0, l0-file-count-limit-stops: 0, memtable-limit-delays: 0, memtable-limit-stops: 182, pending-compaction-bytes-delays: 0, pending-compaction-bytes-stops: 0, total-delays: 0, total-stops: 182, interval: 182 total count";

        let mut map = HashMap::default();
        super::map_write_stalls(&mut map, value, DEFAULT_CF);

        assert_eq!(
            map.get(&PropertyId::column_family(
                "__rocksdb_stalls_l0_file_count_limit_delays_with_ongoing_compaction",
                DEFAULT_CF
            ))
            .unwrap()
            .as_u64()
            .unwrap(),
            0
        );
        assert_eq!(
            map.get(&PropertyId::column_family(
                "__rocksdb_stalls_l0_file_count_limit_stops_with_ongoing_compaction",
                DEFAULT_CF
            ))
            .unwrap()
            .as_u64()
            .unwrap(),
            0
        );
        assert_eq!(
            map.get(&PropertyId::column_family(
                "__rocksdb_stalls_total_stop",
                DEFAULT_CF
            ))
            .unwrap()
            .as_u64()
            .unwrap(),
            182
        );
    }
}
