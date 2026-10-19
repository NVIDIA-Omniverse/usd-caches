// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::{cmp::min, num::NonZeroUsize};

use super::SerializeToToml;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Runtime {
    /// number of threads
    #[serde(default = "default_num_worker_threads")]
    pub num_worker_threads: usize,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            num_worker_threads: default_num_worker_threads(),
        }
    }
}

pub(super) fn default_num_worker_threads() -> usize {
    // The default number of worker threads the Hub server will use. If the CPU has fewer than this many cores,
    // then we use the core count instead
    static DEFAULT_WORKER_THREADS: usize = 4;
    min(
        std::thread::available_parallelism()
            .unwrap_or(NonZeroUsize::new(2_usize).unwrap())
            .get(),
        DEFAULT_WORKER_THREADS,
    )
}

impl SerializeToToml for Runtime {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();

        if self.num_worker_threads != default_num_worker_threads() || include_defaults {
            result.push(format!("num_worker_threads = {}", self.num_worker_threads));
        }

        // Only add header if we're actually serializing values
        if !result.is_empty() {
            result.insert(0, "[runtime]".to_owned());
        }

        result
    }
}
