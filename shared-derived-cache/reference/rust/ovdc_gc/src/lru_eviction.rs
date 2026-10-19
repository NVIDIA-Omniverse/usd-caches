// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_map::{Database, Error as DbError};

use crate::first_pass::TimeFloor;

pub async fn evict<D: Database>(
    db: &D,
    time_floor: TimeFloor,
    _config: crate::Config,
) -> Result<(), DbError> {
    let env = db.clone();
    ovdc_trace::drop!(
        tokio::task::spawn_blocking(move || {
            let cursor = env.open();
            cursor.retain(Box::new(move |entry| {
                // Remove entries that are to old
                entry.metadata().access_time_secs > time_floor.0
            }))
        })
        .await
        .unwrap(),
        "retain filter/compaction failed after deletion"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use ovdc_bytes::keys::{self, Compound};
    use ovdc_map::{environment::Rocks, metadata::Metadata, Database};
    use rand::RngExt;

    use crate::tests::CFG;

    #[tokio::test]
    async fn test_evict() {
        let tempdir = tempfile::tempdir().unwrap();
        let cfg = ovdc_settings::load_document_with(CFG, |_| {});

        let map = Rocks::new(
            tempdir.path().to_string_lossy().as_ref(),
            500_000,
            Default::default(),
            &cfg,
        )
        .unwrap();

        let mut entries = vec![];

        let time = crate::now();

        // Pick some point in time between now and 1 year ago.
        let p40_target = time - rand::rng().random_range(500..52_560);

        for i in 0..1000u64 {
            entries.push((
                Compound::new(
                    Default::default(),
                    keys::Key::new(&i.to_ne_bytes()).unwrap(),
                ),
                Metadata {
                    ttl_ms: None,
                    access_time_secs: p40_target,
                    created_secs: p40_target,
                },
                &[0u8][..],
            ))
        }

        let time = p40_target + 1;
        entries.push((
            Compound::new(
                Default::default(),
                keys::Key::new(&time.to_ne_bytes()).unwrap(),
            ),
            Metadata {
                ttl_ms: None,
                access_time_secs: time,
                created_secs: time,
            },
            &[0u8][..],
        ));

        let cursor = map.open();
        for (key, md, bytes) in entries {
            cursor.put(key, md, bytes).unwrap();
        }

        super::evict(
            &map,
            crate::first_pass::TimeFloor(p40_target),
            crate::Config {
                min_free_capacity: 100,
                delete_keyspace_quantile: 100,
                cooldown_secs: 10,
            },
        )
        .await
        .unwrap();

        let len = Arc::new(AtomicUsize::default());
        let l = len.clone();
        cursor
            .retain(Box::new(move |_| {
                l.fetch_add(1, Ordering::Relaxed);
                true
            }))
            .unwrap();

        let len = len.load(Ordering::Acquire);

        assert_eq!(
            len, 1,
            "scan pass must have evicted all data except for the key(and metadata) for the one key"
        );
    }
}
