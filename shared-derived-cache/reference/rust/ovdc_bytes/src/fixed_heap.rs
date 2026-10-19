// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A fixed-capacity binary max-heap stored inline (no heap allocation).
//!
//! Provides the small subset of [`std::collections::BinaryHeap`] semantics that
//! callers in this workspace need: `push` (fallible, returns the rejected item
//! when full), `pop`, `peek`, and `len`. The top of the heap is always the
//! greatest element.

use std::mem::MaybeUninit;

/// A fixed-capacity binary max-heap of up to `N` elements of type `T`.
///
/// Storage is inline (`[MaybeUninit<T>; N]`); no allocation is performed.
/// Construction via [`FixedHeap::default`] yields an empty heap regardless of
/// whether `T: Default`.
pub struct FixedHeap<T, const N: usize> {
    data: [MaybeUninit<T>; N],
    len: usize,
}

impl<T, const N: usize> Default for FixedHeap<T, N> {
    fn default() -> Self {
        Self {
            data: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }
}

impl<T: Ord, const N: usize> FixedHeap<T, N> {
    /// Returns the number of elements currently in the heap.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns true if the heap contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns a reference to the greatest element, if any.
    #[inline]
    pub fn peek(&self) -> Option<&T> {
        if self.len == 0 {
            None
        } else {
            // SAFETY: index 0 is initialized while len > 0.
            Some(unsafe { self.data[0].assume_init_ref() })
        }
    }

    /// Pushes an element into the heap.
    ///
    /// Returns `Err(item)` if the heap is at capacity, returning ownership of
    /// the rejected item back to the caller (matching `heapless::BinaryHeap`).
    pub fn push(&mut self, item: T) -> Result<(), T> {
        if self.len == N {
            return Err(item);
        }
        let idx = self.len;
        self.data[idx].write(item);
        self.len += 1;
        self.sift_up(idx);
        Ok(())
    }

    /// Removes and returns the greatest element, if any.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: index 0 was initialized; we move it out and place the last
        // element (also initialized) into slot 0 before sifting down.
        let top = unsafe { self.data[0].assume_init_read() };
        if self.len > 0 {
            let last = unsafe { self.data[self.len].assume_init_read() };
            self.data[0].write(last);
            self.sift_down(0);
        }
        Some(top)
    }

    fn sift_up(&mut self, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            // SAFETY: indices 0..len are initialized.
            let cur = unsafe { self.data[i].assume_init_ref() };
            let par = unsafe { self.data[parent].assume_init_ref() };
            if cur > par {
                self.data.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    }

    fn sift_down(&mut self, mut i: usize) {
        loop {
            let left = 2 * i + 1;
            let right = 2 * i + 2;
            let mut largest = i;
            if left < self.len {
                // SAFETY: indices < len are initialized.
                let l = unsafe { self.data[left].assume_init_ref() };
                let cur = unsafe { self.data[largest].assume_init_ref() };
                if l > cur {
                    largest = left;
                }
            }
            if right < self.len {
                let r = unsafe { self.data[right].assume_init_ref() };
                let cur = unsafe { self.data[largest].assume_init_ref() };
                if r > cur {
                    largest = right;
                }
            }
            if largest == i {
                break;
            }
            self.data.swap(i, largest);
            i = largest;
        }
    }
}

impl<T, const N: usize> Drop for FixedHeap<T, N> {
    fn drop(&mut self) {
        // SAFETY: indices 0..len are initialized; drop them in place.
        for slot in &mut self.data[..self.len] {
            unsafe { slot.assume_init_drop() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_returns_max_first() {
        let mut h: FixedHeap<i32, 8> = FixedHeap::default();
        for v in [3, 1, 4, 1, 5, 9, 2, 6] {
            h.push(v).unwrap();
        }
        let mut out = vec![];
        while let Some(v) = h.pop() {
            out.push(v);
        }
        assert_eq!(out, vec![9, 6, 5, 4, 3, 2, 1, 1]);
    }

    #[test]
    fn push_full_returns_err_with_item() {
        let mut h: FixedHeap<i32, 2> = FixedHeap::default();
        h.push(1).unwrap();
        h.push(2).unwrap();
        assert_eq!(h.push(3), Err(3));
        assert_eq!(h.len(), 2);
        assert_eq!(h.peek(), Some(&2));
    }

    #[test]
    fn peek_and_len() {
        let mut h: FixedHeap<i32, 4> = FixedHeap::default();
        assert!(h.is_empty());
        h.push(7).unwrap();
        h.push(3).unwrap();
        assert_eq!(h.peek(), Some(&7));
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn drop_runs_on_remaining_items() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct D;
        impl Drop for D {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl PartialEq for D {
            fn eq(&self, _: &Self) -> bool {
                true
            }
        }
        impl Eq for D {}
        impl PartialOrd for D {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }
        impl Ord for D {
            fn cmp(&self, _: &Self) -> std::cmp::Ordering {
                std::cmp::Ordering::Equal
            }
        }

        DROPS.store(0, Ordering::SeqCst);
        {
            let mut h: FixedHeap<D, 4> = FixedHeap::default();
            let _ = h.push(D);
            let _ = h.push(D);
            let _ = h.push(D);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), 3);
    }
}
