// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use bytesize::ByteSize;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::hub::SerializeToToml;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Cache {
    /// path to cache database and files, if unset, calculated from omniverse paths
    #[serde(default = "default_path")]
    pub path: Option<PathBuf>,
    /// maximum size of the cache (note cache could exceed this value until garbage collection completes)
    #[serde(default = "default_max_size")]
    pub max_size: ByteSize,
    /// percentage of the cache size that garbage collection will attempt to clean up once it goes over the `max_size`
    /// For example, if max_size is 10GB and this value is 10, when the cache size exceeds 10GB,
    /// garbage collection will attempt to reduce the cache size by 10%, to 9GB.
    #[serde(default = "default_cleanup_threshold")]
    pub cleanup_threshold: u64,
    /// garbage collection background interval (seconds)
    #[serde(default = "default_garbage_collection_interval")]
    pub garbage_collection_interval: u64,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            path: default_path(),
            max_size: default_max_size(),
            cleanup_threshold: default_cleanup_threshold(),
            garbage_collection_interval: default_garbage_collection_interval(),
        }
    }
}

pub(super) fn default_path() -> Option<PathBuf> {
    None
}

pub(super) fn default_max_size() -> ByteSize {
    ByteSize::gb(150)
}

pub(super) fn default_cleanup_threshold() -> u64 {
    10
}

pub(super) fn default_garbage_collection_interval() -> u64 {
    300
}

impl SerializeToToml for Cache {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();
        if let Some(path) = self.path.as_ref() {
            // Print debug to properly escape characters
            result.push(format!("path = {path:?}"));
        }

        if self.max_size != default_max_size() || include_defaults {
            result.push(format!("max_size = \"{}\"", self.max_size));
        }

        if self.cleanup_threshold != default_cleanup_threshold() || include_defaults {
            result.push(format!("cleanup_threshold = {}", self.cleanup_threshold));
        }

        if self.garbage_collection_interval != default_garbage_collection_interval() || include_defaults {
            result.push(format!(
                "garbage_collection_interval = {}",
                self.garbage_collection_interval
            ));
        }

        // Only add header if we're actually serializing values
        if !result.is_empty() {
            result.insert(0, "[cache]".to_owned());
        }

        result
    }
}
