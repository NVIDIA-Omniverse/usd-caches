// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use thiserror::Error;

pub type Result<T> = ::std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// error from bincode
    #[error(transparent)]
    Bincode(#[from] bincode::Error),
    /// block was abandoned due to restart
    #[error("Block '{0}' has been abandoned")]
    BlockAbandoned(String),
    /// block file offset overflow
    #[error("Block file(id: {0}) offset overflow")]
    BlockFileOffsetOverflow(u64),
    /// database error
    #[error(transparent)]
    Db(#[from] sled::Error),
    /// file already exists
    #[error("File '{0}' already exists")]
    FileAlreadyExists(String),
    /// file missing extension
    #[error("File '{0}' missing extension")]
    FileMissingExtension(String),
    /// file was not found
    #[error("File '{0}' not found")]
    FileNotFound(String),
    /// file is already open for write
    #[error("File '{0}' already open for write")]
    FileOpenForWrite(String),
    /// file is too large to fit in the cache
    #[error("Key '{0:?}' size ({1}) larger than cache size ({2})")]
    KeyTooLarge(Arc<[u8]>, u64, u64),
    /// invalid argument
    #[error("Invalid argument: '{0}'")]
    InvalidArgument(String),
    /// invalid garbage collection interval, must be > 0
    #[error("Cache garbage collection interval must be > 0")]
    InvalidGarbageCollectionInterval,
    /// invalid cleanup threshold, must be 0 >= threshold <= 100
    #[error("Cache cleanup threshold '{0}' must be <= 100")]
    InvalidGarbageCollectionThreshold(u64),
    /// invalid compression factor, must be 1 >= factor <= 22
    #[error("Cache compression factor '{0}' must be >=1 and <= 22")]
    InvalidCompressionFactor(i32),
    /// internal error
    #[error("Internal error: {0}")]
    InternalError(String),
    /// error from io
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// key already exists and is fully committed to cache
    #[error("Key '{0}' already exists")]
    KeyAlreadyExists(String),
    /// key is invalid
    #[error("Key is empty")]
    KeyEmpty,
    /// key was not found in cache
    /// TODO: remove this
    #[error("Key '{0}' does not exist in the cache")]
    KeyNotFound(String),
    /// key was not open
    #[error("Key '{0}'  is not open")]
    KeyNotOpen(String),
    /// key is currently open for put
    #[error("Key '{0}' already open for put")]
    KeyOpenForPut(String),
    /// failure to update LRU record
    #[error(transparent)]
    LRUUpdateError(#[from] tokio::sync::mpsc::error::SendError<(Arc<[u8]>, u64)>),
    /// task Failed
    #[error(transparent)]
    TaskFailed(#[from] tokio::task::JoinError),
    /// serde JSON error
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// system time error
    #[error(transparent)]
    SystemTime(#[from] std::time::SystemTimeError),
    /// UTF-8 error
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    /// WalkDir error
    #[error(transparent)]
    WalkDir(#[from] walkdir::Error),
    /// Windows API error
    #[cfg(target_os = "windows")]
    #[error(transparent)]
    Windows(#[from] windows::core::Error),
    /// tasking operation would block
    #[error(transparent)]
    WouldBlock(#[from] tokio::sync::TryLockError),
    /// semaphore was closed while waiting to be acquired
    #[error(transparent)]
    SemaphoreClosed(#[from] tokio::sync::AcquireError),
}
