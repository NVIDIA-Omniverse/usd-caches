// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use bytes::{Buf, BufMut};
use ovdc_bytes::keys;

use crate::{metadata::Metadata, AnyEntry, Value};

use super::incr_part_number;

pub struct Parsed<'a> {
    pub flags: u16,
    pub metadata: Metadata,
    pub stored_value: &'a [u8],
}

impl Parsed<'_> {
    pub fn is_blob(&self) -> bool {
        self.flags & FLAG_BLOB != 0
    }

    pub fn invalid(&self) -> Invalid {
        let mut flags = Invalid::empty();
        if self.flags & FLAG_TOMBSTONE != 0 {
            flags |= Invalid::TOMBSTONE;
        }

        if self.flags & FLAG_BLOB != 0 && self.flags & FLAG_BLOB_FILLED == 0 {
            flags |= Invalid::UNFILLED_BLOB;
        }

        if self.metadata.is_expired(crate::now_ms()) {
            flags |= Invalid::EXPIRED;
        }

        flags
    }
}

pub fn parse(value: &[u8]) -> Option<Parsed<'_>> {
    if value.len() < VALUE_OFFSET {
        return None;
    }

    let flags = u16::from_be_bytes(value[..2].try_into().ok()?);
    let metadata = Metadata::from_bytes(&value[2..])?;

    Some(Parsed {
        flags,
        metadata,
        stored_value: &value[VALUE_OFFSET..],
    })
}

pub const VALUE_OFFSET: usize = 66;

pub const FLAG_BLOB: u16 = 0b1 << 15;
pub const FLAG_BLOB_FILLED: u16 = 0b1 << 14;
pub const FLAG_TOMBSTONE: u16 = 0b1;

pub fn format(flags: u16, kind: &StoredKind<'_>) -> Vec<u8> {
    match kind {
        StoredKind::KeyValue(_key, metadata, value) => {
            let mut buffer: Vec<u8> = Vec::with_capacity(value.as_ref().len() + 256);
            buffer.extend_from_slice(&flags.to_be_bytes());
            buffer.extend_from_slice(&metadata.to_bytes());
            buffer.extend_from_slice(&[0u8; 40]);
            buffer.extend_from_slice(value.as_ref());

            buffer
        }
        StoredKind::PartPreamble {
            metadata,
            size,
            parts,
            random,
            ..
        } => {
            let mut buffer: Vec<u8> = Vec::with_capacity(256);
            buffer.extend_from_slice(&flags.to_be_bytes());
            buffer.extend_from_slice(&metadata.to_bytes());
            buffer.extend_from_slice(&[0u8; 40]);
            buffer.put_u64(*size);
            buffer.put_u8(*parts);
            buffer.put_u128(*random);

            buffer
        }
        StoredKind::Part { value, random, .. } => {
            let mut buffer: Vec<u8> = Vec::with_capacity(64 + value.as_ref().len());
            buffer.put_u128(*random);
            buffer.extend_from_slice(value.as_ref());

            buffer
        }
    }
}

#[derive(Clone)]
pub(super) enum ValueKind<'a> {
    Slice(&'a [u8]),
    ArcVec(Arc<Vec<u8>>),
}

impl<'a> From<&'a [u8]> for ValueKind<'a> {
    fn from(value: &'a [u8]) -> Self {
        ValueKind::Slice(value)
    }
}

impl From<Vec<u8>> for ValueKind<'static> {
    fn from(value: Vec<u8>) -> Self {
        ValueKind::ArcVec(value.into())
    }
}

impl From<Arc<Vec<u8>>> for ValueKind<'static> {
    fn from(value: Arc<Vec<u8>>) -> Self {
        ValueKind::ArcVec(value)
    }
}

impl AsRef<[u8]> for ValueKind<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            ValueKind::Slice(x) => x,
            ValueKind::ArcVec(x) => x,
        }
    }
}

#[derive(Clone)]
pub(super) enum StoredKind<'a> {
    KeyValue(keys::Compound, Metadata, ValueKind<'a>),
    PartPreamble {
        key: keys::Compound,
        metadata: Metadata,
        size: u64,
        parts: u8,
        random: u128,
    },
    Part {
        key: keys::Compound,
        value: ValueKind<'a>,
        random: u128,
    },
}

