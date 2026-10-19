// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hash::{BuildHasher, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{
    AtomicBool as StdAtomicBool, AtomicUsize as StdAtomicUsize, Ordering as StdOrdering,
};
use std::sync::{Arc, Barrier};

use super::*;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Value that counts how many times it was dropped.
struct Tracked {
    id: usize,
    drops: Arc<StdAtomicUsize>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, StdOrdering::SeqCst);
    }
}

fn tracked(id: usize, drops: &Arc<StdAtomicUsize>) -> Tracked {
    Tracked {
        id,
        drops: Arc::clone(drops),
    }
}

/// Hashes a `u64` key to itself so tests choose slots and collisions.
#[derive(Clone, Copy, Default)]
struct IdentityHash;

struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 << 8) | u64::from(*byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

impl BuildHasher for IdentityHash {
    type Hasher = IdentityHasher;

    fn build_hasher(&self) -> IdentityHasher {
        IdentityHasher(0)
    }
}

/// Builds a one-shard map with two-slot tables and single-slot steps, so a
/// handful of keys keeps migrations in flight across many operations.
fn small_map<V>() -> RefMap<u64, V, IdentityHash> {
    Builder::new()
        .shards(1)
        .min_capacity(2)
        .migration_step_unchecked(1)
        .hasher(IdentityHash)
        .build()
}

/// Builds a small map with the default migration step, which allows shrinking.
fn shrinking_map<V>() -> RefMap<u64, V, IdentityHash> {
    Builder::new()
        .shards(1)
        .min_capacity(4)
        .migration_step(16)
        .hasher(IdentityHash)
        .build()
}

/// Runs maintenance until retired memory is gone. Other tests share the
/// process-wide epoch, so a concurrently pinned test may delay reclamation
/// briefly; the bound turns a real leak into a failure instead of a hang.
fn drain<K, V, S>(map: &RefMap<K, V, S>) -> Stats {
    for _ in 0..10_000 {
        map.maintain();
        let stats = map.stats();
        if stats.retired == 0 && stats.migrating == 0 {
            return stats;
        }
        std::thread::yield_now();
    }
    map.stats()
}

fn join<T>(handle: std::thread::JoinHandle<T>) -> Result<T, Box<dyn std::error::Error>> {
    handle.join().map_err(|_| "worker thread panicked".into())
}

#[test]
fn refmap_acquire_creates_once_then_joins() {
    // Arrange.
    let map: RefMap<String, usize> = RefMap::new();
    let mut inits = 0;

    // Act.
    let first = map.get_or_insert_with("a", || {
        inits += 1;
        ("a".to_owned(), 7)
    });
    let second = map.get_or_insert_with("a", || unreachable!("live entry must be joined"));

    // Assert.
    assert_eq!(inits, 1);
    assert!(Ref::ptr_eq(&first, &second));
    assert_eq!(*second, 7);
    assert_eq!(first.key(), "a");
    assert_eq!(Ref::users(&first), 2);
    assert_eq!(map.len(), 1);
}

#[test]
fn refmap_get_never_creates() {
    // Arrange.
    let map: RefMap<String, usize> = RefMap::new();

    // Act.
    let missing = map.get("missing");
    let held = map.get_or_insert_with_owned("k".to_owned(), |_| 1);
    let found = map.get("k");

    // Assert.
    assert!(missing.is_none());
    assert!(found.is_some_and(|found| Ref::ptr_eq(&held, &found)));
    assert_eq!(map.len(), 1);
}

#[test]
fn refmap_release_last_handle_drops_value_once_and_unlinks() {
    // Arrange.
    let drops = Arc::new(StdAtomicUsize::new(0));
    let map = small_map();
    let first = map.get_or_insert_with_owned(1, |_| tracked(1, &drops));
    let second = first.clone();

    // Act.
    drop(first);
    let after_first = (drops.load(StdOrdering::SeqCst), map.len());
    drop(second);

    // Assert.
    assert_eq!(after_first, (0, 1), "a remaining handle keeps the entry");
    assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    assert_eq!(map.len(), 0);
    assert!(map.get(&1).is_none());
}

#[test]
fn refmap_acquire_after_release_creates_a_new_generation() {
    // Arrange.
    let drops = Arc::new(StdAtomicUsize::new(0));
    let map = small_map();
    let first = map.get_or_insert_with_owned(5, |_| tracked(1, &drops));
    let first_entry = first.entry;
    drop(first);

    // Act.
    let second = map.get_or_insert_with_owned(5, |_| tracked(2, &drops));

    // Assert.
    assert_eq!(second.id, 2);
    assert_eq!(drops.load(StdOrdering::SeqCst), 1);
    // The first generation is retired, not reused, while readers may hold it.
    assert_ne!(first_entry, second.entry);
}

#[test]
fn refmap_link_losing_candidate_is_destroyed_by_its_builder() {
    // Arrange.
    let drops = Arc::new(StdAtomicUsize::new(0));
    let map = small_map();
    let winner = map.get_or_insert_with_owned(9, |_| tracked(1, &drops));
    let hash = map.hash(&9_u64);
    let shard = map.header().shard(hash);

    // Act.

    // A racing creator that missed the lock-free lookup arrives second.
    let joined = map.link(
        shard,
        Box::new(Entry::new(hash, shard, 9, tracked(2, &drops))),
    );

    // Assert.
    assert!(Ref::ptr_eq(&winner, &joined));
    assert_eq!(joined.id, 1);
    assert_eq!(
        drops.load(StdOrdering::SeqCst),
        1,
        "the loser's value is dropped"
    );
}

#[test]
fn refmap_link_destroys_the_candidate_when_eq_panics_under_the_lock() {
    // Arrange.
    // Equality panics only when the second operand asks for it, so
    // the existing entry is created normally and the locked lookup fails.
    struct Key(u64, bool);
    impl std::hash::Hash for Key {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.0.hash(state);
        }
    }
    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            assert!(!other.1, "key comparison failed");
            self.0 == other.0
        }
    }
    impl Eq for Key {}
    let drops = Arc::new(StdAtomicUsize::new(0));
    let map: RefMap<Key, Tracked, IdentityHash> =
        Builder::new().shards(1).hasher(IdentityHash).build();
    let existing = map.get_or_insert_with_owned(Key(6, false), |_| tracked(1, &drops));
    let hash = map.hash(&Key(6, false));
    let shard = map.header().shard(hash);
    let candidate = Box::new(Entry::new(hash, shard, Key(6, true), tracked(2, &drops)));

    // Act.
    let outcome = catch_unwind(AssertUnwindSafe(|| map.link(shard, candidate)));

    // Assert.
    assert!(outcome.is_err());
    assert_eq!(
        drops.load(StdOrdering::SeqCst),
        1,
        "the candidate's value is dropped"
    );
    assert_eq!(Ref::users(&existing), 1);
}

