// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A concurrent map of reference-counted entries that exist only while a
//! handle to them is alive.
//!
//! [`RefMap::get_or_insert_with`] returns a [`Ref`] to the unique live entry for a key,
//! creating it when none is live, and [`RefMap::get`] gets a reference only to
//! an existing one. When the last `Ref` for an entry drops, its value is
//! destroyed on that thread and the entry leaves the map, so retained memory
//! follows live activity rather than the number of keys ever seen.
//!
//! Lookups probe tables published with epoch-based reclamation and get a
//! reference to an entry with a single atomic add, taking no lock. The
//! exceptions are a thread's first lookup, which registers the thread under a
//! process-wide mutex, and a thread whose thread-locals are being destroyed,
//! which uses the shard lock instead. Structural changes take a per-shard
//! mutex and do bounded work, migrating a growing or shrinking table a few
//! slots per operation, so no insert, final put, or maintenance call rehashes
//! a whole table. Tables are allocated and freed outside every lock while the
//! map is in use, and no counter or lock is shared by all keys on the hot path.
//!
//! [`RefMap::maintain`] is meant to run periodically. It advances unfinished
//! migrations, shrinks tables that emptied after a burst, and frees retired
//! memory while traffic is idle.
//!
//! The reference word follows the Linux kernel's `rcuref` encoding widened to
//! 64 bits, and the reclamation orderings follow `crossbeam-epoch` 0.9. The
//! crate's README explains why both are implemented here rather than taken as
//! dependencies.
//!
//! ```
//! use ovcc_refmap::{Ref, RefMap};
//!
//! let fills: RefMap<String, u32> = RefMap::new();
//! let leader = fills.get_or_insert_with("bucket/object", || ("bucket/object".to_owned(), 7));
//! let follower = fills.get("bucket/object").expect("the leader keeps it live");
//! assert!(Ref::ptr_eq(&leader, &follower));
//! drop((leader, follower));
//! assert!(fills.get("bucket/object").is_none());
//! fills.maintain();
//! ```
//!
//! The map is invariant in its key and value types because it inserts through
//! a shared reference, so the example below does not compile.
//!
//! ```compile_fail
//! use ovcc_refmap::RefMap;
//!
//! fn shorten<'a, 'm>(map: &'m RefMap<&'static str, ()>) -> &'m RefMap<&'a str, ()> {
//!     map
//! }
//! ```

mod epoch;
mod refcount;
mod shard;
mod sync;
mod table;

use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hash};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr::{self, NonNull};

use shard::{Config, Garbage, Header, Lookup, RECLAIM_THRESHOLD, Reserve, Shard};
use sync::{AtomicUsize, Mutex, Ordering};
use table::{Entry, Table};

const MAX_DEFAULT_SHARDS: usize = 1024;

/// Largest configurable shard count.
const MAX_SHARDS: usize = 1 << 16;

/// Largest configurable minimum table, in slots per shard.
const MAX_MIN_CAPACITY: usize = 1 << 30;

/// Smallest migration step that keeps capacity proportional to live entries.
/// A destination reserves one slot per step for inserts arriving before the
/// migration ends, and below eight slots per step that reserve outgrows the
/// source, so tombstone purges would ratchet capacity upward without bound.
const MIN_MIGRATION_STEP: usize = 8;

/// Map from keys to reference-counted entries. See the crate documentation.
///
/// `Eq` and `Borrow` on keys run under a shard lock when hashes match, so they
/// must not call into the same map or block on anything that might.
pub struct RefMap<K, V, S = RandomState> {
    inner: NonNull<Inner<K, V, S>>,
    _owns: PhantomData<(K, V, S)>,
    // Inserting through `&self` requires invariance in the key and value,
    // because a covariant map would accept shorter-lived keys or values and
    // later hand them out at the longer lifetime. The shard mutexes already
    // imply it; this marker keeps it independent of their layout.
    _invariant: PhantomData<*mut (K, V)>,
}

// `Header` stays the first field because `destroy` recovers the full
// allocation from a header pointer.
#[repr(C)]
struct Inner<K, V, S> {
    header: Header<K, V>,
    /// Dropped by the owner, because a `Ref` carries neither the hasher's
    /// type nor its lifetime and the last one may free the allocation.
    hasher: ManuallyDrop<S>,
}

