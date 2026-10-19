// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use crate::environment::rocks_adapter::log::{
    bits::{self, common::log_inline_value_change, ArrayBuf},
    models::{self, KIND_ACCESS, KIND_BLOB, KIND_DROP},
    rollup, snowflake,
};

/// Long enough that nothing in these tests ages out, so only the supersession
/// rules are under test.
const GRACE: Duration = Duration::from_secs(60 * 60);

/// Metadata with a known access time, for tests that fold access records onto it.
fn a_metadata() -> crate::metadata::Metadata {
    crate::metadata::Metadata {
        ttl_ms: None,
        created_secs: 123,
        access_time_secs: 0,
    }
}

fn write_blob(change: models::BlobChange, writer: &mut ArrayBuf) {
    bits::write(KIND_BLOB, snowflake::next(), &change, writer);
}

/// A tag reading `ms` milliseconds older than now.
///
/// A tag is `(millisecond << 64) | sequence` and [`snowflake::age`] reads the
/// high half back, so subtracting there ages a blob exactly, without sleeping.
fn aged_tag(ms: u64) -> u128 {
    snowflake::next() - ((ms as u128) << 64)
}

/// The kind byte of every record in a log, in order.
fn record_kinds(mut log: &[u8]) -> Vec<u8> {
    let mut kinds = vec![];
    while let Some((kind, _tag, _value)) = bits::next(&mut log) {
        kinds.push(kind.0);
    }
    kinds
}

fn blob_changes(mut log: &[u8]) -> Vec<models::BlobChange> {
    let mut all = vec![];
    while let Some((kind, _tag, value)) = bits::next(&mut log) {
        if kind == KIND_BLOB {
            all.push(
                crate::serde::deserialize::<models::BlobChange>(value)
                    .expect("a blob record should decode"),
            );
        }
    }
    all
}

#[test]
fn test_log_empty_after_drop() {
    let mut first_log = ArrayBuf::new();
    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &[1, 2, 3],
        &mut first_log,
    );

    bits::write(
        KIND_DROP,
        snowflake::next(),
        &models::DropBefore,
        &mut first_log,
    );

    let collapsed = rollup::clean(first_log.as_slice());

    // The DROP survives its own collapse. It is the tombstone that suppresses
    // the records it dropped, and those may still exist in a lower level that
    // this collapse did not see, so it cannot go until nothing is left below
    // it. What must be gone is everything else.
    assert_eq!(record_kinds(&collapsed), vec![KIND_DROP.0]);
    assert!(rollup::latest(&collapsed).inline_value.is_none());
    assert!(rollup::is_fully_dropped(&collapsed));
}

/// A record framed the way the previous format framed one is rejected.
///
/// Before versioning, a record began with its kind byte and those ran `0..=4`;
/// a legacy-mode stored value begins with a zero byte. The version byte has to
/// sit outside that range or one of them reads as a version this reader claims
/// to understand, and the bytes after it are then interpreted as a length and a
/// tag they are not. `1` used to be the blob record's kind, which is exactly the
/// value a first version would otherwise have taken.
#[test]
fn test_log_a_record_framed_by_the_previous_format_is_rejected() {
    for kind in 0u8..=4 {
        // The old header: kind, then a u32 length and a u128 tag.
        let mut record = vec![kind];
        record.extend_from_slice(&3u32.to_be_bytes());
        record.extend_from_slice(&1u128.to_be_bytes());
        record.extend_from_slice(&[1, 2, 3]);

        let mut log = record.as_slice();
        assert!(
            bits::_next(&mut log).is_none(),
            "a record framed by the previous format was accepted as kind {kind}"
        );
    }

    assert!(
        !(0u8..=4).contains(&bits::VERSION),
        "the version byte must not collide with a kind byte the old format wrote"
    );
}

