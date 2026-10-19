// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use bytes::Buf;
use ovdc_bytes::keys;
use rocksdb::MergeOperands;

use crate::{Change, Update};

use super::value::{self, Invalid};

pub fn merge_operands(
    _new_key: &[u8],
    _existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    Some(_merge_operands(operands.iter()))
}

fn _merge_operands<'a>(operands: impl Iterator<Item = &'a [u8]> + 'a) -> Vec<u8> {
    let operand: Operand = operands
        .filter_map(|op| {
            ovdc_trace::err!(
                crate::serde::deserialize::<Operand>(op),
                "failed to read merge operator ({op:?})"
            )
            .ok()
        })
        .fold(Operand::default(), |last, next| last + next);

    crate::serde::serialize(&operand)
}

pub fn merge_full(
    key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    if operands.is_empty() {
        return None;
    }

    _merge_full(key, existing_val, operands.iter())
}

fn _merge_full<'a>(
    key: &[u8],
    existing_val: Option<&[u8]>,
    operands: impl Iterator<Item = &'a [u8]>,
) -> Option<Vec<u8>> {
    let key = keys::Compound::new_exact(key.try_into().unwrap());

    let (raw, (mut kind, invalid)) = match existing_val {
        Some(x) => (x, value::entry(key, x)),
        _ => {
            // If a merge entity was created but a value did not exist for a key,
            // then `existing_val` is None.
            // The merge operator must always return a value. To deal with this we
            // return a value that is expired.

            tracing::trace!("merge created tombstone value for {key:?}");
            return Some(vec![]);
        }
    };

    // If already marked as tombstone, don't merge anything.
    if invalid.contains(Invalid::TOMBSTONE) {
        return existing_val.map(|y| y.to_vec());
    }

    let metadata = match &mut kind {
        value::StoredKind::KeyValue(_, metadata, _) => metadata,
        value::StoredKind::PartPreamble { metadata, .. } => metadata,
        value::StoredKind::Part { .. } => {
            // Parts cannot be modified by merge operator. This is the only
            // thing guarding them: an earlier check keyed off `reserved[1]`,
            // which nothing ever sets -- the part number lives in `reserved[0]`
            // -- so it never fired and has been removed.
            return existing_val.map(|y| y.to_vec());
        }
    };
    let mut flags = {
        let buf = &mut &raw[..];
        buf.get_u16()
    };

    for op in operands {
        let op: Operand = match ovdc_trace::err!(
            crate::serde::deserialize::<Operand>(op),
            "failed to read merge operator ({op:?})"
        ) {
            Ok(x) => x,
            _ => continue,
        };

        flags |= op.flags;
        if let Some(new_meta) = op.metadata {
            if let Change::Set(access_time) = new_meta.access_time {
                // Access time is a clock: it only moves forward. Operands can
                // arrive carrying a time older than one already applied, so the
                // later of the two wins. The partial-merge fold below already
                // does this -- assigning here instead would make the stored
                // value depend on whether RocksDB happened to squash the
                // operands before this ran.
                metadata.access_time_secs = metadata.access_time_secs.max(access_time);
            }
            if let Change::Set(ttl_ms) = new_meta.ttl_ms {
                metadata.ttl_ms = ttl_ms;
            }
        }
    }

    // TODO it would be better to run the StoredKind through a function to get a value here.
    Some(value::format(flags, &kind))
}

#[derive(
    Debug,
    Default,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]

struct Operand {
    flags: u16,
    metadata: Option<Update>,
}

impl std::ops::Add for Operand {
    type Output = Operand;

    #[allow(clippy::suspicious_arithmetic_impl)]
    fn add(mut self, rhs: Self) -> Self::Output {
        self.flags |= rhs.flags;

        // `self` is the accumulated operand and `rhs` the newer one. A newer
        // operand that changes nothing must leave the accumulated change
        // intact: discarding it drops an update that was already queued.
        match (&mut self.metadata, rhs.metadata) {
            (Some(acc), Some(newer)) => {
                // Whichever access time is later wins.
                acc.access_time = match (newer.access_time, acc.access_time) {
                    (Change::Set(newer), Change::Set(acc)) => {
                        Change::Set(std::cmp::max(newer, acc))
                    }
                    (Change::Set(newer), Change::No) => Change::Set(newer),
                    (Change::No, acc) => acc,
                };

                // The ttl is an assignment rather than a clock, so the newer
                // explicit value wins -- but only if there is one.
                acc.ttl_ms = match newer.ttl_ms {
                    Change::Set(newer) => Change::Set(newer),
                    Change::No => acc.ttl_ms,
                };
            }
            _ => {
                self.metadata = rhs.metadata;
            }
        }

        self
    }
}

pub enum Merge {
    #[allow(unused)]
    OrFlag(u16),
    UpdateMetadata(Update),
    OrFlagAndUpdateMetadta(u16, Update),
}

impl Merge {
    pub fn to_vec(&self) -> Vec<u8> {
        let operand = match self {
            Merge::OrFlag(flags) => Operand {
                flags: *flags,
                metadata: None,
            },
            Merge::UpdateMetadata(new) => Operand {
                flags: 0,
                metadata: Some(*new),
            },
            Merge::OrFlagAndUpdateMetadta(flags, new) => Operand {
                flags: *flags,
                metadata: Some(*new),
            },
        };
        crate::serde::serialize(&operand)
    }
}

#[cfg(test)]
mod tests {
    use ovdc_bytes::keys;

