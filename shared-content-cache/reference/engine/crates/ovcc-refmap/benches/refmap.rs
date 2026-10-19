// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Compares `RefMap` with two mutex-protected directories on the operations a
//! cache performs to coordinate work on each key.
//!
//! "never_removing" is a single `parking_lot::Mutex` over a `HashMap<String, Arc<_>>`
//! that inserts a key on first use and never removes it, so every key ever
//! acquired stays resident. "single_mutex" is one mutex-protected map that
//! counts the users of each entry under its lock and removes a key at its
//! final put. All three hash with `RandomState` and store the same payload, a
//! small per-key coordination value, under keys of realistic length that are
//! built before any timing starts.
//!
//! Every directory starts each benchmark holding the same population of live
//! keys, which stands in for the fills a busy replica has in flight. Each
//! benchmark times one operation per iteration, including the put of any
//! handle the operation returns. CI pins the CPU suite to one core, so parallel
//! scaling and the comparison with other concurrent maps live in
//! `examples/scaling.rs` instead.
//!
//! No benchmark calls `RefMap::maintain`. Under steady churn, puts advance
//! the reclamation epoch on their own, and each map is dropped at the end of
//! its benchmark, which frees everything it still retired.

use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, criterion_group, criterion_main};
use ovcc_refmap::{Builder, Ref, RefMap};
use parking_lot::Mutex;

/// Fixed because the default of four shards per CPU is 256 on a 64-thread
/// machine but 4 under CI's single-core pinning, which would make the two
/// environments measure different maps.
const SHARDS: usize = 64;

/// Keys every directory holds throughout a benchmark.
const LIVE_KEYS: usize = 1_024;

/// Keys the churn workloads cycle through, far more than are ever in flight,
/// so "never_removing" ends up retaining over a hundred times the live population.
const UNIQUE_KEYS: usize = 1 << 17;

/// A small per-key coordination value, 40 bytes inline on 64-bit targets. The
/// value it models also owns three heap allocations for its tokio mutex, start
/// time, and watch channel, which "never_removing" would pay once per key and the other
/// two on every creation. Leaving them out keeps tokio out of this crate and
/// the measurements on the directories themselves.
#[expect(
    dead_code,
    reason = "only the payload's size matters to the directories"
)]
struct Slot {
    started_at: Mutex<Option<Instant>>,
    fills: AtomicU64,
    waiters: AtomicU64,
}

impl Slot {
    fn new() -> Self {
        Self {
            started_at: Mutex::new(None),
            fills: AtomicU64::new(0),
            waiters: AtomicU64::new(0),
        }
    }
}

/// Maps each key to one shared payload and hands out handles to it.
trait Directory: Default + Sync {
    /// Criterion function name.
    const NAME: &'static str;

    type Handle<'a>
    where
        Self: 'a;

    /// What cloning a held handle produces.
    type Shared;

    /// Gets a reference to the key's payload, creating it when absent.
    fn acquire<'a>(&'a self, key: &'a str) -> Self::Handle<'a>;

    /// Gets a reference to the key's payload only when it already exists.
    fn get<'a>(&'a self, key: &'a str) -> Option<Self::Handle<'a>>;

    fn share(handle: &Self::Handle<'_>) -> Self::Shared;

    /// Summarizes what the directory retains, which the timings cannot show.
    fn footprint(&self) -> String;
}

struct RefMapDirectory(RefMap<String, Slot>);

impl Default for RefMapDirectory {
    fn default() -> Self {
        Self(Builder::new().shards(SHARDS).build())
    }
}

impl Directory for RefMapDirectory {
    const NAME: &'static str = "refmap";

    type Handle<'a> = Ref<String, Slot>;

    type Shared = Ref<String, Slot>;

    fn acquire<'a>(&'a self, key: &'a str) -> Ref<String, Slot> {
        self.0
            .get_or_insert_with(key, || (key.to_owned(), Slot::new()))
    }

    fn get<'a>(&'a self, key: &'a str) -> Option<Ref<String, Slot>> {
        self.0.get(key)
    }

    fn share(handle: &Ref<String, Slot>) -> Ref<String, Slot> {
        Ref::clone(handle)
    }

    fn footprint(&self) -> String {
        let stats = self.0.stats();
        format!(
            "{} entries in {} slots, {} retired awaiting reclamation",
            stats.entries, stats.capacity, stats.retired
        )
    }
}

