<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# `ovcc-refmap`

`ovcc-refmap` is a concurrent map that lets many threads share one entry per
key, keeping the entry alive exactly as long as someone holds it and removing
it when the last holder lets go.

Unlike dashmap, papaya, and scc, it keeps latency low when many requests
contend on one key while still keeping up when keys come and go quickly.

Its memory follows the keys in use rather than every key it has ever seen, and
freed memory is returned gradually without pausing lookups.

Its concurrency protocol and memory ordering, including the weaker ordering
ARM64 allows, are model-checked with TLA+, and it has extensive testing with
Loom, Miri, and multi-threaded stress tests.

```rust
use ovcc_refmap::{Ref, RefMap};

let fills: RefMap<String, u32> = RefMap::new();
let leader = fills.get_or_insert_with("bucket/object", || ("bucket/object".to_owned(), 7));
let follower = fills.get("bucket/object").expect("the leader keeps it live");
assert!(Ref::ptr_eq(&leader, &follower));
drop((leader, follower));
assert!(fills.get("bucket/object").is_none());
fills.maintain();
```

`RefMap::get_or_insert_with` returns a `Ref` to the unique live entry for a
key, creating it when none is live, and `RefMap::get` gets a reference only to
an existing one. `RefMap::maintain` is meant to run periodically, advancing
unfinished migrations, shrinking tables that emptied after a burst, and freeing
retired memory while traffic is idle.

## Motivation

Per-key coordination, such as letting concurrent requests for one object share
a single fill, needs a map from each key to its shared state. A map that
inserts on first use and never removes grows with every key ever requested. A
map behind one process-wide mutex serializes every caller, and when a
`std::collections::HashMap` outgrows its capacity it rehashes every entry
inside that critical section, so one insert stalls every caller for time
proportional to the map's size, because hashbrown's `reserve_rehash` moves all
elements.

This map therefore has to scale past 100 cores, grow and shrink without a
whole-table rehash under any lock, keep memory proportional to current
activity even when it never empties, state its allocation cost per operation,
and cost no more CPU or latency than a mutex-protected map.

## System model

A `RefMap` is a power-of-two array of shards selected by the high bits of a
64-bit key hash. Each shard has a root table that lookups read without
locking, and a mutex that serializes structural changes. A table is an
open-addressing array of slots holding EMPTY, TOMBSTONE, or a pointer to an
entry; its `next` pointer names the destination of the migration that started
from it. An entry holds a reference word, the hash, the key, the value, and a
pointer to its shard. Retired entries and tables wait in three per-shard lists
indexed by the epoch modulo three until a process-wide epoch has advanced
twice past their retirement.

| Actor | Takes the shard lock | Pinned | Work bound |
|---|---|---|---|
| `get`, `get_or_insert_with` fast path | Never, except for a thread's first lookup, which registers it under a process-wide mutex, and on a thread whose thread-locals are being destroyed | During one probe sequence per table | Probe length |
| `get_or_insert_with` slow path | Once, or twice when a table must be allocated | No | One migration step plus one probe per current table |
| Final `Ref` drop | Once | No | One migration step plus one probe per current table |
| `maintain` | Once per step | No | `maintain_steps` steps per shard, each its own critical section |

The safety invariants are that each key has at most one live entry, a dead
entry is never resurrected, exactly one finalizer per entry drops the value
once and retires the entry once, unlinking goes by pointer identity, every live
entry is linked in a current table, no thread dereferences a freed entry or
table, a lookup that returns "absent" overlaps an instant with no live entry
for its key, a migration destination stays at most half full, and each shard
runs at most one migration at a time. For liveness, lookups are wait-free in
the absence of collisions and never block except during a thread's first lookup
or its teardown, finalizers do bounded work, and repeated `maintain` calls on
an idle map free all retired memory and shrink every shard to its minimum
capacity.

## Alternatives

