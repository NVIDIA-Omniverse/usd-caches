// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{collections::BTreeMap, ops::AddAssign, time::Duration};

use bytes::BufMut;

use crate::{
    environment::rocks_adapter::log::{models::KIND_BLOB, snowflake},
    metadata, now_ms, Change,
};

use super::{
    bits,
    models::{
        self, BitOperation, BlobChange, InlineValue, Metadata, KIND_ACCESS, KIND_DROP, KIND_FLAG,
        KIND_INLINE_VALUE, KIND_METADATA,
    },
};

#[derive(Default)]
pub struct Latest {
    pub inline_value: Option<InlineValue>,
    pub blob_value: Option<(u128, u64)>,
    pub flags: u16,
    pub metadata: Option<metadata::Metadata>,
}

/// Decodes a record payload, or `None` if it does not validate.
///
/// This runs inside the RocksDB merge operator, which is reached through an
/// `extern "C"` callback: a panic there cannot unwind and aborts the process,
/// on a background compaction thread, every time the value is touched. A record
/// we cannot read is skipped instead, matching how the legacy merge operator
/// treats an operand it cannot decode.
fn decode<T>(payload: &[u8]) -> Option<T>
where
    T: rkyv::Archive,
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, rkyv::rancor::Error>>
        + rkyv::Deserialize<T, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>>,
{
    match crate::serde::deserialize::<T>(payload) {
        Ok(value) => Some(value),
        Err(error) => {
            bits::corrupt(&format!("log record did not validate: {error}"));
            None
        }
    }
}

