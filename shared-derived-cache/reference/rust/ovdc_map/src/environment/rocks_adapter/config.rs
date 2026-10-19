// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_settings::{Cfg, Snap};
use rocksdb::{ColumnFamilyDescriptor, CompactionDecision, Env, WriteBufferManager};

use crate::{
    environment::rocks_adapter::{
        config::conf::{combined, iter_opt, H_BLOCK, H_CF, H_DB},
        merge,
    },
    FLAG_DB_BLOB_ENABLE, FLAG_DB_LOG_ENABLE,
};

use super::{cache, log};

// See also https://betterprogramming.pub/navigating-the-minefield-of-rocksdb-configuration-options-246af1e1d3f9
// For a distilled view of settings.
//
// See also https://rocksdb.org/blog/2018/08/23/data-block-hash-index.html
// See also https://rocksdb.org/blog/2019/08/15/unordered-write.html
// See also https://rocksdb.org/blog/2021/05/26/integrated-blob-db.html

mod conf {
    use std::collections::{HashMap, HashSet};

    use serde::Deserialize;

    pub fn default_settings() -> &'static str {
        include_str!(concat!(env!("OUT_DIR"), "/default_settings"))
    }

    pub fn iter(kind: &'static str) -> impl Iterator<Item = &'static str> {
        let lines = default_settings();
        let mut iter = lines.lines();

        for line in iter.by_ref() {
            if line.starts_with(kind) {
                break;
            }
        }

        iter.take_while(|y| y.starts_with("  ")).map(|y| y.trim())
    }

    pub fn iter_opt(kind: &'static str) -> impl Iterator<Item = (&'static str, &'static str)> {
        iter(kind).filter_map(|y| y.trim().split_once('='))
    }

    pub const H_DB: &str = "[DBOptions]";
    pub const H_CF: &str = "[CFOptions \"default\"]";
    pub const H_BLOCK: &str = "[TableOptions/BlockBasedTable \"default\"]";

    /// A single RocksDB option value, already converted to the form RocksDB
    /// expects.
    ///
    /// RocksDB must never receive a size or duration string such as `"64M"`.
    /// Its own parser reads `M` as 1048576 while `ovdc_bytes` reads it as
    /// 1000000, so passing the string through would silently change the option
    /// by ~5%. Any value that parses as a human number is converted here, at
    /// the boundary that owns the requirement; everything else (`"kSnappy"`,
    /// `"bloomfilter:10"`, booleans, floats) is rendered verbatim.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct OptionValue(String);

    impl std::fmt::Display for OptionValue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.0.fmt(f)
        }
    }

    impl<'de> Deserialize<'de> for OptionValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct OptionValueVisitor;

            impl serde::de::Visitor<'_> for OptionValueVisitor {
                type Value = OptionValue;

                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a RocksDB option value")
                }

                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(OptionValue(
                        match ovdc_bytes::human_serde::parse_number(v) {
                            Some(number) => number.to_string(),
                            None => v.to_owned(),
                        },
                    ))
                }

                fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                    self.visit_str(&v)
                }

                fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(OptionValue(v.to_string()))
                }

                fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(OptionValue(v.to_string()))
                }

                fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                    Ok(OptionValue(v.to_string()))
                }

                fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(OptionValue(v.to_string()))
                }
            }

            deserializer.deserialize_any(OptionValueVisitor)
        }
    }

    pub fn combined<'a>(
        iter: impl Iterator<Item = (&'a str, &'a str)>,
        input: &HashMap<String, OptionValue>,
    ) -> Vec<String> {
        let mut hash_set = HashSet::new();
        let mut new_config = vec![];
        for (key, value) in iter {
            hash_set.insert(key);
            let value = if let Some(value) = input.get(key) {
                format!("{key}={value}")
            } else {
                format!("{key}={value}")
            };
            new_config.push(value);
        }

        for (key, value) in input {
            if hash_set.contains(key.as_str()) {
                continue;
            }
            new_config.push(format!("{key}={value}"));
        }

        new_config
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct Config {
        #[serde(default)]
        pub use_write_buffer_manager: bool,

        /// Memory budget shared by every memtable, in bytes.
        ///
        /// Zero means no budget, which is what RocksDB documents as "no limit".
        /// The manager still charges memtable memory to the block cache; only
        /// the flush and stall thresholds are disabled.
        #[serde(default = "default_write_buffer_manager_size")]
        pub write_buffer_manager_size: ovdc_bytes::human_serde::HumanByteSize,

        /// Whether exceeding [`Config::write_buffer_manager_size`] stalls writes.
        ///
        /// Test-only. The stall is global -- it applies to every writer sharing
        /// the manager, not just the column family that grew -- and it latches
        /// until a flush completes rather than clearing as soon as memory dips.
        /// It exists so a test can drive the engine into back-pressure on
        /// demand, which is otherwise not reproducible.
        #[serde(default)]
        pub write_buffer_manager_allow_stall: bool,
        #[serde(default = "default_increase_parallelism")]
        pub increase_parallelism: u8,
        #[serde(default = "default_threads_high")]
        pub threads_high: u8,
        #[serde(default = "default_threads_low")]
        pub threads_low: u8,

        pub cache_size: ovdc_bytes::human_serde::HumanByteSize,
        pub block_cache_size: ovdc_bytes::human_serde::HumanByteSize,

        pub db: HashMap<String, OptionValue>,
        pub cf: HashMap<String, OptionValue>,

        #[serde(default)]
        pub block: HashMap<String, OptionValue>,
        #[serde(default)]
        pub blobs_cf: HashMap<String, OptionValue>,
        #[serde(default)]
        pub blobs_block: HashMap<String, OptionValue>,
    }

    const fn default_write_buffer_manager_size() -> ovdc_bytes::human_serde::HumanByteSize {
        ovdc_bytes::human_serde::HumanByteSize(0)
    }
    const fn default_increase_parallelism() -> u8 {
        2
    }
    const fn default_threads_high() -> u8 {
        2
    }
    const fn default_threads_low() -> u8 {
        2
    }
}

