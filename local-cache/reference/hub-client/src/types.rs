// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::{c_char, c_void};
use std::ptr::null;
use std::sync::Arc;

use hub_rpc::cache_aside::{CloseResponse, GetServerApiVersionResponse, RemoveResponse};
use hub_rpc::ResultCode;
use hub_rpc::{Interface, RequestParams, Response};

use std::time::Instant;
use tracing::debug;

use crate::error::Error;
use crate::ClientResultCode;
use crate::Result;

pub const CLIENT_NOT_INITIALIZED_STRING: *const c_char = b"Hub Client not initialized\0" as *const u8 as *const c_char;
pub const FAILED_TO_ALLOCATE_STRING: *const c_char =
    b"Failed to allocate error message string\0" as *const u8 as *const c_char;

/// Represents a file stored in the cache
#[repr(C)]
pub struct LocalFile {
    /// Handle that can be provided to close and remove
    pub handle: u64,
    /// Path to file in the local cache
    pub path: *const c_char,
}

/// Response for CacheOpenForRead and CacheOpenForWrite
#[repr(C)]
pub struct CacheOpenResponse {
    // Result of the operation
    pub result: ResultCode,
    /// Pointer to a LocalFile object. This pointer will only be non-null if result is ResultCode::Success
    pub local_file: *const LocalFile,
}

/// Callback for GetServerAPIVersion. Will be called with the response of the operation and provided context pointer
pub type GetServerApiVersionCallback = extern "C" fn(GetServerApiVersionResponse, *mut c_void);

/// Callback for CacheOpenForRead. Will be called with the response of the operation and provided context pointer
///
/// All pointers provided to this callback are only guaranteed to be valid for the duration of this callback.
/// It is the user's responsibility to copy any data from the response if they wish to use it after this callback
/// returns.
pub type CacheOpenForReadCallback = extern "C" fn(CacheOpenResponse, *mut c_void);

/// Callback for CacheOpenForWrite. Will be called with the response of the operation and provided context pointer
///
/// All pointers provided to this callback are only guaranteed to be valid for the duration of this callback.
/// It is the user's responsibility to copy any data from the response if they wish to use it after this callback
/// returns.
pub type CacheOpenForWriteCallback = extern "C" fn(CacheOpenResponse, *mut c_void);

/// Callback for Close. Will be called with the response of the operation and provided context pointer
pub type CloseCallback = extern "C" fn(CloseResponse, *mut c_void);

/// Callback for Remove. Will be called with the response of the operation and provided context pointer
pub type RemoveCallback = extern "C" fn(RemoveResponse, *mut c_void);

/// Callback called when an error occurs. Will be called with the error code, error message, and the provided context
/// pointer
///
/// All pointers provided to this callback are only guaranteed to be valid for the duration of this callback.
/// It is the user's responsibility to copy any data from the response if they wish to use it after this callback
/// returns.
pub type ErrorCallback = extern "C" fn(ClientResultCode, *const c_char, *mut c_void);

#[derive(Debug, Clone)]
pub enum Callbacks {
    GetServerApiVersion(GetServerApiVersionCallback),
    CacheOpenForRead(CacheOpenForReadCallback),
    CacheOpenForWrite(CacheOpenForWriteCallback),
    Close(CloseCallback),
    Remove(RemoveCallback),
}

pub type ClientCallback = Box<dyn FnOnce(Result<Response>) + Send>;

pub struct ClientRequest {
    pub params: RequestParams,
    pub callback: ClientCallback,
    pub interface: Interface,
}

/// Information about a single block in a put request
#[repr(C)]
pub struct ClientPutRequest {
    /// The key being put
    pub key: *const u8,
    /// The length of the key byte array
    pub key_length: u64,
    /// The size of the block
    pub block_size: u64,
    /// The rank of the block. This influences how garbage collection decides what to evice
    pub rank: u64,
}

/// Represents the location of a block in the database
#[repr(C)]
pub struct BlockLocation {
    /// Path to the file containing the block
    pub file_path: *const c_char,
    /// Offset into the file for the start of the block
    pub file_offset: u64,
    /// Size of the block
    pub size: u64,
    /// Unique handle for this instance of the open block. This handle is passed back to get_close, put_commit, or put_abort
    /// when the client is done with the block.
    pub handle: u64,
}

impl BlockLocation {
    pub fn new(file_path: *const c_char, file_offset: u64, size: u64, handle: u64) -> Self {
        Self {
            file_path,
            file_offset,
            size,
            handle,
        }
    }
}

