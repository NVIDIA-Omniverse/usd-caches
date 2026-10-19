// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Entries and the open-addressing tables that index them.
//!
//! A slot holds EMPTY (null), TOMBSTONE, or a pointer to an entry. A slot never
//! returns to EMPTY, and an entry pointer is only ever replaced by TOMBSTONE,
//! so a lock-free reader that probes from the home slot to the first EMPTY slot
//! passes every entry that stays linked during its probe. Tables store entry
//! pointers, never entries, so migration copies pointers and identity is
//! stable for the lifetime of the entry.

use std::borrow::Borrow;
use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

use crate::refcount::RefCount;
use crate::shard::Shard;
use crate::sync::{AtomicPtr, Ordering, UnsafeCell};

/// Marks a removed entry. It has no provenance and is never dereferenced.
pub(crate) fn tombstone<T>() -> *mut T {
    ptr::without_provenance_mut(1)
}

pub(crate) fn is_tombstone<T>(pointer: *mut T) -> bool {
    pointer.addr() == 1
}

pub(crate) struct Entry<K, V> {
    pub(crate) refs: RefCount,
    pub(crate) hash: u64,
    pub(crate) shard: *const Shard<K, V>,
    /// Intrusive link for retirement lists, written under the shard lock or by
    /// the thread that owns a detached list. Readers never touch it.
    pub(crate) retire_next: Cell<*mut Entry<K, V>>,
    pub(crate) key: K,
    /// Dropped by the finalizer, before the entry is unlinked. The rest of the
    /// entry stays readable by pinned readers until the entry is freed.
    pub(crate) value: UnsafeCell<ManuallyDrop<V>>,
}

impl<K, V> Entry<K, V> {
    pub(crate) fn new(hash: u64, shard: *const Shard<K, V>, key: K, value: V) -> Self {
        Self {
            refs: RefCount::new(),
            hash,
            shard,
            retire_next: Cell::new(ptr::null_mut()),
            key,
            value: UnsafeCell::new(ManuallyDrop::new(value)),
        }
    }
}

pub(crate) struct Table<K, V> {
    mask: usize,
    /// Destination of the migration that started from this table, if any.
    /// Never cleared, so a reader holding a stale table can always reach its
    /// successors.
    pub(crate) next: AtomicPtr<Table<K, V>>,
    pub(crate) retire_next: Cell<*mut Table<K, V>>,
    slots: Box<[AtomicPtr<Entry<K, V>>]>,
}

impl<K, V> Table<K, V> {
    /// Allocates an empty table. `capacity` must be a power of two.
    pub(crate) fn new(capacity: usize) -> Box<Self> {
        assert!(
            capacity.is_power_of_two(),
            "refmap table capacity {capacity}"
        );
        Box::new(Self {
            mask: capacity - 1,
            next: AtomicPtr::new(ptr::null_mut()),
            retire_next: Cell::new(ptr::null_mut()),
            slots: empty_slots(capacity),
        })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.mask + 1
    }

    pub(crate) fn slot(&self, index: usize) -> &AtomicPtr<Entry<K, V>> {
        &self.slots[index]
    }

    /// Gets a reference to the live entry for `key`, skipping dead entries.
    /// The caller must keep every entry reachable from this table allocated
    /// for the duration of the call, as pinning or holding the shard lock does.
    pub(crate) unsafe fn find<Q>(&self, hash: u64, key: &Q) -> Option<NonNull<Entry<K, V>>>
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        let mut index = hash as usize & self.mask;
        for _ in 0..=self.mask {
            let pointer = self.slots[index].load(Ordering::Acquire);
            if pointer.is_null() {
                return None;
            }
            if !is_tombstone(pointer) {
                // The caller keeps every reachable entry allocated.
                let entry = unsafe { &*pointer };
                if entry.hash == hash && entry.key.borrow() == key && entry.refs.try_get() {
                    // The slot's pointer, not one derived from `entry`, keeps
                    // the provenance the finalizer needs to free the entry.
                    // Entry pointers in slots are never null.
                    return Some(unsafe { NonNull::new_unchecked(pointer) });
                }
            }
            index = (index + 1) & self.mask;
        }
        None
    }

    /// Stores `entry` in the first EMPTY or TOMBSTONE slot of its probe
    /// sequence. Returns whether an EMPTY slot was consumed. The caller must
    /// hold the shard lock and keep the table below its load limit.
    pub(crate) fn insert(&self, hash: u64, entry: *mut Entry<K, V>) -> bool {
        let mut index = hash as usize & self.mask;
        for _ in 0..=self.mask {
            let slot = &self.slots[index];
            let current = slot.load(Ordering::Relaxed);
            if current.is_null() || is_tombstone(current) {
                slot.store(entry, Ordering::Release);
                return current.is_null();
            }
            index = (index + 1) & self.mask;
        }
        unreachable!("refmap table has no free slot; the load limit was violated")
    }

    /// Replaces the slot holding exactly `entry` with TOMBSTONE and returns
    /// whether it was present. The caller must hold the shard lock.
    pub(crate) fn remove(&self, hash: u64, entry: *mut Entry<K, V>) -> bool {
        let mut index = hash as usize & self.mask;
        for _ in 0..=self.mask {
            let slot = &self.slots[index];
            let current = slot.load(Ordering::Relaxed);
            if current.is_null() {
                return false;
            }
            if current == entry {
                slot.store(tombstone(), Ordering::Release);
                return true;
            }
            index = (index + 1) & self.mask;
        }
        false
    }

    /// Whether any slot still references an entry. Teardown assertion only.
    pub(crate) fn has_entries(&self) -> bool {
        self.slots.iter().any(|slot| {
            let pointer = slot.load(Ordering::Relaxed);
            !pointer.is_null() && !is_tombstone(pointer)
        })
    }
}

#[cfg(not(loom))]
fn empty_slots<T>(capacity: usize) -> Box<[AtomicPtr<T>]> {
    use std::alloc::{Layout, alloc_zeroed, handle_alloc_error};

    // Zeroed allocation lets large tables start as untouched pages, so the
    // cost of a big table is paid gradually by the migration that fills it.
    let layout = Layout::array::<AtomicPtr<T>>(capacity).expect("refmap table layout");
    // `capacity` is non-zero, so the layout has the non-zero size that
    // `alloc_zeroed` requires.
    let pointer = unsafe { alloc_zeroed(layout) }.cast::<AtomicPtr<T>>();
    if pointer.is_null() {
        handle_alloc_error(layout);
    }
    // `AtomicPtr<T>` has the same in-memory representation as `*mut T`, for
    // which all-zero bytes are null, and the allocation matches the layout
    // `Box<[AtomicPtr<T>]>` deallocates with.
    unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(pointer, capacity)) }
}

#[cfg(loom)]
fn empty_slots<T>(capacity: usize) -> Box<[AtomicPtr<T>]> {
    (0..capacity)
        .map(|_| AtomicPtr::new(ptr::null_mut()))
        .collect()
}
