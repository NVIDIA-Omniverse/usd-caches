// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shard of the map, pairing an RCU-published table chain for readers with a
//! mutex that serializes structural changes, each doing bounded work.

use std::borrow::Borrow;
use std::ptr::{self, NonNull};

use crate::epoch;
use crate::sync::{AtomicPtr, AtomicUsize, CachePadded, Mutex, MutexGuard, Ordering};
use crate::table::{Entry, Table, is_tombstone, tombstone};

/// Retired objects in one shard that make a finalizer try to advance the epoch.
pub(crate) const RECLAIM_THRESHOLD: usize = 64;

/// Entries past their grace period that one foreground operation frees, so no
/// caller pays for a backlog left by a stalled reader. Each operation retires
/// at most one entry, so any limit above one keeps pace; `maintain` frees
/// without a limit.
const INLINE_FREE_ENTRIES: usize = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    pub(crate) min_capacity: usize,
    pub(crate) migration_step: usize,
    pub(crate) maintain_steps: usize,
}

/// Map-wide state reachable from every shard. `RefMap` owns it until the map
/// is dropped. The owner frees it then when no entries are orphaned, and
/// otherwise the last finalizer of the last shard with orphaned entries does.
pub(crate) struct Header<K, V> {
    pub(crate) shards: Box<[Shard<K, V>]>,
    pub(crate) shard_bits: u32,
    pub(crate) config: Config,
    /// One reference for the owner plus one per shard with orphaned entries.
    pub(crate) open: AtomicUsize,
    /// Frees the full allocation, whose hasher type this header erases.
    pub(crate) destroy: unsafe fn(*mut Header<K, V>),
}

impl<K, V> Header<K, V> {
    pub(crate) fn shard(&self, hash: u64) -> &Shard<K, V> {
        let index = if self.shard_bits == 0 {
            0
        } else {
            (hash >> (64 - self.shard_bits)) as usize
        };
        &self.shards[index]
    }

    /// Drops one reference to the header and frees everything with the last
    /// one. The caller must own a reference and touch nothing reachable from
    /// the header afterwards.
    pub(crate) unsafe fn release_open(this: *const Self) {
        // The caller's reference keeps the header alive until this point.
        let previous = unsafe { &*this }.open.fetch_sub(1, Ordering::AcqRel);
        if previous == 1 {
            // No references remain, so no other thread can reach the header.
            unsafe { ((*this).destroy)(this.cast_mut()) }
        }
    }
}

pub(crate) struct Shard<K, V> {
    /// Read by every lookup; kept apart from the writer-side cache lines.
    read: CachePadded<ReadSide<K, V>>,
    write: CachePadded<WriteSide<K, V>>,
}

struct ReadSide<K, V> {
    root: AtomicPtr<Table<K, V>>,
    header: *const Header<K, V>,
}

struct WriteSide<K, V> {
    state: Mutex<ShardState<K, V>>,
    entries: AtomicUsize,
    capacity: AtomicUsize,
    migrating: AtomicUsize,
    retired: AtomicUsize,
}

pub(crate) enum Lookup<K, V> {
    Found(NonNull<Entry<K, V>>),
    Absent,
    /// The thread cannot pin because its thread-locals are being destroyed.
    Unpinned,
}

impl<K, V> Shard<K, V> {
    pub(crate) fn new(capacity: usize) -> Self {
        let root = Box::into_raw(Table::new(capacity));
        Self {
            read: CachePadded(ReadSide {
                root: AtomicPtr::new(root),
                header: ptr::null(),
            }),
            write: CachePadded(WriteSide {
                state: Mutex::new(ShardState::new(root)),
                entries: AtomicUsize::new(0),
                capacity: AtomicUsize::new(capacity),
                migrating: AtomicUsize::new(0),
                retired: AtomicUsize::new(0),
            }),
        }
    }

    /// Records the owning header once the map's allocation is in place.
    pub(crate) fn set_header(&mut self, header: *const Header<K, V>) {
        self.read.header = header;
    }

    pub(crate) fn header(&self) -> *const Header<K, V> {
        self.read.header
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, ShardState<K, V>> {
        self.write.state.lock()
    }

    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, ShardState<K, V>>> {
        self.write.state.try_lock()
    }

    /// Lock-free lookup that gets a reference to the live entry for `key`.
    pub(crate) fn find<Q>(&self, hash: u64, key: &Q) -> Lookup<K, V>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let Some(_guard) = epoch::pin() else {
            return Lookup::Unpinned;
        };
        let mut table = self.read.root.load(Ordering::Acquire);
        loop {
            // This thread is pinned, so the table and every entry reachable
            // from it stay allocated until the guard drops.
            let current = unsafe { &*table };
            // The same pin keeps every entry reachable from the table
            // allocated until the guard drops, as `Table::find` requires.
            if let Some(entry) = unsafe { current.find(hash, key) } {
                return Lookup::Found(entry);
            }
            let next = current.next.load(Ordering::Acquire);
            if next.is_null() {
                return Lookup::Absent;
            }
            table = next;
        }
    }

