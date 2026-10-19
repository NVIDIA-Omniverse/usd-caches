// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Opens several environments including the database, LRU and settings. This
//! creates a context object necessary to operate as a server.

mod config;

use std::{
    iter,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use ovdc_jwt::Verify;
use ovdc_lru::KeyLog;
use ovdc_map::{environment::rocks_metrics, Database, PropertyKind};
use ovdc_settings::{Cfg, Snap};
use ovdc_trace::metrics::Measure;
use rand::Rng;
use tokio::{fs, task::JoinHandle};

pub use config::*;

/// A context object necessary to process events as a server.
#[derive(Clone)]
pub struct Context<D> {
    /// Random id of this context instance.
    #[allow(unused)]
    pub id: u32,

    /// The DB env.
    pub env: D,

    /// Track keys as they are used.
    pub key_log: Arc<KeyLog>,

    /// Hot-reload settings.
    pub settings2: Cfg,

    background: Arc<JoinHandle<()>>,
    gc_task: Arc<JoinHandle<()>>,

    /// Used to verify JWK tokens.
    pub verify_jwk: Option<Arc<Verify>>,

    _gc_env: Arc<ovdc_gc::Environment>,
}

impl<D: Database> Context<D> {
    /// Shuts down all of the environments and gracefully closes the database.
    pub async fn close(&self) {
        self.background.abort();
        self.key_log.stop(Duration::from_secs(3)).await;
        ovdc_trace::drop!(self.env.close(), "failed to close db");
    }
}

impl<D> Drop for Context<D> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.background) == 1 {
            self.background.abort();
            self.gc_task.abort();
        }
    }
}

/// Opens an environment for the KV store.
///
/// `env_dir` must be able to convert to a utf-8 string. This is a requirement
/// of [`Database::new`], which takes the path as a `&str`.
#[tracing::instrument(skip_all)]
pub async fn open<D: Database>(
    env_dir: impl AsRef<Path>,
    settings: Cfg,
) -> Result<Context<D>, Box<dyn std::error::Error + Send + Sync>> {
    let env_dir = env_dir.as_ref();
    tracing::trace!(dir = ?env_dir, "open context");

    let path = Path::new(env_dir);

    let _ = fs::create_dir_all(&path).await;
    let db_path = path.join("db");
    let _ = fs::create_dir_all(&db_path).await;

    // Sections owned by this crate, plus crates that never see the Cfg itself.
    // Each registration checks its section on reload before the new settings
    // are published, so a bad edit fails at the edit rather than at the next
    // caller that happens to read it.
    settings.register_validator::<Store>("store");
    settings.register_validator::<Grpc>("grpc");
    settings.register_validator::<Tls>("grpcTls");
    settings.register_validator::<Jwt>("grpcJwt");
    ovdc_gc::register_settings(&settings);

    let snap = settings.snap();

    let store_cfg: Store = snap.subtable("store");
    let env = {
        let mut flags = 0;
        let (name, or_flags) = match store_cfg.mode {
            EngineMode::Legacy => ("LEGACY", 0),
            EngineMode::Log => (
                "LOG",
                ovdc_map::FLAG_DB_LOG_ENABLE | ovdc_map::FLAG_DB_BLOB_ENABLE,
            ),
            EngineMode::LegacyAndDiscrete => {
                ("LEGACY WITH DISCRETE BLOBS", ovdc_map::FLAG_DB_BLOB_ENABLE)
            }
        };
        flags |= or_flags;
        tracing::info!("Engine: '{name}' ({flags:b})");

        D::new(
            db_path.to_str().unwrap(),
            *store_cfg.max_size as usize,
            flags,
            &settings,
        )
        .map_err(|e| format!("context failed to open the db: {}", e))?
    };

    let gc_env =
        ovdc_gc::Environment::new(settings.snap().into_subtable("garbageCollection")).await?;

    let background = tokio::spawn(report_loop(env.clone(), *store_cfg.max_size));
    let gc_task = ovdc_gc::spawn_background(&gc_env, env.clone(), *store_cfg.max_size);

    let verify_jwk = {
        let grpc_jwt_cfg: Jwt = snap.subtable("grpcJwt");
        match grpc_jwt_cfg.enabled {
            true => {
                let from = match grpc_jwt_cfg.jwk_public_keyset_url == ["debug"] {
                    true => ovdc_jwt::jwk::debug_jwk(),
                    _ => ovdc_jwt::jwk::http_get(grpc_jwt_cfg.jwk_public_keyset_url.clone()),
                };
                Some(Arc::new(
                    ovdc_jwt::Verify::new(
                        from,
                        grpc_jwt_cfg.cache_size_mb * 1_000_000,
                        Duration::from_secs(grpc_jwt_cfg.jwk_update_interval_secs),
                        grpc_jwt_cfg
                            .verify_aud
                            .then(|| vec![grpc_jwt_cfg.aud_claim.clone()]),
                        grpc_jwt_cfg.verify_exp,
                    )
                    .await,
                ))
            }
            false => None,
        }
    };

    Ok(Context {
        id: rand::rng().next_u32(),

        key_log: KeyLog::new(
            2,
            256,
            Duration::from_secs(10),
            LruWriter { db: env.clone() },
        )
        .into(),

        env,
        _gc_env: gc_env.clone(),
        settings2: settings,

        background: background.into(),
        gc_task: gc_task.into(),

        verify_jwk,
    })
}