/// The index of the bottommost level, from a column family's option lines.
///
/// RocksDB numbers levels `0..num_levels`, so the last is one below the count.
/// `num_levels` is in this crate's own CF defaults, so the lookup normally
/// finds it; the fallback is RocksDB's default for the case where it is not.
fn bottommost_level(cf_lines: &[String]) -> u32 {
    const ROCKSDB_DEFAULT_NUM_LEVELS: u32 = 7;

    cf_lines
        .iter()
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == "num_levels")
        .and_then(|(_, value)| value.trim().parse::<u32>().ok())
        .unwrap_or(ROCKSDB_DEFAULT_NUM_LEVELS)
        .saturating_sub(1)
}

/// Levels the default column family is given in log mode, unless configured.
///
/// The compaction filter that reclaims emptied rows only acts at the bottommost
/// level, so rows are only reclaimed once they reach it. With RocksDB's seven a
/// row gets there only after enough data accumulates to fill the six levels
/// above -- which is the opposite of the intent, since in log mode large values
/// live in the blobs column family and this one is meant to stay small. A
/// shallower tree puts the bottom within reach, and `periodic_compaction_seconds`
/// is what walks rows down to it on a store that is not growing.
const LOG_MODE_NUM_LEVELS: &str = "4";

/// Sets a column-family option, replacing any value already present.
///
/// `combined` emits one line per option, so a key that came from the defaults is
/// already in the list. Appending a second line for it would leave two, and
/// [`bottommost_level`] reads the first -- so an override has to replace rather
/// than accumulate, or the filter would compare against the level count RocksDB
/// is not using.
fn set_cf_option(cf_lines: &mut Vec<String>, key: &str, value: &str) {
    let is_key = |line: &str| {
        line.split_once('=')
            .is_some_and(|(name, _)| name.trim() == key)
    };

    match cf_lines.iter_mut().find(|line| is_key(line)) {
        Some(existing) => *existing = format!("{key}={value}"),
        None => cf_lines.push(format!("{key}={value}")),
    }
}

