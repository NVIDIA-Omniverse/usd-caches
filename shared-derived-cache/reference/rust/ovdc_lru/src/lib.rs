// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A very simple collector that is used to coalease touched keys in memory and flush them to an
//! external system.
//!
//! This module provides a very basic collection mechanism. This is possible because RocksDB does
//! the sort of in-memory compaction we are looking for with merge operators.
//! Therefore, the custom logic to maintain a DB of touched keys outside of the DB is not
//! necessary.

use std::{
    collections::{BinaryHeap, VecDeque},
    future::Future,
    sync::{atomic::AtomicUsize, Arc},
    time::Duration,
};

use ovdc_bytes::keys;
use parking_lot::Mutex;
use tokio::{sync::Semaphore, task::JoinHandle};

/// A bounded MPMC queue used to circulate key-log segments between producers
/// (loggers) and the consumer (compactor).
///
/// The queue has fixed capacity; `push` is best-effort and silently drops the
/// item when full. This matches the contract previously provided by
/// `crossbeam_queue::ArrayQueue`.
struct ArrayQueue<T> {
    inner: Mutex<VecDeque<T>>,
    capacity: usize,
}

impl<T> ArrayQueue<T> {
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn push(&self, item: T) -> Result<(), T> {
        let mut q = self.inner.lock();
        if q.len() >= self.capacity {
            return Err(item);
        }
        q.push_back(item);
        Ok(())
    }

    fn pop(&self) -> Option<T> {
        self.inner.lock().pop_front()
    }
}

/// A collection of touched keys.
pub type Keys = BinaryHeap<keys::Compound>;

/// Used when the in-memory log of keys needs to be periodically compacted or flushed.
pub trait Compaction: Send + Clone + 'static {
    /// Compact/flush/store the given keys as having been recently touched.
    /// The LRU system will no longer be tracking these keys once this event is received.
    fn compact(&self, keys: Keys) -> impl Future<Output = ()> + Send;
}

/// The most basic function of the LRU is to log which keys are touched. This trait makes that
/// operation very simple.
///
/// In most cases this is what a consumer of this crate will want to use when recording touched
/// keys.
pub trait LogUsedKeys {
    /// Records the given keys as having been used.
    fn log(&self, keys: &[keys::Compound]);
}

/// Tracks used keys in memory prior to their use being permanently recorded in the database.
pub struct KeyLog {
    next: AtomicUsize,
    rope: Vec<Arc<Rope>>,
    stop: Arc<Stop>,
    tasks: Vec<JoinHandle<()>>,
}

impl KeyLog {
    /// Creates a new key logging system.
    ///
    /// Each shard gets a new background compaction task. Each shard has has multiple vector
    /// segments that can be filled.
    ///
    /// Each shard will periodically flush into [Compaction].
    pub fn new(shards: usize, segments: usize, period: Duration, into: impl Compaction) -> Self {
        let stop = Arc::new(Stop {
            signal: Semaphore::new(0),
            qualified: Semaphore::new(0),
        });
        let mut tasks = vec![];
        Self {
            next: Default::default(),
            stop: stop.clone(),
            rope: (0..shards)
                .map(|_| {
                    let rope = Arc::new(Rope {
                        segments: ArrayQueue::new(segments),
                    });
                    for _ in 0..segments {
                        let _ = rope.segments.push(Default::default());
                    }

                    let rope0 = rope.clone();
                    let into = into.clone();
                    let stop = stop.clone();
                    let task = tokio::spawn(async move {
                        compaction(rope0, stop, period, into).await;
                    });

                    tasks.push(task);

                    rope
                })
                .collect(),
            tasks,
        }
    }

    /// Stops LRU tracking and compacts all in memory events.
    pub async fn stop(&self, timeout: Duration) {
        self.stop.close();
        self.stop.wait(self.rope.len() as u32, timeout).await;

        // We waited enough, abort the tasks.
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Drop for KeyLog {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn compaction(rope: Arc<Rope>, stop: Arc<Stop>, period: Duration, into: impl Compaction) {
    loop {
        let mut heap = BinaryHeap::new();

        for _ in 0..rope.segments.capacity() {
            let Some(mut next) = rope.segments.pop() else {
                continue;
            };

            heap.extend(next.drain());
            heap.shrink_to(2048);
            let _ = rope.segments.push(next);
        }

        into.compact(heap).await;

        // Stop was received, do not continue.
        if stop.is_closed() {
            break;
        }

        tokio::select! {
            // If the stop signal is received, try flush first.
            _ = stop.wait_closed() => {},
            _ = tokio::time::sleep(period) => {},
        }
    }

    stop.done();
}

struct Rope {
    segments: ArrayQueue<Keys>,
}

struct Stop {
    signal: Semaphore,
    qualified: Semaphore,
}

impl Drop for Stop {
    fn drop(&mut self) {
        self.signal.close();
    }
}

impl Stop {
    fn is_closed(&self) -> bool {
        self.signal.is_closed()
    }

    fn done(&self) {
        self.qualified.add_permits(1);
    }

    fn close(&self) {
        self.signal.close();
    }

    async fn wait_closed(&self) {
        let _ = self.signal.acquire().await;
    }

    async fn wait(&self, count: u32, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.qualified.acquire_many(count)).await;
    }
}

impl LogUsedKeys for Arc<KeyLog> {
    fn log(&self, keys: &[keys::Compound]) {
        loop {
            // Round robin the segments
            let next =
                self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.rope.len();
            let rope = &self.rope[next];

            // Find one with an open segment
            let Some(mut segment) = rope.segments.pop() else {
                continue;
            };

            // Add the keys and then put the segment back.
            segment.extend(keys.iter());
            let _ = rope.segments.push(segment);

            // Important!
            break;
        }
    }
}