// Entries are shared across threads, keys are read concurrently and dropped
// on any thread, and values are shared and dropped on any thread, all of which
// the `Send + Sync` bounds on keys and values permit. The hasher's bounds cover
// its use by every thread sharing the map and its drop by the map's owner.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Send + Sync> Send for RefMap<K, V, S> {}
// Sharing the map relies on the same bounds, and every shared mutation is
// synchronized internally.
unsafe impl<K: Send + Sync, V: Send + Sync, S: Send + Sync> Sync for RefMap<K, V, S> {}

/// Counters aggregated from per-shard mirrors without taking any lock. Values
/// read during concurrent updates are approximate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub shards: usize,
    /// Entries linked in the tables, counting live ones and those being
    /// finalized after their final put.
    pub entries: usize,
    /// Slots allocated across the tables in use, including the destinations
    /// of in-progress migrations.
    pub capacity: usize,
    /// Shards with a migration in progress.
    pub migrating: usize,
    /// Retired entries and tables still allocated, waiting for readers to move
    /// on or for a later operation to free them.
    pub retired: usize,
}

/// Work done by one [`RefMap::maintain`] call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Maintenance {
    /// Times the process-wide reclamation epoch advanced.
    pub epochs_advanced: usize,
    pub migration_steps: usize,
    pub migrations_started: usize,
    /// Retired entries and tables freed.
    pub freed: usize,
    /// Shards still migrating or holding retired memory after the call, which
    /// another call would advance.
    pub shards_pending: usize,
}

/// Configures a [`RefMap`].
#[derive(Clone, Debug)]
pub struct Builder<S = RandomState> {
    shards: Option<usize>,
    min_capacity: usize,
    migration_step: usize,
    maintain_steps: usize,
    hasher: S,
}

impl Builder<RandomState> {
    /// Defaults to four shards per available CPU, rounded up to a power of two
    /// and capped at 1024, at least eight slots per shard, sixteen slots
    /// migrated per operation, and up to 1024 maintenance steps per shard per
    /// `maintain` call.
    pub fn new() -> Self {
        Self {
            shards: None,
            min_capacity: 8,
            migration_step: 16,
            maintain_steps: 1024,
            hasher: RandomState::new(),
        }
    }
}

impl Default for Builder<RandomState> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Builder<S> {
    /// Sets the shard count, rounded up to a power of two. Panics if the
    /// rounded count exceeds 65,536.
    pub fn shards(mut self, shards: usize) -> Self {
        let rounded = shards.max(1).checked_next_power_of_two();
        match rounded.filter(|&count| count <= MAX_SHARDS) {
            Some(count) => self.shards = Some(count),
            None => panic!("refmap shard count {shards} exceeds {MAX_SHARDS}"),
        }
        self
    }

    /// Sets the smallest table per shard in slots, rounded up to a power of two
    /// of at least two. Tables never shrink below it. Panics if the rounded
    /// size exceeds 2^30.
    pub fn min_capacity(mut self, slots: usize) -> Self {
        let rounded = slots.max(2).checked_next_power_of_two();
        match rounded.filter(|&capacity| capacity <= MAX_MIN_CAPACITY) {
            Some(capacity) => self.min_capacity = capacity,
            None => panic!("refmap minimum capacity {slots} exceeds {MAX_MIN_CAPACITY}"),
        }
        self
    }

    /// Sets how many source slots each structural operation migrates while a
    /// table is being resized, at least eight. Bounds the work any single
    /// operation adds to a migration; larger steps finish migrations in fewer
    /// operations.
    pub fn migration_step(mut self, slots: usize) -> Self {
        self.migration_step = slots.max(MIN_MIGRATION_STEP);
        self
    }

    /// Allows steps below the minimum so tests can keep migrations in flight
    /// across many operations on tiny tables. Such maps may grow without
    /// shrinking.
    #[cfg(test)]
    pub(crate) fn migration_step_unchecked(mut self, slots: usize) -> Self {
        self.migration_step = slots.max(1);
        self
    }

    /// Caps the migration steps per shard of one [`RefMap::maintain`] call.
    pub fn maintain_steps(mut self, steps: usize) -> Self {
        self.maintain_steps = steps.max(1);
        self
    }

    /// Replaces the key hasher. The default resists collision attacks, and a
    /// replacement should too when keys come from clients.
    pub fn hasher<T>(self, hasher: T) -> Builder<T> {
        Builder {
            shards: self.shards,
            min_capacity: self.min_capacity,
            migration_step: self.migration_step,
            maintain_steps: self.maintain_steps,
            hasher,
        }
    }

