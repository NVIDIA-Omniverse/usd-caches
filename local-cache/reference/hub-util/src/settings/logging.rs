// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use bytesize::ByteSize;
use serde::{Deserialize, Serialize};

use std::{
    fmt::{self, Display},
    path::PathBuf,
};

use super::SerializeToToml;

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    #[default]
    Debug,
    Trace,
}

impl Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LogLevel::Off => write!(f, "off"),
            LogLevel::Error => write!(f, "error"),
            LogLevel::Warn => write!(f, "warn"),
            LogLevel::Info => write!(f, "info"),
            LogLevel::Debug => write!(f, "debug"),
            LogLevel::Trace => write!(f, "trace"),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FileLayer {
    /// is this log layer enabled
    #[serde(default = "default_file_enabled")]
    pub enabled: bool,

    /// format log in json
    #[serde(default = "default_file_json")]
    pub json: bool,

    /// max log level of this layer
    #[serde(default = "default_file_max_level")]
    pub max_level: LogLevel,

    /// file to send log output to
    #[serde(default = "default_file_file")]
    pub file: PathBuf,

    /// number of rotated files to keep
    #[serde(default = "default_file_count")]
    pub count: usize,

    /// file size to rotate log
    #[serde(default = "default_file_max_size")]
    pub max_size: ByteSize,

    /// use compression on rotated files
    #[serde(default = "default_file_compression")]
    pub compression: bool,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct StdIoLayer {
    /// is this log layer enabled
    #[serde(default)]
    pub enabled: bool,

    /// use ANSI colors
    #[serde(default = "default_stdio_color")]
    pub color: bool,

    /// max log level of this layer
    #[serde(default = "default_stdio_max_level")]
    pub max_level: LogLevel,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Logging {
    /// path to cache database and files, if unset, calculated from omniverse paths
    #[serde(default = "default_path")]
    pub path: Option<PathBuf>,
    /// file logging sink
    #[serde(default = "default_file")]
    pub file: FileLayer,
    /// stdout logging sink
    #[serde(default = "default_stdout")]
    pub stdout: StdIoLayer,
    /// stderr logging sink
    #[serde(default = "default_stderr")]
    pub stderr: StdIoLayer,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            path: default_path(),
            file: default_file(),
            stdout: default_stdout(),
            stderr: default_stderr(),
        }
    }
}

pub(super) fn default_path() -> Option<PathBuf> {
    None
}

pub(super) fn default_file_enabled() -> bool {
    true
}

pub(super) fn default_file_file() -> PathBuf {
    PathBuf::from("hub.log")
}

pub(super) fn default_file_json() -> bool {
    true
}

#[cfg(debug_assertions)]
pub(super) fn default_file_max_level() -> LogLevel {
    LogLevel::Debug
}

#[cfg(not(debug_assertions))]
pub(super) fn default_file_max_level() -> LogLevel {
    LogLevel::Info
}

pub(super) fn default_file_count() -> usize {
    7
}

pub(super) fn default_file_max_size() -> ByteSize {
    ByteSize::mib(128)
}

pub(super) fn default_file_compression() -> bool {
    true
}

pub(super) fn default_stdout_enabled() -> bool {
    false
}

pub(super) fn default_stderr_enabled() -> bool {
    false
}

pub(super) fn default_stdio_color() -> bool {
    false
}

pub(super) fn default_stdio_max_level() -> LogLevel {
    LogLevel::default()
}

pub(super) fn default_file() -> FileLayer {
    FileLayer {
        enabled: default_file_enabled(),
        json: default_file_json(),
        max_level: default_file_max_level(),
        file: default_file_file(),
        count: default_file_count(),
        max_size: default_file_max_size(),
        compression: default_file_compression(),
    }
}

pub(super) fn default_stdout() -> StdIoLayer {
    StdIoLayer {
        enabled: default_stdout_enabled(),
        color: default_stdio_color(),
        max_level: default_stdio_max_level(),
    }
}

pub(super) fn default_stderr() -> StdIoLayer {
    StdIoLayer {
        enabled: default_stderr_enabled(),
        color: default_stdio_color(),
        max_level: default_stdio_max_level(),
    }
}

impl SerializeToToml for Logging {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();

        // Path
        if let Some(path) = self.path.as_ref() {
            result.push("[logging]".to_owned());
            // Print debug to properly escape characters
            result.push(format!("path = {path:?}"));
        }

        // File
        {
            let mut file_result = Vec::new();
            if self.file.enabled != default_file_enabled() || include_defaults {
                file_result.push(format!("enabled = {}", self.file.enabled));
            }

            if self.file.json != default_file_json() || include_defaults {
                file_result.push(format!("json = {}", self.file.json));
            }

            if self.file.file != default_file_file() || include_defaults {
                // Print debug to properly escape characters
                file_result.push(format!("file = {:?}", self.file.file));
            }
            if self.file.max_level != default_file_max_level() || include_defaults {
                file_result.push(format!("max_level = \"{}\"", self.file.max_level));
            }

            if !file_result.is_empty() {
                result.push("[logging.file]".to_owned());
                result.append(&mut file_result);
            }
        }

        // Stdout
        {
            let mut stdout_result = Vec::new();

            if self.stdout.enabled != default_stdout_enabled() || include_defaults {
                stdout_result.push(format!("enabled = {}", self.stdout.enabled));
            }

            if self.stdout.color != default_stdio_color() || include_defaults {
                stdout_result.push(format!("color = {}", self.stdout.color));
            }

            if self.stdout.max_level != default_stdio_max_level() || include_defaults {
                stdout_result.push(format!("max_level = \"{}\"", self.stdout.max_level));
            }

            if !stdout_result.is_empty() {
                result.push("[logging.stdout]".to_owned());
                result.append(&mut stdout_result);
            }
        }

        // Stderr
        {
            let mut stderr_result = Vec::new();

            if self.stderr.enabled != default_stderr_enabled() || include_defaults {
                stderr_result.push(format!("enabled = {}", self.stderr.enabled));
            }

            if self.stderr.color != default_stdio_color() || include_defaults {
                stderr_result.push(format!("color = {}", self.stderr.color));
            }

            if self.stderr.max_level != default_stdio_max_level() || include_defaults {
                stderr_result.push(format!("max_level = \"{}\"", self.stderr.max_level));
            }

            if !stderr_result.is_empty() {
                result.push("[logging.stderr]".to_owned());
                result.append(&mut stderr_result);
            }
        }

        result
    }
}
