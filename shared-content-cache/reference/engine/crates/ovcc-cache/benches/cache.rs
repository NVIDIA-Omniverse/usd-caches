// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression guard for the cache engine's own CPU envelope: the lock-free
//! lookup on the HIT path and the per-key herd-lock admission on `begin_fill`.
//!
//! The benches run over an in-process fake [`Storage`] whose `lookup` returns a
//! pre-opened fd and whose `begin_fill` hands back a discarding writer, so what
//! is measured is the `ovcc-cache` layer alone (the single-flight `HashMap` +
//! per-key `Mutex`), not disk I/O. Real storage cost is covered separately by
//! the `ovcc-storage` bench.

use std::fs::File;
use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_cache::{Cache, EvictionPolicy, FillDecision};
use ovcc_core::Result;
use ovcc_storage::{
    CacheProvenance, EvictionCandidate, InProgressRead, Lease, Lookup, RecoveryReport, Storage,
    StoreWrite, StoredMeta,
};

/// A `Storage` that never touches the disk on the hot path. `lookup` clones a
/// single pre-opened fd into the returned [`Lease`], mirroring the open fd a
/// real HIT carries while staying far cheaper than a fresh open, so the delta
/// over a raw storage lookup is the cache layer's contribution.
struct FakeStorage {
    fd: File,
    meta: StoredMeta,
    /// When set, `lookup` reports a HIT; otherwise a cold MISS.
    hit: bool,
    candidates: Vec<EvictionCandidate>,
}

impl FakeStorage {
    fn new(hit: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("object.bin");
        std::fs::write(&path, b"cached-body").unwrap();
        let fd = File::open(&path).unwrap();
        // Replay a representative HIT header set (ETag + Content-Type) so the
        // cloned meta carries the same shape a real serve arm reads back.
        let meta = StoredMeta {
            size: 11,
            etag: Some("\"bench\"".to_string()),
            content_type: Some("application/octet-stream".to_string()),
            headers: vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            ..StoredMeta::default()
        };
        // `dir` is leaked so the backing file outlives the fd for the whole run;
        // the OS reclaims it at process exit.
        std::mem::forget(dir);
        Self {
            fd,
            meta,
            hit,
            candidates: Vec::new(),
        }
    }

    fn with_refused_candidates(count: usize) -> Self {
        let mut storage = Self::new(false);
        storage.candidates = (0..count)
            .map(|index| EvictionCandidate {
                key: format!("obj://bench/manager/{index:03}"),
                revision: index as u64,
                size_bytes: 1,
                last_access: 0,
                hit_count: 0,
                provenance: CacheProvenance::Origin,
                max_idle_secs: None,
            })
            .collect();
        storage
    }
}

impl Storage for FakeStorage {
    fn lookup(&self, _key: &str) -> Result<Lookup> {
        if !self.hit {
            return Ok(Lookup::Miss);
        }
        Ok(Lookup::Hit(Lease {
            path: "obj://bench".into(),
            meta: self.meta.clone(),
            file: self.fd.try_clone()?,
            body_offset: 0,
        }))
    }

    fn peek(&self, _key: &str) -> Result<Option<StoredMeta>> {
        Ok(self.hit.then(|| self.meta.clone()))
    }

    fn begin_fill(&self, _key: &str, _meta: StoredMeta) -> Result<Box<dyn StoreWrite>> {
        Ok(Box::new(NullWriter {
            read: InProgressRead {
                file: Arc::new(self.fd.try_clone()?),
                body_offset: 0,
            },
        }))
    }

    fn evict_data(&self, _key: &str, _revision: u64) -> Result<bool> {
        Ok(false)
    }

    fn remove(&self, _key: &str) -> Result<()> {
        Ok(())
    }

    fn recover_on_open(&self) -> Result<RecoveryReport> {
        Ok(RecoveryReport::default())
    }

    fn eviction_candidates(&self) -> Result<Vec<EvictionCandidate>> {
        Ok(self.candidates.clone())
    }
}

