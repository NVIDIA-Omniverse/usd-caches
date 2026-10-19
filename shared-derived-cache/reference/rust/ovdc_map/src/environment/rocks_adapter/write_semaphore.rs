// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{atomic::AtomicU8, Arc};

const MAX_WRITERS: u8 = 1;

pub type WriterCount = Arc<AtomicU8>;

pub fn try_write(counter: &WriterCount) -> bool {
    if counter.load(std::sync::atomic::Ordering::Acquire) >= MAX_WRITERS {
        return false;
    }

    let load = counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    if load >= MAX_WRITERS {
        counter.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        return false;
    }
    true
}

pub fn decr_writer(counter: &WriterCount) {
    counter.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
}

#[test]
fn test_write_semaphore() {
    let counter = WriterCount::default();
    assert!(try_write(&counter));
    assert!(!try_write(&counter));
    decr_writer(&counter);
    assert!(try_write(&counter));
    assert!(!try_write(&counter));
}
