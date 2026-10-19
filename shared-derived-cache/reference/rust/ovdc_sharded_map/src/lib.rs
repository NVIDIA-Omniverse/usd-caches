// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use hashbrown::hash_table::Entry;
use ovdc_bytes::fixed_heap::FixedHeap;
use parking_lot::RwLockWriteGuard;
use std::{
    hash::{Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
    thread,
};

mod cm_sketch;

pub trait Weight {
    fn weight(&self) -> u64;
}

struct EvictionSample<K> {
    key: K,
    score: u64,
}

impl<K: Eq> Ord for EvictionSample<K> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.cmp(&other.score)
    }
}

impl<K: Eq> Eq for EvictionSample<K> {}
impl<K: Eq> PartialOrd for EvictionSample<K> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<K: Eq> PartialEq for EvictionSample<K> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

struct Shard<K, V> {
    map: hashbrown::HashTable<(K, V)>,
    sketch: parking_lot::RwLock<cm_sketch::Sketch>,
}

#[repr(align(64))]
struct CacheAlignedShard<K, V> {
    shard: parking_lot::RwLock<Shard<K, V>>,
}

pub struct ShardedMap<K, V> {
    shards: Vec<CacheAlignedShard<K, V>>,
    entries: AtomicU64,
    size: AtomicU64,
    evictions: AtomicU64,
    max_size: u64,
    eviction_lock: parking_lot::Mutex<()>,
}

