// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use criterion::{criterion_group, criterion_main, Criterion};
use hub_cache::block::PutRequest;
use hub_cache::Cache;
use std::fs::File;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use uuid::Uuid;

/// Helper class for running the cache benchmark tests
struct CacheBenchmark {
    _tmp_dir: Option<tempfile::TempDir>,
    cache: Option<Cache>,
    key: Arc<[u8]>,
    extension: Arc<str>,
    client_id: Uuid,
}

impl Deref for CacheBenchmark {
    type Target = Cache;

    fn deref(&self) -> &Self::Target {
        self.cache.as_ref().unwrap()
    }
}

impl DerefMut for CacheBenchmark {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.cache.as_mut().unwrap()
    }
}

impl CacheBenchmark {
    pub fn new() -> Self {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cache = Cache::builder(tmp_dir.path()).open().unwrap();

        let bench = Self {
            _tmp_dir: Some(tmp_dir),
            cache: Some(cache),
            key: "key_00".as_bytes().into(),
            extension: "extension".into(),
            client_id: Uuid::new_v4(),
        };

        bench
    }

    fn reset(&mut self) {
        self._tmp_dir.replace(tempfile::tempdir().unwrap());
        self.cache
            .replace(Cache::builder(self._tmp_dir.as_ref().unwrap().path()).open().unwrap());
    }

    pub fn prepopulate_file(&self, key: &Arc<[u8]>) {
        let file_path = self
            .file_put(
                &PutRequest::new(key, 10, 0, Some(self.extension.clone())),
                &self.client_id,
            )
            .unwrap();
        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "{}", file_path.display()).unwrap();
        for i in 0..10 {
            for j in 0..i {
                writeln!(f, "value_{i:02}_{j:02}").unwrap();
            }
        }
        self.file_close_deprecated(key).unwrap();
    }

    pub fn prepopulate_files(&self, count: usize) {
        for i in 0..count {
            self.prepopulate_file(&Arc::from(format!("key_{i}").as_bytes()));
        }
    }

    pub fn run_write(&mut self) {
        self.reset();
        self.file_put(
            &PutRequest::new(&self.key, 10, 0, Some(self.extension.clone())),
            &self.client_id,
        )
        .unwrap();
        self.file_close_deprecated(&self.key).unwrap();
    }

    pub fn run_read(&self) {
        self.file_get(&self.key, &self.extension).unwrap();
        self.file_close_deprecated(&self.key).unwrap();
    }

    pub fn run_write_read(&mut self) {
        self.reset();
        self.file_put(
            &PutRequest::new(&self.key, 10, 0, Some(self.extension.clone())),
            &self.client_id,
        )
        .unwrap();
        self.file_close_deprecated(&self.key).unwrap();

        self.file_get(&self.key, &self.extension).unwrap();
        self.file_close_deprecated(&self.key).unwrap();
    }

    pub fn run_write_block(&mut self) {
        self.reset();
        self.block_put(&[PutRequest::new(&self.key, 10, 0, None)], &self.client_id)
            .unwrap();
        self.block_commit_deprecated(std::slice::from_ref(&self.key)).unwrap();
    }

    pub fn run_read_block(&self) {
        self.block_get(std::slice::from_ref(&self.key));
        self.block_close_deprecated(std::slice::from_ref(&self.key));
    }

    pub fn run_write_read_block(&mut self) {
        self.reset();
        self.block_put(&[PutRequest::new(&self.key, 10, 0, None)], &self.client_id)
            .unwrap();
        self.block_commit_deprecated(std::slice::from_ref(&self.key)).unwrap();

        self.block_get(std::slice::from_ref(&self.key));
        self.block_close_deprecated(std::slice::from_ref(&self.key));
    }

    // Test if database size impacts flush time when only flushing one item
    pub fn run_flush_open_one(&self) {
        self.file_get(&Arc::from("key_0".as_bytes()), &self.extension).unwrap();
        self.flush().unwrap();
    }

    // Test if number of outstanding operations affects flush times
    pub fn run_flush_open_all(&self, count: usize) {
        for i in 0..count {
            self.file_get(&Arc::from(format!("key_{i}").as_bytes()), &self.extension)
                .unwrap();
        }

        self.flush().unwrap();
    }
}

pub fn benchmark_cache(c: &mut Criterion) {
    let mut bench = CacheBenchmark::new();
    let mut group = c.benchmark_group("Cache Benchmarks");
    group.bench_function("write", |b| b.iter(|| bench.run_write()));
    group.bench_function("read", |b| b.iter(|| bench.run_read()));
    group.bench_function("write then read", |b| b.iter(|| bench.run_write_read()));
}

pub fn benchmark_block(c: &mut Criterion) {
    let mut bench = CacheBenchmark::new();
    let mut group = c.benchmark_group("Cache Benchmarks");
    group.bench_function("write block", |b| b.iter(|| bench.run_write_block()));
    group.bench_function("read block", |b| b.iter(|| bench.run_read_block()));
    group.bench_function("write then read block", |b| b.iter(|| bench.run_write_read_block()));
}

pub fn benchmark_flush(c: &mut Criterion) {
    let bench = CacheBenchmark::new();
    bench.prepopulate_files(10000);
    let mut group = c.benchmark_group("Cache Flush Benchmarks");
    group.bench_function("flush one file 100 entries", |b| b.iter(|| bench.run_flush_open_one()));
    group.bench_function("flush one file 1000 entries", |b| b.iter(|| bench.run_flush_open_one()));
    group.bench_function("flush one file 10000 entries", |b| {
        b.iter(|| bench.run_flush_open_one())
    });
    group.bench_function("flush all files 100 entries", |b| {
        b.iter(|| bench.run_flush_open_all(100))
    });
    group.bench_function("flush all files 1000 entries", |b| {
        b.iter(|| bench.run_flush_open_all(1000))
    });
    group.bench_function("flush all files 10000 entries", |b| {
        b.iter(|| bench.run_flush_open_all(10000))
    });
}

criterion_group!(benches, benchmark_cache, benchmark_block, benchmark_flush);
criterion_main!(benches);
