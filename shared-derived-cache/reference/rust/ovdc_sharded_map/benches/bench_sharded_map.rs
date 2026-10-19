// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use criterion::{criterion_group, criterion_main, Criterion};
use ovdc_sharded_map::{ShardedMap, Weight};
use rand::RngExt;
use std::{
    hash::{Hash, Hasher},
    hint::black_box,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

// Declare a large key (256-bits). The unique part is a simple integer.
#[derive(Clone, PartialEq, Eq)]
struct K {
    padding: [u64; 3],
    unique_value: u64,
}

impl Hash for K {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.padding.hash(state);
        self.unique_value.hash(state);
    }
}

#[derive(Clone, Copy)]
struct V;
impl Weight for V {
    fn weight(&self) -> u64 {
        1
    }
}

fn bench_sharded_map_insert(c: &mut Criterion) {
    const NUM_SHARDS: u32 = 2;
    let mut group = c.benchmark_group("insert_varying_num_items");
    for num_items in [10, 1_000, 10_000, 100_000] {
        group.bench_with_input(format!("{}", num_items), &num_items, |b, &size| {
            b.iter_batched_ref(
                || ShardedMap::<K, V>::new(size, Some(NUM_SHARDS)),
                |testee| {
                    for i in 0..size {
                        testee.insert(
                            K {
                                padding: [0; 3],
                                unique_value: i,
                            },
                            V,
                        );
                    }
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

fn bench_sharded_map_get_or_contains_key_with_concurrency<F>(
    c: &mut Criterion,
    group_name: &str,
    operation_fn: &'static F,
) where
    F: Fn(&ShardedMap<K, V>, &K) + Sync + 'static,
{
    const NUM_ITEMS_PREFILL: u64 = 100_000;
    const READS_PER_THREAD: usize = 10_000;

    let map = Arc::new(ShardedMap::<K, V>::new(NUM_ITEMS_PREFILL, None));

    // Prefill the map.
    for i in 0..NUM_ITEMS_PREFILL {
        map.insert(
            K {
                padding: [0; 3],
                unique_value: i,
            },
            V,
        );
    }

    let mut group = c.benchmark_group(group_name);

    for num_threads in [1, 8, 16] {
        group.bench_with_input(
            format!("{}_thread(s)", num_threads),
            &num_threads,
            |b, &num_threads| {
                b.iter_batched(
                    || (),
                    |_| {
                        let mut handles = vec![];
                        for _ in 0..num_threads {
                            let map = map.clone();
                            handles.push(thread::spawn(move || {
                                let mut rng = rand::rng();
                                let mut thread_time = Duration::ZERO;
                                for _ in 0..READS_PER_THREAD {
                                    let key = K {
                                        padding: [0; 3],
                                        unique_value: rng.random_range(0..NUM_ITEMS_PREFILL),
                                    };
                                    let start = Instant::now();
                                    operation_fn(&map, &key);
                                    thread_time += start.elapsed();
                                }
                                thread_time
                            }));
                        }
                        let mut total_time = Duration::ZERO;
                        for handle in handles {
                            let thread_time = handle.join().unwrap();
                            total_time += thread_time;
                        }
                        total_time
                    },
                    criterion::BatchSize::SmallInput,
                )
            },
        );
    }

    group.finish();
}

fn bench_sharded_map_get_with_concurrency(c: &mut Criterion) {
    bench_sharded_map_get_or_contains_key_with_concurrency(
        c,
        "get_with_concurrency",
        &|map, key| {
            black_box(map.get(key));
        },
    );
}

fn bench_sharded_map_contains_key_with_concurrency(c: &mut Criterion) {
    bench_sharded_map_get_or_contains_key_with_concurrency(
        c,
        "contains_key_with_concurrency",
        &|map, key| {
            black_box(map.contains_key(key));
        },
    );
}

criterion_group!(
    benches,
    bench_sharded_map_insert,
    bench_sharded_map_get_with_concurrency,
    bench_sharded_map_contains_key_with_concurrency,
);
criterion_main!(benches);