mod metrics {
    ovdc_trace::meter! {
        static;
        pub ROCKS_INTRINSIC_GAUGE: Gauge { };
        pub ROCKS_INTRINSIC_F64_GAUGE: GaugeFloat { };
        pub ROCKS_INTERVAL_BYTES_PER_SEC: Gauge { };
        pub ROCKS_CAPACITY: Gauge { };
    }
}

struct MetricInterval {
    total_write_bytes: u64,
    total_read_bytes: u64,
    time: std::time::Instant,
}

async fn report_loop<D: Database>(env: D, max: u64) {
    let start = Instant::now();

    let mut last = None;
    while let Ok((next_interval, stat)) = {
        let env = env.clone();
        tokio::task::spawn_blocking(move || {
            let stat = env.stat()?.total;

            metrics::ROCKS_CAPACITY.measure(
                max.saturating_sub(stat),
                &ovdc_trace::label!("name": "bytes_free"),
            );
            metrics::ROCKS_CAPACITY.measure(
                p_free(stat, max).ceil() as u64,
                &ovdc_trace::label!("name": "p_free"),
            );

            Ok((metrics_report(&env, last), stat))
        })
        .await
        .unwrap() as Result<(MetricInterval, u64), ovdc_map::Error>
    } {
        last = Some(next_interval);

        let p_free = ((1.0 - ((stat as f64) / (max as f64))) * 100.0).ceil() as u8;
        tracing::info!(p_free, "background");

        tokio::time::sleep(Duration::from_secs(15)).await;

        crate::grpc::metrics::API_UPTIME_SECS.measure(start.elapsed().as_secs(), &[]);
    }
}

