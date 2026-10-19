// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ovcc_storage::{CacheProvenance, DiskStorage, StagedSegment, Storage, StoredMeta};

#[path = "../tests/support/hit_metadata.rs"]
mod hit_metadata;

fn meta(size: u64) -> StoredMeta {
    StoredMeta {
        logical_key: None,
        size,
        revision: 0,
        provenance: CacheProvenance::Origin,
        max_idle_secs: None,
        status: 200,
        etag: Some("\"bench\"".to_string()),
        last_modified: None,
        content_type: None,
        headers: Vec::new(),
        representation_length: None,
        expires_at: None,
        versionable: false,
        content_range: None,
    }
}

/// A cold fill running `begin_fill` (FetchInProgress txn) + write + `commit` (data fsync +
/// exact-size txn + rename + Cached txn). Runs once per object — not the hot path.
fn bench_fill(c: &mut Criterion) {
    let temp = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(temp.path()).unwrap();
    let body = vec![0xABu8; 4096];

    c.bench_function("disk_storage/fill/4KiB", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let key = format!("obj://bench/{counter}");
            let mut writer = storage
                .begin_fill(black_box(&key), meta(body.len() as u64))
                .unwrap();
            writer.write_chunk(black_box(&body)).unwrap();
            let _lease = writer.commit().unwrap();
        })
    });
}

fn bench_evict(c: &mut Criterion) {
    let temp = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(temp.path()).unwrap();
    let key = "obj://bench/evict";
    c.bench_function("disk_storage/evict/4KiB", |b| {
        b.iter_batched(
            || {
                let mut writer = storage.begin_fill(key, meta(4096)).unwrap();
                writer.write_chunk(&[0xAB; 4096]).unwrap();
                writer.commit().unwrap().meta.revision
            },
            |revision| black_box(storage.evict_data(black_box(key), revision).unwrap()),
            BatchSize::PerIteration,
        )
    });
}

/// The HOT path, a cache HIT. `lookup` on an already-`Cached` key — opens the fd,
/// validates the generation and file length, and bumps access history.
fn bench_hit(c: &mut Criterion) {
    let temp = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(temp.path()).unwrap();
    let body = vec![0xABu8; 4096];
    let key = "obj://bench/hot";
    let mut writer = storage.begin_fill(key, meta(body.len() as u64)).unwrap();
    writer.write_chunk(&body).unwrap();
    let _ = writer.commit().unwrap();

    c.bench_function("disk_storage/hit/4KiB", |b| {
        b.iter(|| {
            let hit = storage.lookup(black_box(key)).unwrap();
            black_box(hit);
        })
    });
}

fn bench_path_deadline(c: &mut Criterion) {
    let root = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(root.path()).unwrap();
    let key = "obj://bench/path-deadline";
    let mut writer = storage.begin_fill(key, meta(256)).unwrap();
    writer.write_chunk(&[0xAB; 256]).unwrap();
    drop(writer.commit().unwrap());
    let ovcc_storage::LocalPathLookup::Hit(candidate) = storage.prepare_local_path(key).unwrap()
    else {
        panic!("prepared raw file is missing");
    };
    c.bench_function("disk_storage/path_deadline/grant", |b| {
        b.iter(|| black_box(candidate.grant(black_box(30_000)).unwrap()))
    });
    c.bench_function("disk_storage/path_deadline/warm_lookup_and_grant", |b| {
        b.iter(|| {
            let candidate = storage.cached_local_path(black_box(key)).unwrap();
            black_box(candidate.grant(black_box(30_000)).unwrap())
        })
    });
}

fn bench_hit_metadata(c: &mut Criterion) {
    for (name, azure, headers) in [("12_headers", false, 12), ("azure_shaped", true, 15)] {
        let (_root, storage, key) = hit_metadata::filled(azure, headers);
        c.bench_function(&format!("disk_storage/hit/{name}"), |b| {
            b.iter(|| black_box(storage.lookup(black_box(&key)).unwrap()))
        });
    }
}

