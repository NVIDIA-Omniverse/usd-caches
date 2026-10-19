// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{ResultCode, SemVer};

/// Cache Aside API
/// Requests

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetServerApiVersionParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectParams {
    pub open_files: Vec<OpenFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenFile {
    pub key: Arc<str>,
    pub provider: Arc<str>,
    pub extension: Arc<str>,
    pub handle: u64,
    pub open_for_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenForReadParams {
    pub key: Arc<str>,
    pub provider: Arc<str>,
    pub extension: Arc<str>,
}

impl OpenForReadParams {
    pub fn new(key: impl AsRef<str>, provider: impl AsRef<str>, extension: impl AsRef<str>) -> Self {
        Self {
            key: key.as_ref().into(),
            provider: provider.as_ref().into(),
            extension: extension.as_ref().into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenForWriteParams {
    pub key: Arc<str>,
    pub provider: Arc<str>,
    pub extension: Arc<str>,
    pub size: u64,
}

impl OpenForWriteParams {
    pub fn new(key: impl AsRef<str>, provider: impl AsRef<str>, extension: impl AsRef<str>, size: u64) -> Self {
        Self {
            key: Arc::from(key.as_ref()),
            provider: Arc::from(provider.as_ref()),
            extension: Arc::from(extension.as_ref()),
            size,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseParams {
    pub handle: u64,
}

impl From<u64> for CloseParams {
    fn from(handle: u64) -> Self {
        Self { handle }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveParams {
    pub handle: u64,
}

impl From<u64> for RemoveParams {
    fn from(handle: u64) -> Self {
        Self { handle }
    }
}

/// Responses

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct GetServerApiVersionResponse {
    pub version: SemVer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectResponse {
    pub result: ResultCode,
    pub file_results: Vec<ResultCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenResponse {
    pub result: ResultCode,
    pub local_file: Option<PathBuf>,
    pub handle: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct CloseResponse {
    pub result: ResultCode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct RemoveResponse {
    pub result: ResultCode,
}
