// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_bytes::keys;
use ovdc_trace::metrics::{timer, Measure};
use rocksdb::AsColumnFamilyRef;

use crate::metadata::Metadata;

use super::{
    merge::Merge,
    metrics::{self},
    value::{self, StoredKind},
    RocksDb, WRITE_NO_SLOWDOWN_OR_WAL,
};

pub fn key_range(key: keys::Compound) -> (keys::Compound, keys::Compound) {
    (key_at(key, 0), key_at(key, 255))
}

pub fn key_at(mut key: keys::Compound, i: u8) -> keys::Compound {
    let mut reserved = key.reserved();
    reserved[0] = i;
    key.set_reserved(reserved);
    key
}

pub fn delete_blob_values(
    db: &RocksDb,
    cf: &impl AsColumnFamilyRef,
    key: keys::Compound,
) -> Result<(), rocksdb::Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_DELETE.clone());
    let (from, to) = key_range(key);
    // Reports back-pressure rather than blocking: this runs from GC and from
    // abandoned-upload cleanup, neither of which can afford to park a thread
    // until a stall clears.
    db.delete_range_cf_opt(cf, from, to, &WRITE_NO_SLOWDOWN_OR_WAL)?;

    Ok(())
}

pub fn put_blob_index(
    db: &RocksDb,
    cf: &impl AsColumnFamilyRef,
    key: keys::Compound,

    metadata: Metadata,
    total_size: u64,

    random: u128,
) -> Result<(), rocksdb::Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_WRITE.clone());
    // Set the new initial value
    let mut metadata = metadata;
    metadata.ttl_ms = Some(14_400_000); // 4 hours

    let value = value::format(
        value::FLAG_BLOB,
        &StoredKind::PartPreamble {
            key,
            metadata,
            size: total_size,
            parts: 0,
            random,
        },
    );

    db.put_cf_opt(cf, key, &value, &super::WRITE_NO_SLOWDOWN_OR_WAL)?;
    metrics::PUT.measure(1, &[]);
    metrics::PUT_BYTE_HIST.measure(value.len() as u64, &[]);

    Ok(())
}

pub fn put_blob_value_segment(
    db: &RocksDb,
    cf: &impl AsColumnFamilyRef,
    mut key: keys::Compound,
    value: &[u8],
    part: u8,
    random: u128,
) -> Result<(), rocksdb::Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_WRITE.clone());
    let mut reserved = key.reserved();
    reserved[0] = part;
    key.set_reserved(reserved);

    let value = value::format(
        0,
        &StoredKind::Part {
            key,
            random,
            value: value.into(),
        },
    );

    db.put_cf_opt(cf, key, &value, &WRITE_NO_SLOWDOWN_OR_WAL)?;
    metrics::PUT.measure(1, &[]);
    metrics::PUT_BYTE_HIST.measure(value.len() as u64, &[]);

    Ok(())
}