    pub fn build<K, V>(self) -> RefMap<K, V, S>
    where
        K: Hash + Eq,
        S: BuildHasher,
    {
        let shard_count = self.shards.unwrap_or_else(default_shards);
        let config = Config {
            min_capacity: self.min_capacity,
            migration_step: self.migration_step,
            maintain_steps: self.maintain_steps,
        };
        let shards: Box<[Shard<K, V>]> = (0..shard_count)
            .map(|_| Shard::new(config.min_capacity))
            .collect();
        let inner = Box::into_raw(Box::new(Inner {
            header: Header {
                shards,
                shard_bits: shard_count.trailing_zeros(),
                config,
                open: AtomicUsize::new(1),
                destroy: destroy::<K, V, S>,
            },
            hasher: ManuallyDrop::new(self.hasher),
        }));
        // The allocation made above is not shared with any other thread.
        let header = unsafe { ptr::addr_of_mut!((*inner).header) };
        // The same unshared allocation lets this thread mutate the shards.
        for shard in unsafe { (*header).shards.iter_mut() } {
            shard.set_header(header);
        }
        RefMap {
            // `Box::into_raw` never returns null.
            inner: unsafe { NonNull::new_unchecked(inner) },
            _owns: PhantomData,
            _invariant: PhantomData,
        }
    }
}

fn default_shards() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    cpus.saturating_mul(4)
        .next_power_of_two()
        .min(MAX_DEFAULT_SHARDS)
}

/// Frees a map's allocation once its owner and every entry are gone. The
/// owner already dropped the hasher.
///
/// The caller must pass the header of a boxed `Inner<K, V, S>` that no other
/// thread can reach.
unsafe fn destroy<K, V, S>(header: *mut Header<K, V>) {
    // `Inner` is `repr(C)` with the header first, so the header's address is
    // the address of the boxed `Inner`, which the caller owns exclusively.
    let inner = unsafe { Box::from_raw(header.cast::<Inner<K, V, S>>()) };
    for shard in inner.header.shards.iter() {
        // Exclusive ownership means no other thread can reach the shard, and
        // every entry was finalized.
        unsafe { shard.lock().free_tables() };
    }
    drop(inner);
}

impl<K: Hash + Eq, V> RefMap<K, V, RandomState> {
    /// Creates a map with the default [`Builder`] settings.
    pub fn new() -> Self {
        Builder::new().build()
    }
}

impl<K: Hash + Eq, V> Default for RefMap<K, V, RandomState> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, S> RefMap<K, V, S> {
    fn inner(&self) -> &Inner<K, V, S> {
        // The owner's reference to the shared state keeps the allocation alive
        // until the map drops.
        unsafe { self.inner.as_ref() }
    }

    fn header(&self) -> &Header<K, V> {
        &self.inner().header
    }

    /// Returns a lock-free snapshot of the per-shard counters.
    pub fn stats(&self) -> Stats {
        let header = self.header();
        let mut stats = Stats {
            shards: header.shards.len(),
            ..Stats::default()
        };
        for shard in header.shards.iter() {
            let shard = shard.stats();
            stats.entries += shard.entries;
            stats.capacity += shard.capacity;
            stats.migrating += shard.migrating;
            stats.retired += shard.retired;
        }
        stats
    }

