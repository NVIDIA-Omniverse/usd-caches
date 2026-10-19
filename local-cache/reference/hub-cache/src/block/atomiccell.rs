// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::cell::Cell;
use std::fmt;

/// this is for items that can load/store atomically do not require the
/// strictness of thread consistency of atomics
pub struct AtomicCell<T>(Cell<T>);

impl<T: Copy + Clone> AtomicCell<T> {
    pub fn new(value: T) -> Self {
        Self(Cell::new(value))
    }

    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, value: T) {
        self.0.set(value)
    }
}

unsafe impl<T> Sync for AtomicCell<T> where T: Sync {}

impl<T> fmt::Debug for AtomicCell<T>
where
    T: Copy + Clone + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.get())
    }
}

impl<T> fmt::Display for AtomicCell<T>
where
    T: Copy + Clone + fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}