pub fn higher_order_key(key: &keys::Compound) -> keys::Compound {
    let mut key = *key;
    key.set_reserved([255, 255, 255, 255]);
    key
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid(u8);

impl Invalid {
    pub const EXPIRED: Self = Self(1);
    pub const TOMBSTONE: Self = Self(1 << 1);
    pub const UNFILLED_BLOB: Self = Self(1 << 2);

    #[inline]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for Invalid {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Invalid {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

pub(crate) fn stored_kind_into_entry(key: keys::Compound, kind: StoredKind<'_>) -> AnyEntry {
    let (metadata, value) = match kind {
        StoredKind::KeyValue(_compound, metadata, value_kind) => (
            metadata,
            match value_kind {
                ValueKind::ArcVec(x) => Value::Vec(x),
                _ => Value::Vec(Arc::new(value_kind.as_ref().to_vec())),
            },
        ),
        StoredKind::PartPreamble {
            key,
            metadata,
            size,
            random,
            ..
        } => (
            metadata,
            Value::Blob {
                tag: random,
                size,
                next: {
                    let mut key = key;
                    incr_part_number(&mut key);
                    key
                },
                value: None,
            },
        ),
        StoredKind::Part {
            key, value, random, ..
        } => (
            Default::default(),
            Value::Blob {
                tag: random,
                size: 0,
                next: {
                    let mut key = key;
                    incr_part_number(&mut key);
                    key
                },
                value: Some(match value {
                    ValueKind::ArcVec(x) => x,
                    _ => Arc::new(value.as_ref().to_vec()),
                }),
            },
        ),
    };

    AnyEntry {
        key,
        value,
        metadata,
    }
}

pub fn entry(key: keys::Compound, mut value: &[u8]) -> (StoredKind<'_>, Invalid) {
    // If the value is empty, assume that it is a tombstone created by a bad merge operator.
    if value.len() < 16 {
        return (
            StoredKind::KeyValue(
                key,
                Metadata {
                    ttl_ms: None,
                    created_secs: 0,
                    access_time_secs: 0,
                },
                ValueKind::Slice(&[]),
            ),
            Invalid::TOMBSTONE,
        );
    }

    if is_blob_marker(&key) {
        let random = value.get_u128();
        return (
            StoredKind::Part {
                key,
                value: value.into(),
                random,
            },
            Invalid::empty(),
        );
    }

    let parsed = parse(value).unwrap_or_else(|| {
        tracing::error!(?key, "failed to parse entry");
        Parsed {
            flags: FLAG_TOMBSTONE,
            metadata: Default::default(),
            stored_value: &[],
        }
    });

    if parsed.is_blob() {
        let mut buf = parsed.stored_value;
        return (
            StoredKind::PartPreamble {
                key,
                metadata: parsed.metadata,
                size: buf.get_u64(),
                parts: buf.get_u8(),
                random: buf.get_u128(),
            },
            parsed.invalid(),
        );
    }

    let new_offset = value.len() - parsed.stored_value.len();
    value = &value[new_offset..];

    (
        StoredKind::KeyValue(key, parsed.metadata, value.into()),
        parsed.invalid(),
    )
}

fn is_blob_marker(key: &keys::Compound) -> bool {
    let reserved = key.reserved();
    reserved[0] > 0
}

#[test]
fn test_parsed_is_readable() {
    let mut parsed = Parsed {
        flags: FLAG_BLOB | FLAG_BLOB_FILLED,
        metadata: Metadata {
            ttl_ms: None,
            created_secs: 0,
            access_time_secs: 0,
        },
        stored_value: &[],
    };
    assert!(parsed.invalid().is_empty());

    parsed.flags = FLAG_BLOB;
    assert!(!parsed.invalid().is_empty());

    parsed.flags = 0;
    parsed.metadata.ttl_ms = Some(1);
    assert!(!parsed.invalid().is_empty());
}
