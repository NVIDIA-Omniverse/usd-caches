// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use rocksdb::MergeOperands;

use crate::environment::rocks_adapter::log::rollup;

pub fn merge_operands(
    _new_key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    if operands.is_empty() {
        return None;
    }
    let mut value = existing_val.unwrap_or_default().to_vec();

    for op in operands {
        value.extend_from_slice(op);
    }

    Some(value)
}

pub fn merge_full(
    _key: &[u8],
    existing_val: Option<&[u8]>,
    operands: &MergeOperands,
) -> Option<Vec<u8>> {
    if operands.is_empty() {
        return None;
    }

    let existing = existing_val.unwrap_or_default();
    let capacity = existing.len() + operands.iter().map(<[u8]>::len).sum::<usize>();

    // Base value first, then operands oldest -> newest.
    let mut chronological = Vec::with_capacity(capacity);
    chronological.extend_from_slice(existing);
    for op in operands {
        chronological.extend_from_slice(op);
    }

    // Safe to collapse here, and the only place it can happen: RocksDB calls a
    // full merge only once it holds the key's whole history -- either it found
    // the `Put` the operands sit on, which supersedes everything below it, or it
    // is certain nothing older exists (`merge_helper.cc`, `surely_seen_the_beginning`).
    // Where it cannot be sure it declines to full merge and lets the operands
    // sink a level instead. A partial merge has no such guarantee, which is why
    // `merge_operands` above only concatenates.
    //
    // This is also the only thing that bounds a row: the compaction filter can
    // drop a dead row whole, but nothing else trims a live one, and every read
    // appends an access record.
    Some(rollup::clean(&chronological))
}