/// Result of a get_open or put_open request for a block.
#[repr(C)]
pub struct BlockResult {
    /// The result of the operation
    pub result: ResultCode,
    /// The actual block information. If result != OmniHubResultCode_Success, this will be a null pointer.
    /// The pointer is only valid for the duration of the callback and should be copied to be used later.
    pub block: *const BlockLocation,
}

impl BlockResult {
    pub fn new(result: ResultCode, block: *const BlockLocation) -> Self {
        Self { result, block }
    }
}

/// The result of a get_open or put_open request.
#[repr(C)]
pub struct OpenResult {
    /// The overall result of the operation.
    pub result: ClientResultCode,
    /// The results for each block in the request. If result != OmniHubResultCode_Success, this will be a null pointer.
    /// The pointer is only valid for the duration of the callback and should be copied to be used later.
    pub blocks: *const BlockResult,
    /// The number of blocks in blocks. If result != OmniHubResultCode_Success, this will be 0.
    pub block_count: u64,
}

impl OpenResult {
    pub fn new(result: ClientResultCode, blocks: *const BlockResult, block_count: u64) -> Self {
        Self {
            result,
            blocks,
            block_count,
        }
    }
}

impl From<ClientResultCode> for OpenResult {
    fn from(value: ClientResultCode) -> Self {
        Self {
            result: value,
            blocks: null(),
            block_count: 0,
        }
    }
}

impl From<&Error> for OpenResult {
    fn from(value: &Error) -> Self {
        Self {
            result: ClientResultCode::from(value),
            blocks: null(),
            block_count: 0,
        }
    }
}

/// Callback for the get_open API method
/// Each block result can be one of:
///  - OmniHubBlockResultCode_Success: if the key was retrieved successfully
///  - OmniHubBlockResultCode_ErrorNotFound: if the key does not exist
///  - OmniHubBlockResultCode_ErrorPendingWrite: if the key is being written
pub type GetOpenCallback = extern "C" fn(OpenResult, *mut c_void);

/// Callback for the put_open API method
/// Each block result can be one of:
///  - OmniHubBlockResultCode_Success: if the key was opened successfully
///  - OmniHubBlockResultCode_ErrorAlreadyExists: if the key already exists in the cache
///  - OmniHubBlockResultCode_ErrorPendingWrite: if the key is being written
pub type PutOpenCallback = extern "C" fn(OpenResult, *mut c_void);

/// The result of a get_close, put_commit, or put_abort operation.
#[repr(C)]
pub struct CloseResult {
    /// The overal result of the operation.
    pub result: ClientResultCode,
    /// The results for each block in the request. If result != OmniHubResultCode_Success, this will be a null pointer.
    /// The pointer is only valid for the duration of the callback and should be copied to be used later.
    pub results: *const ResultCode,
    /// The number of blocks in results. If result != OmniHubResultCode_Success, this will be 0.
    pub result_count: u64,
}

impl CloseResult {
    pub fn new(result: ClientResultCode, results: *const ResultCode, result_count: u64) -> Self {
        Self {
            result,
            results,
            result_count,
        }
    }
}

impl From<ClientResultCode> for CloseResult {
    fn from(value: ClientResultCode) -> Self {
        Self {
            result: value,
            results: null(),
            result_count: 0,
        }
    }
}

impl From<&Error> for CloseResult {
    fn from(value: &Error) -> Self {
        Self {
            result: ClientResultCode::from(value),
            results: null(),
            result_count: 0,
        }
    }
}

/// Callback for the get_close method.
/// Each block result can be one of:
///  - OmniHubBlockResultCode_Success: if the key was closed successfully
///  - OmniHubBlockResultCode_ErrorNotFound: if the key does not exist
///  - OmniHubBlockResultCode_HandleNotFound: if the provided handle doesn't exist
pub type GetCloseCallback = extern "C" fn(CloseResult, *mut c_void);

/// Callback for the put_commit method.
/// Each block result can be one of:
///  - OmniHubBlockResultCode_Success: if the key was closed successfully
///  - OmniHubBlockResultCode_HandleNotFound: if the provided handle doesn't exist
///  - OmniHubBlockResultCode_ErrorAlreadyExists: if the key already exists in the cache
///  - OmniHubBlockResultCode_ErrorPendingWrite: if the key is being written by someone else
pub type PutCommitCallback = extern "C" fn(CloseResult, *mut c_void);

