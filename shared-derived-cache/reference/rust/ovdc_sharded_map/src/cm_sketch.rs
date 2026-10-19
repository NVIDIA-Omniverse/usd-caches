// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hash::{Hash, Hasher};

#[inline(always)]
pub fn hasher_0() -> seahash::SeaHasher {
    seahash::SeaHasher::with_seeds(
        0x16f11fe89b0d677c,
        0xb480a793d8e6c86c,
        0x6fe2e5aaf078ebc9,
        0x14f994a4c5259381,
    )
}
#[inline(always)]
fn hasher_1() -> seahash::SeaHasher {
    seahash::SeaHasher::with_seeds(
        0xf835b9fd723bb8fb,
        0xba84f2356a08f586,
        0x4bad1a028fc0a869,
        0x53e436c93c72447b,
    )
}
#[inline(always)]
fn hasher_2() -> seahash::SeaHasher {
    seahash::SeaHasher::with_seeds(
        0xa9d34b876f40acc9,
        0x15d780348a6f06ff,
        0x11fac68f2a22105d,
        0xbab781c37b8b4e6c,
    )
}
#[inline(always)]
fn hasher_3() -> seahash::SeaHasher {
    seahash::SeaHasher::with_seeds(
        0xd348499dfefbe912,
        0x867b2fdd7588802e,
        0xf2df53fe1527ff35,
        0xd2710bbb094c5193,
    )
}

fn hashes<T: Hash + ?Sized>(value: &T) -> (usize, usize, usize, usize) {
    let mut hasher = hasher_0();
    value.hash(&mut hasher);
    let i_0 = hasher.finish() as usize % WIDTH;

    let mut hasher = hasher_1();
    value.hash(&mut hasher);
    let i_1 = hasher.finish() as usize % WIDTH;

    let mut hasher = hasher_2();
    value.hash(&mut hasher);
    let i_2 = hasher.finish() as usize % WIDTH;

    let mut hasher = hasher_3();
    value.hash(&mut hasher);
    let i_3 = hasher.finish() as usize % WIDTH;

    (i_0, i_1, i_2, i_3)
}

/// Error rate >= 0.001
const WIDTH: usize = 4096;

/// Probability of error >= 93%
const DEPTH: usize = 4;

const BITS_PER_COUNTER: usize = 8;
const COUNTER_PER_U64: usize = 64 / BITS_PER_COUNTER;

const TOTAL_WIDTH_U64: usize = (WIDTH * DEPTH * BITS_PER_COUNTER) / 64;

const MASK: u64 = !(!0 << BITS_PER_COUNTER);

pub struct Sketch {
    table: Box<[u64]>,
    observed: u64,
}

impl Sketch {
    pub fn new() -> Self {
        Self {
            table: vec![0u64; TOTAL_WIDTH_U64].into_boxed_slice(),
            observed: 0,
        }
    }

    pub fn incr<T: Hash + ?Sized>(&mut self, value: &T) {
        if self.observed == 1_000_000 {
            self.decay();
        }
        let (i_0, i_1, i_2, i_3) = hashes(value);

        self.incr_at(1, i_0);
        self.incr_at(2, i_1);
        self.incr_at(3, i_2);
        self.incr_at(4, i_3);
    }

    pub fn estimate(&self, value: &impl std::hash::Hash) -> u64 {
        let (i_0, i_1, i_2, i_3) = hashes(value);

        let i_0 = self.get_at(1, i_0);
        let i_1 = self.get_at(2, i_1);
        let i_2 = self.get_at(3, i_2);
        let i_3 = self.get_at(4, i_3);

        i_0.min(i_1).min(i_2).min(i_3)
    }

    #[inline]
    fn incr_at(&mut self, depth: usize, width: usize) {
        let location = depth * width;

        let index = location / COUNTER_PER_U64;
        let counter_in_index = location % COUNTER_PER_U64;

        incr_counter_at(&mut self.table[index], counter_in_index);
    }

    #[inline]
    fn get_at(&self, depth: usize, width: usize) -> u64 {
        let location = depth * width;

        let index = location / COUNTER_PER_U64;
        let counter_in_index = location % COUNTER_PER_U64;

        counter_at(self.table[index], counter_in_index)
    }

    fn decay(&mut self) {
        self.observed = 0;
        for cell in self.table.iter_mut() {
            *cell = (*cell & R_MASK) >> 1
        }
    }
}

fn incr_counter_at(counter_rep: &mut u64, counter: usize) {
    let counter = counter + 1;

    let shift = (COUNTER_PER_U64 - counter) * BITS_PER_COUNTER;
    let mask = MASK << shift;

    // do not overflow
    if *counter_rep & mask == mask {
        return;
    }

    *counter_rep += 1u64 << shift;
}

fn counter_at(counter_rep: u64, counter: usize) -> u64 {
    const OFFSET: usize = BITS_PER_COUNTER * (COUNTER_PER_U64 - 1);
    let shift = OFFSET - (counter * BITS_PER_COUNTER);

    (counter_rep >> shift) & MASK
}

#[test]
fn test_no_counter_overflow() {
    let mut value = 0u64;

    for _ in 0..u16::MAX {
        incr_counter_at(&mut value, 0);
    }

    assert_eq!(counter_at(value, 0), 255);
}

#[test]
fn test_incrememnt_counter_at() {
    let mut value = 0u64;
    incr_counter_at(&mut value, 7);

    assert_eq!(value.leading_zeros(), 63);
    assert_eq!(counter_at(value, 7), 1);

    incr_counter_at(&mut value, 0);

    assert_eq!(value.leading_zeros(), 7);
    assert_eq!(counter_at(value, 0), 1);
}

#[test]
fn test_incr_and_estimate() {
    let mut sketch = Sketch::new();

    sketch.incr(&"hello");
    sketch.incr(&"hello");
    sketch.incr(&"hello");

    assert_eq!(sketch.estimate(&"hello"), 3);
}

const R_MASK: u64 = {
    let mut val = 1u64;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    val <<= BITS_PER_COUNTER;
    val |= 1;
    !val
};