/// Inserts a key on first use and never removes it. `entry` takes an owned
/// key, so even getting a reference to an existing entry allocates and frees a
/// copy of the key while holding the lock.
#[derive(Default)]
struct NeverRemovingDirectory(Mutex<HashMap<String, Arc<Slot>>>);

impl Directory for NeverRemovingDirectory {
    const NAME: &'static str = "never_removing";

    type Handle<'a> = Arc<Slot>;

    type Shared = Arc<Slot>;

    fn acquire<'a>(&'a self, key: &'a str) -> Arc<Slot> {
        let mut map = self.0.lock();
        map.entry(key.to_owned())
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone()
    }

    fn get<'a>(&'a self, key: &'a str) -> Option<Arc<Slot>> {
        self.0.lock().get(key).cloned()
    }

    fn share(handle: &Arc<Slot>) -> Arc<Slot> {
        Arc::clone(handle)
    }

    fn footprint(&self) -> String {
        format!("{} entries", self.0.lock().len())
    }
}

/// Adds a user count per entry to the map shape and lock of "never_removing", so the
/// final put removes the key.
#[derive(Default)]
struct SingleMutexDirectory(Mutex<HashMap<String, (Arc<Slot>, usize)>>);

/// Borrows the caller's key so that a put can find its entry. An owned
/// handle would have to copy the key instead, so the borrow spares this
/// baseline an allocation per acquisition.
struct SingleMutexHandle<'a> {
    directory: &'a SingleMutexDirectory,
    key: &'a str,
    slot: Arc<Slot>,
}

impl Directory for SingleMutexDirectory {
    const NAME: &'static str = "single_mutex";

    type Handle<'a> = SingleMutexHandle<'a>;

    type Shared = Arc<Slot>;

    fn acquire<'a>(&'a self, key: &'a str) -> SingleMutexHandle<'a> {
        let mut map = self.0.lock();
        let slot = match map.get_mut(key) {
            Some((slot, users)) => {
                *users += 1;
                Arc::clone(slot)
            }
            None => {
                let slot = Arc::new(Slot::new());
                map.insert(key.to_owned(), (Arc::clone(&slot), 1));
                slot
            }
        };
        drop(map);
        SingleMutexHandle {
            directory: self,
            key,
            slot,
        }
    }

    fn get<'a>(&'a self, key: &'a str) -> Option<SingleMutexHandle<'a>> {
        let mut map = self.0.lock();
        let (slot, users) = map.get_mut(key)?;
        *users += 1;
        let slot = Arc::clone(slot);
        drop(map);
        Some(SingleMutexHandle {
            directory: self,
            key,
            slot,
        })
    }

    fn share(handle: &SingleMutexHandle<'_>) -> Arc<Slot> {
        Arc::clone(&handle.slot)
    }

    fn footprint(&self) -> String {
        format!("{} entries", self.0.lock().len())
    }
}

impl Drop for SingleMutexHandle<'_> {
    fn drop(&mut self) {
        let mut map = self.directory.0.lock();
        let Some((_, users)) = map.get_mut(self.key) else {
            return;
        };
        *users -= 1;
        if *users == 0 {
            let removed = map.remove_entry(self.key);
            // Freeing the key and payload after unlocking keeps the critical
            // section as short as this baseline can make it.
            drop(map);
            drop(removed);
        }
    }
}

/// Every key the benchmarks use, rendered once, outside all timing.
struct Keys {
    live: Vec<String>,
    /// Held by a long-lived handle for the whole benchmark.
    held: String,
    /// Never acquired.
    absent: String,
    /// Never held between operations, so each acquisition finds no entry.
    idle: String,
    unique: Vec<String>,
}

impl Keys {
    fn new() -> Self {
        let unique = LIVE_KEYS + 3;
        Self {
            live: (0..LIVE_KEYS).map(cache_key).collect(),
            held: cache_key(LIVE_KEYS),
            absent: cache_key(LIVE_KEYS + 1),
            idle: cache_key(LIVE_KEYS + 2),
            unique: (unique..unique + UNIQUE_KEYS).map(cache_key).collect(),
        }
    }

    /// Acquires the live population and the held key in `directory`; the
    /// returned handles keep them alive until dropped.
    fn hold<'a, D: Directory>(&'a self, directory: &'a D) -> Vec<D::Handle<'a>> {
        self.live
            .iter()
            .chain([&self.held])
            .map(|key| directory.acquire(key))
            .collect()
    }
}

