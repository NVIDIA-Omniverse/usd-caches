// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

/// Key-pair metadata.
///
/// Metadata is the same for all key-pairs in storage. Every pair has a metadata
/// entry.
#[derive(
    Debug,
    Copy,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Metadata {
    /// The time to live (in milliseconds) of the key.
    pub ttl_ms: Option<u64>,

    /// The time at which the key was created as a UNIX timestamp.
    pub created_secs: u64,

    /// The time at which the entry was last accessed, as a UNIX timestamp in
    /// seconds.
    ///
    /// This is not bucketed. Access times are coalesced in memory before they
    /// reach the database, so the stored value trails the true access time by
    /// up to one LRU flush interval.
    pub access_time_secs: u64,
}

impl Default for Metadata {
    fn default() -> Self {
        let time = crate::now_secs();

        Self {
            ttl_ms: None,
            created_secs: time,
            access_time_secs: time,
        }
    }
}

impl Metadata {
    /// Converts from the DB representation to [Metadata].
    pub(crate) fn from_bytes(input: &[u8]) -> Option<Self> {
        if input.len() < 24 {
            return None;
        }

        let expires_secs = u64::from_le_bytes(input[0..8].try_into().unwrap());
        let ttl = (expires_secs > 0).then_some(expires_secs);
        let created_secs = u64::from_le_bytes(input[8..16].try_into().unwrap());
        let access_secs = u64::from_le_bytes(input[16..24].try_into().unwrap());

        Some(Self {
            ttl_ms: ttl,
            created_secs,
            access_time_secs: access_secs,
        })
    }

    /// Converts this [Metadata] to bytes.
    pub(crate) fn to_bytes(self) -> [u8; 24] {
        let mut buffer = [0u8; 24];
        buffer[0..8].copy_from_slice(&self.ttl_ms.unwrap_or(0).to_le_bytes());
        buffer[8..16].copy_from_slice(&self.created_secs.to_le_bytes());
        buffer[16..24].copy_from_slice(&self.access_time_secs.to_le_bytes());

        buffer
    }

    /// Returns true if the item is expired.
    ///
    /// A `ttl_ms` of zero means "never expires"; that is the sentinel, not a
    /// large value. The arithmetic is done in `u128` because both operands can
    /// reach the top of their range: `ttl_ms` arrives unvalidated from the
    /// client, and `created_secs` is read raw off disk. Widening only the sum,
    /// as this once did, still overflows -- a client sending `u64::MAX` as an
    /// "infinite TTL" wrapped to just before its own creation time and read
    /// back as expired on the very first get.
    pub fn is_expired(&self, now_ms: u128) -> bool {
        if let Some(ttl_ms) = self.ttl_ms {
            if ttl_ms == 0 {
                return false;
            }
            if now_ms > (self.created_secs as u128 * 1000) + ttl_ms as u128 {
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use crate::metadata::Metadata;

    #[test]
    fn metadata_serialize_test() {
        let start = Metadata {
            ttl_ms: Some(1000),
            created_secs: 1337,
            access_time_secs: 123123,
        };

        let end = Metadata::from_bytes(&start.to_bytes()).unwrap();

        assert_eq!(start.ttl_ms, end.ttl_ms);
        assert_eq!(start.created_secs, end.created_secs);
        assert_eq!(start.access_time_secs, end.access_time_secs);
    }

    #[test]
    fn test_expire_time() {
        let md = Metadata {
            ttl_ms: Some(1000),
            created_secs: 100,
            access_time_secs: 123123,
        };

        assert!(md.is_expired(100000 + 1001));
        assert!(!md.is_expired(0));
    }

    /// `ttl_ms` arrives unvalidated from the client as a `uint64`. A client
    /// using `u64::MAX` to mean "never expires" must not get an entry that
    /// reads back as already expired.
    #[test]
    fn test_expire_time_does_not_overflow_on_a_huge_ttl() {
        let md = Metadata {
            ttl_ms: Some(u64::MAX),
            created_secs: 1_750_000_000,
            access_time_secs: 0,
        };

        assert!(!md.is_expired(1_750_000_000_000));
    }

    /// `created_secs` is read raw off disk, so a corrupted or foreign-format
    /// row can push the left operand to the top of its range on its own.
    #[test]
    fn test_expire_time_does_not_overflow_on_a_huge_creation_time() {
        let md = Metadata {
            ttl_ms: Some(1000),
            created_secs: u64::MAX,
            access_time_secs: 0,
        };

        assert!(!md.is_expired(1_750_000_000_000));
    }
}
