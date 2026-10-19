// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Generate semi-random connection ids.

use std::{
    fmt::Display,
    hash::Hasher,
    sync::{atomic::AtomicU32, LazyLock},
};

static RAND: LazyLock<u32> = LazyLock::new(rand::random);

/// A semi-random connection id.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub struct ConnectionId(u32);

impl From<u32> for ConnectionId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        static LAST: AtomicU32 = AtomicU32::new(0);
        let rand = *RAND;

        let id = LAST.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut hasher = seahash::SeaHasher::default();
        hasher.write_u32(rand);
        hasher.write_u32(id);

        Self::from(hasher.finish() as u32)
    }
}

impl Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::fmt::Debug for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl AsRef<u32> for ConnectionId {
    fn as_ref(&self) -> &u32 {
        &self.0
    }
}
