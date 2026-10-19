// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Synchronization primitives, replaced by Loom's model-checked versions under
//! `--cfg loom` so the same protocol code runs in both worlds.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};
#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering, fence,
};

/// Keeps a hot value on its own cache line pair. 128 bytes covers the
/// adjacent-line prefetcher on x86-64 and is the line size of some ARM cores
/// (circa 2026).
#[repr(align(128))]
#[derive(Debug, Default)]
pub(crate) struct CachePadded<T>(pub(crate) T);

impl<T> std::ops::Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for CachePadded<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// Non-poisoning mutex.
///
/// Production uses `parking_lot` because its unlock performs no memory access
/// to the lock after a waiter can acquire it. The map's owner and the last
/// finalizer of a dropped map rely on that, because either may free the
/// shards while another thread is still returning from an unlock. Miri runs a
/// spin lock with the same property, because `parking_lot`'s futex parker
/// passes an atomic reference to a variadic `syscall` that Miri rejects.
pub(crate) struct Mutex<T> {
    #[cfg(not(any(loom, miri)))]
    inner: parking_lot::Mutex<T>,
    #[cfg(loom)]
    inner: loom::sync::Mutex<T>,
    #[cfg(all(miri, not(loom)))]
    inner: spin::Mutex<T>,
}

#[cfg(not(any(loom, miri)))]
pub(crate) type MutexGuard<'a, T> = parking_lot::MutexGuard<'a, T>;
#[cfg(loom)]
pub(crate) type MutexGuard<'a, T> = loom::sync::MutexGuard<'a, T>;
#[cfg(all(miri, not(loom)))]
pub(crate) type MutexGuard<'a, T> = spin::MutexGuard<'a, T>;

impl<T> Mutex<T> {
    #[cfg(not(any(loom, miri)))]
    pub(crate) const fn new(value: T) -> Self {
        Self {
            inner: parking_lot::Mutex::new(value),
        }
    }

    #[cfg(all(miri, not(loom)))]
    pub(crate) const fn new(value: T) -> Self {
        Self {
            inner: spin::Mutex::new(value),
        }
    }

    #[cfg(loom)]
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: loom::sync::Mutex::new(value),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        #[cfg(not(loom))]
        {
            self.inner.lock()
        }
        #[cfg(loom)]
        {
            self.inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
        }
    }

    /// Unlocks, handing the lock directly to a waiter if one is parked, so a
    /// thread that relocks in a loop cannot starve it.
    pub(crate) fn unlock_fair(guard: MutexGuard<'_, T>) {
        #[cfg(not(any(loom, miri)))]
        {
            parking_lot::MutexGuard::unlock_fair(guard);
        }
        #[cfg(any(loom, miri))]
        {
            drop(guard);
        }
    }

    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        #[cfg(not(loom))]
        {
            self.inner.try_lock()
        }
        #[cfg(loom)]
        {
            match self.inner.try_lock() {
                Ok(guard) => Some(guard),
                Err(std::sync::TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            }
        }
    }
}

/// Spin lock for Miri runs. Unlocking is one release store, the last access to
/// the lock, matching the property the production lock provides.
#[cfg(all(miri, not(loom)))]
mod spin {
    use std::cell::UnsafeCell;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) struct Mutex<T> {
        locked: AtomicBool,
        value: UnsafeCell<T>,
    }

    // The lock serializes every access to the value, so sharing the mutex only
    // hands the value from thread to thread, which `T: Send` permits.
    unsafe impl<T: Send> Sync for Mutex<T> {}

    pub(crate) struct MutexGuard<'a, T> {
        lock: &'a Mutex<T>,
    }

    impl<T> Mutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self {
                locked: AtomicBool::new(false),
                value: UnsafeCell::new(value),
            }
        }

        pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
            loop {
                if let Some(guard) = self.try_lock() {
                    return guard;
                }
                std::thread::yield_now();
            }
        }

        pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
            self.locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .ok()
                .map(|_| MutexGuard { lock: self })
        }
    }

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // Only the lock holder can reach the value, and this guard
            // holds the lock.
            unsafe { &*self.lock.value.get() }
        }
    }

    impl<T> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // Only the lock holder can reach the value, and this guard
            // holds the lock.
            unsafe { &mut *self.lock.value.get() }
        }
    }

    impl<T> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            self.lock.locked.store(false, Ordering::Release);
        }
    }
}

/// `UnsafeCell` whose accesses Loom tracks for causality violations.
pub(crate) struct UnsafeCell<T> {
    #[cfg(not(loom))]
    inner: std::cell::UnsafeCell<T>,
    #[cfg(loom)]
    inner: loom::cell::UnsafeCell<T>,
}

impl<T> UnsafeCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            #[cfg(not(loom))]
            inner: std::cell::UnsafeCell::new(value),
            #[cfg(loom)]
            inner: loom::cell::UnsafeCell::new(value),
        }
    }

    /// Runs `f` with a shared pointer to the contents. The caller must ensure
    /// that no mutable access runs concurrently.
    pub(crate) unsafe fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
        #[cfg(not(loom))]
        {
            f(self.inner.get())
        }
        #[cfg(loom)]
        {
            self.inner.with(f)
        }
    }

    /// Runs `f` with an exclusive pointer to the contents. The caller must
    /// ensure that no other access runs concurrently.
    pub(crate) unsafe fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        #[cfg(not(loom))]
        {
            f(self.inner.get())
        }
        #[cfg(loom)]
        {
            self.inner.with_mut(f)
        }
    }
}