#[test]
fn refmap_link_releases_the_joined_entry_when_the_loser_destructor_panics() {
    // Arrange.
    struct Bomb(bool);
    impl Drop for Bomb {
        fn drop(&mut self) {
            if self.0 {
                panic!("candidate destructor failed");
            }
        }
    }
    let map = small_map();
    let winner = map.get_or_insert_with_owned(8, |_| Bomb(false));
    let hash = map.hash(&8_u64);
    let shard = map.header().shard(hash);

    // Act.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        drop(map.link(shard, Box::new(Entry::new(hash, shard, 8, Bomb(true)))));
    }));

    // Assert.

    // The reference taken by the get unwound with the panic instead of leaking.
    assert!(outcome.is_err());
    assert_eq!(Ref::users(&winner), 1);
}

#[test]
fn refmap_growth_migrates_incrementally_without_losing_entries() {
    // Arrange.
    let map = small_map::<u64>();
    let mut held = Vec::new();
    let mut saw_migration = false;

    // Act.
    for key in 0..200_u64 {
        held.push(map.get_or_insert_with_owned(key, |key| *key * 10));
        saw_migration |= map.stats().migrating > 0;

        // Assert.
        // Every earlier key stays visible after each insert.
        for probe in (0..=key).step_by(7) {
            assert_eq!(map.get(&probe).map(|found| *found), Some(probe * 10));
        }
    }

    // Assert.
    assert!(
        saw_migration,
        "single-slot steps leave migrations in flight"
    );
    assert_eq!(map.len(), 200);
    drop(held);
    assert_eq!(map.len(), 0);
}

