// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Serialization and deserialization utilities using rkyv.
//!
//! This module provides a centralized interface for serializing and deserializing
//! data structures, replacing the unmaintained bincode crate with rkyv.

use rkyv::api::high::HighSerializer;
use rkyv::rancor::Error;
use rkyv::ser::allocator::ArenaHandle;
use rkyv::util::AlignedVec;

/// Serialize a value into a byte vector using rkyv.
///
/// # Panics
/// Panics if serialization fails.
#[inline]
pub fn serialize<T>(value: &T) -> Vec<u8>
where
    T: for<'a> rkyv::Serialize<HighSerializer<AlignedVec, ArenaHandle<'a>, Error>>,
{
    rkyv::to_bytes::<Error>(value).unwrap().to_vec()
}

/// Deserialize a value from a byte slice using rkyv with validation.
///
/// Returns an error if the bytes are not valid rkyv-serialized data for `T`.
#[inline]
pub fn deserialize<T>(bytes: &[u8]) -> Result<T, Error>
where
    T: rkyv::Archive,
    T::Archived: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, Error>>
        + rkyv::Deserialize<T, rkyv::rancor::Strategy<rkyv::de::Pool, Error>>,
{
    rkyv::from_bytes::<T, Error>(bytes)
}
