// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use etcetera::{choose_base_strategy, BaseStrategy};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// omniverse.toml, this is not exhaustive list, only ones hub cares about
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Omniverse {
    #[serde(default)]
    pub paths: Paths,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Paths {
    /// root directory for omniverse apps to use for cached data
    #[serde(default = "default_cache_root")]
    pub cache_root: PathBuf,
    /// root directory for omniverse apps to use for app specific data
    #[serde(default = "default_data_root")]
    pub data_root: PathBuf,
    /// root directory for omniverse packages (apps installed from launcher)
    #[serde(default = "default_library_root")]
    pub library_root: PathBuf,
    /// root directory for all omniverse apps log files
    #[serde(default = "default_logs_root")]
    pub logs_root: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            cache_root: default_cache_root(),
            data_root: default_data_root(),
            library_root: default_library_root(),
            logs_root: default_logs_root(),
        }
    }
}

// if any of the dirs dirs fails, fallback to this
fn fallback_dir() -> PathBuf {
    PathBuf::from(".nvidia-omniverse")
}

// defaults taken from
// the Omniverse Launcher settings (LauncherSettings.ts).

pub(super) fn default_cache_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.cache_dir())
        .unwrap_or(fallback_dir())
        .join("ov/cache")
}

pub(super) fn default_data_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.data_dir())
        .unwrap_or(fallback_dir())
        .join("ov/data")
}

// NOTE: The data_dir location on Windows is AppData\Roaming, however we've
//       standardized on installing Hub to AppData\Local. So use cache_dir
//       here instead.

#[cfg(target_os = "windows")]
pub(super) fn default_library_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.cache_dir())
        .unwrap_or(fallback_dir())
        .join("ov/pkg")
}

#[cfg(not(target_os = "windows"))]
pub(super) fn default_library_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.data_dir())
        .unwrap_or(fallback_dir())
        .join("ov/pkg")
}

pub(super) fn default_logs_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.home_dir().to_path_buf())
        .unwrap_or(fallback_dir())
        .join(".nvidia-omniverse/logs")
}
