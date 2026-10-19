// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use crate::Config;

use ovdc_map::{Database, Error as DbError};
use parking_lot::Mutex;

/// The time chosen for eviction.
///
/// Any access time equal to or less than this value is a candidate for eviction.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct TimeFloor(pub u64);

pub async fn first_pass<D: Database>(env: &D, config: Config) -> Result<TimeFloor, DbError> {
    // TODO we could remember a limited number of older keys and then try to remove those first.

    const ONE_DAY: u64 = 24 * 60 * 60;

    // Allows the histogram to be accurate for up to 24 hours in to the future. We do this so that
    // GC can take a long time if required.
    let ceiling = crate::now() + ONE_DAY;

    let histogram: hdrhistogram::Histogram<u32> = hdrhistogram::Histogram::new(5).unwrap();
    let histogram = Arc::new(Mutex::new(histogram));
    let hist = histogram.clone();
    let db = env.clone();

    tokio::task::spawn_blocking(move || {
        let cursor = db.open();

        cursor.retain(Box::new(move |entry| {
            let histogram = &hist;
            let offset_time = ceiling - entry.metadata().access_time_secs;
            ovdc_trace::drop!(
                histogram.lock().record(offset_time),
                "measuring access time failed"
            );

            true
        }))
    })
    .await
    .unwrap()?;

    let floor = histogram
        .lock()
        .value_at_quantile((100 - config.delete_keyspace_quantile) as f64 / 100.0);

    Ok(TimeFloor(ceiling - floor))
}

#[cfg(test)]
mod tests {
    use ovdc_bytes::keys::{self, Compound};
    use ovdc_map::{environment::Rocks, metadata::Metadata, Database};
    use rand::RngExt;

    use crate::tests::CFG;

    #[tokio::test]
    async fn test_first_pass_selects_good_floor() {
        let tempdir = tempfile::tempdir().unwrap();
        let cfg = ovdc_settings::load_document_with(CFG, |_| {});

        let map = Rocks::new(
            tempdir.path().to_string_lossy().as_ref(),
            32_000_000,
            Default::default(),
            &cfg,
        )
        .unwrap();

        let mut entries = vec![];

        // Pick some point in time between now and 1 year ago.
        let time = crate::now() - rand::rng().random_range(100..31_536_000);

        for i in 0..10_000u64 {
            entries.push((
                Compound::new(
                    Default::default(),
                    keys::Key::new(&i.to_ne_bytes()).unwrap(),
                ),
                Metadata {
                    ttl_ms: None,
                    access_time_secs: time + i,
                    created_secs: time + i,
                },
                &[0][..],
            ))
        }

        let cursor = map.open();
        for (key, md, bytes) in entries {
            cursor.put(key, md, bytes).unwrap();
        }

        let free_if_before = crate::first_pass::first_pass(
            &map,
            crate::Config {
                min_free_capacity: 80,
                delete_keyspace_quantile: 40,
                cooldown_secs: 10,
            },
        )
        .await
        .unwrap();

        let diff = free_if_before.0 - time;

        assert!(
            (3500..=4500).contains(&diff),
            "histogram was not accurate within +/- 5%"
        );
    }
}