/// A log we cannot parse must not panic. `collapse_logs` runs inside the
/// RocksDB merge operator, reached through an `extern "C"` callback, so a panic
/// there cannot unwind and aborts the process on a compaction thread -- and it
/// would do so again on every restart, since the bytes are on disk.
///
/// Each case keeps whatever prefix decoded and drops the rest, so collapsing
/// the value cleans up the damage.
#[test]
fn test_log_damaged_records_do_not_panic() {
    let metadata = crate::metadata::Metadata {
        ttl_ms: None,
        created_secs: 123,
        access_time_secs: 246,
    };

    let mut good = ArrayBuf::new();
    log_inline_value_change(metadata, &[1, 2, 3], &mut good);
    let good = good.as_slice().to_vec();

    // A record header claiming a payload longer than what follows.
    let mut bad_length = good.clone();
    let len_at = bad_length.len() - 4;
    bad_length.truncate(len_at);
    bad_length.extend_from_slice(&[0u8; bits::HEADER_LEN]);
    bad_length[len_at + 2] = 0xFF;

    // An unrecognised framing version, which is what a value written by a
    // different format -- or by the legacy engine -- looks like here.
    let mut bad_version = good.clone();
    bad_version[0] = bits::VERSION.wrapping_add(1);

    // A trailing fragment too short to be a header at all.
    let mut short_tail = good.clone();
    short_tail.extend_from_slice(&[7u8; bits::HEADER_LEN - 1]);

    // A payload that frames correctly but does not validate as its model.
    let mut bad_payload = good.clone();
    let last = bad_payload.len() - 1;
    bad_payload[last] ^= 0xFF;

    for (name, log) in [
        ("truncated", &good[..good.len() / 2]),
        ("bad length", &bad_length[..]),
        ("bad version", &bad_version[..]),
        ("short tail", &short_tail[..]),
        ("bad payload", &bad_payload[..]),
        ("garbage", &[0xFFu8; 64][..]),
        ("empty", &[][..]),
    ] {
        // Neither reading nor collapsing may panic.
        let _ = rollup::latest(log);
        let _ = rollup::stale_blobs(log, GRACE).len();
        let _ = rollup::is_fully_dropped(log);

        let collapsed = rollup::clean(log);

        // Whatever survived collapsing must itself be parseable, so a damaged
        // value converges instead of staying broken forever.
        let _ = rollup::latest(&collapsed);

        let twice = rollup::clean(&collapsed);
        assert_eq!(
            twice, collapsed,
            "collapsing {name} did not reach a fixed point"
        );
    }
}

/// The intact prefix of a damaged log is still served.
#[test]
fn test_log_keeps_the_prefix_before_the_damage() {
    let metadata = crate::metadata::Metadata {
        ttl_ms: None,
        created_secs: 123,
        access_time_secs: 246,
    };

    let mut log = ArrayBuf::new();
    log_inline_value_change(metadata, &[1, 2, 3], &mut log);

    let mut damaged = log.as_slice().to_vec();
    damaged.extend_from_slice(&[0xFFu8; 40]);

    let latest = rollup::latest(&damaged);
    assert_eq!(
        latest
            .inline_value
            .expect("the intact prefix should still parse")
            .inner,
        vec![1, 2, 3]
    );
}

/// Records used to be staged in a fixed 16,000 byte buffer, and `ArrayBuf` was
/// a fixed 128,000 byte array, so a large enough inline value aborted the
/// process rather than failing the request. Both grow now.
#[test]
fn test_log_inline_value_larger_than_the_old_fixed_buffer() {
    let value: Vec<u8> = (0..64 * 1024).map(|y| y as u8).collect();

    let mut writer = ArrayBuf::new();
    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &value,
        &mut writer,
    );

    let latest = rollup::latest(writer.as_slice());
    assert_eq!(
        latest
            .inline_value
            .expect("inline value was not stored")
            .inner,
        value
    );

    // And it survives a collapse, which re-frames every record it keeps.
    let collapsed = rollup::clean(writer.as_slice());

    let latest = rollup::latest(&collapsed);
    assert_eq!(
        latest
            .inline_value
            .expect("inline value was lost in collapse")
            .inner,
        value
    );
}

#[test]
fn test_log_inline_value_changed() {
    let mut writer = ArrayBuf::new();

    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &[1, 2, 3],
        &mut writer,
    );

    {
        let latest = rollup::latest(writer.as_slice());
        assert!(latest.blob_value.is_none());
        let value = latest.inline_value.expect("must have an inline value");
        assert_eq!(&value.inner, &[1, 2, 3]);
    }

    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &[4, 5, 6],
        &mut writer,
    );

    {
        let latest = rollup::latest(writer.as_slice());
        assert!(latest.blob_value.is_none());
        let value = latest.inline_value.expect("must have an inline value");
        assert_eq!(&value.inner, &[4, 5, 6]);
    }
}

