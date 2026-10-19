// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Collects and deletes garbage from [ovdc_map].

#![deny(unused, missing_docs)]

mod first_pass;
mod lru_eviction;

use std::{
    collections::HashMap,
    io,
    sync::{atomic::AtomicU8, Arc, Weak},
    time::Duration,
};

pub use first_pass::TimeFloor;
use ovdc_bytes::keys;
use ovdc_map::Database;
use ovdc_settings::{SharedSnap, Snap};
use tokio::task::JoinHandle;

/// Configuration for Garbage Collection.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// The minimum free capacity target for the target map.
    ///
    /// If the percentage of free map space falls below the given amount, GC
    /// will begin.
    pub min_free_capacity: u8,

    /// Once GC begins, the system will attempt to remove the given quantile of the entire
    /// keyspace.
    ///
    /// If this number is 60, then 60% of keys will be deleted.
    pub delete_keyspace_quantile: u8,

    /// How long to wait after a GC pass finishes before checking free capacity again.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

const fn default_cooldown_secs() -> u64 {
    10
}

/// The settings section this crate reads its [Config] from.
pub const SETTINGS_SECTION: &str = "garbageCollection";

/// Registers this crate's settings section for reload-time validation.
///
/// [Environment] is handed an already-scoped [Snap], so it never sees the
/// [ovdc_settings::Cfg] itself. Call this once from whoever owns the settings
/// handle; the section name and its type stay here.
pub fn register_settings(settings: &ovdc_settings::Cfg) {
    settings.register_validator::<Config>(SETTINGS_SECTION);
}

/// An [Environment] for garbage collection applied to an [ovdc_map].
pub struct Environment {
    config: SharedSnap<Config>,
}

impl Environment {
    /// Creates a new [Environment] from the given directory and configuration.
    ///
    /// The directory is used to store state and configuration for the GC
    /// process.
    pub async fn new(
        config: impl Snap<Config> + Send + Sync + 'static,
    ) -> Result<Arc<Self>, io::Error> {
        Ok(Self {
            config: config.shared(),
        }
        .into())
    }

    async fn collect_garbage<D: Database>(&self, env: &D) {
        let _t = ovdc_trace::metrics::timer(
            &metrics::GC_TIME_HIST,
            ovdc_trace::label!("operation": "gc"),
        );
        let config = self.config.update_and_get();

        let free_if_before = match first_pass::first_pass(env, config).await {
            Ok(x) => x,
            Err(error_message) => {
                tracing::error!(
                    ?error_message,
                    "failed to perform first garbage collection pass"
                );
                return;
            }
        };

        tracing::info!(floor = free_if_before.0, "will evict entries");

        ovdc_trace::drop!(
            lru_eviction::evict(env, free_if_before, config).await,
            "failed to evict keys"
        );

        tracing::info!("GC stop");
    }
}

/// Map that indicates whether a key can be removed.
///
/// If the atomic is true, the key cannot be removed.
pub type Candidates = Arc<CandidatesMap>;

/// Inner type of candidates map.
pub type CandidatesMap = HashMap<keys::Compound, AtomicU8>;

/// Spawns the background task that drives GC forward for the given [ovdc_map].
pub fn spawn_background<D: Database>(
    env: &Arc<Environment>,
    db_env: D,
    max: u64,
) -> JoinHandle<()> {
    let weak = Arc::downgrade(env);
    tokio::spawn(async move { background(weak, &db_env, max).await })
}

async fn background<D: Database>(weak: Weak<Environment>, env: &D, max: u64) {
    while let Some(gc_env) = weak.upgrade() {
        let min_free = gc_env.config.update_and_get().min_free_capacity;

        let consumed = env.stat().unwrap().total;
        let p_free = p_free(consumed, max);

        if p_free > min_free {
            drop(gc_env);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        tracing::info!(
            p_free,
            min_free,
            "db max capacity reached, garbage collection will start"
        );

        gc_env.collect_garbage(env).await;

        let cooldown_secs = gc_env.config.update_and_get().cooldown_secs;

        // TODO need a better way to do a cool off period
        drop(gc_env);
        tokio::time::sleep(Duration::from_secs(cooldown_secs)).await;
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

pub(crate) mod metrics {
    ovdc_trace::meter! {
        static;
        pub GC_TIME_HIST: HistFloat {
            buckets: vec![60.0, 300.0, 600.0, 1800.0, 3600.0, 10800.0, 18000.0, 28800.0, 43200.0, 86400.0, 172800.0]
        };
    }
}

fn p_free(consumed: u64, max: u64) -> u8 {
    ((1.0 - ((consumed as f64) / (max as f64))) * 100.0).ceil() as u8
}

#[cfg(test)]
mod tests {
    pub const CFG: &str = include_str!("../../ovdc/src/config.toml");
}