/// Whether a compaction reading `level` should drop this value.
///
/// Split out of the filter closure so the rule can be asserted directly. It
/// cannot be reached from the outside: RocksDB offers a value to the filter only
/// once a compaction has materialised it, and in a store small enough to drive
/// from a test that materialisation already lands at the bottom -- so the
/// declining branch never runs, and a test written against row counts alone
/// passes whether or not the level is checked.
fn drops_at_level(level: u32, bottommost: u32, value: &[u8]) -> bool {
    level == bottommost && log::rollup::is_fully_dropped(value)
}

pub fn fs_write_and_load_settings(
    cfg: &Cfg,
    flags: u8,
) -> (
    rocksdb::Options,
    Vec<rocksdb::ColumnFamilyDescriptor>,
    cache::Cache,
) {
    let is_logs_based_enabled = flags & FLAG_DB_LOG_ENABLE != 0;
    let is_blobs_cf_enabled = (flags & FLAG_DB_BLOB_ENABLE != 0) || is_logs_based_enabled;

    // Idempotent; this runs once per DB open.
    cfg.register_validator::<conf::Config>("engine");
    let engine_cfg: conf::Config = cfg.snap().subtable("engine");

    let mut env = Env::new().unwrap();

    let db_lines = combined(iter_opt(H_DB), &engine_cfg.db);
    let mut cf_lines = combined(iter_opt(H_CF), &engine_cfg.cf);
    let block_lines = combined(iter_opt(H_BLOCK), &engine_cfg.block);

    let mut blobs_cf_lines = combined(iter_opt(H_CF), &engine_cfg.blobs_cf);
    let blobs_block_lines = combined(iter_opt(H_BLOCK), &engine_cfg.blobs_block);

    env.set_high_priority_background_threads(engine_cfg.threads_high as _);
    env.set_low_priority_background_threads(engine_cfg.threads_low as _);

    // TODO block_cache_bits
    let block_cache = rocksdb::Cache::new_lru_cache(*engine_cfg.block_cache_size as usize);

    let mut options = rocksdb::Options::default();

    let db_lines = db_lines.join(";");
    let mut options = options.get_options_from_string(&db_lines).unwrap();

    options.create_missing_column_families(true);
    options.enable_statistics();
    options.set_statistics_level(rocksdb::statistics::StatsLevel::All);
    options.increase_parallelism(engine_cfg.increase_parallelism as _);
    options.add_compact_on_deletion_collector_factory(100, 10, 0.6);

    if engine_cfg.use_write_buffer_manager {
        // A size of zero is RocksDB's documented "no limit", and is what this
        // passed before by way of `!0` -- which reaches the same thresholds by
        // accident rather than by contract, since the constructor computes
        // `buffer_size * 7 / 8` and that multiply overflows at `usize::MAX`.
        // Either way the block cache is still charged for memtable memory,
        // which is the reason this manager is here.
        let wbm = WriteBufferManager::new_write_buffer_manager_with_cache(
            *engine_cfg.write_buffer_manager_size as usize,
            engine_cfg.write_buffer_manager_allow_stall,
            block_cache.clone(),
        );
        options.set_write_buffer_manager(&wbm);
    }

    let cache = if *engine_cfg.cache_size > 0 {
        cache::Cache::new(*engine_cfg.cache_size)
    } else {
        cache::Cache::default()
    };

    let default_cf = {
        // A shallower tree than RocksDB's default, so the level the compaction
        // filter acts on is one this column family actually reaches. Only when
        // the deployment has not said otherwise -- an explicit `cf.num_levels`
        // is a decision about the tree, and the filter follows it rather than
        // the other way round.
        if is_logs_based_enabled && !engine_cfg.cf.contains_key("num_levels") {
            set_cf_option(&mut cf_lines, "num_levels", LOG_MODE_NUM_LEVELS);
        }

        let inner = block_lines.join(";");
        cf_lines.push(format!("block_based_table_factory={{{}}}", inner));

        // Read from the same lines RocksDB is configured with, so the two
        // cannot drift: a `cf.num_levels` in the TOML would otherwise move the
        // bottom out from under the filter, which would then simply stop
        // matching -- no error, just rows accumulating again.
        let bottommost_level = bottommost_level(&cf_lines);

        let cf_lines = cf_lines.join(";");
        let mut options = options.get_options_from_string(&cf_lines).unwrap();

        if is_logs_based_enabled {
            options.set_merge_operator(
                "log_merge_op",
                log::merge::merge_full,
                log::merge::merge_operands,
            );

            // A row whose log has been emptied is still a row: the merge
            // operator returns the collapsed log, and a collapsed log with
            // nothing in it is a zero-length value rather than a deletion.
            // Nothing else removes those, so they accumulate for the life of
            // the store -- one per key ever evicted.
            //
            // Only at the bottom. Removing a key here drops it from the output
            // without leaving a tombstone, so an older version surviving below
            // would become visible again. At the bottommost level there is
            // nothing below, and because that level is a single sorted run any
            // file holding another version of the key overlaps this range and
            // is an input to the same compaction.
            options.set_compaction_filter("log_compaction_filter", move |level, _key, value| {
                match drops_at_level(level, bottommost_level, value) {
                    true => CompactionDecision::Remove,
                    false => CompactionDecision::Keep,
                }
            });
        } else {
            options.set_merge_operator("merge_op", merge::merge_full, merge::merge_operands);
        }

        options
    };

    let mut cfs = vec![ColumnFamilyDescriptor::new("default", default_cf)];

    if is_blobs_cf_enabled {
        let inner = blobs_block_lines.join(";");
        blobs_cf_lines.push(format!("block_based_table_factory={{{}}}", inner));

        let cf_lines = blobs_cf_lines.join(";");
        //let mut options = options.get_options_from_string(&db_lines).unwrap();
        let options = options.get_options_from_string(&cf_lines).unwrap();

        cfs.push(ColumnFamilyDescriptor::new(crate::BLOBS_CF, options));
    }

    (options, cfs, cache)
}