/// Callback for the put_abort method.
/// Each block result can be one of:
///  - OmniHubBlockResultCode_Success: if the key was closed successfully
///  - OmniHubBlockResultCode_HandleNotFound: if the provided handle doesn't exist
///  - OmniHubBlockResultCode_ErrorAlreadyExists: if the key already exists in the cache
///  - OmniHubBlockResultCode_ErrorPendingWrite: if the key is being written by someone else
pub type PutAbortCallback = extern "C" fn(CloseResult, *mut c_void);

/// Data provided to each call of the stat callback
#[repr(C)]
pub struct BlockMetadata {
    /// The size of the block
    pub size: u64,
}

impl BlockMetadata {
    pub fn new(size: u64) -> Self {
        Self { size }
    }
}

/// Data provided to each call of the stat callback
#[repr(C)]
pub struct MetadataResult {
    /// Result of the operation, this can be one of:
    ///  - OmniHubResultCode_Success: if the key was retrieved successfully
    ///  - OmniHubResultCode_ErrorNotFound: if the key does not exist
    pub result: ResultCode,
    /// The actual metadata information. If result != OmniHubResultCode_Success, this will be a null pointer.
    /// The pointer is only valid for the duration of the callback and should be copied to be used later.
    pub metadata: *const BlockMetadata,
}

impl MetadataResult {
    pub fn new(result: ResultCode, metadata: *const BlockMetadata) -> Self {
        Self { result, metadata }
    }
}

/// Data provided to each call of the stat callback
#[repr(C)]
pub struct StatResult {
    /// The overal result of the operation.
    pub result: ClientResultCode,
    /// The metadata for each block in the request. If result != OmniHubResultCode_Success, this will be a null pointer.
    /// The pointer is only valid for the duration of the callback and should be copied to be used later.
    pub metadata: *const MetadataResult,
    /// The number of blocks in metadata. If result != OmniHubResultCode_Success, this will be 0.
    pub metadata_count: u64,
}

impl StatResult {
    pub fn new(result: ClientResultCode, metadata: *const MetadataResult, metadata_count: u64) -> Self {
        Self {
            result,
            metadata,
            metadata_count,
        }
    }
}

impl From<ClientResultCode> for StatResult {
    fn from(value: ClientResultCode) -> Self {
        Self {
            result: value,
            metadata: null(),
            metadata_count: 0,
        }
    }
}

impl From<&Error> for StatResult {
    fn from(value: &Error) -> Self {
        Self {
            result: ClientResultCode::from(value),
            metadata: null(),
            metadata_count: 0,
        }
    }
}

/// Callback for the stat API method
pub type StatCallback = extern "C" fn(StatResult, *mut c_void);

/// Log Levels for messages output to the log callback
/// cbindgen:prefix-with-name
#[repr(C)]
#[derive(PartialEq, PartialOrd, Default, Clone, Copy)]
pub enum LogLevel {
    /// Most verbose
    Trace,
    /// Verbose
    Debug,
    /// Useful runtime information. This is the default.
    #[default]
    Info,
    /// Warnings of potential issues
    Warn,
    /// Only actual errors
    Error,
}

/// Logging callback that will be called with log messages from the Hub Client. Each call will have the level of the log and the
/// log message to print.
/// The strings provided to this function will only be valid for the duration of the function call, so the callback function
/// should log them immediately or copy them to log later.
pub type LogCallback = extern "C" fn(LogLevel, *const c_char);

// Tracing spans cannot be sent between threads, so make a simple timer to measure how long it takes for a callback to be called
pub struct CallbackTimer {
    start: Instant,
    name: String,
    first_key: Option<Arc<[u8]>>,
    first_handle: Option<u64>,
}

impl CallbackTimer {
    pub fn new(name: &str) -> Self {
        Self {
            start: Instant::now(),
            name: name.to_owned(),
            first_key: None,
            first_handle: None,
        }
    }

    pub fn new_with_key(name: &str, key: Option<Arc<[u8]>>) -> Self {
        Self {
            start: Instant::now(),
            name: name.to_owned(),
            first_key: key,
            first_handle: None,
        }
    }

    pub fn new_with_handle(name: &str, handle: Option<&u64>) -> Self {
        Self {
            start: Instant::now(),
            name: name.to_owned(),
            first_key: None,
            first_handle: handle.copied(),
        }
    }

    pub fn end(&self) {
        let end = Instant::now();
        let busy = end.duration_since(self.start);
        let name = &self.name;
        let first_key = &self.first_key;
        let first_handle = self.first_handle;
        debug!(name, ?self.start, ?busy, ?first_key, ?first_handle);
    }
}
