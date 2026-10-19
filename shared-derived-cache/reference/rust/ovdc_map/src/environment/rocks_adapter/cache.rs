// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use ovdc_bytes::keys;
use ovdc_trace::{label, metrics::Measure as _};

use crate::{AnyEntry, Entry};

use super::metrics;

#[derive(Clone)]
struct RocksEntryWeight(AnyEntry);
impl ovdc_sharded_map::Weight for RocksEntryWeight {
    fn weight(&self) -> u64 {
        64 + self.0.value().len() + (ovdc_bytes::keys::COMPOUND_KEY_SIZE as u64)
    }
}

label! {
    static LABELS = ["level": 0; 1];
}

#[derive(Default, Clone)]
pub(crate) struct Cache {
    inner: Option<Arc<ovdc_sharded_map::ShardedMap<keys::Compound, RocksEntryWeight>>>,
}

impl Cache {
    pub fn new(max: u64) -> Self {
        Self {
            inner: (max > 0).then(|| Arc::new(ovdc_sharded_map::ShardedMap::new(max, None))),
        }
    }

    pub fn insert(&self, key: keys::Compound, value: AnyEntry) {
        let Some(cache) = &self.inner else {
            return;
        };

        tracing::trace!(?key, "insert cache");
        metrics::ROWCACHE_INSERT.measure(1, &[]);
        cache.insert(key, RocksEntryWeight(value));
    }

    pub fn invalidate(&self, key: &keys::Compound) {
        let Some(cache) = &self.inner else {
            return;
        };

        tracing::trace!(?key, "invalidate key");
        cache.remove(key);
    }

    pub fn get(&self, key: &keys::Compound) -> Option<AnyEntry> {
        // Nothing is counted when the cache is disabled (`cacheSize: 0`, a
        // supported setting). Counting the query here but returning before
        // `ANSWER`/`MISS` would leave `QUERY{level=0}` climbing against zero
        // hits, which reads as a 0% hit rate rather than as "not running" --
        // and 0% is a plausible enough number to be believed.
        let cache = self.inner.as_ref()?;
        metrics::QUERY.measure(1, &*LABELS);

        let value = cache.get(key).map(|y| y.0);
        let len = value.as_ref().map(|y| y.value().len()).unwrap_or(0);

        tracing::trace!(is_cached = { value.is_some() }, "get key");

        if value.is_some() {
            metrics::GET_BYTES.measure(len, &*LABELS);
            &*metrics::ANSWER
        } else {
            &*metrics::MISS
        }
        .measure(1, &*LABELS);

        value
    }
}