#[test]
fn refmap_maintain_shrinks_and_frees_everything_when_idle() {
    // Arrange.
    let map = shrinking_map::<u64>();
    let held: Vec<_> = (0..512_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let peak = map.stats().capacity;
    drop(held);

    // Act.
    let stats = drain(&map);

    // Assert.
    assert!(peak >= 512);
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.migrating, 0);
    assert_eq!(stats.capacity, 4, "shrunk from {peak} slots to the minimum");
    assert_eq!(
        stats.retired, 0,
        "idle maintenance frees all retired memory"
    );
}

#[test]
fn refmap_maintain_reclaims_around_a_permanently_live_sentinel() {
    // Arrange.
    let map = shrinking_map::<u64>();
    let _sentinel = map.get_or_insert_with_owned(u64::MAX, |_| 0);

    for round in 0..5_u64 {
        // Act.
        let burst: Vec<_> = (0..1_000_u64)
            .map(|key| map.get_or_insert_with_owned(round * 10_000 + key, |_| key))
            .collect();
        drop(burst);
        let stats = drain(&map);

        // Assert.
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.retired, 0);
        assert!(
            stats.capacity <= 8,
            "capacity {} after round {round}",
            stats.capacity
        );
    }
}

#[test]
fn refmap_builder_clamps_migration_step_to_keep_capacity_bounded() {
    // Arrange.
    let builder = Builder::new().migration_step(1);

    // Act.
    let step = builder.migration_step;

    // Assert.
    assert_eq!(step, MIN_MIGRATION_STEP);
}

