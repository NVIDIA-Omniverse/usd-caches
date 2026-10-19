// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use serde::{Deserialize, Serialize};

use crate::{metadata, Change};

/// Which of the types below a record's payload holds.
///
/// Written as the second byte of every record header (see [`super::bits`]) and
/// dispatched on by [`super::rollup::latest`]. The values are on disk, so an
/// existing constant cannot be renumbered or reused for a different type --
/// only new ones added. A record whose kind no reader recognises is skipped,
/// which is what lets a newer writer's record types pass through an older
/// reader rather than corrupting the roll-up.
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct Kind(pub u8);

/// Payload is an [`InlineValue`]: the whole value, stored in the row itself.
pub const KIND_INLINE_VALUE: Kind = Kind(1);
/// Payload is a [`Flag`]. Reserved -- see the note on [`Flag`], nothing writes
/// this today.
pub const KIND_FLAG: Kind = Kind(2);
/// Payload is a [`Metadata`]: either a full replacement or a field-wise change.
pub const KIND_METADATA: Kind = Kind(3);
/// Payload is a [`DropBefore`]: everything tagged below this record is gone.
pub const KIND_DROP: Kind = Kind(201);

/// Payload is a [`BlobChange`]: one transition in a blob's lifecycle.
pub const KIND_BLOB: Kind = Kind(10);

pub const KIND_ACCESS: Kind = Kind(8);

/// One transition in a blob's lifecycle, recorded against its blob id.
///
/// A blob is never described by a single record. The log holds every
/// transition, and [`super::rollup`] folds them per blob id: `Created` when the
/// upload starts, `Completed` when it finishes, `Deleted` once its parts are off
/// disk. Only `Completed` publishes a readable value, so an upload that stops
/// short leaves a `Created` record that reads as nothing -- which is what lets
/// `begin_write` tell an unfinished blob from one it must not overwrite.
///
/// Being retired is not one of them. A blob is stale when a newer one supersedes
/// it or it outlives the upload grace period, and both are derived while reading
/// -- `rollup::stale_blobs` decides it fresh each time, as `BlobCond`. Nothing
/// records it, because nothing can: it is a fact about the rest of the log and
/// about the clock, and it changes without the blob being written to.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Archive, RkyvSerialize, RkyvDeserialize)]
pub enum BlobChange {
    /// The upload has begun and its parts are being written.
    ///
    /// Written once, with the blob's first part. It does not make the blob
    /// readable: a key whose newest blob is `Created` has no value, and the
    /// next writer replaces it rather than being refused.
    ///
    /// `id` identifies the blob across its records and locates its parts -- it
    /// is a field of every part key in the blobs column family. It comes from
    /// the same snowflake clock as log tags, because it is compared like one:
    /// a blob sorting below the newest completed one is retired. An id from any
    /// other source compares meaninglessly against a snowflake and leaves the
    /// blob it belongs to unreclaimable.
    Created { id: u128 },
    /// The upload finished; the blob is this key's value.
    ///
    /// The only variant [`super::rollup::latest`] publishes a blob value from,
    /// and the only one that makes `begin_write` refuse a new upload.
    Completed {
        id: u128,

        size: u64,
        /// How many parts the blob was written as.
        ///
        /// Not derivable from `size`: the chunk size is the writer's choice and
        /// is not recorded, so two blobs of equal size can have different part
        /// counts.
        ///
        /// **Nothing reads this today.** Every consumer destructures
        /// `Completed { id, size, .. }`. It is recorded so a future reclaim path
        /// can find a blob's parts without walking the key range, and
        /// `test_log_blob_records_its_part_count` pins that it survives a
        /// delete; until something uses it, treat it as reserved rather than
        /// load-bearing.
        parts: u32,
    },
    /// The parts are off disk; forget the blob entirely.
    ///
    /// Coalescing drops the blob id outright, so nothing revisits it. Only
    /// write this once the parts are actually gone -- a `Deleted` record for
    /// parts still on disk orphans them permanently, since no later pass can
    /// see that they were ever there.
    Deleted { id: u128 },
}

/// Payload of a [`KIND_INLINE_VALUE`] record: a whole value held in the row.
///
/// The alternative to a blob, for values small enough not to be worth spreading
/// across parts. Each record carries the complete value rather than a delta, so
/// the newest surviving one wins outright.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct InlineValue {
    pub inner: Vec<u8>,
}

/// Payload of a [`KIND_METADATA`] record.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub enum Metadata {
    /// Replaces the key's metadata wholesale. Written alongside a value, by the
    /// put and blob-completion paths.
    Set(metadata::Metadata),
    /// Adjusts individual fields, leaving the rest alone.
    ///
    /// Applies only on top of metadata that already exists -- a `Change` that
    /// no `Set` precedes has nothing to modify and is discarded, so touching a
    /// key does not resurrect one that was dropped.
    Change(Modifications),
}

/// How a [`Flag`] combines with the flags accumulated so far.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub enum BitOperation {
    And,
    Or,
    Xor,
}

/// Payload of a [`KIND_FLAG`] record: a bitwise update to a key's flags.
///
/// Reserved rather than live. [`super::rollup::latest`] folds these records
/// into `Latest::flags`, but nothing in the workspace writes one and nothing
/// reads the folded result, so no log contains them today. The reader is kept
/// so that a writer can be added without a format change.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Flag {
    pub value: u16,
    pub operator: BitOperation,
}

/// The metadata fields of a key that can be modified.
///
/// When a field is non None, the field is updated for the key.
/// Fields set to None are not modified.
#[derive(
    Default,
    Clone,
    Copy,
    serde::Serialize,
    serde::Deserialize,
    Debug,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
pub struct Modifications {
    /// Changes the time to live to the given value.
    ///
    /// Access time is not here: it is the one field a read changes, and it
    /// travels as an [`Access`] record so the frequent case folds on its own.
    pub ttl_ms: Change<Option<u64>>,
}

/// Payload of a [`KIND_DROP`] record: everything older than this is gone.
///
/// Empty on purpose -- the record's own tag is the whole message, so there is
/// nothing to carry. Reading a DROP discards every record tagged below it,
/// which is how a delete, an overwrite and an eviction are all expressed in an
/// append-only log: the old records stay until a merge collapses them away, but
/// stop counting the moment the DROP lands.
///
/// The comparison is strict, so records sharing the DROP's tag survive it. Put
/// and blob completion rely on that, writing their DROP and their new value
/// under one tag to replace a key's contents in a single record run.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct DropBefore;

/// Payload of a [`KIND_ACCESS`] record: that a key was read, and when.
///
/// A read is the most frequent thing that happens to a key, and every one of
/// them appends a record. Its own kind rather than a [`Metadata::Change`]
/// because these fold with nothing but a comparison -- keep the later time --
/// so any number of them collapse to one without consulting anything else.
///
/// It also makes them recognisable as not being content. A row holding only
/// access records describes a key whose value is gone, so `is_fully_dropped`
/// can reclaim it; while they were metadata changes, they were indistinguishable
/// from a description of a value that is still there.
#[derive(Debug, Serialize, Deserialize, Archive, RkyvSerialize, RkyvDeserialize)]
pub struct Access {
    /// The newest access time seen, in seconds. Only ever moves forward.
    pub latest: u64,
}