#[cfg(test)]
mod tests {
    use crate::{tests::CFG, FLAG_DB_BLOB_ENABLE};

    /// The filter compares against this number, so getting it wrong does not
    /// fail -- it just stops matching, and the rows it was added to reclaim
    /// quietly accumulate again. These pin the reading rather than the value.
    #[test]
    fn test_bottommost_level_is_read_from_the_configured_levels() {
        let lines = |levels: &str| vec![format!("num_levels={levels}")];

        assert_eq!(super::bottommost_level(&lines("7")), 6);
        assert_eq!(super::bottommost_level(&lines("2")), 1);

        // Whitespace, and a key that merely ends in `num_levels`.
        assert_eq!(super::bottommost_level(&lines(" 3 ")), 2);
        assert_eq!(
            super::bottommost_level(&["  num_levels = 4 ".to_string()]),
            3
        );
        assert_eq!(
            super::bottommost_level(&["max_num_levels=99".to_string()]),
            6,
            "a key that only ends in num_levels should not be read as it"
        );
    }

    /// Only the bottom, and only when the row holds nothing.
    ///
    /// Removing a key above the bottom drops it with no tombstone, so a version
    /// of it surviving in a lower level would come back to life. The level test
    /// is what prevents that, and no end-to-end test can reach it -- see the
    /// note on `drops_at_level`.
    #[test]
    fn test_only_the_bottommost_level_drops_a_dropped_row() {
        // A log holding one drop record and nothing else.
        let dropped = {
            let mut record = Vec::new();
            super::log::bits::write(
                super::log::models::KIND_DROP,
                1,
                &super::log::models::DropBefore,
                &mut record,
            );
            record
        };
        assert!(
            super::log::rollup::is_fully_dropped(&dropped),
            "fixture should be a fully dropped log"
        );

        assert!(super::drops_at_level(6, 6, &dropped));

        for level in [0, 1, 5, 7] {
            assert!(
                !super::drops_at_level(level, 6, &dropped),
                "a dropped row was taken at level {level}, which is not the bottom"
            );
        }
    }