#[test]
fn refmap_churn_with_default_step_keeps_capacity_proportional_to_live_entries() {
    // Arrange.
    // Two live keys while thousands of distinct keys come and go, so
    // tombstones keep forcing purge migrations.
    let map = shrinking_map::<u64>();
    let _anchors: Vec<_> = (0..2_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let mut peak = 0;

    // Act.
    for key in 100..20_000_u64 {
        drop(map.get_or_insert_with_owned(key, |_| key));
        peak = peak.max(map.stats().capacity);
    }

    // Assert.

    // Purges never ratchet the table beyond a small multiple of the
    // live entries, even without maintenance.
    assert!(peak <= 64, "peak capacity {peak} with two live entries");
}

#[test]
fn refmap_maintain_with_single_slot_steps_never_shrinks() {
    // Arrange.
    let map = small_map::<u64>();
    let held: Vec<_> = (0..64_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    drop(held);

    // Act.
    let stats = drain(&map);

    // Assert.
    assert_eq!(stats.entries, 0);
    assert_eq!(stats.retired, 0);
    assert!(
        stats.capacity > 4,
        "the headroom rule keeps step-1 tables at their peak"
    );
}

#[test]
fn refmap_maintain_defers_reclamation_while_a_reader_is_pinned() {
    // Arrange.
    let map = small_map::<u64>();
    let held: Vec<_> = (0..16_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let reader = epoch::pin().expect("thread-local available");

    // Act.
    drop(held);
    for _ in 0..8 {
        map.maintain();
    }
    let while_pinned = map.stats().retired;
    drop(reader);
    let after = drain(&map);

    // Assert.
    assert!(
        while_pinned >= 16,
        "retired {while_pinned} while a reader was pinned"
    );
    assert_eq!(after.retired, 0);
}

/// Advances the process-wide epoch `times` times without reclaiming retired
/// memory from any shard.
fn advance_epoch(times: usize) -> TestResult {
    let mut advanced = 0;
    for _ in 0..100_000 {
        if advanced == times {
            return Ok(());
        }
        if epoch::try_advance() {
            advanced += 1;
        }
        std::thread::yield_now();
    }
    Err(format!("the epoch advanced {advanced} of {times} times").into())
}

#[test]
fn refmap_acquire_frees_a_bounded_share_of_an_expired_backlog() -> TestResult {
    // Arrange.
    // A pinned reader holds back reclamation while a thousand entries
    // retire, so the backlog waits until the two epoch advances that follow
    // the reader's unpin.
    let map = shrinking_map::<u64>();
    let reader = epoch::pin().ok_or("thread-local available")?;
    for key in 0..1_000_u64 {
        drop(map.get_or_insert_with_owned(key, |_| key));
    }
    drop(reader);
    advance_epoch(2)?;
    let before = map.stats().retired;

    // Act.
    let _held = map.get_or_insert_with_owned(5_000, |_| 0);
    let after = map.stats().retired;

    // Assert.
    // One operation frees at most 32 entries and one table and leaves the
    // rest for later operations and `maintain`.
    assert!(before >= 1_000, "retired {before} before the acquire");
    assert!(
        before - after <= 33,
        "one acquire freed {} objects",
        before - after
    );
    assert_eq!(drain(&map).retired, 0);
    Ok(())
}

/// Value that fails the test if it is dropped twice.
struct Probe {
    key: u64,
    alive: StdAtomicBool,
    dropped: Arc<StdAtomicUsize>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        assert!(
            self.alive.swap(false, StdOrdering::SeqCst),
            "value dropped twice"
        );
        self.dropped.fetch_add(1, StdOrdering::SeqCst);
    }
}

#[test]
fn refmap_maintain_races_repinning_holders_on_tiny_tables() -> TestResult {
    // Arrange.
    // A pin that does not release the accesses made under earlier pins shows
    // up as a data race with a free under Miri, whose seeds vary the schedule.
    // Four workers create entries for six keys on two-slot tables, look them
    // up, and get and put references to them while a maintainer advances the
    // epoch and frees without pause, so workers pin again and again around
    // frees of entries they probed or failed to get.
    const THREADS: usize = 4;
    const KEYS: usize = 6;
    let iterations = if cfg!(miri) { 25 } else { 20_000 };
    let map = small_map::<Probe>();
    let created = StdAtomicUsize::new(0);
    let dropped = Arc::new(StdAtomicUsize::new(0));
    let held: Vec<StdAtomicUsize> = (0..THREADS * KEYS)
        .map(|_| StdAtomicUsize::new(0))
        .collect();
    let stop = StdAtomicBool::new(false);

    // Act.
    let outcome = std::thread::scope(|scope| -> Result<(), String> {
        let maintainer = scope.spawn(|| {
            while !stop.load(StdOrdering::Relaxed) {
                map.maintain();
                std::thread::yield_now();
            }
        });
        let workers: Vec<_> = (0..THREADS)
            .map(|me| {
                let (map, created, dropped, held) = (&map, &created, &dropped, &held);
                scope.spawn(move || -> Result<(), String> {
                    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ (me as u64 + 1);
                    for _ in 0..iterations {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        let key = state % KEYS as u64;
                        let handle = if state & 0x100 == 0 {
                            Some(map.get_or_insert_with_owned(key, |&key| {
                                created.fetch_add(1, StdOrdering::SeqCst);
                                Probe {
                                    key,
                                    alive: StdAtomicBool::new(true),
                                    dropped: Arc::clone(dropped),
                                }
                            }))
                        } else {
                            map.get(&key)
                        };
                        let Some(handle) = handle else { continue };
                        if !handle.alive.load(StdOrdering::SeqCst) || handle.key != key {
                            return Err(format!("key {key} returned a dead or foreign entry"));
                        }
                        // Each worker publishes the entry it holds before reading
                        // the others', so of two overlapping holders of different
                        // entries at least one sees the other.
                        let mine = std::ptr::from_ref::<Probe>(&handle) as usize;
                        let slot = &held[me * KEYS + key as usize];
                        slot.store(mine, StdOrdering::SeqCst);
                        for other in 0..THREADS {
                            let theirs =
                                held[other * KEYS + key as usize].load(StdOrdering::SeqCst);
                            if theirs != 0 && theirs != mine {
                                return Err(format!("key {key} has two live entries"));
                            }
                        }
                        if !map
                            .get(&key)
                            .is_some_and(|again| Ref::ptr_eq(&again, &handle))
                        {
                            return Err(format!("a lookup missed the held entry for key {key}"));
                        }
                        if state & 0x200 != 0 {
                            std::thread::yield_now();
                        }
                        slot.store(0, StdOrdering::SeqCst);
                        drop(handle);
                    }
                    Ok(())
                })
            })
            .collect();
        let mut outcome = Ok(());
        for worker in workers {
            let result = worker
                .join()
                .unwrap_or_else(|_| Err("worker thread panicked".to_owned()));
            outcome = outcome.and(result);
        }
        stop.store(true, StdOrdering::Relaxed);
        maintainer
            .join()
            .map_err(|_| "maintainer thread panicked".to_owned())?;
        outcome
    });
    outcome?;

    // Assert.
    assert_eq!(map.len(), 0);
    assert_eq!(
        created.load(StdOrdering::SeqCst),
        dropped.load(StdOrdering::SeqCst)
    );
    Ok(())
}

/// Builds a one-shard map with sixteen-slot minimum tables and steps that
/// copy a whole table, so each migration finishes within the next operation.
fn sizing_map<V>() -> RefMap<u64, V, IdentityHash> {
    Builder::new()
        .shards(1)
        .min_capacity(16)
        .migration_step(64)
        .hasher(IdentityHash)
        .build()
}

#[test]
fn refmap_acquire_starts_growth_only_past_three_quarters_full() {
    // Arrange.
    // Twelve of sixteen slots is exactly three quarters.
    let map = sizing_map::<u64>();
    let mut held: Vec<_> = (0..12_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let at_limit = map.stats();

    // Act.
    held.push(map.get_or_insert_with_owned(12, |_| 12));
    let past_limit = map.stats();

    // Assert.
    assert_eq!((at_limit.capacity, at_limit.migrating), (16, 0));
    assert_eq!(past_limit.migrating, 1, "{past_limit:?}");
}

#[test]
fn refmap_maintain_starts_a_shrink_only_below_one_eighth_full() {
    // Arrange.
    let map = sizing_map::<u64>();
    let mut held: Vec<_> = (0..40_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let capacity = drain(&map).capacity;
    held.truncate(capacity / 8);
    let at_threshold = drain(&map);

    // Act.
    held.pop();
    let report = map.maintain();

    // Assert.
    assert_eq!(capacity, 64);
    assert_eq!(
        at_threshold.capacity, capacity,
        "one eighth live keeps the table"
    );
    assert_eq!(
        report.migrations_started, 1,
        "fewer live entries start a shrink"
    );
}

#[test]
fn refmap_maintain_skips_a_shard_another_holder_has_locked() {
    // Arrange.
    // The shard lock is held while a migration is in flight, as by a
    // foreground operation on another thread.
    let map = small_map::<u64>();
    let _held: Vec<_> = (0..9_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let shard = &map.header().shards[0];
    let locked = shard.lock();

    // Act.
    let report = map.maintain();
    drop(locked);

    // Assert.
    assert_eq!(
        report.migration_steps, 0,
        "maintain must not wait for the lock"
    );
    assert_eq!(report.shards_pending, 1);
}

#[test]
fn refmap_drop_map_keeps_handles_valid_until_released() {
    // Arrange.
    let drops = Arc::new(StdAtomicUsize::new(0));
    let map = small_map();
    let held: Vec<_> = (0..50_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| tracked(key as usize, &drops)))
        .collect();
    let released: Vec<_> = (100..150_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| tracked(key as usize, &drops)))
        .collect();
    drop(released);

    // Act.
    drop(map);
    let clones: Vec<_> = held.iter().map(Ref::clone).collect();
    drop(held);
    let before_last = drops.load(StdOrdering::SeqCst);
    let ids: Vec<_> = clones.iter().map(|handle| handle.id).collect();
    drop(clones);

    // Assert.
    assert_eq!(before_last, 50);
    assert_eq!(ids, (0..50).collect::<Vec<_>>());
    assert_eq!(drops.load(StdOrdering::SeqCst), 100);
}

#[test]
fn refmap_drop_map_mid_migration_frees_both_tables() {
    // Arrange.
    let map = small_map::<u64>();
    let held: Vec<_> = (0..9_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let migrating = map.stats().migrating;

    // Act.

    // The last handle frees the shards, including the destination table.
    drop(map);
    drop(held);

    // Assert.
    assert_eq!(
        migrating, 1,
        "the map is dropped while a migration is in flight"
    );
}

#[test]
fn refmap_release_with_panicking_value_destructor_still_unlinks() {
    // Arrange.
    struct Bomb(bool);
    impl Drop for Bomb {
        fn drop(&mut self) {
            if self.0 {
                panic!("value destructor failed");
            }
        }
    }
    let map = small_map();
    let handle = map.get_or_insert_with_owned(3, |_| Bomb(true));

    // Act.
    let outcome = catch_unwind(AssertUnwindSafe(|| drop(handle)));
    let replacement = map.get_or_insert_with_owned(3, |_| Bomb(false));

    // Assert.
    assert!(outcome.is_err());
    assert!(!replacement.0);
    assert_eq!(map.len(), 1);
}

#[test]
fn refmap_acquire_with_panicking_init_leaves_no_trace() {
    // Arrange.
    let map = small_map::<u64>();

    // Act.
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        map.get_or_insert_with_owned(4, |_| panic!("init failed"));
    }));
    let retry = map.get_or_insert_with_owned(4, |_| 40);

    // Assert.
    assert!(outcome.is_err());
    assert_eq!(*retry, 40);
    assert_eq!(map.len(), 1);
}

#[test]
fn refmap_get_skips_tombstones_inside_collision_runs() {
    // Arrange.
    // Keys sharing their low bits collide in every table size here.
    let map = small_map::<u64>();
    let keys: Vec<u64> = (0..40).map(|i| i << 20).collect();
    let mut held: Vec<_> = keys
        .iter()
        .map(|&key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let removed: Vec<u64> = keys.iter().copied().step_by(2).collect();

    // Act.
    held.retain(|handle| !removed.contains(handle.key()));

    // Assert.
    for key in &removed {
        assert!(map.get(key).is_none());
    }
    for handle in &held {
        assert!(
            map.get(handle.key())
                .is_some_and(|found| Ref::ptr_eq(handle, &found))
        );
    }
    for key in &removed {
        assert_eq!(*map.get_or_insert_with_owned(*key, |key| key + 1), key + 1);
    }
}

#[test]
fn refmap_stats_track_entries_capacity_and_retirement() {
    // Arrange.
    let map = shrinking_map::<u64>();
    let empty = map.stats();

    // Act.
    let held: Vec<_> = (0..3_u64)
        .map(|key| map.get_or_insert_with_owned(key, |_| key))
        .collect();
    let loaded = map.stats();
    // A pinned reader keeps the process-wide epoch, which other tests advance
    // concurrently, from moving the two steps that would free the entries.
    let reader = epoch::pin().expect("thread-local available");
    drop(held);
    let released = map.stats();
    drop(reader);

    // Assert.
    assert_eq!(
        empty,
        Stats {
            shards: 1,
            entries: 0,
            capacity: 4,
            migrating: 0,
            retired: 0
        }
    );
    assert_eq!(loaded.entries, 3);
    assert_eq!(released.entries, 0);
    assert_eq!(released.retired, 3, "released entries wait for an epoch");
}

#[test]
fn refmap_acquire_concurrently_yields_one_live_entry_per_key() -> TestResult {
    // Arrange.
    let map = Arc::new(RefMap::<u64, StdAtomicUsize>::new());
    let (threads, keys) = if cfg!(miri) {
        (3, 16_u64)
    } else {
        (8, 1_000_u64)
    };
    let barrier = Arc::new(Barrier::new(threads));

    // Act.

    // Every thread holds every key while all threads mark their handles.
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let map = Arc::clone(&map);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let handles: Vec<_> = (0..keys)
                    .map(|key| map.get_or_insert_with_owned(key, |_| StdAtomicUsize::new(0)))
                    .collect();
                for handle in &handles {
                    handle.fetch_add(1, StdOrdering::Relaxed);
                }
                barrier.wait();
                handles
                    .iter()
                    .map(|handle| handle.load(StdOrdering::Relaxed))
                    .collect::<Vec<_>>()
            })
        })
        .collect();

    // Assert.

    // All threads got a reference to the same entry for each key.
    for worker in workers {
        assert!(join(worker)?.iter().all(|&count| count == threads));
    }
    assert_eq!(map.len(), 0);
    Ok(())
}
