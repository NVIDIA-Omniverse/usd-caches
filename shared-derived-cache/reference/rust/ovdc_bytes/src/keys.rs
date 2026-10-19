// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the different key types that can be used for the KV store.

use std::fmt::{Debug, Display};

use base64::Engine;
use bytes::Buf;

/// The size of the user portion of the key.
///
/// This is the size of [Key].
pub const USER_KEY_SIZE: usize = 32;

/// The number of bytes used to indicate the tenant for the key.
pub const TENANT_KEY_SIZE: usize = 48;

const RESERVED: usize = 4;

/// The static key size for all keys stored in the database.
///
/// This is the size of [Compound].
pub const COMPOUND_KEY_SIZE: usize = USER_KEY_SIZE + TENANT_KEY_SIZE + RESERVED;

/// Represents a user-input key.
///
/// All user keys are [USER_KEY_SIZE] bytes in size. Larger input is not accepted. Input
/// that is smaller than [USER_KEY_SIZE] will be padded with 0s.
#[derive(Default, Clone, Copy)]
pub struct Key {
    key: [u8; USER_KEY_SIZE],
    size: u16,
}

impl From<[u8; USER_KEY_SIZE]> for Key {
    fn from(value: [u8; USER_KEY_SIZE]) -> Self {
        Self {
            key: value,
            size: USER_KEY_SIZE as u16,
        }
    }
}

impl Eq for Key {}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.key.eq(&other.key)
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::hash::Hash for Key {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl Key {
    /// Creates a new [Key] from the input bytes.
    ///
    /// If the key is smaller than 32 bytes, None is returned.
    pub fn new(key: &[u8]) -> Option<Self> {
        if key.len() > USER_KEY_SIZE {
            return None;
        }

        let mut buffer = [0u8; USER_KEY_SIZE];
        buffer[..key.len()].copy_from_slice(key);

        Some(Self {
            key: buffer,
            size: key.len() as u16,
        })
    }

    /// Creates a new [Key] from the input buf.
    pub fn new_from_buf(buf: &mut impl Buf, len: u16) -> Self {
        let mut buffer = [0u8; USER_KEY_SIZE];
        buf.copy_to_slice(&mut buffer[..len as usize]);

        Self {
            key: buffer,
            size: len,
        }
    }

    /// Creates a new [Key] from a u64 number.
    /// This function will create the key using native endian-ness.
    pub fn from_u64_ne(num: u64) -> Self {
        Self::new(&num.to_ne_bytes()).unwrap()
    }

    /// Creates a new [Key] from a u64 number.
    /// This function will create the key using big endian-ness.
    pub fn from_u64_be(num: u64) -> Self {
        Self::new(&num.to_be_bytes()).unwrap()
    }

    /// Returns the user-input key as a slice.
    ///
    /// Note that this slice may be smaller than 256 bytes if the input was
    /// not 256 bytes.
    ///
    /// The size of the original input is kept. The padded 0s are not included
    /// in this returned slice.
    pub fn as_input_slice(&self) -> &[u8] {
        &self.key[..self.size as usize]
    }

    /// Returns the 256 byte key.
    pub fn into_inner(self) -> [u8; USER_KEY_SIZE] {
        self.key
    }

    /// Returns the length of the key. If the input bytes were less than 256
    /// than this function will return that length.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u16 {
        self.size
    }
}

impl From<u64> for Key {
    fn from(value: u64) -> Self {
        Self::from_u64_be(value)
    }
}

impl AsRef<[u8]> for Key {
    fn as_ref(&self) -> &[u8] {
        &self.key
    }
}

/// The tenant portion of a key.
///
/// All stored [Compound] keys begin with the [Tenant]. This is used to
/// potentially partition and identify who is the "owner" of what data.
///
/// Tenants to may use the same keyspace as there cannot be collisions between
/// tennants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tenant {
    key: [u8; TENANT_KEY_SIZE],
}