impl<K, V> ShardedMap<K, V>
where
    K: Clone + std::hash::Hash + Eq,
    V: Weight + Clone,
{
    pub fn new(max_size: u64, num_shards: Option<u32>) -> Self {
        let num_shards = match num_shards {
            Some(n) => n as usize,
            None => thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .next_power_of_two(),
        };
        assert!(
            num_shards.is_power_of_two(),
            "The number of shards must be power of two to avoid using `%` operator."
        );

        Self {
            max_size,
            eviction_lock: Default::default(),
            entries: Default::default(),
            evictions: Default::default(),
            size: Default::default(),
            shards: (0..num_shards)
                .map(|_| CacheAlignedShard {
                    shard: parking_lot::RwLock::new(Shard {
                        map: hashbrown::HashTable::new(),
                        sketch: cm_sketch::Sketch::new().into(),
                    }),
                })
                .collect(),
        }
    }

    pub fn evict(&self) {
        let Some(_guard) = self.eviction_lock.try_lock() else {
            return;
        };

        // Instead of a random number generator we will use a hasher and a counter to pick
        // uniformly distributed offsets in the hashmaps.
        let seed = seed(&self.evictions);

        let len = self.entries.load(std::sync::atomic::Ordering::Acquire);

        let remove_count = ((len as f64) * 0.3).ceil() as usize;

        let mut heap: FixedHeap<EvictionSample<K>, 256> = FixedHeap::default();
        let mut shard_heap: FixedHeap<EvictionSample<K>, 32> = FixedHeap::default();

        for shard in &self.shards {
            {
                let r_shard = shard.shard.read();
                let r_sketch = r_shard.sketch.read();

                let loop_len = 1 + (r_shard.map.len() > 16) as u8;

                // Random sampling of keys from the shard.
                for i in 0..loop_len {
                    sample_map_entries(seed + i as u64, &r_shard.map, 16, |key| {
                        let score = r_sketch.estimate(key);

                        replace(
                            &mut shard_heap,
                            EvictionSample {
                                key: key.clone(),
                                score,
                            },
                        );
                    });
                }
            }

            // Take only the best.
            while let Some(pop) = shard_heap.pop() {
                replace(&mut heap, pop);
            }
        }

        if heap.len() > remove_count {
            // If the number of items to remove is smaller than the total number of samples, then
            // skip the difference.
            let skip = heap.len() - remove_count;
            for _ in 0..skip {
                let _ = heap.pop();
            }
        }

        // Remove as many keys as is required.
        for _ in 0..remove_count {
            let Some(pop) = heap.pop() else {
                break;
            };

            self.try_remove(&pop.key);
        }
    }

    fn try_remove(&self, key: &K) -> Option<V> {
        let key_hash = hash_key(&key);
        let shard_index = shard_by_key_hash(key_hash, self.shards.len());

        let shard = self.shards[shard_index].shard.try_write()?;
        self.remove_from_shard(key_hash, key, shard)
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let key_hash = hash_key(&key);
        let shard_index = shard_by_key_hash(key_hash, self.shards.len());

        let shard = self.shards[shard_index].shard.write();
        self.remove_from_shard(key_hash, key, shard)
    }

    fn remove_from_shard(
        &self,
        key_hash: u64,
        key: &K,
        mut shard: RwLockWriteGuard<'_, Shard<K, V>>,
    ) -> Option<V> {
        let removed = shard
            .map
            .find_entry(key_hash, |(fkey, _value)| fkey == key)
            .map(|y| y.remove())
            .ok()
            .map(|y| y.0 .1);

        drop(shard);

        if let Some(value) = &removed {
            self.entries
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            self.size
                .fetch_sub(value.weight(), std::sync::atomic::Ordering::AcqRel);
        }

        removed
    }

    pub fn insert(&self, key: K, value: V) {
        let key_hash = hash_key(&key);
        let shard_index = shard_by_key_hash(key_hash, self.shards.len());
        let mut inserted = false;
        let weight = value.weight();

        let mut shard = self.shards[shard_index].shard.write();
        match shard
            .map
            .entry(key_hash, |y| y.0 == key, |y| hash_key(&y.0))
        {
            Entry::Vacant(v) => {
                v.insert((key, value));
                inserted = true;
            }
            Entry::Occupied(o) => {
                *o.into_mut() = (key, value);
            }
        }
        drop(shard);

        if inserted {
            self.entries
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let size = self
                .size
                .fetch_add(weight, std::sync::atomic::Ordering::AcqRel);

            if size + weight > self.max_size {
                // TODO(danielvl): Offload this operation to a dedicated thread.
                // It is unfair for one caller to handle the eviction process alone.
                self.evict();
            }
        }
    }

    pub fn get<T>(&self, key: &T) -> Option<V>
    where
        T: Hash + hashbrown::Equivalent<K> + ?Sized,
    {
        let key_hash = hash_key(&key);
        let shard_index = shard_by_key_hash(key_hash, self.shards.len());
        let shard = self.shards[shard_index].shard.read();

        let entry = shard
            .map
            .find(key_hash, |y| key.equivalent(&y.0))
            .map(|y| y.1.clone());
        if entry.is_some() {
            if let Some(mut sketch) = shard.sketch.try_write() {
                sketch.incr(key);
            }
        }
        entry
    }

    pub fn contains_key(&self, key: &K) -> bool {
        let key_hash = hash_key(key);
        let shard_index = shard_by_key_hash(key_hash, self.shards.len());
        let shard = self.shards[shard_index].shard.read();
        shard.map.find(key_hash, |y| &y.0 == key).is_some()
    }

    pub fn size(&self) -> u64 {
        self.size.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn hash_key<T: Hash>(key: &T) -> u64 {
    let mut hasher = cm_sketch::hasher_0();
    key.hash(&mut hasher);
    hasher.finish()
}

fn shard_by_key_hash(hash: u64, num_shards: usize) -> usize {
    assert!(num_shards.is_power_of_two());
    (hash as usize) & (num_shards - 1)
}

#[inline]
fn seed(counter: &AtomicU64) -> u64 {
    let count = counter.fetch_add(1, Ordering::Relaxed);
    let mut hash = cm_sketch::hasher_0();
    hash.write_u64(count);
    hash.finish()
}

#[inline]
fn replace<K, const N: usize>(heap: &mut FixedHeap<EvictionSample<K>, N>, next: EvictionSample<K>)
where
    K: Clone + std::hash::Hash + Eq,
{
    let Err(next) = heap.push(next) else {
        return;
    };

    {
        let peek = heap.peek().unwrap();
        if peek.score < next.score {
            return;
        }
    }

    heap.pop();
    let _ = heap.push(next);
}

fn sample_map_entries<K, V>(
    seed: u64,
    map: &hashbrown::HashTable<(K, V)>,
    max: usize,
    mut f: impl FnMut(&K),
) where
    K: Clone + std::hash::Hash + Eq,
    V: Clone,
{
    let len = map.num_buckets();

    // Start in a random place inside of the hashmap. This prevents eviction of only the first few
    // elements.
    let mut bucket_i = match len <= max {
        false => seed as usize % len,
        _ => 0,
    };

    // Find "filled" buckets and check if they should be removed.
    for _ in 0..(max.min(len)) {
        bucket_i = (bucket_i + 1) % len;

        let Some((key, _value)) = map.get_bucket(bucket_i) else {
            continue;
        };

        f(key)
    }
}

#[cfg(test)]
mod tests {
    use super::{ShardedMap, Weight};

    #[derive(Clone, Copy)]
    struct V;
    impl Weight for V {
        fn weight(&self) -> u64 {
            1
        }
    }

    #[derive(Clone, Copy)]
    struct L;
    impl Weight for L {
        fn weight(&self) -> u64 {
            1000
        }
    }

    #[test]
    fn test_evict_tends_less_frequent() {
        let map: ShardedMap<u32, V> = ShardedMap::new(1000, Some(64));

        for i in 0..1000 {
            map.insert(i, V);
        }

        for i in (0..1000).step_by(2) {
            map.get(&i);
        }

        map.insert(99999, V);

        let mut odd = 0u32;
        let mut even = 0u32;
        for i in 0..1000 {
            if map.get(&i).is_some() {
                if i % 2 == 0 {
                    even += 1;
                } else {
                    odd += 1;
                }
            }
        }

        let p_diff = even.abs_diff(odd) as f64 / ((odd + even) as f64 / 2.0);

        println!("{odd} {even}");
        assert!(p_diff >= 0.29)
    }

    #[test]
    fn test_small_map_evict() {
        let map: ShardedMap<u32, L> = ShardedMap::new(1000, Some(64));

        for i in 0..1000 {
            map.insert(i, L);
            assert_eq!(1, map.entries.load(std::sync::atomic::Ordering::Acquire));
        }
    }
}