fn metrics_report<D: Database>(db: &D, last: Option<MetricInterval>) -> MetricInterval {
    let props = db.properties();

    let total_read_bytes = rocks_metrics::PROP__ROCKSDB_OPTIONAL_BYTES_READ
        .extract(&props)
        .unwrap()
        .as_u64()
        .unwrap()
        + rocks_metrics::PROP__ROCKSDB_OPTIONAL_BLOBDB_BLOB_FILE_BYTES_READ
            .extract(&props)
            .unwrap()
            .as_u64()
            .unwrap();

    let total_write_bytes = rocks_metrics::PROP__ROCKSDB_OPTIONAL_BYTES_WRITTEN
        .extract(&props)
        .unwrap()
        .as_u64()
        .unwrap()
        + rocks_metrics::PROP__ROCKSDB_OPTIONAL_BLOBDB_BLOB_FILE_BYTES_WRITTEN
            .extract(&props)
            .unwrap()
            .as_u64()
            .unwrap();
    let now = Instant::now();

    if let Some(last) = last {
        let interval_time = now - last.time;

        let interval_written_bytes = total_write_bytes - last.total_write_bytes;
        let interval_read_bytes = total_read_bytes - last.total_read_bytes;

        metrics::ROCKS_INTERVAL_BYTES_PER_SEC.measure(
            bps(interval_time, interval_written_bytes),
            &ovdc_trace::label!("name": "write"),
        );
        metrics::ROCKS_INTERVAL_BYTES_PER_SEC.measure(
            bps(interval_time, interval_read_bytes),
            &ovdc_trace::label!("name": "read"),
        );
    }

    if let Some(is_write_stopped) = rocks_metrics::PROP_ROCKSDB_IS_WRITE_STOPPED.extract(&props) {
        let is_write_stopped = is_write_stopped.as_bool().unwrap();
        let is_compaction_pending = rocks_metrics::PROP_ROCKSDB_COMPACTION_PENDING
            .extract(&props)
            .unwrap()
            .as_bool()
            .unwrap();

        let running_flushes = rocks_metrics::PROP_ROCKSDB_NUM_RUNNING_FLUSHES
            .extract(&props)
            .unwrap()
            .as_u64()
            .unwrap();

        let running_compactions = rocks_metrics::PROP_ROCKSDB_NUM_RUNNING_COMPACTIONS
            .extract(&props)
            .unwrap()
            .as_u64()
            .unwrap();

        let memtable_pending_flush = rocks_metrics::PROP_ROCKSDB_MEM_TABLE_FLUSH_PENDING
            .extract(&props)
            .unwrap()
            .as_bool()
            .unwrap();

        tracing::info!(
            is_compaction_pending,
            is_write_stopped,
            running_flushes,
            running_compactions,
            memtable_pending_flush,
            "rocks info"
        );
    }

    for (id, value) in props {
        // Every series carries a `cf` label so the dimension is uniform:
        // database-wide values report "-" rather than omitting the label, which
        // would leave the same metric name sometimes one-dimensional and
        // sometimes two.
        let labels = ovdc_trace::label!("name": id.name, "cf": id.scope.label());

        match value {
            ovdc_map::PropertyValue::Bool(x) => {
                metrics::ROCKS_INTRINSIC_GAUGE.measure(x as u64, &labels);
            }
            ovdc_map::PropertyValue::Number(x) => {
                metrics::ROCKS_INTRINSIC_GAUGE.measure(x, &labels);
            }
            ovdc_map::PropertyValue::Float(x) => {
                metrics::ROCKS_INTRINSIC_F64_GAUGE.measure(x, &labels);
            }
            _ => {}
        }
    }

    MetricInterval {
        total_write_bytes,
        total_read_bytes,
        time: now,
    }
}

fn bps(time: Duration, bytes: u64) -> u64 {
    let bytes_per_micro = bytes as f64 / (time.as_micros() as f64);
    let avg_byte_per_second = bytes_per_micro * 1_000_000.0;
    avg_byte_per_second.round() as u64
}

#[derive(Clone)]
struct LruWriter<D> {
    db: D,
}

impl<D: Database> ovdc_lru::Compaction for LruWriter<D> {
    async fn compact(&self, mut keys: ovdc_lru::Keys) {
        let time = ovdc_map::now_secs();
        let cursor = self.db.open();

        let mut batch = vec![];
        let mut attempt = 0;
        while !keys.is_empty() || !batch.is_empty() {
            // Collect the next batch of keys that will be modified.
            if batch.is_empty() {
                batch.extend(iter::from_fn(|| keys.pop()).take(256));
            }

            let result = {
                let _opts = ovdc_map::thread_local_flags(ovdc_map::FLAGS_HIGHER_DURABILITY);
                cursor.update_many(
                    batch
                        .iter()
                        .copied()
                        .map(|key| {
                            (
                                key,
                                ovdc_map::Update {
                                    access_time: ovdc_map::Change::Set(time),
                                    ttl_ms: ovdc_map::Change::No,
                                },
                            )
                        })
                        .collect(),
                )
            };

            if result.is_err() {
                tokio::time::sleep(Duration::from_millis(100)).await;

                // Try a few times.
                attempt += 1;
                if attempt < 3 {
                    continue;
                }
                tracing::warn!("some access time events were lost because the db is busy");
            }

            // The batch was written
            batch.clear();
            attempt = 0;
            tokio::task::yield_now().await;
        }
    }
}

#[inline(always)]
fn p_free(used: u64, cap: u64) -> f64 {
    if used >= cap {
        0.0
    } else {
        let (used, cap) = (used as f64, cap as f64);
        100.0 - ((used * 100.0) / cap)
    }
}