    pub(crate) fn publish_stats(&self, state: &ShardState<K, V>) {
        let write = &self.write;
        write.entries.store(state.linked, Ordering::Relaxed);
        write.capacity.store(state.capacity(), Ordering::Relaxed);
        write
            .migrating
            .store(usize::from(!state.dst.is_null()), Ordering::Relaxed);
        write.retired.store(state.retired(), Ordering::Relaxed);
    }

    pub(crate) fn stats(&self) -> ShardStats {
        let write = &self.write;
        ShardStats {
            entries: write.entries.load(Ordering::Relaxed),
            capacity: write.capacity.load(Ordering::Relaxed),
            migrating: write.migrating.load(Ordering::Relaxed),
            retired: write.retired.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn root(&self) -> &AtomicPtr<Table<K, V>> {
        &self.read.root
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShardStats {
    pub(crate) entries: usize,
    pub(crate) capacity: usize,
    pub(crate) migrating: usize,
    pub(crate) retired: usize,
}

/// Writer state, protected by the shard mutex.
pub(crate) struct ShardState<K, V> {
    /// The table readers start from; the migration source while migrating.
    src: *mut Table<K, V>,
    /// Migration destination (`src.next`), or null.
    dst: *mut Table<K, V>,
    /// Next `src` slot to migrate.
    cursor: usize,
    /// Non-EMPTY slots in `src` and `dst`.
    src_used: usize,
    dst_used: usize,
    /// Distinct entries present in `src` or `dst`, live or awaiting unlink.
    linked: usize,
    /// Entries created here that their finalizer has not yet unlinked.
    pub(crate) outstanding: usize,
    limbo: Limbo<K, V>,
    /// Garbage past its grace period that foreground operations left for
    /// later ones.
    expired: Garbage<K, V>,
    pub(crate) closed: bool,
}

/// What an insert needs before it can proceed.
pub(crate) enum Reserve {
    Ready,
    NeedTable(usize),
}

impl<K, V> ShardState<K, V> {
    fn new(root: *mut Table<K, V>) -> Self {
        Self {
            src: root,
            dst: ptr::null_mut(),
            cursor: 0,
            src_used: 0,
            dst_used: 0,
            linked: 0,
            outstanding: 0,
            limbo: Limbo::new(),
            expired: Garbage::new(),
            closed: false,
        }
    }

    fn src(&self) -> &Table<K, V> {
        // `src` is always a live table owned by this shard.
        unsafe { &*self.src }
    }

    fn dst(&self) -> Option<&Table<K, V>> {
        // A non-null `dst` is a live table owned by this shard.
        unsafe { self.dst.as_ref() }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.src().capacity() + self.dst().map_or(0, Table::capacity)
    }

    pub(crate) fn is_migrating(&self) -> bool {
        !self.dst.is_null()
    }

    /// Gets a reference to the live entry for `key` in the current tables.
    pub(crate) fn find<Q>(&self, hash: u64, key: &Q) -> Option<NonNull<Entry<K, V>>>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        // Under the shard lock, entries linked in the current tables are not
        // retired, so they stay allocated.
        unsafe {
            self.src()
                .find(hash, key)
                .or_else(|| self.dst().and_then(|dst| dst.find(hash, key)))
        }
    }

    /// Decides whether an insert may proceed, starting a growth migration with
    /// `spare` when the source table is at its load limit.
    pub(crate) fn reserve(
        &mut self,
        config: &Config,
        spare: &mut Option<Box<Table<K, V>>>,
    ) -> Reserve {
        if self.is_migrating() {
            return Reserve::Ready;
        }
        let capacity = self.src().capacity();
        if (self.src_used + 1) * 4 <= capacity * 3 {
            return Reserve::Ready;
        }
        let target = target_capacity(self.linked, capacity, config);
        match spare.take() {
            Some(table) if table.capacity() >= target && table.capacity() <= target * 4 => {
                self.start_migration(table);
                Reserve::Ready
            }
            unsuitable => {
                *spare = unsuitable;
                Reserve::NeedTable(target)
            }
        }
    }

    /// Links a new entry. The caller found no live entry for its key and
    /// `reserve` returned `Ready`.
    pub(crate) fn insert(&mut self, entry: *mut Entry<K, V>) {
        // The caller owns the entry being linked.
        let hash = unsafe { (*entry).hash };
        if self.dst.is_null() {
            // `src` is a live table owned by this shard.
            if unsafe { &*self.src }.insert(hash, entry) {
                self.src_used += 1;
            }
        } else {
            // A non-null `dst` is a live table owned by this shard.
            let dst = unsafe { &*self.dst };
            if dst.insert(hash, entry) {
                self.dst_used += 1;
            }
            debug_assert!(
                self.dst_used * 2 <= dst.capacity(),
                "migration headroom exceeded"
            );
        }
        self.linked += 1;
        self.outstanding += 1;
    }

    /// Tombstones `entry` wherever it is linked. Never compares keys.
    pub(crate) fn unlink(&mut self, entry: *mut Entry<K, V>) {
        // The finalizer keeps the entry allocated until it retires it.
        let hash = unsafe { (*entry).hash };
        let in_src = self.src().remove(hash, entry);
        let in_dst = self.dst().is_some_and(|dst| dst.remove(hash, entry));
        if in_src || in_dst {
            self.linked -= 1;
        }
    }

    /// Migrates at most `migration_step` source slots.
    pub(crate) fn step(
        &mut self,
        shard: &Shard<K, V>,
        config: &Config,
        garbage: &mut Garbage<K, V>,
    ) -> bool {
        if self.dst.is_null() {
            return false;
        }
        // Both tables are live and owned by this shard, and the references do
        // not borrow `self`, whose counters change below.
        let (src, dst) = unsafe { (&*self.src, &*self.dst) };
        let end = (self.cursor + config.migration_step).min(src.capacity());
        let mut copied = 0;
        let mut dropped = 0;
        for index in self.cursor..end {
            let slot = src.slot(index);
            let pointer = slot.load(Ordering::Relaxed);
            if pointer.is_null() || is_tombstone(pointer) {
                continue;
            }
            // Linked entries are not retired while the lock is held.
            let entry = unsafe { &*pointer };
            if entry.refs.is_dead() {
                // Its finalizer will find nothing left to unlink.
                slot.store(tombstone(), Ordering::Release);
                dropped += 1;
            } else if dst.insert(entry.hash, pointer) {
                copied += 1;
            }
        }
        self.linked -= dropped;
        self.dst_used += copied;
        self.cursor = end;
        if end == src.capacity() {
            self.complete_migration(shard, garbage);
        }
        true
    }

    fn start_migration(&mut self, table: Box<Table<K, V>>) {
        let dst = Box::into_raw(table);
        self.src().next.store(dst, Ordering::Release);
        self.dst = dst;
        self.cursor = 0;
        self.dst_used = 0;
    }

    fn complete_migration(&mut self, shard: &Shard<K, V>, garbage: &mut Garbage<K, V>) {
        shard.root().store(self.dst, Ordering::Release);
        let old = self.src;
        self.src = self.dst;
        self.dst = ptr::null_mut();
        self.cursor = 0;
        self.src_used = self.dst_used;
        self.dst_used = 0;
        self.retire_table(old, garbage);
    }

    /// Shrink target when the source table is mostly empty and idle.
    pub(crate) fn shrink_target(&self, config: &Config) -> Option<usize> {
        if self.is_migrating() {
            return None;
        }
        let capacity = self.src().capacity();
        if capacity <= config.min_capacity || self.linked * 8 >= capacity {
            return None;
        }
        let target = target_capacity(self.linked, capacity, config);
        (target < capacity).then_some(target)
    }

    /// Starts a shrink migration into `table` if the shard still wants exactly
    /// that size; otherwise hands the table back to be freed outside the lock.
    pub(crate) fn start_shrink(
        &mut self,
        config: &Config,
        table: Box<Table<K, V>>,
    ) -> Option<Box<Table<K, V>>> {
        if self.shrink_target(config) == Some(table.capacity()) {
            self.start_migration(table);
            None
        } else {
            Some(table)
        }
    }

    /// Retires an unlinked entry, or schedules it for an immediate free once
    /// the map is dropped and no reader can exist.
    pub(crate) fn retire_entry(&mut self, entry: *mut Entry<K, V>, garbage: &mut Garbage<K, V>) {
        if self.closed {
            garbage.entries.push(entry);
        } else {
            let epoch = epoch::retire_epoch();
            self.limbo.push_entry(entry, epoch, garbage);
        }
    }

    fn retire_table(&mut self, table: *mut Table<K, V>, garbage: &mut Garbage<K, V>) {
        if self.closed {
            garbage.tables.push(table);
        } else {
            let epoch = epoch::retire_epoch();
            self.limbo.push_table(table, epoch, garbage);
        }
    }

    /// Detaches garbage that no pinned reader can still reach, including any
    /// backlog earlier operations handed back.
    pub(crate) fn collect(&mut self, garbage: &mut Garbage<K, V>) {
        if !self.limbo.is_empty() {
            self.limbo.collect(epoch::collect_epoch(), garbage);
        }
        garbage.append(&mut self.expired);
    }

    /// Returns the bounded share of `garbage` that a foreground operation
    /// frees after unlocking and keeps the rest for later operations, which
    /// `maintain` drains while the map is alive and `destroy` frees after it is
    /// dropped. Splitting under the lock spares a finalizer from touching
    /// the shard after unlocking, when the map's owner may already have freed
    /// it.
    pub(crate) fn detach_inline(&mut self, mut garbage: Garbage<K, V>) -> Garbage<K, V> {
        let now = garbage.split_inline();
        self.expired.append(&mut garbage);
        now
    }

    /// Retired objects still allocated, waiting for readers or for a free.
    pub(crate) fn retired(&self) -> usize {
        self.limbo.len() + self.expired.len()
    }

    /// Records that the owner is dropping the map. Returns whether orphaned
    /// entries remain, in which case the shard keeps the header alive.
    pub(crate) fn close(&mut self, garbage: &mut Garbage<K, V>) -> bool {
        self.closed = true;
        self.limbo.take_all(garbage);
        garbage.append(&mut self.expired);
        self.outstanding > 0
    }

    /// Frees the tables of a shard whose entries are all finalized. The caller
    /// must ensure that no other thread can reach the shard.
    pub(crate) unsafe fn free_tables(&mut self) {
        debug_assert_eq!(self.outstanding, 0);
        for table in [self.src, self.dst] {
            if !table.is_null() {
                // This shard owns the table, and no other thread can reach it.
                let table = unsafe { Box::from_raw(table) };
                debug_assert!(!table.has_entries(), "teardown found a linked entry");
                drop(table);
            }
        }
        self.src = ptr::null_mut();
        self.dst = ptr::null_mut();
    }
}

/// Destination capacity of a migration, twice the entries it can hold at its
/// end, which are the current entries plus one insert per step it takes to
/// finish.
pub(crate) fn target_capacity(linked: usize, source_capacity: usize, config: &Config) -> usize {
    let window = source_capacity.div_ceil(config.migration_step);
    (2 * (linked + window + 1))
        .max(config.min_capacity)
        .next_power_of_two()
}

/// Intrusive singly linked list threaded through `retire_next`.
pub(crate) struct List<T: Retire> {
    head: *mut T,
    tail: *mut T,
    len: usize,
}

pub(crate) trait Retire {
    fn retire_link(&self) -> &std::cell::Cell<*mut Self>;
}

impl<K, V> Retire for Entry<K, V> {
    fn retire_link(&self) -> &std::cell::Cell<*mut Self> {
        &self.retire_next
    }
}

impl<K, V> Retire for Table<K, V> {
    fn retire_link(&self) -> &std::cell::Cell<*mut Self> {
        &self.retire_next
    }
}

impl<T: Retire> List<T> {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, item: *mut T) {
        // The list owns `item`, and readers never touch its retirement link.
        unsafe { (*item).retire_link().set(self.head) };
        if self.head.is_null() {
            self.tail = item;
        }
        self.head = item;
        self.len += 1;
    }

    fn append(&mut self, other: &mut Self) {
        if other.head.is_null() {
            return;
        }
        if self.head.is_null() {
            *self = std::mem::replace(other, Self::new());
            return;
        }
        // `tail` is the last item of this owned list.
        unsafe { (*self.tail).retire_link().set(other.head) };
        self.tail = other.tail;
        self.len += other.len;
        *other = Self::new();
    }

    /// Splits off the first `count` items, walking them. The caller owns both
    /// lists, either under the shard lock or as detached garbage.
    fn split_front(&mut self, count: usize) -> Self {
        if count == 0 || self.head.is_null() {
            return Self::new();
        }
        if count >= self.len {
            return std::mem::replace(self, Self::new());
        }
        let mut last = self.head;
        for _ in 1..count {
            // The list owns its items and holds more than `count` of them.
            last = unsafe { (*last).retire_link().get() };
        }
        let front = Self {
            head: self.head,
            tail: last,
            len: count,
        };
        // The list owns `last`, which is not its final item because the list
        // holds more than `count` items.
        self.head = unsafe { (*last).retire_link().get() };
        // The front list owns `last`, its final item.
        unsafe { (*last).retire_link().set(ptr::null_mut()) };
        self.len -= count;
        front
    }

    fn pop(&mut self) -> Option<*mut T> {
        let item = self.head;
        if item.is_null() {
            return None;
        }
        // The list owns its items.
        self.head = unsafe { (*item).retire_link().get() };
        if self.head.is_null() {
            self.tail = ptr::null_mut();
        }
        self.len -= 1;
        Some(item)
    }
}

/// Three retirement lists indexed by epoch modulo three, so a list is reused
/// only after the grace period of its epoch has elapsed.
struct Limbo<K, V> {
    epochs: [u64; 3],
    entries: [List<Entry<K, V>>; 3],
    tables: [List<Table<K, V>>; 3],
}

impl<K, V> Limbo<K, V> {
    fn new() -> Self {
        Self {
            epochs: [0; 3],
            entries: [List::new(), List::new(), List::new()],
            tables: [List::new(), List::new(), List::new()],
        }
    }

    fn len(&self) -> usize {
        self.entries.iter().map(|list| list.len).sum::<usize>()
            + self.tables.iter().map(|list| list.len).sum::<usize>()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn slot(&mut self, epoch: u64, garbage: &mut Garbage<K, V>) -> usize {
        let index = (epoch % 3) as usize;
        if self.epochs[index] != epoch {
            // The list holds garbage from epoch - 3 or older, whose grace
            // period has elapsed.
            garbage.entries.append(&mut self.entries[index]);
            garbage.tables.append(&mut self.tables[index]);
            self.epochs[index] = epoch;
        }
        index
    }

    fn push_entry(&mut self, entry: *mut Entry<K, V>, epoch: u64, garbage: &mut Garbage<K, V>) {
        let index = self.slot(epoch, garbage);
        self.entries[index].push(entry);
    }

    fn push_table(&mut self, table: *mut Table<K, V>, epoch: u64, garbage: &mut Garbage<K, V>) {
        let index = self.slot(epoch, garbage);
        self.tables[index].push(table);
    }

    fn collect(&mut self, global: u64, garbage: &mut Garbage<K, V>) {
        for index in 0..3 {
            if epoch::is_expired(self.epochs[index], global) {
                garbage.entries.append(&mut self.entries[index]);
                garbage.tables.append(&mut self.tables[index]);
            }
        }
    }

    fn take_all(&mut self, garbage: &mut Garbage<K, V>) {
        for index in 0..3 {
            garbage.entries.append(&mut self.entries[index]);
            garbage.tables.append(&mut self.tables[index]);
        }
    }
}

/// Objects detached under the lock and freed after it is released.
pub(crate) struct Garbage<K, V> {
    pub(crate) entries: List<Entry<K, V>>,
    pub(crate) tables: List<Table<K, V>>,
}

impl<K, V> Garbage<K, V> {
    pub(crate) const fn new() -> Self {
        Self {
            entries: List::new(),
            tables: List::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len + self.tables.len
    }

    fn append(&mut self, other: &mut Self) {
        self.entries.append(&mut other.entries);
        self.tables.append(&mut other.tables);
    }

    /// Splits off a bounded number of entries and at most one table, whose
    /// size follows peak activity, for one foreground operation to free.
    pub(crate) fn split_inline(&mut self) -> Self {
        Self {
            entries: self.entries.split_front(INLINE_FREE_ENTRIES),
            tables: self.tables.split_front(1),
        }
    }

    /// Frees everything and returns the number of objects freed. Freeing an
    /// entry drops its key; its value was dropped by the finalizer.
    pub(crate) fn free(mut self) -> usize {
        let count = self.entries.len + self.tables.len;
        self.drain();
        count
    }

    /// If a key destructor panics, `Drop` resumes draining while unwinding,
    /// and a second panic aborts.
    fn drain(&mut self) {
        while let Some(table) = self.tables.pop() {
            // Detached garbage is owned here and unreachable by readers, and
            // tables were allocated with `Box`.
            drop(unsafe { Box::from_raw(table) });
        }
        while let Some(entry) = self.entries.pop() {
            // The same holds for entries, which were also allocated with
            // `Box`. The value is `ManuallyDrop`, so this drops only the key
            // and the allocation.
            drop(unsafe { Box::from_raw(entry) });
        }
    }
}

impl<K, V> Drop for Garbage<K, V> {
    fn drop(&mut self) {
        self.drain();
    }
}
