// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;

pub use hub_rpc::block_cache::PutRequest;
use hub_rpc::ResultCode;

use crate::error::ClientResultCode;
use crate::types::{BlockMetadata, CloseResult, OpenResult, StatResult};

use crate::{Client, Error, Result};

/// Gets data for the provided keys
pub async fn get_open(client: &Client, keys: &[&[u8]], timeout_us: u64) -> Result<Vec<BlockResponse>> {
    let key_bytes: Vec<Arc<[u8]>> = keys.iter().map(|k| Arc::from(*k)).collect();

    let mut ctx = OpenContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .get_open_c(&key_bytes, timeout_us, &mut ctx as *mut _ as *mut c_void, open_callback)?;

    ctx.notify.notified().await;
    ctx.results.unwrap()
}

pub async fn get_close(client: &Client, handles: &[u64]) -> Result<Vec<ResultCode>> {
    let mut ctx = CloseContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .get_close_c(handles, &mut ctx as *mut _ as *mut c_void, close_callback)?;

    ctx.notify.notified().await;
    ctx.results.unwrap()
}

/// Begins a put operation for the provided keys.
pub async fn put_open(client: &Client, blocks: Vec<PutRequest>) -> Result<Vec<BlockResponse>> {
    let mut ctx = OpenContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .put_open_c(blocks, &mut ctx as *mut _ as *mut c_void, open_callback)?;
    ctx.notify.notified().await;
    ctx.results.unwrap()
}

pub async fn put_commit(client: &Client, handles: &[u64]) -> Result<Vec<ResultCode>> {
    let mut ctx = CloseContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .put_commit_c(handles, &mut ctx as *mut _ as *mut c_void, close_callback)?;

    ctx.notify.notified().await;
    ctx.results.unwrap()
}

pub async fn put_abort(client: &Client, handles: &[u64]) -> Result<Vec<ResultCode>> {
    let mut ctx = CloseContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .put_abort_c(handles, &mut ctx as *mut _ as *mut c_void, close_callback)?;

    ctx.notify.notified().await;
    ctx.results.unwrap()
}

/// Gets information for the provided keys
pub async fn stat(client: &Client, keys: &[&[u8]], timeout_us: u64) -> Result<Vec<StatResponse>> {
    let key_bytes: Vec<Arc<[u8]>> = keys.iter().map(|k| Arc::from(*k)).collect();

    let mut ctx = StatContext {
        notify: Notify::new(),
        results: None,
    };

    client
        .block_cache
        .stat_c(&key_bytes, timeout_us, &mut ctx as *mut _ as *mut c_void, stat_callback)?;

    ctx.notify.notified().await;
    ctx.results.unwrap()
}

// Rust API Types

/// Represents the location of a block in the block cache
pub struct Block {
    pub file_path: PathBuf,
    pub file_offset: u64,
    pub size: u64,
    pub handle: u64,
}

impl Block {
    pub fn new(file_path: PathBuf, file_offset: u64, size: u64, handle: u64) -> Self {
        Self {
            file_path,
            file_offset,
            size,
            handle,
        }
    }
}

pub struct BlockResponse {
    pub result: ResultCode,
    pub block: Option<Block>,
}

impl BlockResponse {
    pub fn new(result: ResultCode, block: Option<Block>) -> Self {
        Self { result, block }
    }
}

pub struct StatResponse {
    pub result: ResultCode,
    pub metadata: Option<BlockMetadata>,
}

impl StatResponse {
    pub fn new(result: ResultCode, metadata: Option<BlockMetadata>) -> Self {
        Self { result, metadata }
    }
}

struct OpenContext {
    notify: Notify,
    results: Option<Result<Vec<BlockResponse>>>,
}

extern "C" fn open_callback(result: OpenResult, user_data: *mut c_void) {
    let context = user_data as *mut OpenContext;
    match result.result {
        ClientResultCode::Success => unsafe {
            let mut res: Vec<BlockResponse> = Vec::new();
            for i in 0..result.block_count {
                let block = if (*result.blocks.add(i as usize)).block.is_null() {
                    None
                } else {
                    let b = &*(*result.blocks.add(i as usize)).block;
                    Some(Block {
                        file_path: PathBuf::from(CStr::from_ptr(b.file_path).to_str().unwrap()),
                        file_offset: b.file_offset,
                        size: b.size,
                        handle: b.handle,
                    })
                };
                res.push(BlockResponse {
                    result: (*result.blocks.add(i as usize)).result,
                    block,
                })
            }
            (*context).results = Some(Ok(res));
        },
        _ => unsafe {
            (*context).results = Some(Err(Error::Internal(format!("{:?}", result.result))));
        },
    }
    unsafe {
        (*context).notify.notify_one();
    }
}

struct CloseContext {
    notify: Notify,
    results: Option<Result<Vec<ResultCode>>>,
}

extern "C" fn close_callback(result: CloseResult, user_data: *mut c_void) {
    let context = user_data as *mut CloseContext;
    match result.result {
        ClientResultCode::Success => unsafe {
            let mut res: Vec<ResultCode> = Vec::new();
            for i in 0..result.result_count {
                res.push(*result.results.add(i as usize));
            }
            (*context).results = Some(Ok(res));
        },
        _ => unsafe {
            (*context).results = Some(Err(Error::Internal(format!("{:?}", result.result))));
        },
    }
    unsafe {
        (*context).notify.notify_one();
    }
}

struct StatContext {
    notify: Notify,
    results: Option<Result<Vec<StatResponse>>>,
}

extern "C" fn stat_callback(result: StatResult, user_data: *mut c_void) {
    let context = user_data as *mut StatContext;
    match result.result {
        ClientResultCode::Success => unsafe {
            let mut res: Vec<StatResponse> = Vec::new();
            for i in 0..result.metadata_count {
                let metadata = if (*result.metadata.add(i as usize)).metadata.is_null() {
                    None
                } else {
                    let m = &*(*result.metadata.add(i as usize)).metadata;
                    Some(BlockMetadata { size: m.size })
                };
                res.push(StatResponse {
                    result: (*result.metadata.add(i as usize)).result,
                    metadata,
                })
            }
            (*context).results = Some(Ok(res));
        },
        _ => unsafe {
            (*context).results = Some(Err(Error::Internal(format!("{:?}", result.result))));
        },
    }
    unsafe {
        (*context).notify.notify_one();
    }
}
