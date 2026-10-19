// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod block_cache;
pub mod cache_aside;
pub mod conversions;
pub mod proto_codec;

pub mod proto {
    include!(concat!(
        env!("OUT_DIR"),
        "/nvidia.omniverse.usd_caches.local_cache.v1beta.rs"
    ));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum RequestParams {
    GetVersionInfoParamsDeprecated(GetVersionInfoParams),
    GetVersionInfoParams(GetVersionInfoParams),
    ConnectParams(ConnectParams),
    CacheAsideCloseParams(cache_aside::CloseParams),
    CacheAsideConnectParams(cache_aside::ConnectParams),
    CacheAsideGetServerApiVersionParams(cache_aside::GetServerApiVersionParams),
    CacheAsideOpenForReadParams(cache_aside::OpenForReadParams),
    CacheAsideOpenForWriteParams(cache_aside::OpenForWriteParams),
    CacheAsideRemoveParams(cache_aside::RemoveParams),
    BlockConnectParams(block_cache::ConnectParams),
    BlockGetCloseParams(block_cache::CloseParams),
    BlockGetOpenParams(block_cache::GetParams),
    BlockPutAbortParams(block_cache::AbortParams),
    BlockPutCommitParams(block_cache::CommitParams),
    BlockPutOpenParams(block_cache::PutParams),
    BlockStatParams(block_cache::StatParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(u8)]
pub enum OperationResponse {
    GetVersionInfoResponse(GetVersionInfoResponse),
    ConnectResponse,
    CacheAsideCloseResponse(cache_aside::CloseResponse),
    CacheAsideConnectResponse(cache_aside::ConnectResponse),
    CacheAsideGetServerApiVersionResponse(cache_aside::GetServerApiVersionResponse),
    CacheAsideOpenForReadResponse(cache_aside::OpenResponse),
    CacheAsideOpenForWriteResponse(cache_aside::OpenResponse),
    CacheAsideRemoveResponse(cache_aside::RemoveResponse),
    BlockConnectResponse(block_cache::ConnectResponse),
    BlockGetCloseResponse(block_cache::CloseResponse),
    BlockGetOpenResponse(block_cache::GetResponse),
    BlockPutAbortResponse(block_cache::AbortResponse),
    BlockPutCommitResponse(block_cache::CommitResponse),
    BlockPutOpenResponse(block_cache::PutResponse),
    BlockStatResponse(block_cache::StatResponse),
    ErrorResponse(ErrorResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub request_id: u64,
    pub params: RequestParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub request_id: u64,
    pub result: OperationResponse,
}

/// cbindgen:prefix-with-name
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResultCode {
    Success,
    ErrorAbandoned,
    ErrorAlreadyExists,
    ErrorTooLarge,
    ErrorHandleNotFound,
    ErrorInternal,
    ErrorNotFound,
    ErrorPendingWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub message: String,
}

/// Represents a Semantic Version
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(C)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetVersionInfoParams {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectParams {
    pub client_name: String,
    pub client_uuid: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[repr(C)]
pub struct ShutdownResponse {
    pub result: ResultCode,
}

// The interface issuing the request. This is used for version checking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Interface {
    CacheAside,
    BlockCache,
    // The Unknown value lets old clients deserialize new interfaces as they're added
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceVersion {
    pub interface: Interface,
    pub version: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub software_version: SemVer,
    pub interface_versions: Vec<InterfaceVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetVersionInfoResponse {
    pub version_info: VersionInfo,
}