    /// A row that still holds a value is never taken, at any level.
    #[test]
    fn test_a_live_row_is_never_dropped() {
        let live = {
            let mut record = Vec::new();
            super::log::bits::write(
                super::log::models::KIND_INLINE_VALUE,
                1,
                &super::log::models::InlineValue {
                    inner: vec![1, 2, 3],
                },
                &mut record,
            );
            record
        };
        assert!(!super::log::rollup::is_fully_dropped(&live));

        for level in 0..=7 {
            assert!(!super::drops_at_level(level, 6, &live));
        }
    }

    /// An override has to replace the line already there, not sit beside it --
    /// `bottommost_level` reads the first match, so a duplicate would leave the
    /// filter comparing against the count RocksDB is not using.
    #[test]
    fn test_setting_a_cf_option_replaces_rather_than_duplicates() {
        let mut lines = vec![
            "write_buffer_size=64".to_string(),
            "num_levels=7".to_string(),
            "max_num_levels=99".to_string(),
        ];

        super::set_cf_option(&mut lines, "num_levels", "4");

        assert_eq!(
            lines,
            vec!["write_buffer_size=64", "num_levels=4", "max_num_levels=99"],
            "the existing line should be rewritten in place"
        );
        assert_eq!(super::bottommost_level(&lines), 3);

        // Absent, it is appended.
        super::set_cf_option(&mut lines, "periodic_compaction_seconds", "28800");
        assert!(lines.contains(&"periodic_compaction_seconds=28800".to_string()));
    }

    /// Log mode gets the shallower tree; nothing else does, and a deployment
    /// that names its own level count keeps it.
    ///
    /// Also pins the periodic compaction that makes the shallower tree work. A
    /// store that is not growing produces no compactions of its own, so without
    /// it rows would settle above the bottom and the filter would never be
    /// offered them however few levels there are.
    #[test]
    fn test_log_mode_lowers_the_default_column_family_levels() {
        let option_in_log = |cfg: &ovdc_settings::Cfg, flags: u8, option: &str| {
            let temp = tempfile::tempdir().unwrap();
            let (options, cfs, _cache) = super::fs_write_and_load_settings(cfg, flags);
            let db =
                rocksdb::DB::open_cf_descriptors(&options, temp.path(), cfs).expect("open the db");
            drop(db);

            let needle = format!("{option}:");
            let log = std::fs::read_to_string(temp.path().join("LOG")).unwrap();
            log.lines()
                .filter(|line| line.contains(&needle))
                .map(|line| {
                    line.rsplit(':')
                        .next()
                        .unwrap()
                        .trim()
                        .parse::<u64>()
                        .unwrap()
                })
                .next()
                .unwrap_or_else(|| panic!("{option} should be reported in the LOG"))
        };

        // The default column family is the first reported, so this is its
        // value; the blobs family leaves it at RocksDB's zero.
        assert_eq!(
            option_in_log(
                &ovdc_settings::load_document_with(CFG, |_| {}),
                crate::FLAG_DB_LOG_ENABLE,
                "Options.periodic_compaction_seconds"
            ),
            28800,
            "the default column family needs periodic compaction to walk rows down"
        );

        let levels_in_log = |cfg: &ovdc_settings::Cfg, flags: u8| {
            let temp = tempfile::tempdir().unwrap();
            let (options, cfs, _cache) = super::fs_write_and_load_settings(cfg, flags);
            let db =
                rocksdb::DB::open_cf_descriptors(&options, temp.path(), cfs).expect("open the db");
            drop(db);

            let log = std::fs::read_to_string(temp.path().join("LOG")).unwrap();
            log.lines()
                .filter(|line| line.contains("Options.num_levels:"))
                .map(|line| {
                    line.rsplit(':')
                        .next()
                        .unwrap()
                        .trim()
                        .parse::<u32>()
                        .unwrap()
                })
                .next()
                .expect("num_levels should be reported in the LOG")
        };

        let shipped = ovdc_settings::load_document_with(CFG, |_| {});
        assert_eq!(levels_in_log(&shipped, crate::FLAG_DB_LOG_ENABLE), 4);
        assert_eq!(levels_in_log(&shipped, FLAG_DB_BLOB_ENABLE), 7);

        let explicit = ovdc_settings::load_document_with(CFG, |doc| {
            doc["engine"]["cf"]["num_levels"] = toml_edit::value(5i64);
        });
        assert_eq!(
            levels_in_log(&explicit, crate::FLAG_DB_LOG_ENABLE),
            5,
            "a configured level count should win over the log-mode default"
        );
    }

