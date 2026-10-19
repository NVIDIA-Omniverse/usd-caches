// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ResultCode;

/// Block Cache API
/// Requests

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectRequest {
    /// Key to reopen
    pub key: Arc<[u8]>,
    /// File extension
    pub extension: Option<Arc<str>>,
    /// Handle of the key
    pub handle: u64,
    /// True if the key is currently being put
    pub open_for_put: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectParams {
    // The list of blocks the client has open
    pub requests: Vec<ConnectRequest>,
}

impl ConnectParams {
    pub fn new() -> Self {
        Self { requests: Vec::new() }
    }
}

impl Default for ConnectParams {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetParams {
    pub keys: Vec<Arc<[u8]>>,
    pub timeout_us: u64,
}

impl GetParams {
    pub fn with_keys(keys: &[impl AsRef<[u8]>]) -> Self {
        Self {
            keys: keys.iter().map(|key| Arc::from(key.as_ref())).collect(),
            timeout_us: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseParams {
    pub handles: Vec<u64>,
}

impl CloseParams {
    pub fn with_handles(handles: &[u64]) -> Self {
        Self {
            handles: handles.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutRequest {
    /// key to put into cache
    pub key: Arc<[u8]>,
    /// size of key's data in bytes to put into cache
    pub size: u64,
    /// Garbage collection rank
    pub rank: u64,
    /// storage data in standalone / unique file with extension
    pub extension: Option<Arc<str>>,
}

impl PutRequest {
    pub fn new(key: impl AsRef<[u8]>, size: u64, rank: u64, extension: Option<Arc<str>>) -> Self {
        Self {
            key: Arc::from(key.as_ref()),
            size,
            rank,
            extension,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutParams {
    pub requests: Vec<PutRequest>,
}

impl PutParams {
    pub fn with_requests(requests: &[PutRequest]) -> Self {
        Self {
            requests: requests.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitParams {
    pub handles: Vec<u64>,
}

impl CommitParams {
    pub fn with_handles(handles: &[u64]) -> Self {
        Self {
            handles: handles.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortParams {
    pub handles: Vec<u64>,
}

impl AbortParams {
    pub fn with_handles(handles: &[u64]) -> Self {
        Self {
            handles: handles.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatParams {
    pub keys: Vec<Arc<[u8]>>,
    pub timeout_us: u64,
}

/// Responses

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// relative path to file (relative to cache directory)
    pub path: PathBuf,
    /// id of data file
    pub id: u64,
    /// offset in bytes to data within data file
    pub offset: u64,
    /// size in bytes of data
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Block {
    pub handle: u64,
    pub location: Location,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectResponse {
    pub results: Vec<ResultCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenResult {
    pub result: ResultCode,
    pub block: Option<Block>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetResponse {
    pub results: Vec<OpenResult>,
    // Base path of the cache directory. Individual result paths must be appended to this to make the full file path
    pub base_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseResponse {
    pub results: Vec<ResultCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutResponse {
    pub results: Vec<OpenResult>,
    // Base path of the cache directory. Individual result paths must be appended to this to make the full file path
    pub base_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResponse {
    pub results: Vec<ResultCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortResponse {
    pub results: Vec<ResultCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    /// seconds since EPOCH of last access
    pub last_accessed: u64,
    /// number of times get() or stat() was called on key
    pub accesses: u64,
    /// size in bytes
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatResult {
    pub result: ResultCode,
    pub metadata: Option<Metadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatResponse {
    pub results: Vec<StatResult>,
}