/// An 86-byte key laid out as
/// `<generation>|<host>|<method>|<uri>|<args>|<if_match>|<range>`. Keys share
/// their long prefix, as keys of one bucket do, so every equality check
/// compares most of the key.
fn cache_key(index: usize) -> String {
    let etag = (index as u32).wrapping_mul(0x9E37_79B9);
    format!(
        "0|bucket.s3.us-west-2.amazonaws.com|GET|/scenes/factory/mesh_{index:08x}.usd||\"{etag:08x}\"|"
    )
}

/// Runs `$workload::<D>` for every directory inside one group named after the
/// workload, so the three implementations always time the same routine.
macro_rules! compare {
    ($c:expr, $keys:expr, $workload:ident) => {{
        let mut group = $c.benchmark_group(concat!("refmap/", stringify!($workload)));
        $workload::<RefMapDirectory>(&mut group, $keys);
        $workload::<NeverRemovingDirectory>(&mut group, $keys);
        $workload::<SingleMutexDirectory>(&mut group, $keys);
        group.finish();
    }};
}

fn single_threaded(c: &mut Criterion) {
    let keys = Keys::new();
    compare!(c, &keys, acquire_release_unique_keys);
    compare!(c, &keys, acquire_join_existing);
    compare!(c, &keys, get_hit);
    compare!(c, &keys, get_miss);
    compare!(c, &keys, handle_clone_drop);
    compare!(c, &keys, same_key_sequential_reacquire);
}

/// Acquires and puts a different key each iteration. `RefMap` and
/// "single_mutex" create and free an entry every time, while "never_removing" keeps
/// every key it has seen, which is the growth `RefMap` exists to remove.
fn acquire_release_unique_keys<D: Directory>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    keys: &Keys,
) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    // The cursor outlives each sample, so the cycle covers every key instead
    // of restarting at the first one.
    let mut next = 0;
    let mut passes = 0;
    let mut ran = false;
    group.bench_function(D::NAME, |b| {
        ran = true;
        b.iter(|| {
            let key = keys.unique[next].as_str();
            next += 1;
            if next == keys.unique.len() {
                next = 0;
                passes += 1;
            }
            directory.acquire(black_box(key))
        })
    });
    if ran {
        eprintln!(
            "refmap/acquire_release_unique_keys/{}: {} after {passes} full passes over \
             {UNIQUE_KEYS} keys",
            D::NAME,
            directory.footprint()
        );
    }
}

/// A long-lived handle holds the key, so each iteration gets a reference to
/// the live entry and then puts it, as each request of a herd does while one
/// of them fills the missing object.
fn acquire_join_existing<D: Directory>(group: &mut BenchmarkGroup<'_, WallTime>, keys: &Keys) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    group.bench_function(D::NAME, |b| {
        b.iter(|| directory.acquire(black_box(keys.held.as_str())))
    });
}

fn get_hit<D: Directory>(group: &mut BenchmarkGroup<'_, WallTime>, keys: &Keys) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    group.bench_function(D::NAME, |b| {
        b.iter(|| directory.get(black_box(keys.held.as_str())))
    });
}

fn get_miss<D: Directory>(group: &mut BenchmarkGroup<'_, WallTime>, keys: &Keys) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    group.bench_function(D::NAME, |b| {
        b.iter(|| directory.get(black_box(keys.absent.as_str())))
    });
}

/// `Ref::clone` for `RefMap`, against the `Arc` clone that the handles of both
/// baselines carry.
fn handle_clone_drop<D: Directory>(group: &mut BenchmarkGroup<'_, WallTime>, keys: &Keys) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    let handle = directory.acquire(keys.held.as_str());
    group.bench_function(D::NAME, |b| b.iter(|| D::share(black_box(&handle))));
}

/// Acquires and puts one key that no other handle holds, so `RefMap` and
/// "single_mutex" recreate its entry every time. This is the case in which
/// retention spares "never_removing" from rebuilding the entry, although its `entry`
/// call still copies the key.
fn same_key_sequential_reacquire<D: Directory>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    keys: &Keys,
) {
    let directory = D::default();
    let _live = keys.hold(&directory);
    group.bench_function(D::NAME, |b| {
        b.iter(|| directory.acquire(black_box(keys.idle.as_str())))
    });
}

criterion_group!(benches, single_threaded);
criterion_main!(benches);