impl Latest {
    pub fn is_expired(&self) -> bool {
        self.metadata
            .map(|y| y.is_expired(now_ms()))
            .unwrap_or_default()
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DropCond {
    Drop,
    Tag(u128),
    BlobDeleted(u128),
}

fn eval_drops<'a>(mut log: &'a [u8]) -> Vec<(DropCond, bits::Next<'a>)> {
    // Keyed by tag *and* arrival, because a tag is not unique: one write emits
    // several records under a single snowflake -- a put writes DROP, value and
    // metadata together, and so does blob completion -- and the DROP's strict
    // comparison is what lets them survive their own drop record. Keying on the
    // tag alone would let each of those overwrite the last, so a put would store
    // whichever record it happened to write final. The counter keeps records
    // sharing a tag in the order they were written, while the tag still decides
    // the order between writes.
    let mut all: BTreeMap<(u128, usize), (DropCond, bits::Next<'a>)> = Default::default();

    let mut drop_before = 0u128;
    let mut arrival = 0usize;

    while let Some(next) = bits::_next(&mut log) {
        let at = (next.tag, arrival);
        arrival += 1;

        if next.kind == KIND_DROP {
            if next.tag > drop_before {
                drop_before = next.tag;
            }
            all.insert(at, (DropCond::Tag(next.tag), next));
        } else if next.kind == KIND_BLOB {
            let Some(change) = decode::<BlobChange>(next.value) else {
                continue;
            };

            match change {
                BlobChange::Created { id } | BlobChange::Completed { id, .. } => {
                    all.insert(at, (DropCond::BlobDeleted(id), next));
                }
                BlobChange::Deleted { id } => {
                    for (cond, _next) in all.values_mut() {
                        if *cond == DropCond::BlobDeleted(id) {
                            *cond = DropCond::Drop;
                        }
                    }

                    // The tombstone goes too. It exists to cancel the records
                    // above, and by the time this runs it has: keeping it would
                    // leave a row saying only that something used to be there.
                    all.insert(at, (DropCond::Drop, next));
                }
            }
        } else {
            all.insert(at, (DropCond::Tag(next.tag), next));
        }
    }

    for (cond, _next) in all.values_mut() {
        match *cond {
            DropCond::Tag(tag) if tag < drop_before => {
                *cond = DropCond::Drop;
            }
            _ => {}
        }
    }

    all.into_values().collect()
}

/// Marks the records that only describe a value, once no value is left.
///
/// Metadata and access records say something *about* a row's value: when it was
/// read, when it expires. Neither is content, so once the value is gone they
/// describe nothing and a row holding only them can never be read. Dropping them
/// is what lets such a row collapse instead of lingering as something no reader
/// can use -- which is what an `update` on a key that was never written leaves
/// behind, and what once made those rows unreclaimable.
///
/// A drop record is not a value either: it is the tombstone doing the removing.
fn drop_notes_without_a_value(all: &mut [(DropCond, bits::Next<'_>)]) {
    let is_note = |kind: &models::Kind| *kind == KIND_ACCESS || *kind == KIND_METADATA;

    let describes_a_value = all.iter().any(|(cond, next)| {
        *cond != DropCond::Drop && !is_note(&next.kind) && next.kind != KIND_DROP
    });

    if describes_a_value {
        return;
    }

    for (cond, next) in all.iter_mut() {
        if is_note(&next.kind) {
            *cond = DropCond::Drop;
        }
    }
}

/// Folds a key's surviving access records into one, or drops them when they
/// describe nothing.
///
/// A read is the most frequent thing that happens to a key and each one appends
/// a record, so these are what a busy key accumulates most of. They fold with
/// nothing but a comparison -- keep the later time -- which is the reason access
/// time has its own kind rather than riding on a metadata change.
///
/// Records describing a key that no longer has a value are dropped rather than
/// folded, so a row that was read and then evicted collapses to nothing instead
/// of keeping a note of when it was last touched.
///
/// Returns the replacement record and the index it belongs at, if one is needed.
fn squash_access(all: &mut [(DropCond, bits::Next<'_>)]) -> Option<(usize, Vec<u8>)> {
    let access: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, (cond, next))| *cond != DropCond::Drop && next.kind == KIND_ACCESS)
        .map(|(index, _)| index)
        .collect();

    if access.is_empty() {
        return None;
    }

    let mut folded = models::Access { latest: 0 };
    for index in &access {
        if let Some(one) = decode::<models::Access>(all[*index].1.value) {
            folded.latest = folded.latest.max(one.latest);
        }
    }

    // Already one record: folding it would only re-serialize what is there.
    if access.len() == 1 {
        return None;
    }

    // The last one's tag, so the fold sits no earlier in tag order than the
    // newest record it stands for and no later than anything written after it.
    let at = *access.last().unwrap();
    let tag = all[at].1.tag;

    let mut record = Vec::new();
    bits::write(KIND_ACCESS, tag, &folded, &mut record);

    for index in access {
        all[index].0 = DropCond::Drop;
    }

    Some((at, record))
}

/// True when this log holds nothing but its own drop records.
///
/// The evaluation [`clean`] performs, without building the output. A compaction
/// filter asks this of every value it sees, and copying a whole inline value
/// only to measure the result would make the check cost scale with the data it
/// is trying not to keep.
///
/// A surviving DROP does not count as content. [`clean`] keeps the newest one
/// -- it is the tombstone that suppresses older records, so a dropped row
/// collapses to that record rather than to nothing -- but at the bottommost
/// level there are no older records left anywhere for it to suppress, so it has
/// no work to do. That is what makes this question answerable only there, and
/// why the caller must check the level before acting on the answer.
///
/// Note this is a stronger question than "is there a readable value". A blob
/// whose upload is still in flight has no value a reader can return, but its
/// record is what points at the parts already on disk -- dropping that row
/// would strand them. Only a log with nothing left to say can go.
pub fn is_fully_dropped(log: &[u8]) -> bool {
    let mut entries = eval_drops(log);

    // Notes describing a value that is gone go with it, which is usually what
    // leaves a row with nothing in it.
    drop_notes_without_a_value(&mut entries);

    // A fold standing in for several access records is itself a survivor, and it
    // only survives where a value did.
    if squash_access(&mut entries).is_some() {
        return false;
    }

    // Only the tombstone may remain: everything a reader could want is dropped.
    entries
        .iter()
        .all(|(cond, next)| *cond == DropCond::Drop || next.kind == KIND_DROP)
}

pub fn clean(log: &[u8]) -> Vec<u8> {
    let mut entries = eval_drops(log);

    drop_notes_without_a_value(&mut entries);

    let squashed = squash_access(&mut entries);

    let mut after = Vec::<u8>::default();

    for (index, (cond, next)) in entries.iter().enumerate() {
        match squashed.as_ref().filter(|(at, _)| *at == index) {
            // The fold replaces the last of the records it covers, which is
            // marked dropped along with the rest.
            Some((_, record)) => after.put_slice(record),
            None if *cond == DropCond::Drop => continue,
            None => after.put_slice(next.raw),
        }
    }

    after
}

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum BlobCond {
    #[default]
    /// An upload still inside its grace period.
    ///
    /// The default, because a blob is built up from whatever records the log
    /// holds and `+=` mostly takes the more retired of the two -- so the
    /// starting point has to be the least retired state, or nothing could ever
    /// climb out of it.
    ///
    /// `+=` is *not* a plain maximum: `Stale + Active` answers `Active`, because
    /// the only way to reach `Stale` inside the fold is an aged `Created`, and a
    /// `Completed` for the same id says that upload landed after all. That one
    /// arm is what stops a slow upload having its parts reclaimed underneath a
    /// blob that is the key's current value.
    ///
    /// Only age retires this. `Created` is the only record an upload writes
    /// while it runs, and a live upload and an abandoned one leave exactly the
    /// same thing behind -- so a DROP, or a newer blob completing, says nothing
    /// about whether this one is still being written. Keeping it out of
    /// `Active` is what stops the supersession rule below reclaiming the parts
    /// of an upload that is still going.
    Writing,
    /// A finished blob, current unless something newer supersedes it.
    Active,
    /// Retired: its parts can be reclaimed.
    Stale,
    /// Already reclaimed; the parts are gone.
    Deleted,
}

impl AddAssign for BlobCond {
    fn add_assign(&mut self, rhs: Self) {
        *self = match (*self, rhs) {
            (BlobCond::Writing, BlobCond::Writing) => *self,
            (BlobCond::Writing, BlobCond::Active)
            | (BlobCond::Writing, BlobCond::Stale)
            | (BlobCond::Writing, BlobCond::Deleted) => rhs,

            (BlobCond::Active, BlobCond::Writing) | (BlobCond::Active, BlobCond::Active) => *self,
            (BlobCond::Active, BlobCond::Stale) | (BlobCond::Active, BlobCond::Deleted) => rhs,

            (BlobCond::Stale, BlobCond::Writing) | (BlobCond::Stale, BlobCond::Stale) => *self,
            (BlobCond::Stale, BlobCond::Deleted | BlobCond::Active) => rhs,

            (BlobCond::Deleted, _) => *self,
        };
    }
}

/// The blobs of a key whose parts can be reclaimed.
///
/// A `Created` record is all an upload leaves in the log while it runs, and it
/// is indistinguishable from one that was abandoned, so `age` is what separates
/// them: past the grace period the upload is taken to have aborted, whether or
/// not a writer is still holding it. That covers the case nothing else can --
/// a process that died mid-upload, where no guard will ever run to clean up.
///
/// The grace period therefore has to exceed the longest upload the deployment
/// expects. One that outlives it has its parts reclaimed underneath it.
pub fn stale_blobs<'a>(mut log: &'a [u8], age: Duration) -> Vec<u128> {
    let mut all: BTreeMap<u128, (BlobCond, bits::Next<'a>)> = Default::default();

    let mut drop_before = 0u128;
    let mut latest_complete = 0u128;

    while let Some(next) = bits::_next(&mut log) {
        if next.kind == KIND_DROP {
            // The newest, not the last seen. `merge_full` sorts, but the
            // compaction filter and `retain` are both handed logs it has not
            // been through -- a partial merge only concatenates -- so a DROP can
            // arrive after a newer one. Taking it as the watermark would lower
            // the line and leave a retired blob holding its parts. `eval_drops`
            // and `latest` read it the same way.
            if next.tag > drop_before {
                drop_before = next.tag;
            }
        } else if next.kind == KIND_BLOB {
            let Some(change) = decode::<BlobChange>(next.value) else {
                continue;
            };

            match change {
                BlobChange::Created { id } => {
                    let cond = match snowflake::age(id) > age {
                        // Past the grace period the upload is taken to have
                        // aborted; before it, it may still be running.
                        true => BlobCond::Stale,
                        false => BlobCond::Writing,
                    };
                    let entry = all.entry(id).or_insert((BlobCond::default(), next));
                    entry.0 += cond;
                }
                BlobChange::Completed { id, .. } => {
                    // Keep the completion's own record, not a `Created` that may
                    // already be stored here: the supersession test below reads
                    // its tag, and only completion has a tag that means "after
                    // everything this blob replaced".
                    let entry = all
                        .entry(id)
                        .and_modify(|entry| entry.1 = next)
                        .or_insert((BlobCond::default(), next));
                    entry.0 += BlobCond::Active;
                    if id > latest_complete {
                        latest_complete = id;
                    }
                }
                BlobChange::Deleted { id } => {
                    let entry = all.entry(id).or_insert((BlobCond::default(), next));
                    entry.0 += BlobCond::Deleted;
                }
            }
        }
    }

    // Supersession applies only to blobs that finished -- `Active` is reached
    // solely from `Completed`. A blob still being written is `Writing` and is
    // left alone: being older than a DROP means it started before something
    // else wrote the key, not that it stopped.
    //
    // The DROP test is against the record's own tag, not the blob id, because
    // the two are minted at different times: the id when the upload began, the
    // record when it finished. `done` writes a DROP to retire whatever the key
    // held before, so a blob's id is always older than the DROP that its own
    // completion wrote -- comparing ids here retired every blob the moment it
    // was written. `latest` draws the same line the same way.
    for (id, (cond, next)) in &mut all {
        if *cond == BlobCond::Active && (next.tag < drop_before || *id < latest_complete) {
            *cond = BlobCond::Stale;
        }
    }

    all.iter()
        .filter(|(_, v)| v.0 == BlobCond::Stale)
        .map(|(k, _)| *k)
        .collect()
}

pub fn latest(mut log: &[u8]) -> Latest {
    let mut combined = Latest {
        inline_value: None,
        blob_value: None,
        flags: 0,
        metadata: None,
    };

    let mut drop_before = 0;

    while let Some((kind, tag, kind_value)) = bits::next(&mut log) {
        if kind == KIND_BLOB {
            let Some(change) = decode::<BlobChange>(kind_value) else {
                continue;
            };

            if let BlobChange::Completed { id, size, .. } = change {
                if tag < drop_before {
                    continue;
                }
                combined.blob_value = Some((id, size));
            }
        } else if kind == KIND_INLINE_VALUE {
            let Some(inline_value) = decode::<models::InlineValue>(kind_value) else {
                continue;
            };
            if tag < drop_before {
                continue;
            }

            combined.inline_value = Some(inline_value);
        } else if kind == KIND_FLAG {
            let Some(models::Flag { value, operator }) = decode::<models::Flag>(kind_value) else {
                continue;
            };
            if tag < drop_before {
                continue;
            }

            match operator {
                BitOperation::And => combined.flags &= value,
                BitOperation::Or => combined.flags |= value,
                BitOperation::Xor => combined.flags ^= value,
            }
        } else if kind == KIND_METADATA {
            let Some(metadata_changed) = decode::<models::Metadata>(kind_value) else {
                continue;
            };

            match metadata_changed {
                Metadata::Set(metadata) => {
                    if tag < drop_before {
                        continue;
                    }
                    combined.metadata = Some(metadata);
                }
                Metadata::Change(change) => {
                    if tag < drop_before {
                        continue;
                    }
                    let Some(metadata) = combined.metadata.as_mut() else {
                        continue;
                    };

                    if let Change::Set(value) = change.ttl_ms {
                        metadata.ttl_ms = value;
                    }
                }
            }
        } else if kind == KIND_ACCESS {
            let Some(access) = decode::<models::Access>(kind_value) else {
                continue;
            };
            if tag < drop_before {
                continue;
            }
            // Access time is a clock, so it only ever moves forward: operands
            // can arrive carrying an older reading than one already folded in,
            // and taking the later of the two is what makes the fold agree
            // whatever order they come in. Inert without a value to describe,
            // exactly as a metadata change is.
            if let Some(metadata) = combined.metadata.as_mut() {
                metadata.access_time_secs = metadata.access_time_secs.max(access.latest);
            }
        } else if kind == KIND_DROP && tag > drop_before {
            drop_before = tag;
            combined = Default::default();
        }
    }

    combined
}
