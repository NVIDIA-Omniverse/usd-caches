// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Test-only helpers shared across `ovdc_trace` unit tests.
//!
//! Primarily: a single `EnvGuard` + `ENV_MUTEX` pair so every test that
//! mutates `OTEL_*` env vars serializes through the same lock, even across
//! modules (config.rs and lib.rs both run in the same test binary).

use std::sync::Mutex;

/// Serializes tests that mutate process-global environment variables.
/// Held by every `EnvGuard::{set,unset}` caller for the lifetime of the guard.
pub(crate) static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Sets or unsets env vars for the duration of a test and restores the
/// previous values on drop.
///
/// SAFETY: callers must hold `ENV_MUTEX` (or another mutex that serializes
/// every test in the binary that mutates process env). `std::env::set_var`
/// and `remove_var` are `unsafe` because they race with concurrent readers
/// of the environment block; `ENV_MUTEX` is the serialization point.
pub(crate) struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    pub(crate) fn set(vars: &[(&'static str, &str)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, v)| {
                let prev = std::env::var(k).ok();
                // SAFETY: serialized by ENV_MUTEX held by the caller.
                unsafe { std::env::set_var(k, v) };
                (*k, prev)
            })
            .collect();
        Self(saved)
    }

    pub(crate) fn unset(keys: &[&'static str]) -> Self {
        let saved = keys
            .iter()
            .map(|k| {
                let prev = std::env::var(k).ok();
                // SAFETY: serialized by ENV_MUTEX held by the caller.
                unsafe { std::env::remove_var(k) };
                (*k, prev)
            })
            .collect();
        Self(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, prev) in &self.0 {
            match prev {
                // SAFETY: serialized by ENV_MUTEX held by the caller.
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}