    use crate::{Change, Update};

    use super::super::{multi_part, value};
    use super::Merge;

    /// A merge operand applied to a blob part must leave the part untouched.
    ///
    /// This used to be guarded twice: once by a `reserved[1] == 1` check at the
    /// top of `merge_full`, and once by the `StoredKind::Part` arm. The first
    /// was dead -- see `test_part_numbers_only_ever_use_the_first_reserved_byte`
    /// -- and has been removed, so this pins the behaviour to the arm that
    /// actually provides it.
    #[test]
    fn test_merge_on_a_part_key_leaves_the_part_alone() {
        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(11));
        let part_key = multi_part::key_at(key, 3);

        let stored = value::format(
            0,
            &value::StoredKind::Part {
                key: part_key,
                value: b"part bytes".as_slice().into(),
                random: 0xABCD,
            },
        );

        let operand = Merge::UpdateMetadata(Update {
            access_time: Change::Set(9999),
            ttl_ms: Change::No,
        })
        .to_vec();

        let output = super::_merge_full(
            part_key.as_ref(),
            Some(&stored),
            [operand.as_slice()].into_iter(),
        );

        assert_eq!(
            output.as_deref(),
            Some(stored.as_slice()),
            "a merge operand rewrote a blob part"
        );
    }

    /// The part number lives in `reserved[0]`, and nothing writes `reserved[1]`.
    ///
    /// That is why the removed guard could never fire. If a later change starts
    /// using the second byte, this fails and the assumption above needs
    /// revisiting.
    #[test]
    fn test_part_numbers_only_ever_use_the_first_reserved_byte() {
        let key = keys::Compound::new(Default::default(), keys::Key::from_u64_be(12));

        for part in 0..=u8::MAX {
            let reserved = multi_part::key_at(key, part).reserved();
            assert_eq!(reserved[0], part);
            assert_eq!(
                &reserved[1..],
                &[0, 0, 0],
                "part {part} wrote outside the first reserved byte"
            );
        }
    }

    #[test]
    fn test_merge_operands() {
        let first = Merge::OrFlag(0b001).to_vec();
        let second = Merge::OrFlag(0b100).to_vec();
        let output = super::_merge_operands([&*first, &*second].into_iter());

        let operand: super::Operand = crate::serde::deserialize::<super::Operand>(&output).unwrap();

        assert_eq!(operand.flags, 0b101);

        let first = Merge::OrFlag(0b001).to_vec();
        let second = Merge::UpdateMetadata(Update {
            access_time: Change::Set(1000),
            ttl_ms: Change::No,
        })
        .to_vec();
        let third = Merge::UpdateMetadata(Update {
            access_time: Change::Set(10),
            ttl_ms: Change::No,
        })
        .to_vec();
        let output = super::_merge_operands([&*first, &*second, &*third].into_iter());

        let operand: super::Operand = crate::serde::deserialize::<super::Operand>(&output).unwrap();

        assert!(matches!(
            operand.metadata,
            Some(Update {
                access_time: Change::Set(1000),
                ttl_ms: Change::No
            })
        ));
    }

    /// `put_blob_index` stamps a 4 hour placeholder ttl on the preamble, and
    /// `set_filled` clears it once the blob is complete. If a partial merge
    /// dropped that clear, a finished blob would keep the placeholder and be
    /// reaped as expired four hours after it was written.
    #[test]
    fn test_merge_operands_keep_ttl_change_under_a_newer_operand() {
        let filled = Merge::OrFlagAndUpdateMetadta(
            0b10,
            Update {
                access_time: Change::No,
                ttl_ms: Change::Set(None),
            },
        )
        .to_vec();
        // An LRU access-time refresh, which does not touch the ttl.
        let touch = Merge::UpdateMetadata(Update {
            access_time: Change::Set(1000),
            ttl_ms: Change::No,
        })
        .to_vec();

        let output = super::_merge_operands([&*filled, &*touch].into_iter());
        let operand: super::Operand = crate::serde::deserialize::<super::Operand>(&output).unwrap();

        assert_eq!(operand.flags, 0b10);
        assert!(
            matches!(
                operand.metadata,
                Some(Update {
                    access_time: Change::Set(1000),
                    ttl_ms: Change::Set(None)
                })
            ),
            "newer operand dropped the accumulated ttl change: {:?}",
            operand.metadata
        );
    }

    /// The mirror case: a newer operand that does not touch the access time
    /// must not discard a refresh that is already queued, or the key looks
    /// untouched to the LRU and is evicted early.
    #[test]
    fn test_merge_operands_keep_access_time_under_a_newer_operand() {
        let touch = Merge::UpdateMetadata(Update {
            access_time: Change::Set(1000),
            ttl_ms: Change::No,
        })
        .to_vec();
        let filled = Merge::OrFlagAndUpdateMetadta(
            0b10,
            Update {
                access_time: Change::No,
                ttl_ms: Change::Set(None),
            },
        )
        .to_vec();

        let output = super::_merge_operands([&*touch, &*filled].into_iter());
        let operand: super::Operand = crate::serde::deserialize::<super::Operand>(&output).unwrap();

        assert!(
            matches!(
                operand.metadata,
                Some(Update {
                    access_time: Change::Set(1000),
                    ttl_ms: Change::Set(None)
                })
            ),
            "newer operand dropped the accumulated access time: {:?}",
            operand.metadata
        );
    }
}