    /// Returns the linked entry count, approximate under concurrent updates.
    pub fn len(&self) -> usize {
        self.stats().entries
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Performs bounded housekeeping, advancing the reclamation epoch, freeing
    /// garbage past its grace period, moving in-progress migrations forward by
    /// at most the configured number of steps per shard, and starting shrink
    /// migrations for shards whose tables mostly emptied. Each step runs in its
    /// own short critical section that hands the lock to any waiting thread,
    /// and a shard another thread is using is left to that thread's own steps,
    /// so maintenance never holds back a concurrent operation by more than one
    /// step.
    ///
    /// It is meant to run periodically, for example once per second. On an
    /// idle map, repeated calls free all retired memory and shrink every table
    /// toward its minimum. A shrink of `C` slots takes `C / migration_step`
    /// steps, so `Maintenance::shards_pending` reports shards that need another
    /// call.
    pub fn maintain(&self) -> Maintenance {
        let header = self.header();
        let config = header.config;
        let mut report = Maintenance::default();
        // Two advances let the grace period of garbage retired before this
        // call elapse within it when no reader stays pinned.
        for _ in 0..2 {
            if epoch::try_advance() {
                report.epochs_advanced += 1;
            }
        }
        for shard in header.shards.iter() {
            let mut idle = true;
            for _ in 0..config.maintain_steps {
                let Some(mut state) = shard.try_lock() else {
                    idle = false;
                    break;
                };
                let mut garbage = Garbage::new();
                let stepped = state.step(shard, &config, &mut garbage);
                state.collect(&mut garbage);
                shard.publish_stats(&state);
                if stepped {
                    report.migration_steps += 1;
                }
                let more = stepped && state.is_migrating();
                Mutex::unlock_fair(state);
                report.freed += garbage.free();
                if !more {
                    break;
                }
            }
            if idle {
                report.migrations_started += usize::from(Self::start_shrink(shard, &config));
            }
            let stats = shard.stats();
            if stats.migrating > 0 || stats.retired > 0 {
                report.shards_pending += 1;
            }
        }
        report
    }

    /// Starts a shrink migration on an uncontended shard whose table mostly
    /// emptied, allocating the destination outside the lock.
    fn start_shrink(shard: &Shard<K, V>, config: &Config) -> bool {
        let target = shard
            .try_lock()
            .and_then(|state| state.shrink_target(config));
        let Some(capacity) = target else {
            return false;
        };
        let table = Table::new(capacity);
        let Some(mut state) = shard.try_lock() else {
            return false;
        };
        let unused = state.start_shrink(config, table);
        shard.publish_stats(&state);
        drop(state);
        let started = unused.is_none();
        drop(unused);
        started
    }
}

impl<K, V, S> RefMap<K, V, S>
where
    K: Hash + Eq,
    S: BuildHasher,
{
    fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
        self.inner().hasher.hash_one(key)
    }

    /// Returns a handle to the live entry for `key`, creating it with `init`
    /// when no live entry exists.
    ///
    /// `init` runs outside every lock and only when a lookup found no live
    /// entry. Threads racing to create the same key may each run it; the
    /// losers drop what they built, on their own thread, and get a reference
    /// to the winner's entry. `init` must return a key equal to `key`, and the
    /// call panics otherwise.
    pub fn get_or_insert_with<Q, F>(&self, key: &Q, init: F) -> Ref<K, V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        F: FnOnce() -> (K, V),
    {
        let hash = self.hash(key);
        let shard = self.header().shard(hash);
        if let Some(entry) = Self::find_existing(shard, hash, key) {
            return Ref::adopt(entry);
        }
        let (owned, value) = init();
        // An entry stored under another key would break uniqueness for both.
        assert!(owned.borrow() == key, "init returned a different key");
        self.link(shard, Box::new(Entry::new(hash, shard, owned, value)))
    }

    /// Behaves like [`get_or_insert_with`](Self::get_or_insert_with) with an owned key, which is
    /// dropped when the call gets a reference to an existing entry.
    pub fn get_or_insert_with_owned<F>(&self, key: K, init: F) -> Ref<K, V>
    where
        F: FnOnce(&K) -> V,
    {
        let hash = self.hash(&key);
        let shard = self.header().shard(hash);
        if let Some(entry) = Self::find_existing(shard, hash, &key) {
            return Ref::adopt(entry);
        }
        let value = init(&key);
        self.link(shard, Box::new(Entry::new(hash, shard, key, value)))
    }