pub fn set_filled(
    db: &RocksDb,
    cf: &impl AsColumnFamilyRef,
    mut key: keys::Compound,
    metadata: Metadata,
) -> Result<(), rocksdb::Error> {
    let _t = timer(&metrics::IO_TIME, metrics::LABEL_IO_MERGE.clone());
    let mut reserved = key.reserved();
    reserved[0] = 0;
    key.set_reserved(reserved);

    db.merge_cf_opt(
        cf,
        key,
        Merge::OrFlagAndUpdateMetadta(
            value::FLAG_BLOB_FILLED,
            crate::Update {
                // Don't update the access time.
                access_time: crate::Change::No,
                // Remove the default TTL time.
                ttl_ms: crate::Change::Set(metadata.ttl_ms),
            },
        )
        .to_vec(),
        &super::WRITE_NO_SLOWDOWN_OR_WAL,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use ovdc_bytes::keys;

    use crate::{
        environment::rocks_adapter::{
            adapt,
            value::{self, Invalid},
        },
        metadata::Metadata,
        tests::temp_db,
    };

    #[test]
    fn test_blob_unfilled_set() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));

        super::put_blob_index(&db.inner, &cf, key, Metadata::default(), 8, 123)
            .expect("unable to put new value in map");

        let value =
            adapt::get_key_as_entry(&db.inner, &db.cache, &cf, &key, adapt::DeepestLevel::Tier2);
        assert!(
            value.is_err(),
            "the blob is not filled so no value can be returned"
        );

        let value = adapt::get_key(&db.inner, &cf, &key, adapt::DeepestLevel::Tier2)
            .expect("key must exist");
        let (kind, invalid) = value::entry(key, &value);

        assert!(invalid.contains(Invalid::UNFILLED_BLOB));

        let forced_entry = value::stored_kind_into_entry(key, kind);
        assert!(forced_entry.metadata.ttl_ms.is_some());
        assert!(forced_entry.value.is_blob());
        assert!(forced_entry.value.is_empty());
    }

    #[test]
    fn test_blob_is_filled() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let metadata = Metadata::default();

        super::put_blob_index(&db.inner, &cf, key, metadata, 8, 123)
            .expect("unable to put new value in map");

        super::set_filled(&db.inner, &cf, key, metadata).unwrap();

        let value =
            adapt::get_key_as_entry(&db.inner, &db.cache, &cf, &key, adapt::DeepestLevel::Tier2);
        assert!(value.is_ok(), "the blob is filled");
    }

    #[test]
    fn test_blob_part_put() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let metadata = Metadata::default();

        super::put_blob_index(&db.inner, &cf, key, metadata, 8, 123)
            .expect("unable to put new value in map");

        super::put_blob_value_segment(&db.inner, &cf, key, &[1, 2], 1, 123).unwrap();
        super::put_blob_value_segment(&db.inner, &cf, key, &[3, 4], 2, 123).unwrap();

        super::set_filled(&db.inner, &cf, key, metadata).unwrap();

        let value =
            adapt::get_key_as_entry(&db.inner, &db.cache, &cf, &key, adapt::DeepestLevel::Tier2);
        assert!(value.is_ok(), "the blob is filled");

        let value_1 = adapt::get_key_as_entry(
            &db.inner,
            &db.cache,
            &cf,
            &super::key_at(key, 1),
            adapt::DeepestLevel::Tier2,
        );
        assert!(value_1.is_ok(), "blob part not found");
        let value_2 = adapt::get_key_as_entry(
            &db.inner,
            &db.cache,
            &cf,
            &super::key_at(key, 2),
            adapt::DeepestLevel::Tier2,
        );
        assert!(value_2.is_ok(), "blob part not found");
    }

    #[test]
    fn test_blobs_removed() {
        let (_dir, db) = temp_db();
        let cf = db.inner.cf_handle("default").unwrap();

        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_ne(0));
        let metadata = Metadata::default();

        super::put_blob_index(&db.inner, &cf, key, metadata, 8, 123)
            .expect("unable to put new value in map");
        super::put_blob_value_segment(&db.inner, &cf, key, &[1, 2], 1, 123).unwrap();
        super::put_blob_value_segment(&db.inner, &cf, key, &[3, 4], 2, 123).unwrap();
        super::set_filled(&db.inner, &cf, key, metadata).unwrap();

        let value =
            adapt::get_key_as_entry(&db.inner, &db.cache, &cf, &key, adapt::DeepestLevel::Tier2);
        assert!(value.is_ok(), "the blob is filled");

        super::delete_blob_values(&db.inner, &cf, key).expect("all parts must be removed");

        let part = adapt::get_key_as_entry(
            &db.inner,
            &db.cache,
            &cf,
            &super::key_at(key, 1),
            adapt::DeepestLevel::Tier2,
        );
        assert!(part.is_err(), "blob part not removed");
    }
}