/// Discards written bytes; the admission benches only exercise `begin_fill` and
/// release the writer via `abort`, so `commit` is never reached.
struct NullWriter {
    read: InProgressRead,
}

impl StoreWrite for NullWriter {
    fn write_chunk(&mut self, _buf: &[u8]) -> Result<()> {
        Ok(())
    }

    fn in_progress_read(&self) -> Result<InProgressRead> {
        Ok(self.read.clone())
    }

    fn commit(self: Box<Self>) -> Result<Lease> {
        unreachable!("cache admission bench releases the writer with abort, never commit")
    }

    fn abort(self: Box<Self>) {}
}

/// The HOT path. `get`/`lookup` never take the herd lock, so this measures the
/// enum match plus the fd clone a HIT carries; `get` on a MISS pays neither.
fn bench_lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/lookup");

    let miss = Cache::new(FakeStorage::new(false));
    group.bench_function("get_miss", |b| {
        b.iter(|| black_box(miss.get(black_box("obj://bench/x")).unwrap()))
    });

    let hit = Cache::new(FakeStorage::new(true));
    group.bench_function("get_hit", |b| {
        b.iter(|| black_box(hit.get(black_box("obj://bench/x")).unwrap()))
    });
    group.bench_function("lookup_hit", |b| {
        b.iter(|| black_box(hit.lookup(black_box("obj://bench/x")).unwrap()))
    });

    group.finish();
}

/// Single-flight admission cost, including the per-key entry's allocation and
/// last-holder cleanup. Both repeated and unique keys release their entries
/// between iterations; the unique-key arm also formats its key.
fn bench_begin_fill(c: &mut Criterion) {
    let mut group = c.benchmark_group("cache/begin_fill");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();

    let cache = Cache::new(FakeStorage::new(false));
    group.bench_function("acquire_hot_key", |b| {
        b.to_async(&runtime).iter(|| async {
            let decision = cache
                .acquire_fill(black_box("obj://bench/hot"))
                .await
                .unwrap();
            let FillDecision::Owner(permit) = decision else {
                unreachable!("cold benchmark storage always elects an owner")
            };
            drop(permit);
        });
    });
    group.bench_function("hot_key", |b| {
        b.iter(|| {
            let writer = cache
                .begin_fill(black_box("obj://bench/hot"), StoredMeta::default())
                .unwrap();
            writer.abort();
        })
    });

    group.bench_function("unique_keys", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let key = format!("obj://bench/{counter}");
            let writer = cache
                .begin_fill(black_box(&key), StoredMeta::default())
                .unwrap();
            writer.abort();
        })
    });

    let permit = match runtime.block_on(cache.acquire_fill("obj://bench/shareable")) {
        Ok(FillDecision::Owner(permit)) => permit,
        _ => unreachable!("cold shareable benchmark key must elect an owner"),
    };
    let shareable = cache
        .begin_shareable_acquired_fill("obj://bench/shareable", StoredMeta::default(), permit)
        .unwrap();
    group.bench_function("join_in_progress", |b| {
        b.iter(|| {
            black_box(
                cache
                    .join_in_progress(black_box("obj://bench/shareable"))
                    .unwrap(),
            );
        })
    });
    shareable.abort();

    group.finish();
}

/// Measures the bounded manager scan when every exact-generation eviction is
/// refused. The 101st candidate proves the pass stops after 100 attempts.
fn bench_manager_step(c: &mut Criterion) {
    let cache = Cache::with_policy(
        FakeStorage::with_refused_candidates(101),
        EvictionPolicy {
            max_idle_secs: Some(1),
            ..EvictionPolicy::default()
        },
    );
    c.bench_function("cache/manager_step/refused_100_of_101", |b| {
        b.iter(|| black_box(cache.manager_step_at(black_box(100)).unwrap()))
    });
}

criterion_group!(benches, bench_lookup, bench_begin_fill, bench_manager_step);
criterion_main!(benches);