    /// Returns a handle to the live entry for `key` without ever creating one.
    pub fn get<Q>(&self, key: &Q) -> Option<Ref<K, V>>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hash(key);
        let shard = self.header().shard(hash);
        Self::find_existing(shard, hash, key).map(Ref::adopt)
    }

    /// Gets a reference to the live entry for `key`, taking the shard lock
    /// only when this thread can no longer pin.
    fn find_existing<Q>(shard: &Shard<K, V>, hash: u64, key: &Q) -> Option<NonNull<Entry<K, V>>>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        match shard.find(hash, key) {
            Lookup::Found(entry) => Some(entry),
            Lookup::Absent => None,
            Lookup::Unpinned => shard.lock().find(hash, key),
        }
    }

    /// Links `candidate` unless a live entry with its key appeared, in which
    /// case the call discards the unpublished entry and gets a reference to
    /// the existing entry.
    fn link(&self, shard: &Shard<K, V>, candidate: Box<Entry<K, V>>) -> Ref<K, V> {
        let config = self.header().config;
        // Destroys the unpublished entry's value too if `Eq` panics under
        // the lock.
        let mut candidate = Candidate(Some(candidate));
        let mut spare: Option<Box<Table<K, V>>> = None;
        loop {
            let mut garbage = Garbage::new();
            let mut state = shard.lock();
            state.step(shard, &config, &mut garbage);
            let entry = candidate
                .0
                .as_ref()
                .expect("candidate is linked at most once");
            if let Some(existing) = state.find(entry.hash, &entry.key) {
                state.collect(&mut garbage);
                let garbage = state.detach_inline(garbage);
                shard.publish_stats(&state);
                drop(state);
                // Adopting the existing entry's reference first lets unwinding
                // put it if a key destructor among the garbage or the
                // unpublished entry's destructor panics.
                let joined = Ref::adopt(existing);
                garbage.free();
                drop(spare);
                drop(candidate);
                return joined;
            }
            match state.reserve(&config, &mut spare) {
                Reserve::Ready => {
                    let raw = Box::into_raw(candidate.0.take().expect("candidate present"));
                    state.insert(raw);
                    state.collect(&mut garbage);
                    let garbage = state.detach_inline(garbage);
                    shard.publish_stats(&state);
                    drop(state);
                    // `Box::into_raw` never returns null, and the inserted
                    // entry holds the creator's reference. Adopting it first
                    // lets unwinding put it if a key destructor among the
                    // garbage panics.
                    let created = Ref::adopt(unsafe { NonNull::new_unchecked(raw) });
                    garbage.free();
                    drop(spare);
                    return created;
                }
                Reserve::NeedTable(capacity) => {
                    state.collect(&mut garbage);
                    let garbage = state.detach_inline(garbage);
                    shard.publish_stats(&state);
                    drop(state);
                    garbage.free();
                    // Allocates outside the lock and retries from the start,
                    // because the shard may have changed meanwhile.
                    spare = Some(Table::new(capacity));
                }
            }
        }
    }
}

/// An entry built for insertion; destroyed with its value unless linked.
struct Candidate<K, V>(Option<Box<Entry<K, V>>>);

impl<K, V> Drop for Candidate<K, V> {
    fn drop(&mut self) {
        if let Some(entry) = self.0.take() {
            // The unpublished entry was never shared, so its value is
            // initialized and owned here, and nothing uses the slot after the
            // value is taken.
            let value = unsafe {
                entry
                    .value
                    .with_mut(|value| ManuallyDrop::take(&mut *value))
            };
            drop(entry);
            drop(value);
        }
    }
}

impl<K, V, S> Drop for RefMap<K, V, S> {
    fn drop(&mut self) {
        // The owning pointer, not a reference, gives the header pointer the
        // provenance `Header::destroy` needs to free the whole allocation.
        let header = self.inner.as_ptr().cast::<Header<K, V>>().cast_const();
        let mut garbage = Garbage::new();
        // The owner's reference to the shared state keeps the header alive
        // until the guard below puts it.
        for shard in unsafe { &*header }.shards.iter() {
            let mut state = shard.lock();
            if state.close(&mut garbage) {
                // The owner's reference also keeps the header alive here.
                // Relaxed suffices because the shard lock orders this
                // increment before any finalizer can see `closed` set and put
                // the shard's reference.
                unsafe { &*header }.open.fetch_add(1, Ordering::Relaxed);
            }
            shard.publish_stats(&state);
        }
        // Puts the owner's reference even if a destructor below panics.
        let _owner = OpenHold(header);
        // Only the owner reads the hasher, because finalizers locate entries by
        // their cached hash, and the projection to the hasher field does not
        // alias the header, which finalizers may still read.
        unsafe { ManuallyDrop::drop(&mut *ptr::addr_of_mut!((*self.inner.as_ptr()).hasher)) };
        // No reader can exist once the owner is gone, so retired memory is
        // freed without waiting for an epoch.
        garbage.free();
    }
}

impl<K, V, S> fmt::Debug for RefMap<K, V, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefMap")
            .field("stats", &self.stats())
            .finish()
    }
}

/// Owned handle to a live entry. Cloning adds a user; dropping the last one
/// destroys the value on the dropping thread and removes the entry.
pub struct Ref<K, V> {
    entry: NonNull<Entry<K, V>>,
    _owns: PhantomData<Entry<K, V>>,
}

// A handle shares the value across threads and may drop it, other threads'
// lookups read the key, and dropping a handle may free retired entries' keys.
unsafe impl<K: Send + Sync, V: Send + Sync> Send for Ref<K, V> {}
// Sharing a handle is sound for the same reasons.
unsafe impl<K: Send + Sync, V: Send + Sync> Sync for Ref<K, V> {}

