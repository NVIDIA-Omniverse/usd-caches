// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Reference count for entries reached through lock-free lookups.
//!
//! The word stores `references - 1`, so a live entry holds a value from zero
//! to `MAXREF`. A get is a single `fetch_add`, so a herd of readers on one
//! hot key never retries. A put is a compare-and-swap that moves the last
//! reference straight to `DEAD`, so the word never passes through zero
//! references, no get can resurrect an entry during its final put, and no
//! put touches the entry after giving up its reference. The thread whose
//! exchange stores `DEAD` is the unique finalizer, and once the word reaches
//! `DEAD` it never leaves the dead zone.

use crate::sync::{AtomicU64, Ordering};

const ONEREF: u64 = 0;
/// Largest encoding of a live count; one past it means overflow.
const MAXREF: u64 = 0x7FFF_FFFF_FFFF_FFFF;
/// Start of the dead zone, wide enough to absorb concurrent failed gets.
const RELEASED: u64 = 0xC000_0000_0000_0000;
/// Canonical dead value.
const DEAD: u64 = 0xE000_0000_0000_0000;

pub(crate) struct RefCount(AtomicU64);

impl RefCount {
    /// A count holding the creator's reference.
    pub(crate) fn new() -> Self {
        Self(AtomicU64::new(ONEREF))
    }

    /// Gets a reference to an entry found through a table. Fails only once
    /// the entry is dead.
    #[inline]
    pub(crate) fn try_get(&self) -> bool {
        let new = self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if new <= MAXREF {
            return true;
        }
        self.get_slow(new)
    }

    #[cold]
    fn get_slow(&self, new: u64) -> bool {
        if new >= RELEASED {
            // Undoes the drift of concurrent failed gets inside the dead zone.
            self.0.store(DEAD, Ordering::Relaxed);
            return false;
        }
        overflow()
    }

    /// Adds a reference on behalf of a caller that already holds one.
    #[inline]
    pub(crate) fn clone_ref(&self) {
        let old = self.0.fetch_add(1, Ordering::Relaxed);
        if old >= MAXREF {
            overflow();
        }
    }

    /// Drops a reference. Returns true for the unique finalizer, which observes
    /// every other holder's accesses before it destroys the value.
    #[inline]
    pub(crate) fn put(&self) -> bool {
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            match self.put_from(current) {
                Ok(last) => return last,
                Err(actual) => current = actual,
            }
        }
    }

    /// One compare-and-swap from `current`, which the caller's reference keeps
    /// inside the live range.
    #[inline]
    fn put_from(&self, current: u64) -> Result<bool, u64> {
        if current > MAXREF {
            imbalanced();
        }
        let last = current == ONEREF;
        let (next, success) = if last {
            (DEAD, Ordering::AcqRel)
        } else {
            (current - 1, Ordering::Release)
        };
        self.0
            .compare_exchange_weak(current, next, success, Ordering::Relaxed)
            .map(|_| last)
    }

    /// Whether a finalizer has marked the entry dead.
    #[inline]
    pub(crate) fn is_dead(&self) -> bool {
        self.0.load(Ordering::Relaxed) >= RELEASED
    }

    /// Approximate number of references, for diagnostics only.
    pub(crate) fn approximate(&self) -> usize {
        let value = self.0.load(Ordering::Relaxed);
        if value <= MAXREF {
            usize::try_from(value).map_or(usize::MAX, |n| n.saturating_add(1))
        } else {
            0
        }
    }
}

#[cold]
fn overflow() -> ! {
    // Aborts rather than risking a use after free once the count overflows.
    std::process::abort()
}

#[cold]
fn imbalanced() -> ! {
    // Unreachable through the safe API, where every put matches one acquired
    // reference.
    std::process::abort()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn refmap_refcount_last_put_finalizes_exactly_once() {
        // Arrange.
        let count = RefCount::new();
        count.clone_ref();

        // Act.
        let first = count.put();
        let second = count.put();

        // Assert.
        assert!(!first);
        assert!(second);
        assert_eq!(
            count.0.load(Ordering::Relaxed),
            DEAD,
            "no zero-reference state"
        );
    }

    #[test]
    fn refmap_refcount_put_retries_after_a_racing_join() {
        // Arrange.
        // The word changes between the put's load and its exchange, as when
        // another thread gets a reference concurrently.
        let count = RefCount::new();
        let stale = count.0.load(Ordering::Relaxed);
        assert!(count.try_get());

        // Act.
        let raced = count.put_from(stale);
        let retried = count.put();

        // Assert.
        assert!(raced.is_err(), "the stale exchange must not finalize");
        assert!(!retried, "the joined reader still holds a reference");
        assert!(count.put(), "the reader's leave finalizes");
    }

    #[test]
    fn refmap_refcount_dead_entries_reject_joins_and_stay_dead() {
        // Arrange.
        let count = RefCount::new();
        assert!(count.put());

        // Act.
        let joins: Vec<bool> = (0..3).map(|_| count.try_get()).collect();

        // Assert.
        assert_eq!(joins, [false, false, false]);
        assert!(count.is_dead());
        assert_eq!(count.0.load(Ordering::Relaxed), DEAD);
    }

    #[test]
    fn refmap_refcount_approximate_tracks_live_references() {
        // Arrange.
        let count = RefCount::new();

        // Act.
        count.clone_ref();
        let two = count.approximate();
        assert!(!count.put());
        assert!(count.put());

        // Assert.
        assert_eq!(two, 2);
        assert_eq!(count.approximate(), 0);
    }
}