    /// Absent or unreadable, the fallback has to be RocksDB's own default --
    /// the tree is whatever RocksDB built, not whatever we failed to parse.
    #[test]
    fn test_bottommost_level_falls_back_to_the_rocksdb_default() {
        assert_eq!(super::bottommost_level(&[]), 6);
        assert_eq!(super::bottommost_level(&["write_buffer_size=64".into()]), 6);
        assert_eq!(super::bottommost_level(&["num_levels=".into()]), 6);
        assert_eq!(super::bottommost_level(&["num_levels=many".into()]), 6);

        // A single-level tree has the bottom at zero, not at minus one.
        assert_eq!(super::bottommost_level(&["num_levels=0".into()]), 0);
    }

    #[tokio::test]
    async fn test_update_settings() {
        let temp = tempfile::tempdir().unwrap();

        let cfg = ovdc_settings::load_document_with(CFG, |cfg| {
            cfg["engine"]["db"]["max_open_files"] = 137.into();
        });

        type RocksDb = rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>;

        let (opts, cfs, _cache) = super::fs_write_and_load_settings(&cfg, 0);

        {
            let db = RocksDb::open_cf_descriptors(&opts, temp.path(), cfs).unwrap();
            let _ = db.flush();
            db.cancel_all_background_work(true);
            drop(db);
        }

        let opts_str = std::fs::read_to_string(temp.path().join("LOG")).unwrap();
        assert!(opts_str.contains("max_open_files: 137"));

        // Size-suffixed option values must reach RocksDB already converted.
        // RocksDB's own parser reads "64M" as 1048576*64; ovdc_bytes reads it as
        // 1000000*64. Asserting the resolved number pins that conversion happens
        // on our side, so the two never disagree.
        // Match the option name exactly: RocksDB logs `Options.<name>: <value>`,
        // and a substring match would confuse `write_buffer_size` with
        // `db_write_buffer_size`.
        let resolved = |key: &str| {
            let needle = format!(".{key}: ");
            opts_str
                .lines()
                .find_map(|line| line.split_once(&needle))
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_else(|| {
                    let candidates = opts_str
                        .lines()
                        .filter(|line| line.contains(key))
                        .collect::<Vec<_>>();
                    panic!("{key} not found in RocksDB LOG; candidates: {candidates:#?}")
                })
        };
        assert_eq!(resolved("write_buffer_size"), "64000000");
        assert_eq!(resolved("max_log_file_size"), "64000000");
    }

    #[tokio::test]
    async fn test_update_settings_with_blobs() {
        let temp = tempfile::tempdir().unwrap();

        let cfg = ovdc_settings::load_document_with(CFG, |cfg| {
            cfg["engine"]["db"]["max_open_files"] = 137.into();
        });

        type RocksDb = rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>;

        let (opts, cfs, _cache) = super::fs_write_and_load_settings(&cfg, FLAG_DB_BLOB_ENABLE);

        {
            let db = RocksDb::open_cf_descriptors(&opts, temp.path(), cfs).unwrap();
            let _ = db.flush();
            db.cancel_all_background_work(true);
            drop(db);
        }

        let opts_str = std::fs::read_to_string(temp.path().join("LOG")).unwrap();
        assert!(opts_str.contains("max_open_files: 137"));
    }
}