impl<K, V> Ref<K, V> {
    /// Wraps a reference the caller already acquired.
    fn adopt(entry: NonNull<Entry<K, V>>) -> Self {
        Self {
            entry,
            _owns: PhantomData,
        }
    }

    fn entry(&self) -> &Entry<K, V> {
        // This handle's reference keeps the entry allocated.
        unsafe { self.entry.as_ref() }
    }

    pub fn key(&self) -> &K {
        &self.entry().key
    }

    /// Returns whether both handles refer to the same entry. Two entries for
    /// one key exist only one after the other, never both live.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        this.entry == other.entry
    }

    /// Returns the entry's handle count, a racy figure for diagnostics.
    pub fn users(this: &Self) -> usize {
        this.entry().refs.approximate()
    }
}

impl<K, V> Clone for Ref<K, V> {
    fn clone(&self) -> Self {
        self.entry().refs.clone_ref();
        Self::adopt(self.entry)
    }
}

impl<K, V> Deref for Ref<K, V> {
    type Target = V;

    fn deref(&self) -> &V {
        // The value is dropped only by the finalizer, which runs after every
        // handle, this one included, is gone.
        unsafe { self.entry().value.with(|value| &**value) }
    }
}

impl<K, V> Drop for Ref<K, V> {
    fn drop(&mut self) {
        // The put never touches the entry after giving up its reference, so it
        // needs no pin even while the map's owner frees retired memory.
        if self.entry().refs.put() {
            // A put that returns true moved the reference word to `DEAD`, so
            // this thread is the entry's unique finalizer.
            unsafe { finalize(self.entry) };
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Ref<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ref")
            .field("key", self.key())
            .field("value", &**self)
            .finish()
    }
}

/// Destroys the value, then unlinks and retires the entry.
///
/// The caller must have moved the entry's reference word to `DEAD`.
unsafe fn finalize<K, V>(entry: NonNull<Entry<K, V>>) {
    struct Unlink<K, V>(NonNull<Entry<K, V>>);

    impl<K, V> Drop for Unlink<K, V> {
        fn drop(&mut self) {
            // This guard exists only inside `finalize`, whose contract holds
            // here, and it runs after the value's destructor even if that
            // destructor panics.
            unsafe { unlink(self.0) }
        }
    }

    let unlink_on_exit = Unlink(entry);
    // The entry is dead, so no handle or lookup can reach the value.
    unsafe {
        entry
            .as_ref()
            .value
            .with_mut(|value| ManuallyDrop::drop(&mut *value))
    };
    drop(unlink_on_exit);
}

/// Unlinks and retires the entry.
///
/// The caller must meet the contract of [`finalize`] and must already have
/// dropped the value.
unsafe fn unlink<K, V>(entry: NonNull<Entry<K, V>>) {
    let raw = entry.as_ptr();
    // The entry stays allocated until it is retired below, and it keeps its
    // shard alive until it stops counting as outstanding.
    let shard = unsafe { &*(*raw).shard };
    let header = shard.header();
    // The shard's entries keep the header alive.
    let config = unsafe { (*header).config };
    let mut garbage = Garbage::new();
    let (release, advance, garbage) = {
        let mut state = shard.lock();
        state.step(shard, &config, &mut garbage);
        state.unlink(raw);
        state.outstanding -= 1;
        state.retire_entry(raw, &mut garbage);
        state.collect(&mut garbage);
        let garbage = state.detach_inline(garbage);
        shard.publish_stats(&state);
        // Each multiple of the threshold prompts one advance attempt, which
        // keeps the clock read and participant scan off most final puts.
        let retired = state.retired();
        (
            state.closed && state.outstanding == 0,
            !state.closed && retired >= RECLAIM_THRESHOLD && retired % RECLAIM_THRESHOLD == 0,
            garbage,
        )
    };
    // Nothing below touches the shard, which the owner may close and free once
    // this entry stops counting as outstanding unless this thread holds the
    // shard's last reference to the shared state. The guard puts that reference
    // even if a key destructor panics below.
    let _hold = release.then(|| OpenHold(header));
    garbage.free();
    if advance {
        epoch::maybe_advance();
    }
}

/// A reference to a map's shared state, put when dropped.
struct OpenHold<K, V>(*const Header<K, V>);

impl<K, V> Drop for OpenHold<K, V> {
    fn drop(&mut self) {
        // The guard owns one reference, either the map owner's or one taken for
        // a shard with orphaned entries, and its holder touches nothing in the
        // map afterwards.
        unsafe { Header::release_open(self.0) };
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;