impl Tenant {
    /// Creates a new [Tenant] from a perfectly sized array.
    ///
    /// All tenant keys must be [TENANT_KEY_SIZE] bytes in size.
    ///
    /// A max value array is not valid.
    pub fn new(key: [u8; TENANT_KEY_SIZE]) -> Self {
        if key == [255u8; TENANT_KEY_SIZE] {
            panic!("tennant key is not allowed to be max value");
        }
        Self { key }
    }

    /// Creates a new [Tenant] from a u64.
    ///
    /// A max value u32 is not a valid value.
    pub fn from_u64_be(value: u32) -> Self {
        let mut key = [0u8; TENANT_KEY_SIZE];
        key[..4].copy_from_slice(&value.to_be_bytes()[..]);
        Self::new(key)
    }

    /// Returns a [Tenant] that is reserved for use by the system.
    pub fn system_reserved() -> Self {
        Self {
            key: [255u8; TENANT_KEY_SIZE],
        }
    }

    /// Attempts to convert self in to a printable string
    pub fn with_str<F, O>(&self, with: F) -> O
    where
        F: FnOnce(Option<&str>) -> O,
    {
        tenant_bytes_as_str(&self.key, with)
    }
}

impl AsRef<[u8]> for Tenant {
    fn as_ref(&self) -> &[u8] {
        &self.key[..]
    }
}

const EMPTY_TENANT: [u8; TENANT_KEY_SIZE] = [0u8; TENANT_KEY_SIZE];

impl Default for Tenant {
    fn default() -> Self {
        Self { key: EMPTY_TENANT }
    }
}

impl serde::Serialize for Tenant {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        tenant_bytes_as_str(&self.key, |as_str| match as_str {
            Some(x) => serializer.serialize_str(x),
            _ => serializer.serialize_str("null"),
        })
    }
}

fn tenant_bytes_as_str<F, O>(tenant: &[u8; TENANT_KEY_SIZE], with: F) -> O
where
    F: FnOnce(Option<&str>) -> O,
{
    if tenant == &EMPTY_TENANT {
        return with(None);
    }

    fn trim(input: &[u8]) -> &[u8] {
        let mut i = 0;
        for b in input.iter().rev().copied() {
            if b != 0 {
                break;
            }
            i += 1;
        }
        &input[..input.len() - i]
    }

    // If the bytes pass for utf-8, display a string.
    if let Ok(as_str) = std::str::from_utf8(trim(tenant)) {
        let mut hex = [0u8; TENANT_KEY_SIZE + 4];
        hex[..4].copy_from_slice(b"str_");
        let len = 4 + as_str.len();
        hex[4..len].copy_from_slice(as_str.as_bytes());
        return with(Some(std::str::from_utf8(&hex[..len]).unwrap()));
    }

    // Otherwise use base64
    let mut hex = [0u8; TENANT_KEY_SIZE * 2];
    hex[..4].copy_from_slice(b"b64_");
    let len = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode_slice(tenant, &mut hex[4..])
        .unwrap();

    with(Some(std::str::from_utf8(&hex[..4 + len]).unwrap()))
}

/// An aggregate of [Tenant] and [Key]. Allows for multiple portions of a key
/// to be easily controlled.
///
/// A [Compound] places the [Tenant] before the [Key]. This allows the key space
/// to be shared.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Compound {
    key: [u8; COMPOUND_KEY_SIZE],
}

impl Default for Compound {
    fn default() -> Self {
        Self {
            key: [0u8; COMPOUND_KEY_SIZE],
        }
    }
}

impl AsRef<[u8]> for Compound {
    fn as_ref(&self) -> &[u8] {
        &self.key
    }
}

impl Compound {
    /// Constructs a [Compound] key from a [Tenant] and [Key].
    pub fn new(tenant: Tenant, user: Key) -> Self {
        let mut buffer = [0u8; COMPOUND_KEY_SIZE];

        buffer[..TENANT_KEY_SIZE].copy_from_slice(&tenant.key);
        buffer[TENANT_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE].copy_from_slice(&user.key);

        Self { key: buffer }
    }

