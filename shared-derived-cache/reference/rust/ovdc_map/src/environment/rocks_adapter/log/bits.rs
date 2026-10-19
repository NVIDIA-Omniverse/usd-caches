// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use bytes::{Buf, BufMut};
use ovdc_trace::metrics::Measure;
use smallvec::SmallVec;

use super::models::Kind;
use crate::environment::rocks_adapter::metrics;

/// Version of the record framing, written as the first byte of every record.
///
/// Lets a reader reject a layout it does not understand rather than
/// misinterpreting it.
///
/// The value is chosen to be one no older format could produce in this
/// position. Before versioning, a record began with its kind byte, and those
/// ran `0..=4`; a legacy-mode stored value begins with a zero byte. Any of
/// those is rejected here rather than being read as a version this reader
/// claims to understand -- which is what a low number would have allowed, since
/// `1` was the old blob record's kind.
///
/// New versions should keep the high bit set for the same reason.
pub const VERSION: u8 = 0x81;

/// Size of a record header: version (u8), `kind` (u8), payload length (u32) and
/// `tag` (u128).
pub const HEADER_LEN: usize = 1 + 1 + 4 + 16;

pub fn write<T>(kind: Kind, tag: u128, value: &T, writer: &mut impl std::io::Write)
where
    T: for<'a> rkyv::Serialize<
        rkyv::api::high::HighSerializer<
            rkyv::util::AlignedVec,
            rkyv::ser::allocator::ArenaHandle<'a>,
            rkyv::rancor::Error,
        >,
    >,
{
    // `serialize` already returns an owned buffer, so writing the header and
    // the payload straight through costs one copy less than staging both in a
    // fixed-size buffer first -- and there is no size at which it stops working.
    write_raw(kind, tag, &crate::serde::serialize(value), writer);
}

/// Frames an already-serialized payload as a record.
pub fn write_raw(kind: Kind, tag: u128, payload: &[u8], writer: &mut impl std::io::Write) {
    let mut header = [0u8; HEADER_LEN];
    {
        let mut buf = &mut header[..];
        buf.put_u8(VERSION);
        buf.put_u8(kind.0);
        buf.put_u32(payload.len() as u32);
        buf.put_u128(tag);
    }

    writer.write_all(&header).unwrap();
    writer.write_all(payload).unwrap();
}

/// A record read out of a log: its framed bytes, its payload, and its header.
///
/// `Copy` because it is only ever borrowed from the buffer being read -- passing
/// one around costs four words and never the record.
#[derive(Clone, Copy)]
pub struct Next<'a> {
    pub raw: &'a [u8],
    pub value: &'a [u8],

    pub tag: u128,
    pub kind: Kind,
}

/// Reads the next record, or `None` once the log is exhausted.
///
/// Framing that does not make sense -- an unrecognised version, or a length
/// running past the end of the value -- also yields `None`. The framing is
/// self-described, so once it stops making sense there is no way to find where
/// the following record would begin. The caller keeps whatever prefix decoded,
/// and collapsing the log drops the rest.
pub fn _next<'a>(buffer: &mut &'a [u8]) -> Option<Next<'a>> {
    if buffer.len() < HEADER_LEN {
        if !buffer.is_empty() {
            *buffer = &[];
            corrupt("trailing bytes are shorter than a record header");
        }
        return None;
    }

    let (version, kind, len, tag) = {
        let mut header = &buffer[..HEADER_LEN];
        (
            header.get_u8(),
            Kind(header.get_u8()),
            header.get_u32() as usize,
            header.get_u128(),
        )
    };

    if version != VERSION {
        *buffer = &[];
        corrupt("unrecognised log record version");
        return None;
    }

    if HEADER_LEN + len > buffer.len() {
        *buffer = &[];
        corrupt("log record length runs past the end of the value");
        return None;
    }

    let (raw, rest) = buffer.split_at(HEADER_LEN + len);
    let value = &raw[HEADER_LEN..];

    *buffer = rest;

    Some(Next {
        raw,
        value,
        tag,
        kind,
    })
}

/// Reads the next record, or `None` once the log is exhausted.
///
/// Framing that does not make sense -- an unrecognised version, or a length
/// running past the end of the value -- also yields `None`. The framing is
/// self-described, so once it stops making sense there is no way to find where
/// the following record would begin. The caller keeps whatever prefix decoded,
/// and collapsing the log drops the rest.
pub fn next<'a>(buffer: &mut &'a [u8]) -> Option<(Kind, u128, &'a [u8])> {
    let next = _next(buffer)?;
    Some((next.kind, next.tag, next.value))
}

/// Records that a log was truncated at a record we could not make sense of.
///
/// The remainder is dropped rather than served, so this is data loss and is
/// reported as an error, not a warning.
pub(super) fn corrupt(reason: &str) {
    metrics::ERROR.measure(1, &ovdc_trace::label!("kind": "log_record_corrupt"));
    tracing::error!("{reason}; discarding the remainder of the log for this key");
}

/// Inline capacity of [`ArrayBuf`].
///
/// Sized for the records written per key inside the `retain` and `update_many`
/// loops -- DROP, BLOB and metadata records, a few hundred bytes at most -- so
/// those stay allocation-free. An inline value can be far larger, but that path
/// already allocates while serializing, so spilling to the heap there costs
/// nothing over what it does today.
const INLINE_CAPACITY: usize = 1024;

/// A growable buffer for accumulating log records.
#[derive(Default)]
pub struct ArrayBuf(SmallVec<[u8; INLINE_CAPACITY]>);

impl ArrayBuf {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl std::io::Write for ArrayBuf {
    fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(value);
        Ok(value.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub mod common {
    use crate::{
        environment::rocks_adapter::log::{
            models::{self, DropBefore, InlineValue, KIND_DROP, KIND_INLINE_VALUE, KIND_METADATA},
            snowflake,
        },
        metadata::Metadata,
    };

    pub fn log_inline_value_change(
        metadata: Metadata,
        value: &[u8],
        writer: &mut impl std::io::Write,
    ) {
        let snowflake = snowflake::next();

        super::write(KIND_DROP, snowflake, &DropBefore, writer);
        super::write(
            KIND_INLINE_VALUE,
            snowflake,
            &InlineValue {
                inner: value.to_vec(),
            },
            writer,
        );
        super::write(
            KIND_METADATA,
            snowflake,
            &models::Metadata::Set(metadata),
            writer,
        );
    }
}