/// HIT preparation for the two native layouts. Segmented lookup validates the
/// manifest and each immutable fragment sequentially while retaining only the
/// directory lease; the 1,969 case guards Azure's observed production shape.
fn bench_hit_layouts(c: &mut Criterion) {
    let temp = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(temp.path()).unwrap();
    let uploads = temp.path().join("benchmark-uploads");
    std::fs::create_dir_all(&uploads).unwrap();
    let mut group = c.benchmark_group("disk_storage/hit_preparation");
    for &count in &[1usize, 16, 256, 1_969] {
        let contiguous_key = format!("obj://bench/contiguous/{count}");
        let mut writer = storage
            .begin_fill(&contiguous_key, meta((count * 16) as u64))
            .unwrap();
        writer.write_chunk(&vec![0xAB; count * 16]).unwrap();
        drop(writer.commit().unwrap());

        let segmented_key = format!("obj://bench/segmented/{count}");
        let mut segments = Vec::with_capacity(count);
        for index in 0..count {
            let path = uploads.join(format!("{count}-{index}"));
            std::fs::write(&path, [index as u8; 16]).unwrap();
            segments.push(StagedSegment::new(format!("{count}-{index}"), path, 16));
        }
        drop(
            storage
                .adopt_segmented(&segmented_key, meta((count * 16) as u64), &segments)
                .unwrap(),
        );

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("contiguous", count),
            &contiguous_key,
            |b, key| b.iter(|| black_box(storage.lookup(black_box(key)).unwrap())),
        );
        group.bench_with_input(
            BenchmarkId::new("segmented", count),
            &segmented_key,
            |b, key| b.iter(|| black_box(storage.lookup(black_box(key)).unwrap())),
        );
    }
    group.finish();
}

/// Models the writer side of a large peer transfer using the 512 KiB chunks the
/// proxy streams into `StoreWrite`. Network and TLS costs stay outside this I/O
/// benchmark.
fn bench_peer_stream_fill(c: &mut Criterion) {
    const BODY_BYTES: usize = 64 * 1024 * 1024;
    const CHUNK_BYTES: usize = 512 * 1024;

    let temp = tempfile::tempdir().unwrap();
    let storage = DiskStorage::open(temp.path()).unwrap();
    let chunk = vec![0xCD; CHUNK_BYTES];
    let mut counter = 0_u64;
    let mut group = c.benchmark_group("disk_storage/fill/peer_stream");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(BODY_BYTES as u64));
    group.bench_function("64MiB_512KiB_chunks", |b| {
        b.iter(|| {
            counter += 1;
            let key = format!("obj://bench/peer/{counter}");
            let mut writer = storage
                .begin_fill(black_box(&key), meta(BODY_BYTES as u64))
                .unwrap();
            let reader = writer.in_progress_read().unwrap();
            for _ in 0..(BODY_BYTES / CHUNK_BYTES) {
                writer.write_chunk(black_box(&chunk)).unwrap();
            }
            black_box(&reader);
            let lease = writer.commit().unwrap();
            black_box(reader);
            black_box(lease);
        })
    });
    group.finish();
}

struct OptionalPolicy;

impl ovcc_storage::KeyPolicy for OptionalPolicy {
    fn validate(&self, _key: &ovcc_cachekey::LogicalKey) -> ovcc_core::Result<()> {
        Ok(())
    }
    fn render(&self, parts: ovcc_cachekey::PhysicalKeyParts<'_>) -> ovcc_cachekey::CacheKey {
        ovcc_cachekey::CacheKey::from_raw(format!(
            "{}|{}",
            parts.get("object").flatten().unwrap_or(""),
            parts.get("variant").flatten().unwrap_or("")
        ))
    }
}

fn bench_optional_lookup(c: &mut Criterion) {
    use ovcc_cachekey::{KeyPart, LogicalKey};
    let temp = tempfile::tempdir().unwrap();
    let storage =
        DiskStorage::open_with_key_policy(temp.path(), Some(std::sync::Arc::new(OptionalPolicy)))
            .unwrap();
    let key = |variant: Option<&str>| {
        LogicalKey::new(
            "bench.v1".into(),
            vec![
                KeyPart {
                    name: "object".into(),
                    value: Some("object".into()),
                    optional: false,
                },
                KeyPart {
                    name: "variant".into(),
                    value: variant.map(str::to_owned),
                    optional: true,
                },
            ],
        )
        .unwrap()
    };
    let mut writer = storage
        .begin_fill(
            "object|A",
            StoredMeta {
                logical_key: Some(key(Some("A"))),
                ..meta(4096)
            },
        )
        .unwrap();
    writer.write_chunk(&[0xAB; 4096]).unwrap();
    drop(writer.commit().unwrap());
    let query = key(None);
    c.bench_function("disk_storage/optional_lookup/exact", |b| {
        b.iter(|| black_box(storage.lookup("object|A").unwrap()))
    });
    c.bench_function("disk_storage/optional_lookup/fallback", |b| {
        b.iter(|| black_box(storage.lookup_logical(black_box(&query)).unwrap()))
    });
}

criterion_group!(
    benches,
    bench_fill,
    bench_evict,
    bench_hit,
    bench_path_deadline,
    bench_hit_metadata,
    bench_hit_layouts,
    bench_peer_stream_fill,
    bench_optional_lookup
);
criterion_main!(benches);