| Option | Mechanism | Breaking condition | Verdict |
|---|---|---|---|
| Global mutex with removal at the final put | A mutex-protected map plus a removal path | Every acquire and put serializes on one lock, and a resize rehashes all entries under it | Unsound for scaling and freeze-free resizing |
| Sharded locks over `std` maps, as in `dashmap` | Hash-selected shard locks | A shard rehash still stalls that shard for time proportional to its size, and a hot key's gets convoy on its shard lock | Sound only with incremental migration and lock-free reads, which is the chosen design |
| `scc` or `papaya` | Concurrent map with incremental resizing | Retired memory waits in collector state owned by the retiring thread, `papaya` never shrinks below its largest table, and neither exposes a bounded idle drain, according to their documentation; their internals cannot be modeled at the granularity this protocol needs | Unverified for idle reclamation, and a new dependency |
| `crossbeam-epoch` with a custom table | Thread-local bags of deferred destructors | A thread's bag holds up to 64 destructors until that thread pins again, and sealing a bag allocates, as `crossbeam-epoch` 0.9.20 `src/internal.rs` shows | Leaves garbage behind idle threads |
| Chosen | Own tables, per-shard writers with incremental migration, lock-free readers, and a process-wide epoch with per-shard retirement lists | See [Limits](#limits) | Sound for the modeled bounds, with the evidence below |

## Design

### Lookups and gets

A lookup pins the epoch, loads the shard's root table, and probes from the
key's home slot to the first EMPTY slot, skipping tombstones and entries whose
get fails. A get is one `fetch_add` on the reference word. On a miss it
follows the table's `next` pointer, because entries inserted during a
migration live only in the destination. A slot never returns to EMPTY within a
table, and an entry pointer is only replaced by TOMBSTONE, so a probe passes
every entry that stays linked while it runs.

### Reference word

The word stores `references - 1` in 64 bits, with zones for overflow, a wide
dead zone, and `DEAD`, after the Linux kernel's `rcuref` encoding. A get is
one `fetch_add`, so a herd of readers on one hot key never retries. A put is
a compare-and-swap that moves the word one reference down, or from the last
reference straight to `DEAD`, and the thread whose exchange stores `DEAD`
finalizes. The word never holds zero references, so a get cannot resurrect an
entry during its final put, and a put never touches the entry after
its exchange succeeds.

`rcuref` instead puts with one `fetch_sub` to a zero-reference value and
finalizes through a second exchange to dead, which touches the entry after its
caller gave up its reference. The TLA+ protocol model found the interleaving
that frees the entry in between, in which a get resurrects the entry, a put
finalizes it, and maintenance retires it and advances the epoch twice. Linux
documents the same race and forbids a grace period between the decrement and
the exchange, disabling preemption around `rcuref_put` or requiring an RCU
read-side section for `rcuref_put_rcusafe`, in `lib/rcuref.c` and
`include/linux/rcuref.h` as of 2026-10-02. Pinning the epoch across the put
would be the equivalent here, but dropping the map and every finalization on a
shard of the dropped map free entries without a grace period, so a put instead
retries when it races another get or put on the same entry, and a get stays a
single instruction.

### Creation

`get_or_insert_with` runs `init` outside every lock after a lookup misses,
allocates an unpublished entry outside the lock, then locks the shard, performs
one migration step, and searches the current tables again. The caller gets a
reference to a live entry found there, and the unpublished entry is destroyed
by its builder after the lock is released. Otherwise the unpublished entry is
inserted into the destination when a migration is in progress, or into the
source. When the source would exceed three quarters full, the caller releases
the lock, allocates the destination, and retries, so no table is ever allocated
under a lock.

### Finalization

The finalizer drops the value on its own thread outside every lock, then locks
the shard, performs one migration step, tombstones the slots holding exactly
its entry in both current tables, and retires the entry. A guard performs the
unlink even when the value's destructor panics. Writers that replace a dead
entry never retire it.

### Incremental migration

Every structural critical section first migrates `migration_step` source
slots, copying pointers of live entries into the destination and tombstoning
dead ones. When the cursor reaches the end, the root swings to the destination
and the source is retired. The destination capacity is
`next_pow2(2 * (linked + ceil(source / step) + 1))`. At most
`ceil(source / step)` inserts can arrive before the migration ends, because
each first performs a step, so the destination stays at most half full and no
second migration can be requested before the first finishes. `target_capacity`
computes it, a debug assertion in `ShardState::insert` checks it, and the
protocol model's `MigrationHeadroom` invariant covers it.

With `step >= 8`, a migration started at capacity `C` with `L` linked entries
produces at most `next_pow2(2L + C/4 + 2)` slots, so a table grows only while
`C <= 8(L + 1)`, and after any migration the capacity is at most `8(L + 1)`.
Smaller steps let the insert reserve exceed the source, which would ratchet
capacity upward under tombstone churn, so `Builder::migration_step` clamps to
eight. `maintain` starts shrink migrations once a shard is less than one
eighth full.

### Reclamation

| Point | Ordering | Purpose |
|---|---|---|
| Pin | `Release` store of the observed epoch, then `fence(SeqCst)` | Announces the reader before it loads any table pointer, and publishes its accesses under earlier pins to an advancer that reads this store instead of the preceding unpin |
| Unpin | `Release` store | Publishes the reader's accesses to the advancer |
| Retire | `fence(SeqCst)`, then `Acquire` load of the epoch | Tags an object after it became unreachable, and orders the free of any list three epochs old that the retirement hands over, which a panicking `Eq` in `link` frees before the reclamation's own load |
| Advance | Load, `fence(SeqCst)`, scan participants, `fence(Acquire)`, `Release` store | Moves on only when every pinned thread observed the current epoch |
| Reclaim | `Acquire` load | Frees lists tagged at least two epochs back |

These match `crossbeam-epoch` 0.9.20 except at the pin. Its x86 path pins with
a `SeqCst` compare-exchange, and its other targets, aarch64 included, store
`Relaxed` and fence, as its `src/internal.rs` shows. Under the C++20 rules
Rust follows, only read-modify-writes extend a release sequence, so an
advancer that reads a `Relaxed` re-pin instead of the `Release` unpin before
it synchronizes with nothing, and the reader's accesses under its earlier pin
race with the free. Miri found that race with a `Relaxed` pin store, and the
`Release` store closes it at the expected cost of an `stlr` on aarch64 and
nothing on x86.

Retired objects live in per-shard lists under the shard lock, so retirement
never allocates and `maintain` frees them while the threads that retired them
stay idle. Finalizers try to advance the epoch each time their shard's backlog
reaches a multiple of 64 retired objects, at most once per 100 microseconds
across the process, and `maintain` always tries twice. Advancing takes the
participant registry with `try_lock` and never blocks.

### Shutdown

Dropping the `RefMap` closes every shard, drops the hasher on the owner's
thread, frees retired memory immediately because no reader can exist without
a `&RefMap`, and leaves one reference on the shared state per shard with
orphaned entries. The hasher cannot wait for the last handle, because a
`Ref` carries neither its type nor its lifetime, so finalizers locate their
slots by the cached hash instead. The finalizer that empties a shard of the
dropped map puts the shard's reference after unlocking, and the final put frees
the shared state. That relies on `parking_lot`'s unlock never touching the lock
after a waiter can acquire it, which holds in `parking_lot` 0.12 because
`RawMutex::unlock_slow` clears the locked bit inside the queue callback.

A finalizer therefore splits its garbage into the share it frees and the share
it stashes while it still holds the shard lock, and touches nothing of the
shard after unlocking, because once its entry stops counting as outstanding
the owner may close the shard and free the map unless the finalizer holds the
shared state's last reference. A version that took the lock again after
unlocking to stash the overflow let the protocol model reach a use after free
when the owner dropped the map in between, and Miri reports the same race in
`refmap_drop_map_races_final_releases_on_other_threads` at seed 246 of 256 on
that version and none on the fix.

### Allocation per operation

| Operation | Allocations |
|---|---|
| Lookup hit, `get`, `Ref::clone`, non-final drop | 0 |
| Creating an entry | 1 for the entry, plus whatever `init` allocates for the key and value |
| Losing a creation race | The unpublished entry's allocations, freed by its builder |
| Final drop | 0, because the entry is freed later in a batch |
| Migration | 1 table, outside the lock, amortized over `source / step` operations |
| First use on a thread | 1 participant record, reused after the thread exits |

## Correctness evidence

The repository's CI runs the unit, race, stress, API soundness, and lifecycle
conformance tests, regenerates the lifecycle fixtures with TLC, checks every
memory-ordering configuration, and checks a reduced protocol configuration,
together with each model's negative controls. The full protocol
configurations, Loom, and Miri run locally. The models and their results are
documented in
[`skills/ovcc-refmap-protocol/SKILL.md`](skills/ovcc-refmap-protocol/SKILL.md).

| Check | What it establishes | Result |
|---|---|---|
| TLA+ protocol model, interleaving semantics | Uniqueness, no resurrection, single finalizer, identity unlink, live entries linked, no use after free, reader completeness, migration headroom, and clean quiescence, with the owner dropping the map while handles remain | Found three use after frees, all fixed: the split put described under [Reference word](#reference-word), a pinned variant of it that the map's immediate frees still broke, and a finalizer that touched its shard after unlocking, described under [Shutdown](#shutdown). The fixed model passes 48,272,879 distinct states at depth 81 with the owner drop, 48,052,657 without it, and 42,655,589 with eight-slot steps, and all 23 reachability witnesses are reached. These full-bounds runs predate the `relock_after_unlock` variant, which leaves the correct model unchanged, as the CI configuration's identical state count before and after it shows. Each of the 11 bug variants, the three use after frees among them, fails with its named property, and the CI configuration, one getter instead of two, passes 1,633,866 states in one to two minutes on 4 CPUs and reaches 21 of the witnesses. One shard, two epochs, sequential consistency |
| TLA+ memory-ordering model | Every atomic with the ordering the source gives it, under C++20 release sequences and either Loom's or coherence-only `SeqCst` fences, and happens-before for every non-atomic access, including frees | Publication, handoff, reclamation, migration, shutdown, re-pin, and table recycling pass. Of 23 mutants, 20 fail with a race or use after free, and the other three, a final put that only acquires and `Relaxed` migration copies and tombstones, pass and are kept stronger in the code. It confirmed the re-pin race and found that the retirement's epoch load must acquire, because a panicking `Eq` in `link` can free a list that retirement handed over before the reclamation's own load runs, and both are fixed |
| TLA+ lifecycle model and conformance | The sizing, migration, and reclamation policy observed through the public API | 569 states and 2,715 raw edges, every edge replayed against the code with a seeded continuation, and 11 of 16 injected policy mutants killed. The replay compares only what the public API exposes, so a 7/8 load limit or a 4 or 16 times shrink trigger passes it, and two boundary unit tests pin both constants and fail for 7/8, 2/3, 4, and 16 |
| Loom | Interleavings of the reference word, epoch protection, and the map paths at shared-memory granularity | All 8 models pass. The reference-count model explores all 454,136 schedules of a get racing two puts, the epoch model 384, racing creators 133, a lookup racing the final put 36, replacement racing the old finalizer 22, and map drop racing a final put 26. The maintenance model, which took more than 15 minutes unbounded, explores 14,775 schedules with up to three preemptions. Loom 0.7.2 tracks only the last access per atomic, as its `rt/atomic.rs` and `rt/execution.rs` show, so a writer's own load can hide a reader's racing load, and the map-level models explore far fewer schedules than exist; the migration model explores one |
| Miri | Undefined behavior, data races, leaks, and aliasing on the real code | Unit, API soundness, and stress tests pass under strict provenance, and the race tests pass over 256 seeds with Stacked Borrows and 64 with Tree Borrows. The lifecycle conformance tests are skipped, because they parse multi-megabyte fixtures and replay over 100,000 steps. `refmap_maintain_races_repinning_holders_on_tiny_tables` passes 32 seeds and, with a `Relaxed` pin store, reports a probe's read of an entry hash racing the entry's free at seed 8. `refmap_drop_map_races_final_releases_on_other_threads` reports the finalizer's shard access racing the map's free at seed 246 on the version that took the shard lock again after unlocking. Miri also found a provenance bug, since fixed, where a lookup returned a pointer narrowed to a shared reference that later reached `Box::from_raw`. Miri builds use a spin lock, because `parking_lot`'s futex parker trips Miri's variadic `syscall` check |
| Race and stress tests | Seeded multi-threaded churn, hot-key herds, migration storms, and dropping the map under live handles, against a uniqueness oracle | Pass natively and under Miri. A ten-second run performs hundreds of millions of operations, and the oracle or the map's own assertions caught each of 12 injected map bugs |
| API soundness witnesses | Hasher ownership after the map drops, panics in key and value destructors, overflowing configuration, and teardown lookups | All 7 fail on the code before review and pass after, and Miri reports the hasher use after free on the old code |

These commands run from the workspace root.

```bash
cargo test -p ovcc-refmap
# Exhaustive interleavings of the synchronization core.
RUSTFLAGS="--cfg loom" CARGO_TARGET_DIR=target/loom cargo test -p ovcc-refmap --lib --release loom_
# Undefined behavior, data races, and leaks, with the nightly toolchain's Miri.
MIRIFLAGS="-Zmiri-strict-provenance -Zmiri-many-seeds=0..256" cargo +nightly miri test -p ovcc-refmap --test races
# Longer stress runs; the seed printed on failure replays a run.
REFMAP_STRESS_MILLIS=10000 cargo test -p ovcc-refmap --release --test stress
```

## Performance

### Memory

`examples/memory.rs` counts live requested bytes with a counting global
allocator and reports RSS separately. RSS is not expected to fall, because the
allocator is never asked to trim. Each value is a per-key coordination payload
of 40 bytes inline and heap blocks of 360, 56, and 40 bytes, sized like a fill
lock, a start timestamp, and a watch channel.

| Scenario | `RefMap` | Never-removing mutex map |
|---|---|---|
| 10 million unique 64-byte keys, 4,096 live handles plus a permanently held sentinel | 2.80 MB live throughout, and 158,552 B after the window is released and 4 idle `maintain` calls, equal to the empty map with its sentinel | 6.31 GB live, about 630 B per key ever seen |
| Burst of 1 million held keys, then idle | 650 MB while held, and back to 158,552 B after 7 `maintain` calls taking 6.7 ms | Not run, because it never shrinks |
| Five bursts of 200,000 keys | Every round drains to the same 158,552 B | Not run |

`memory compare` drives the same keys, payload, and window through `RefMap`
and, through the adapters in `examples/maps`, through a
`parking_lot::Mutex<HashMap>` that removes at the final put, `papaya` 0.2.5,
`dashmap` 6.2.1, and `scc` 3.8.8. It then holds and releases a burst of 1
million keys and gives each map what an idle owner can call.

| Map | 10 million unique keys, live bytes while streaming | Burst released | After idle | Idle call |
|---|--:|--:|--:|---|
| `RefMap` | 2.80 MB | 18.4 MB | 158,628 B | `maintain()` until settled, 7 calls |
| Mutex map | 2.93 MB | 69.2 MB | 753 B | `shrink_to_fit()`, which rehashes every live entry under the one lock |
| `papaya` | 2.74 to 2.91 MB | 19.0 MB | 19.0 MB | None, because it never shrinks |
| `dashmap` | 2.97 MB | 69.2 MB | 33,521 B | `shrink_to_fit()`, which rehashes each shard under its write lock |
| `scc` | 2.68 MB | 102,856 B | 102,887 B | None needed, because it shrinks as entries leave |

Every map that removes at the final put stays flat while streaming, so
removal alone stops the growth. They differ once a burst ends. `papaya` keeps
its largest table, the mutex map and `dashmap` give memory back only through a
rehash that blocks their lock or shard, `scc` shrinks on its own in the
foreground, and `RefMap` shrinks through bounded `maintain()` steps that never
block a lookup. `RefMap`'s idle floor of about 155 KB comes from its 256
default shards at minimum capacity, four per hardware thread on the 64-thread
measuring host, plus the sentinel.

### Throughput and latency

Criterion medians on one pinned core, with the arguments the repository's
benchmark suite uses, compare `RefMap` against a never-removing mutex map and
a single-mutex map that removes at the final put.

| Operation | `RefMap` | Never-removing map | Single mutex |
|---|--:|--:|--:|
| Acquire a reference to a new key and put it | 173.6 ns | 187.4 ns | 171.1 ns |
| Get a reference to an existing entry | 38.6 ns | 52.4 ns | 83.2 ns |
| Lookup hit | 36.0 ns | 36.5 ns | 83.8 ns |
| Lookup miss | 32.8 ns | 29.7 ns | 39.8 ns |
| Clone and drop a handle | 4.6 ns | 3.4 ns | 3.4 ns |
| Recreate one key repeatedly | 117.1 ns | 46.9 ns | 175.2 ns |

`examples/scaling.rs` drives the same per-key lifecycle through `RefMap`, the
never-removing map, the single-mutex map, a map of 256 independently locked
shards, and `papaya`, `dashmap`, and `scc` through the remove-at-final-put
adapters in `examples/maps`. Each point runs in a fresh process for 3 s after
0.5 s of warmup, on a 64-thread workstation under WSL2 with a load average of
7.9 from unrelated processes at the start. The table gives millions of
operations per second at 56 threads, with p99 latency in microseconds.

| Workload | `RefMap` | Never-removing map | Single mutex | Sharded mutex | `papaya` | `dashmap` | `scc` |
|---|--:|--:|--:|--:|--:|--:|--:|
| Distinct keys, one entry created and destroyed per operation | 21.2 (23) | 0.20 (1,638) | 0.14 (1,638) | 45.3 (2.6) | 1.25 (2,621) | 50.8 (2.7) | 0.02 (8,913) |
| Uniform over 1 million keys | 20.4 (25) | 0.20 (1,704) | 0.14 (1,704) | 38.2 (2.9) | 1.18 (2,753) | 47.3 (2.9) | 0.02 (8,389) |
| Zipf-skewed | 10.5 (156) | 0.25 (1,376) | 0.15 (1,573) | 1.56 (688) | 1.24 (2,753) | 21.1 (43) | 0.01 (8,913) |
| One hot key | 11.5 (15) | 0.33 (983) | 0.19 (1,180) | 0.20 (1,180) | 16.5 (9) | 4.45 (344) | 0.01 (8,913) |
| Small active set per thread | 16.4 (32) | 0.12 (2,753) | 0.14 (1,573) | 33.5 (3.5) | 1.27 (2,621) | 38.8 (3.2) | 0.51 (4,719) |
| Growth burst | 2.28 | 0.21 | 0.15 | 2.27 | 1.27 | 2.09 | 2.53 |

Against the never-removing map, `RefMap` is 35 to 137 times faster on the
steady workloads and 11 times faster in the growth burst at 56 threads, its p99
falls from milliseconds to tens of microseconds, and the longest single
operation in a growth burst falls from 3.2 s, a rehash under the global lock,
to 282 ms. Among the alternatives, `dashmap` with a reference count on top is
about twice as fast on churn with ten times lower p99, but is 2.6 times slower
on a hot key, and it returns memory only through `shrink_to_fit`, which
rehashes a shard under its write lock. The sharded mutex map collapses on skew
and hot keys, `papaya` matches `RefMap` on a hot key but collapses under churn
and keeps its largest table, and `scc` collapses whenever threads contend over
a small live set. The causes named here follow from each design rather than
from profiling. `RefMap` spends about 2.4 times `dashmap`'s CPU per churn
operation, most likely because retired entries wait for an epoch and are freed
in batches, often on a thread other than the one that allocated them. Every map
stalls for tens of milliseconds in the 56-thread growth burst on this host, and
an earlier run with glibc's fast bins disabled removed the single-thread stalls
but not those, which points at the allocator and the operating system rather
than the maps.

1,024 shards with 64-slot minimum tables raise `RefMap`'s throughput at 56
threads to 27.1 million operations per second on distinct keys, 26.9 on
uniform keys, 12.0 on skewed keys, and 21.5 on small active sets, about 30
percent more on churn, at the cost of a larger idle floor. The defaults stay
at four shards per hardware thread and eight-slot minimum tables, which every
model and test here checks, and a caller can choose the knobs for its memory
budget.

```bash
cargo bench -p ovcc-refmap
cargo run --release -p ovcc-refmap --example scaling -- --verify
cargo run --release -p ovcc-refmap --example scaling -- --threads 1,16,32,48,56
cargo run --release -p ovcc-refmap --example memory -- all
cargo run --release -p ovcc-refmap --example memory -- compare
```

## Limits

- Validation above 64 hardware threads has not run, and the measuring
  workstation has 32 physical cores. The scaling harness is the reproduction
  path.
- Churn throughput trails `dashmap` with a reference count by about two times,
  the price of deferred reclamation. Freeing retired entries on the threads
  that allocated them would narrow it, but needs per-thread retirement that
  `maintain` can still drain while those threads sit idle.
- `Hash` and `Eq` on keys must not call back into the same map, because `Eq`
  runs under the shard lock when hashes match.
- A thread that stays pinned for a long time delays reclamation for every map
  in the process, although lookups pin only for one probe sequence.
- The map knows nothing of single-flight, timeouts, or sharing policy; callers
  that build those on it keep their own contracts.