#[test]
fn test_log_inline_value_changed_out_of_order() {
    let mut writer = ArrayBuf::new();

    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &[1, 2, 3],
        &mut writer,
    );

    let mut writer2 = ArrayBuf::new();
    log_inline_value_change(
        crate::metadata::Metadata {
            ttl_ms: None,
            created_secs: 123,
            access_time_secs: 246,
        },
        &[4, 5, 6],
        &mut writer2,
    );

    let mut buff = vec![];
    buff.extend_from_slice(writer2.as_slice());
    buff.extend_from_slice(writer.as_slice());

    let latest = rollup::latest(&buff);
    assert!(latest.blob_value.is_none());
    let value = latest.inline_value.expect("must have an inline value");
    assert_eq!(&value.inner, &[4, 5, 6]);
}

#[test]
fn test_log_blob_incomplete() {
    let mut writer = ArrayBuf::new();
    let id = snowflake::next();
    write_blob(models::BlobChange::Created { id }, &mut writer);

    let latest = rollup::latest(writer.as_slice());
    assert!(latest.blob_value.is_none());
}

#[test]
fn test_log_blob_complete() {
    let mut writer = ArrayBuf::new();
    let id = snowflake::next();

    write_blob(
        models::BlobChange::Completed {
            id,
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    let latest = rollup::latest(writer.as_slice());

    assert_eq!(
        latest.blob_value.expect("blob to be set and completed"),
        (id, 2)
    );
}

#[test]
fn test_log_blob_incomplete_to_complete() {
    let mut writer = ArrayBuf::new();
    let id = snowflake::next();
    write_blob(models::BlobChange::Created { id }, &mut writer);

    let latest = rollup::latest(writer.as_slice());
    assert!(latest.blob_value.is_none());

    write_blob(
        models::BlobChange::Completed {
            id,
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    let latest = rollup::latest(writer.as_slice());

    assert_eq!(
        latest.blob_value.expect("blob to be set and completed"),
        (id, 2)
    );
}

#[test]
fn test_log_blob_multiple_incomplete() {
    let mut writer = ArrayBuf::new();
    let id = snowflake::next();

    write_blob(
        models::BlobChange::Created {
            id: snowflake::next(),
        },
        &mut writer,
    );
    write_blob(models::BlobChange::Created { id }, &mut writer);
    write_blob(
        models::BlobChange::Created {
            id: snowflake::next(),
        },
        &mut writer,
    );

    let latest = rollup::latest(writer.as_slice());
    assert!(latest.blob_value.is_none());

    write_blob(
        models::BlobChange::Completed {
            id,
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    let latest = rollup::latest(writer.as_slice());

    assert_eq!(
        latest.blob_value.expect("blob to be set and completed"),
        (id, 2)
    );
}

/// A blob that finished and was then superseded by a newer one is reclaimable.
#[test]
fn test_log_blob_superseded_by_a_newer_complete_is_reclaimed() {
    let mut writer = ArrayBuf::new();
    let old = snowflake::next();
    let new = snowflake::next();

    for id in [old, new] {
        write_blob(
            models::BlobChange::Completed {
                id,
                size: 2,
                parts: 1,
            },
            &mut writer,
        );
    }

    assert_eq!(
        rollup::stale_blobs(writer.as_slice(), GRACE),
        vec![old],
        "only the superseded blob should be reclaimed"
    );
}

/// An upload that has not finished is left alone while it may still be running,
/// even once something newer has superseded it.
///
/// `Created` is the only record a live upload writes, and an abandoned one
/// leaves exactly the same thing behind, so a newer blob completing or a DROP
/// landing says nothing about whether this upload stopped -- only age does.
/// Retiring on supersession deleted the parts of an upload still in flight, and
/// a plain `put` on the key was enough to trigger it, because a put writes a
/// DROP.
#[test]
fn test_log_blob_incomplete_is_not_reclaimed_while_it_may_be_running() {
    let mut writer = ArrayBuf::new();
    let running = snowflake::next();
    write_blob(models::BlobChange::Created { id: running }, &mut writer);
    write_blob(
        models::BlobChange::Completed {
            id: snowflake::next(),
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    assert!(
        !rollup::stale_blobs(writer.as_slice(), GRACE).contains(&running),
        "an unfinished upload was reclaimed because a newer blob completed"
    );

    let mut writer = ArrayBuf::new();
    let running = snowflake::next();
    write_blob(models::BlobChange::Created { id: running }, &mut writer);
    bits::write(
        KIND_DROP,
        snowflake::next(),
        &models::DropBefore,
        &mut writer,
    );

    assert!(
        !rollup::stale_blobs(writer.as_slice(), GRACE).contains(&running),
        "an unfinished upload was reclaimed because a put dropped the key"
    );
}

/// Age is what reclaims an unfinished upload, and the only thing that does.
#[test]
fn test_log_blob_incomplete_is_reclaimed_once_it_ages_out() {
    let mut writer = ArrayBuf::new();
    let abandoned = aged_tag(5_000);
    write_blob(models::BlobChange::Created { id: abandoned }, &mut writer);

    assert!(
        rollup::stale_blobs(writer.as_slice(), GRACE).is_empty(),
        "reclaimed before the grace period had elapsed"
    );
    assert_eq!(
        rollup::stale_blobs(writer.as_slice(), Duration::from_secs(1)),
        vec![abandoned],
        "an upload past its grace period should be reclaimed"
    );
}

/// A slow upload that finished is not reclaimed just because it *started* long
/// ago.
///
/// Age retires a `Created` that never completed. A `Completed` for the same id
/// says the upload landed, so the age verdict no longer applies -- and the blob
/// is the key's current value, so reclaiming its parts is silent data loss with
/// nothing to signal it: `latest` still publishes the blob, and a reader finds
/// an empty part range.
///
/// The two halves are each covered on their own -- an aged `Created` alone, and
/// `Created`-then-`Completed` inside the grace -- but never together, which is
/// how the case survived. It is reachable whenever an upload outlives
/// `ABANDONED_UPLOAD_GRACE`, which the grace period is sized to allow.
#[test]
fn test_log_blob_completed_is_not_reclaimed_for_a_slow_upload() {
    let mut writer = ArrayBuf::new();
    let slow = aged_tag(5_000);

    write_blob(models::BlobChange::Created { id: slow }, &mut writer);
    write_blob(
        models::BlobChange::Completed {
            id: slow,
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    // One second of grace, so `slow` is unambiguously past it: the `Created`
    // on its own would be reclaimed here (see
    // `test_log_blob_incomplete_is_reclaimed_once_it_ages_out`).
    assert!(
        rollup::stale_blobs(writer.as_slice(), Duration::from_secs(1)).is_empty(),
        "a completed blob was reclaimed because its upload started before the grace period"
    );

    // And it really is the key's current value, so reclaiming it would have
    // been loss rather than a harmless early delete.
    assert_eq!(
        rollup::latest(writer.as_slice())
            .blob_value
            .expect("the completed blob should be the key's value"),
        (slow, 2)
    );
}

/// A DROP retires every blob that finished: the key's value is gone, so the
/// parts behind it can go too.
#[test]
fn test_log_blob_drop_retires_completed_blobs() {
    let mut writer = ArrayBuf::new();
    let id_a = snowflake::next();
    let id_b = snowflake::next();

    for id in [id_a, id_b] {
        write_blob(
            models::BlobChange::Completed {
                id,
                size: 2,
                parts: 1,
            },
            &mut writer,
        );
    }

    bits::write(
        KIND_DROP,
        snowflake::next(),
        &models::DropBefore,
        &mut writer,
    );

    // Ascending: `stale_blobs` collects from a map keyed by blob id.
    assert_eq!(
        rollup::stale_blobs(writer.as_slice(), GRACE),
        vec![id_a, id_b]
    );
}

/// An older DROP arriving after a newer one must not lower the line.
///
/// `merge_full` sorts by tag, but it is not the only reader: a partial merge
/// only concatenates, and both the compaction filter and `retain` are handed
/// logs that have not been through a full merge. So records can be out of order
/// here, and taking the last DROP seen as the watermark rather than the newest
/// would un-retire a blob -- leaving its parts on disk with nothing that ever
/// revisits them, which is the leak this scan exists to prevent.
#[test]
fn test_log_an_out_of_order_drop_does_not_lower_the_watermark() {
    let mut writer = ArrayBuf::new();

    let id = snowflake::next();
    write_blob(
        models::BlobChange::Completed {
            id,
            size: 2,
            parts: 1,
        },
        &mut writer,
    );

    // The DROP that retires it, then one older than the blob itself.
    let newest = snowflake::next();
    bits::write(KIND_DROP, newest, &models::DropBefore, &mut writer);
    bits::write(KIND_DROP, id - 1, &models::DropBefore, &mut writer);

    assert!(
        newest > id,
        "the retiring drop should be newer than the blob record"
    );
    assert_eq!(
        rollup::stale_blobs(writer.as_slice(), GRACE),
        vec![id],
        "a stale blob was un-retired by a drop record older than the one before it"
    );
}

/// A blob record outlives a DROP, and only an explicit `Deleted` removes it.
///
/// The record is the one thing pointing at the parts on disk, so dropping it
/// while they are still there would strand them. GC writes `Deleted` once it
/// has actually removed them, and that is what lets the record go.
#[test]
fn test_log_drop_with_blobs() {
    let mut writer = ArrayBuf::new();
    let ids = [snowflake::next(), snowflake::next(), snowflake::next()];

    for id in ids {
        write_blob(models::BlobChange::Created { id }, &mut writer);
    }

    bits::write(
        KIND_DROP,
        snowflake::next(),
        &models::DropBefore,
        &mut writer,
    );

    let collapsed = rollup::clean(writer.as_slice());
    assert_eq!(
        blob_changes(&collapsed).len(),
        3,
        "the blob records did not survive the drop"
    );
    assert!(!rollup::is_fully_dropped(&collapsed));

    for id in ids {
        write_blob(models::BlobChange::Deleted { id }, &mut writer);
    }

    let collapsed = rollup::clean(writer.as_slice());
    assert!(
        blob_changes(&collapsed).is_empty(),
        "a deleted blob should leave no record behind"
    );

    // Only the DROP is left, which is the tombstone rather than content.
    assert_eq!(record_kinds(&collapsed), vec![KIND_DROP.0]);
    assert!(rollup::is_fully_dropped(&collapsed));
}

/// A completed blob records how many parts it was written as, and retiring it
/// keeps that count.
///
/// It is not derivable after the fact: the chunk size is the writer's choice and
/// is not stored, so two blobs of the same size can have different part counts.
/// Retirement used to replace the whole `change`, which threw the count away at
/// the one moment something might want it. Nothing rewrites the record now --
/// it stays `Completed` until GC removes the parts and writes `Deleted` -- so
/// the count survives by not being touched.
#[test]
fn test_log_blob_records_its_part_count() {
    let (_dir, db) =
        crate::tests::temp_db_flags(crate::FLAG_DB_LOG_ENABLE | crate::FLAG_DB_BLOB_ENABLE);
    let key = ovdc_bytes::keys::Compound::new(
        Default::default(),
        ovdc_bytes::keys::Key::from_u64_be(5_200),
    );
    let metadata = crate::metadata::Metadata::default();

    let (mut cx, _tag) = crate::BlobProvider::blobs(&db)
        .begin_write(key, metadata, 12)
        .unwrap();
    for _ in 0..3 {
        assert!(crate::BlobProvider::blobs(&db).schedule(&mut cx, &[1, 2, 3, 4]));
    }
    crate::BlobProvider::blobs(&db).done(&mut cx).unwrap();
    drop(cx);

    let raw = db
        .inner
        .get_cf(&db.inner.cf_handle("default").unwrap(), key)
        .unwrap()
        .expect("the blob record should be stored");

    let completed = blob_changes(&raw)
        .into_iter()
        .find_map(|y| match y {
            models::BlobChange::Completed { parts, .. } => Some(parts),
            _ => None,
        })
        .expect("a completed blob record");
    assert_eq!(completed, 3, "wrong part count recorded");

    // Retiring it must not lose the count.
    crate::Database::open(&db).delete(key).unwrap();

    let raw = db
        .inner
        .get_cf(&db.inner.cf_handle("default").unwrap(), key)
        .unwrap()
        .expect("the record should still be there until GC runs");

    let retired = blob_changes(&raw)
        .into_iter()
        .find_map(|y| match y {
            models::BlobChange::Completed { parts, .. } => Some(parts),
            _ => None,
        })
        .expect("the blob record should outlive the delete, so GC can find the parts");
    assert_eq!(retired, 3, "part count was lost when the blob was retired");
}

/// Several records written under one tag all survive a merge.
///
/// A put emits a DROP, its value and its metadata under a single snowflake --
/// the DROP's comparison is strict, so a write replaces a key's contents while
/// keeping its own records. Collapsing has to honour that: ordering by tag alone
/// would let each of those records take the place of the last, leaving whichever
/// happened to be written final, and a put would store its metadata in place of
/// its value.
#[test]
fn test_log_records_sharing_a_tag_all_survive_a_merge() {
    let mut writer = ArrayBuf::new();
    log_inline_value_change(a_metadata(), &[1, 2, 3, 4], &mut writer);

    let before = record_kinds(writer.as_slice());
    assert!(
        before.len() > 1,
        "a put should write more than one record, or this proves nothing"
    );

    let merged = rollup::clean(writer.as_slice());

    assert_eq!(
        record_kinds(&merged),
        before,
        "a merge dropped records that shared a tag"
    );
    assert_eq!(
        rollup::latest(&merged).inline_value.map(|y| y.inner),
        Some(vec![1, 2, 3, 4]),
        "the value did not survive the merge"
    );
}

/// Access records fold to one, keeping the latest time.
///
/// A read is the most frequent thing that happens to a key, so these are what a
/// busy row accumulates. Folding is why access time has its own kind rather than
/// riding on a metadata change -- it needs nothing but a comparison.
#[test]
fn test_log_access_records_fold_to_the_latest_time() {
    let mut writer = ArrayBuf::new();
    log_inline_value_change(a_metadata(), &[7], &mut writer);

    // Deliberately not in time order, to show the fold takes the latest rather
    // than the last one seen.
    for latest in [40u64, 90, 60] {
        bits::write(
            KIND_ACCESS,
            snowflake::next(),
            &models::Access { latest },
            &mut writer,
        );
    }

    let collapsed = rollup::clean(writer.as_slice());

    let access: Vec<models::Access> = {
        let mut log = collapsed.as_slice();
        let mut all = vec![];
        while let Some((kind, _tag, value)) = bits::next(&mut log) {
            if kind == KIND_ACCESS {
                all.push(crate::serde::deserialize::<models::Access>(value).unwrap());
            }
        }
        all
    };

    assert_eq!(access.len(), 1, "access records did not fold to one");
    assert_eq!(
        access[0].latest, 90,
        "the fold did not keep the latest time"
    );
    assert_eq!(
        rollup::latest(&collapsed)
            .metadata
            .unwrap()
            .access_time_secs,
        90,
        "the folded access time did not reach the metadata a reader sees"
    );
}

/// A row holding only notes about a value that is gone can be reclaimed.
///
/// Access and metadata records describe a value; they are not one. Once it is
/// gone they can never be read, so a row left with nothing else is dead weight
/// and the compaction filter has to be able to say so.
#[test]
fn test_log_a_row_of_only_notes_is_fully_dropped() {
    let mut writer = ArrayBuf::new();
    bits::write(
        KIND_ACCESS,
        snowflake::next(),
        &models::Access { latest: 5 },
        &mut writer,
    );

    assert!(
        rollup::is_fully_dropped(writer.as_slice()),
        "a row holding only an access record should be reclaimable"
    );

    // The same row, once it has a value to describe, must not be.
    let mut writer = ArrayBuf::new();
    log_inline_value_change(a_metadata(), &[1], &mut writer);
    bits::write(
        KIND_ACCESS,
        snowflake::next(),
        &models::Access { latest: 5 },
        &mut writer,
    );

    assert!(
        !rollup::is_fully_dropped(writer.as_slice()),
        "a row with a value was reported as fully dropped"
    );
}