    /// Construct a [Compound] from an exact sized key.
    pub fn new_exact(key: [u8; COMPOUND_KEY_SIZE]) -> Self {
        Self { key }
    }

    /// Attempt to construct a [Compound] from a slice.
    pub fn try_from_slice(key: &[u8]) -> Option<Self> {
        Some(Self::new_exact(key.try_into().ok()?))
    }

    /// Returns the key as a slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.key[..]
    }

    /// Returns the user portion of the key as a slice.
    pub fn as_user_slice(&self) -> &[u8] {
        &self.key[TENANT_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE]
    }

    /// Returns the key as an array.
    pub fn into_inner(self) -> [u8; COMPOUND_KEY_SIZE] {
        self.key
    }

    /// Converts the [Compound] key into its [Tenant] and [Key] components.
    pub fn into_parts(self) -> (Tenant, Key) {
        let tenant = {
            let mut key = [0u8; TENANT_KEY_SIZE];
            key.copy_from_slice(&self.key[..TENANT_KEY_SIZE]);
            Tenant { key }
        };
        let user = {
            let mut key = [0u8; USER_KEY_SIZE];
            key.copy_from_slice(&self.key[TENANT_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE]);
            Key {
                key,
                size: USER_KEY_SIZE as u16,
            }
        };

        (tenant, user)
    }

    /// Checks if the marker byte is equal to 'marker'.
    pub fn is_mark(&mut self, marker: u8) -> bool {
        self.key[self.key.len() - 1] == marker
    }

    /// Provides a slice of bytes that comprise the user portion of the key.
    pub fn user_key_slice(&self) -> &[u8] {
        &self.key[TENANT_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE]
    }

    /// Sets the reserved bytes to some value.
    pub fn set_reserved(&mut self, value: [u8; RESERVED]) {
        self.key[TENANT_KEY_SIZE + USER_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE + RESERVED]
            .copy_from_slice(&value);
    }

    /// Gets the reserved bytes.
    pub fn reserved(&self) -> [u8; RESERVED] {
        let slice =
            &self.key[TENANT_KEY_SIZE + USER_KEY_SIZE..TENANT_KEY_SIZE + USER_KEY_SIZE + RESERVED];
        slice.try_into().unwrap()
    }

    /// Returns a hash of the entire key.
    pub fn hash(&self) -> u64 {
        seahash::hash(self.as_slice())
    }
}

impl Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = seahash::hash(&self.key[..self.size as usize]);
        f.write_fmt(format_args!("user_kfp{hash:x}"))
    }
}

impl Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self, f)
    }
}

impl Display for Tenant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("tenant_{:x?}", &self.key))
    }
}

impl Display for Compound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("compound_{:x?}", &self.key))
    }
}

impl Debug for Compound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hash = seahash::hash(self.as_slice());
        f.write_fmt(format_args!("kfp{hash:x}"))
    }
}

#[cfg(test)]
mod test {
    use crate::keys::EMPTY_TENANT;

    use super::TENANT_KEY_SIZE;

    #[test]
    fn test_tenant_serialzes() {
        let mut buf = [0u8; TENANT_KEY_SIZE];
        {
            let example = "example-tenant-000000000000000000000000000b";
            buf[..example.len()].copy_from_slice(example.as_bytes());
        }
        let as_str = super::tenant_bytes_as_str(&buf, |as_str| as_str.map(|y| y.to_owned()));
        assert_eq!(
            as_str.as_deref(),
            Some("str_example-tenant-000000000000000000000000000b")
        );
        // Make the utf8 invalid
        buf[0] = 0b1110_0000;
        buf[1] = 0b0000_1111;

        // Must be a base64 string
        let as_str = super::tenant_bytes_as_str(&buf, |as_str| as_str.map(|y| y.to_owned()));
        assert_eq!(
            as_str.as_deref(),
            Some("b64_4A9hbXBsZS10ZW5hbnQtMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwYgAAAAAA")
        );

        let as_str =
            super::tenant_bytes_as_str(&EMPTY_TENANT, |as_str| as_str.map(|y| y.to_owned()));
        assert!(as_str.is_none());
    }
}
