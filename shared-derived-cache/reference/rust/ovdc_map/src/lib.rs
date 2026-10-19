// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![deny(missing_docs)]
#![recursion_limit = "1024"]

//! An abstract non-volatile key-value store.
//!
//! This crate provides abstraction over a key-value store. The currently
//! selected store is Rocks. This may not always be the case.

/// Allows a KV environment to be opened over a working directory.
pub mod environment;
/// Key-value pairs have associated metadata that is not user defined.
pub mod metadata;

mod adapter;
mod serde;

pub use adapter::*;

#[cfg(test)]
mod tests;

/// Current time in milliseconds.
///
/// A host clock set before 1970 reads as zero rather than panicking. These sit
/// on the read and write paths, and the release profile sets `panic = 'abort'`,
/// so a bad RTC would take the process down on the first request instead of
/// merely making everything look freshly created.
pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|y| y.as_millis())
        .unwrap_or_default()
}

/// Current time in seconds.
///
/// See [`now_ms`] for the pre-1970 behaviour.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|y| y.as_secs())
        .unwrap_or_default()
}
