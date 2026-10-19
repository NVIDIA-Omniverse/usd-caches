// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Tracks multi-part uploads currently in flight so that two concurrent
//! requests for the same key cannot both believe they "own" it.
//!
//! [`GuardMap`] is an insert-iff-vacant set of keys. [`MultiPartGuard`] is an
//! RAII handle: while it lives, the key is reserved; on drop, the key is
//! released and an optional completion callback fires.

use std::{
    hash::{BuildHasher, Hasher},
    sync::{atomic::AtomicBool, Arc, Weak},
};

use hashbrown::{DefaultHashBuilder, HashSet};
use ovdc_bytes::keys;
use parking_lot::Mutex;

/// Set of keys for which a multi-part operation is currently in flight.
///
/// Sharded internally so unrelated keys don't contend on a single lock.
pub(crate) struct GuardMap {
    shards: Box<[Mutex<HashSet<keys::Compound>>]>,
    hasher: DefaultHashBuilder,
}

const SHARD_COUNT: usize = 4;

impl Default for GuardMap {
    fn default() -> Self {
        let shards = (0..SHARD_COUNT)
            .map(|_| Mutex::new(HashSet::new()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            hasher: DefaultHashBuilder::default(),
        }
    }
}

impl GuardMap {
    fn shard(&self, key: &keys::Compound) -> &Mutex<HashSet<keys::Compound>> {
        let mut h = self.hasher.build_hasher();
        std::hash::Hash::hash(key, &mut h);
        &self.shards[(h.finish() as usize) & (SHARD_COUNT - 1)]
    }

    /// Insert `key` if it is not already present. Returns `true` on success.
    pub(crate) fn try_insert(&self, key: keys::Compound) -> bool {
        self.shard(&key).lock().insert(key)
    }

    /// Remove `key` from the set.
    pub(crate) fn remove(&self, key: &keys::Compound) {
        self.shard(key).lock().remove(key);
    }
}

/// RAII guard reserving a key for the duration of a multi-part operation.
pub struct MultiPartGuard {
    key: keys::Compound,
    map: Weak<GuardMap>,
    flag: Arc<AtomicBool>,
    func: Option<Box<dyn Fn(bool, keys::Compound) + Send + Sync + 'static>>,
}

impl MultiPartGuard {
    /// Mark this guard's operation as having completed successfully. The
    /// completion callback receives this flag on drop.
    pub fn assert_complete(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn new(
        key: keys::Compound,
        map: &Arc<GuardMap>,
        f: impl Fn(bool, keys::Compound) + Send + Sync + 'static,
    ) -> Option<Arc<Self>> {
        if !map.try_insert(key) {
            return None;
        }

        Some(Arc::new(Self {
            key,
            map: Arc::downgrade(map),
            flag: Default::default(),
            func: Some(Box::new(f)),
        }))
    }
}

impl Drop for MultiPartGuard {
    fn drop(&mut self) {
        let Some(map) = self.map.upgrade() else {
            return;
        };

        let Some(func) = self.func.take() else {
            return;
        };

        let is_flag_set = self.flag.load(std::sync::atomic::Ordering::Acquire);

        (func)(is_flag_set, self.key);

        map.remove(&self.key);
    }
}
