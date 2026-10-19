// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use uuid::Uuid;

use super::block_cache as bc;
use super::cache_aside as ca;
use super::*;

////////////////////////////////////////////////////////////////////////////////
// Utility
////////////////////////////////////////////////////////////////////////////////

pub fn convert_vec<T, U>(v: Vec<T>) -> Vec<U>
where
    T: Into<U>,
{
    v.into_iter().map(Into::into).collect()
}

pub fn convert_rvec<T, U>(v: &[T]) -> Vec<U>
where
    U: for<'a> std::convert::From<&'a T>,
{
    v.iter().map(Into::into).collect()
}

////////////////////////////////////////////////////////////////////////////////
// Common
////////////////////////////////////////////////////////////////////////////////

impl From<ResultCode> for i32 {
    fn from(val: ResultCode) -> Self {
        val as i32
    }
}

impl From<i32> for ResultCode {
    fn from(val: i32) -> ResultCode {
        match val {
            0 => ResultCode::Success,
            1 => ResultCode::ErrorAbandoned,
            2 => ResultCode::ErrorAlreadyExists,
            3 => ResultCode::ErrorTooLarge,
            4 => ResultCode::ErrorHandleNotFound,
            5 => ResultCode::ErrorInternal,
            6 => ResultCode::ErrorNotFound,
            7 => ResultCode::ErrorPendingWrite,
            _ => ResultCode::ErrorInternal,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Request -> proto::Request (for client)
////////////////////////////////////////////////////////////////////////////////

impl From<Request> for proto::Request {
    fn from(val: Request) -> Self {
        proto::Request {
            request_id: val.request_id,
            params: Some(val.params.into()),
        }
    }
}

impl From<RequestParams> for proto::RequestParams {
    fn from(val: RequestParams) -> Self {
        proto::RequestParams {
            params: Some(val.into()),
        }
    }
}

impl From<RequestParams> for proto::request_params::Params {
    fn from(val: RequestParams) -> Self {
        match val {
            RequestParams::GetVersionInfoParamsDeprecated(p) => {
                proto::request_params::Params::ServiceVersionInfo(p.into())
            },
            RequestParams::GetVersionInfoParams(p) => proto::request_params::Params::ServiceVersionInfo(p.into()),
            RequestParams::ConnectParams(p) => proto::request_params::Params::ClientConnect(p.into()),
            RequestParams::CacheAsideCloseParams(p) => proto::request_params::Params::CacheAsideClose(p.into()),
            RequestParams::CacheAsideConnectParams(p) => proto::request_params::Params::CacheAsideConnect(p.into()),
            RequestParams::CacheAsideGetServerApiVersionParams(p) => {
                proto::request_params::Params::CacheAsideVersionInfo(p.into())
            },
            RequestParams::CacheAsideOpenForReadParams(p) => {
                proto::request_params::Params::CacheAsideOpenForRead(p.into())
            },
            RequestParams::CacheAsideOpenForWriteParams(p) => {
                proto::request_params::Params::CacheAsideOpenForWrite(p.into())
            },
            RequestParams::CacheAsideRemoveParams(p) => proto::request_params::Params::CacheAsideRemove(p.into()),
            RequestParams::BlockConnectParams(p) => proto::request_params::Params::BlockCacheConnect(p.into()),
            RequestParams::BlockGetCloseParams(p) => proto::request_params::Params::BlockCacheClose(p.into()),
            RequestParams::BlockGetOpenParams(p) => proto::request_params::Params::BlockCacheGet(p.into()),
            RequestParams::BlockPutAbortParams(p) => proto::request_params::Params::BlockCacheAbort(p.into()),
            RequestParams::BlockPutCommitParams(p) => proto::request_params::Params::BlockCacheCommit(p.into()),
            RequestParams::BlockPutOpenParams(p) => proto::request_params::Params::BlockCachePut(p.into()),
            RequestParams::BlockStatParams(p) => proto::request_params::Params::BlockCacheStat(p.into()),
        }
    }
}

impl From<GetVersionInfoParams> for proto::ServiceVersionInfoParams {
    fn from(_val: GetVersionInfoParams) -> Self {
        proto::ServiceVersionInfoParams {}
    }
}

impl From<ConnectParams> for proto::ClientConnectParams {
    fn from(val: ConnectParams) -> Self {
        proto::ClientConnectParams {
            client_name: val.client_name,
            client_uuid: val.client_uuid.into(),
        }
    }
}

impl From<ca::CloseParams> for proto::CacheAsideCloseParams {
    fn from(val: ca::CloseParams) -> Self {
        proto::CacheAsideCloseParams { handle: val.handle }
    }
}

impl From<ca::ConnectParams> for proto::CacheAsideConnectParams {
    fn from(val: ca::ConnectParams) -> Self {
        proto::CacheAsideConnectParams {
            files: convert_vec(val.open_files),
        }
    }
}

impl From<ca::OpenFile> for proto::FileConnectItem {
    fn from(val: ca::OpenFile) -> Self {
        proto::FileConnectItem {
            key: val.key.as_ref().into(),
            provider: val.provider.as_ref().into(),
            extension: val.extension.as_ref().into(),
            handle: val.handle,
            open_for_write: val.open_for_write,
        }
    }
}

impl From<ca::GetServerApiVersionParams> for proto::CacheAsideVersionInfoParams {
    fn from(_val: ca::GetServerApiVersionParams) -> Self {
        proto::CacheAsideVersionInfoParams {}
    }
}

impl From<ca::OpenForReadParams> for proto::CacheAsideOpenForReadParams {
    fn from(val: ca::OpenForReadParams) -> Self {
        proto::CacheAsideOpenForReadParams {
            key: val.key.as_ref().into(),
            provider: val.provider.as_ref().into(),
            extension: val.extension.as_ref().into(),
        }
    }
}

impl From<ca::OpenForWriteParams> for proto::CacheAsideOpenForWriteParams {
    fn from(val: ca::OpenForWriteParams) -> Self {
        proto::CacheAsideOpenForWriteParams {
            key: val.key.as_ref().into(),
            provider: val.provider.as_ref().into(),
            extension: val.extension.as_ref().into(),
            size: val.size,
        }
    }
}

impl From<ca::RemoveParams> for proto::CacheAsideRemoveParams {
    fn from(val: ca::RemoveParams) -> Self {
        proto::CacheAsideRemoveParams { handle: val.handle }
    }
}

impl From<bc::ConnectParams> for proto::BlockCacheConnectParams {
    fn from(val: bc::ConnectParams) -> Self {
        proto::BlockCacheConnectParams {
            blocks: convert_vec(val.requests),
        }
    }
}

impl From<bc::ConnectRequest> for proto::BlockConnectItem {
    fn from(val: bc::ConnectRequest) -> Self {
        proto::BlockConnectItem {
            key: val.key.as_ref().into(),
            handle: val.handle,
            open_for_put: val.open_for_put,
        }
    }
}

impl From<bc::CloseParams> for proto::BlockCacheCloseParams {
    fn from(val: bc::CloseParams) -> Self {
        proto::BlockCacheCloseParams {
            handles: convert_vec(val.handles),
        }
    }
}

impl From<bc::GetParams> for proto::BlockCacheGetParams {
    fn from(val: bc::GetParams) -> Self {
        proto::BlockCacheGetParams {
            keys: val.keys.iter().map(|e| e.as_ref().into()).collect(),
            timeout_us: val.timeout_us,
        }
    }
}

impl From<bc::AbortParams> for proto::BlockCacheAbortParams {
    fn from(val: bc::AbortParams) -> Self {
        proto::BlockCacheAbortParams {
            handles: convert_vec(val.handles),
        }
    }
}

impl From<bc::CommitParams> for proto::BlockCacheCommitParams {
    fn from(val: bc::CommitParams) -> Self {
        proto::BlockCacheCommitParams {
            handles: convert_vec(val.handles),
        }
    }
}

impl From<bc::PutParams> for proto::BlockCachePutParams {
    fn from(val: bc::PutParams) -> Self {
        proto::BlockCachePutParams {
            blocks: convert_vec(val.requests),
        }
    }
}

impl From<bc::PutRequest> for proto::BlockPutItem {
    fn from(val: bc::PutRequest) -> Self {
        proto::BlockPutItem {
            key: val.key.as_ref().into(),
            size: val.size,
            rank: val.rank,
        }
    }
}

impl From<bc::StatParams> for proto::BlockCacheStatParams {
    fn from(val: bc::StatParams) -> Self {
        proto::BlockCacheStatParams {
            keys: val.keys.iter().map(|e| e.as_ref().into()).collect(),
            timeout_us: val.timeout_us,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// proto::Response -> Response (for client)
////////////////////////////////////////////////////////////////////////////////

impl From<proto::Response> for Response {
    fn from(val: proto::Response) -> Self {
        if let Some(result) = val.result {
            Response {
                request_id: val.request_id,
                result: result.into(),
            }
        } else {
            Response {
                request_id: val.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse {
                    message: "no result".into(),
                }),
            }
        }
    }
}

impl From<proto::ResponseResult> for OperationResponse {
    fn from(val: proto::ResponseResult) -> Self {
        match val.result {
            Some(proto::response_result::Result::ClientConnect(_)) => OperationResponse::ConnectResponse,
            Some(proto::response_result::Result::ServiceVersionInfo(r)) => {
                OperationResponse::GetVersionInfoResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideVersionInfo(r)) => {
                OperationResponse::CacheAsideGetServerApiVersionResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideConnect(r)) => {
                OperationResponse::CacheAsideConnectResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideOpenForRead(r)) => {
                OperationResponse::CacheAsideOpenForReadResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideOpenForWrite(r)) => {
                OperationResponse::CacheAsideOpenForWriteResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideClose(r)) => {
                OperationResponse::CacheAsideCloseResponse(r.into())
            },
            Some(proto::response_result::Result::CacheAsideRemove(r)) => {
                OperationResponse::CacheAsideRemoveResponse(r.into())
            },
            Some(proto::response_result::Result::BlockCacheConnect(r)) => {
                OperationResponse::BlockConnectResponse(r.into())
            },
            Some(proto::response_result::Result::BlockCacheGet(r)) => OperationResponse::BlockGetOpenResponse(r.into()),
            Some(proto::response_result::Result::BlockCachePut(r)) => OperationResponse::BlockPutOpenResponse(r.into()),
            Some(proto::response_result::Result::BlockCacheClose(r)) => {
                OperationResponse::BlockGetCloseResponse(r.into())
            },
            Some(proto::response_result::Result::BlockCacheCommit(r)) => {
                OperationResponse::BlockPutCommitResponse(r.into())
            },
            Some(proto::response_result::Result::BlockCacheAbort(r)) => {
                OperationResponse::BlockPutAbortResponse(r.into())
            },
            Some(proto::response_result::Result::BlockCacheStat(r)) => OperationResponse::BlockStatResponse(r.into()),
            Some(proto::response_result::Result::ErrorInfo(r)) => OperationResponse::ErrorResponse(r.into()),
            None => OperationResponse::ErrorResponse(ErrorResponse {
                message: "no result".into(),
            }),
        }
    }
}

impl From<proto::ServiceVersionInfoResult> for GetVersionInfoResponse {
    fn from(val: proto::ServiceVersionInfoResult) -> Self {
        GetVersionInfoResponse {
            version_info: val.version_info.unwrap_or_default().into(),
        }
    }
}

impl From<proto::VersionInfo> for VersionInfo {
    fn from(val: proto::VersionInfo) -> Self {
        VersionInfo {
            software_version: val.software_version.unwrap_or_default().into(),
            interface_versions: convert_vec(val.interface_versions),
        }
    }
}

impl From<proto::SemVer> for SemVer {
    fn from(val: proto::SemVer) -> Self {
        SemVer {
            major: val.major,
            minor: val.minor,
            patch: val.patch,
        }
    }
}

impl From<proto::InterfaceVersion> for InterfaceVersion {
    fn from(val: proto::InterfaceVersion) -> Self {
        InterfaceVersion {
            interface: proto::Interface::try_from(val.interface)
                .unwrap_or(proto::Interface::Unknown)
                .into(),
            version: val.version as u16,
        }
    }
}

impl From<proto::Interface> for Interface {
    fn from(val: proto::Interface) -> Self {
        match val {
            proto::Interface::Unknown => Interface::Unknown,
            proto::Interface::CacheAside => Interface::CacheAside,
            proto::Interface::BlockCache => Interface::BlockCache,
        }
    }
}

impl From<proto::CacheAsideVersionInfoResult> for cache_aside::GetServerApiVersionResponse {
    fn from(val: proto::CacheAsideVersionInfoResult) -> Self {
        cache_aside::GetServerApiVersionResponse {
            version: val.version.unwrap_or_default().into(),
        }
    }
}

impl From<proto::CacheAsideConnectResult> for cache_aside::ConnectResponse {
    fn from(val: proto::CacheAsideConnectResult) -> Self {
        cache_aside::ConnectResponse {
            result: val.result_code.into(),
            file_results: convert_vec(val.file_result_codes),
        }
    }
}

impl From<proto::CacheAsideOpenForReadResult> for cache_aside::OpenResponse {
    fn from(val: proto::CacheAsideOpenForReadResult) -> Self {
        cache_aside::OpenResponse {
            result: val.result_code.into(),
            local_file: val.local_file.map(|e| e.into()),
            handle: val.handle,
        }
    }
}

impl From<proto::CacheAsideOpenForWriteResult> for cache_aside::OpenResponse {
    fn from(val: proto::CacheAsideOpenForWriteResult) -> Self {
        cache_aside::OpenResponse {
            result: val.result_code.into(),
            local_file: val.local_file.map(|e| e.into()),
            handle: val.handle,
        }
    }
}

impl From<proto::CacheAsideCloseResult> for cache_aside::CloseResponse {
    fn from(val: proto::CacheAsideCloseResult) -> Self {
        cache_aside::CloseResponse {
            result: val.result_code.into(),
        }
    }
}

impl From<proto::CacheAsideRemoveResult> for cache_aside::RemoveResponse {
    fn from(val: proto::CacheAsideRemoveResult) -> Self {
        cache_aside::RemoveResponse {
            result: val.result_code.into(),
        }
    }
}

impl From<proto::BlockCacheConnectResult> for block_cache::ConnectResponse {
    fn from(val: proto::BlockCacheConnectResult) -> Self {
        block_cache::ConnectResponse {
            results: convert_vec(val.result_codes),
        }
    }
}

impl From<proto::BlockCacheGetResult> for block_cache::GetResponse {
    fn from(val: proto::BlockCacheGetResult) -> Self {
        block_cache::GetResponse {
            results: convert_vec(val.results),
            base_path: val.base_path.into(),
        }
    }
}

impl From<proto::BlockGetResult> for block_cache::OpenResult {
    fn from(val: proto::BlockGetResult) -> Self {
        block_cache::OpenResult {
            result: val.result_code.into(),
            block: val.block.map(|e| e.into()),
        }
    }
}

impl From<proto::BlockInfo> for block_cache::Block {
    fn from(val: proto::BlockInfo) -> Self {
        block_cache::Block {
            handle: val.handle,
            location: val.location.unwrap_or_default().into(),
        }
    }
}

impl From<proto::BlockLocation> for block_cache::Location {
    fn from(val: proto::BlockLocation) -> Self {
        block_cache::Location {
            path: val.path.into(),
            id: val.id,
            offset: val.offset,
            size: val.size,
        }
    }
}

impl From<proto::BlockCachePutResult> for block_cache::PutResponse {
    fn from(val: proto::BlockCachePutResult) -> Self {
        block_cache::PutResponse {
            results: convert_vec(val.results),
            base_path: val.base_path.into(),
        }
    }
}

impl From<proto::BlockPutResult> for block_cache::OpenResult {
    fn from(val: proto::BlockPutResult) -> Self {
        block_cache::OpenResult {
            result: val.result_code.into(),
            block: val.block.map(|e| e.into()),
        }
    }
}

impl From<proto::BlockCacheCloseResult> for block_cache::CloseResponse {
    fn from(val: proto::BlockCacheCloseResult) -> Self {
        block_cache::CloseResponse {
            results: convert_vec(val.result_codes),
        }
    }
}

impl From<proto::BlockCacheCommitResult> for block_cache::CommitResponse {
    fn from(val: proto::BlockCacheCommitResult) -> Self {
        block_cache::CommitResponse {
            results: convert_vec(val.result_codes),
        }
    }
}

impl From<proto::BlockCacheAbortResult> for block_cache::AbortResponse {
    fn from(val: proto::BlockCacheAbortResult) -> Self {
        block_cache::AbortResponse {
            results: convert_vec(val.result_codes),
        }
    }
}

impl From<proto::BlockCacheStatResult> for block_cache::StatResponse {
    fn from(val: proto::BlockCacheStatResult) -> Self {
        block_cache::StatResponse {
            results: convert_vec(val.results),
        }
    }
}

impl From<proto::BlockStatResult> for block_cache::StatResult {
    fn from(val: proto::BlockStatResult) -> Self {
        block_cache::StatResult {
            result: val.result_code.into(),
            metadata: val.metadata.map(|e| e.into()),
        }
    }
}

impl From<proto::BlockMetadata> for block_cache::Metadata {
    fn from(val: proto::BlockMetadata) -> Self {
        block_cache::Metadata {
            last_accessed: val.last_accessed,
            accesses: val.accesses,
            size: val.size,
        }
    }
}

impl From<proto::ErrorInfoResult> for ErrorResponse {
    fn from(val: proto::ErrorInfoResult) -> Self {
        ErrorResponse { message: val.message }
    }
}

////////////////////////////////////////////////////////////////////////////////
// proto::Request -> Request (for server)
////////////////////////////////////////////////////////////////////////////////

impl TryFrom<&proto::ClientConnectParams> for ConnectParams {
    type Error = uuid::Error;

    fn try_from(val: &proto::ClientConnectParams) -> Result<Self, Self::Error> {
        Ok(ConnectParams {
            client_name: val.client_name.as_str().into(),
            client_uuid: Uuid::parse_str(&val.client_uuid)?,
        })
    }
}

impl From<&proto::CacheAsideConnectParams> for ca::ConnectParams {
    fn from(val: &proto::CacheAsideConnectParams) -> Self {
        ca::ConnectParams {
            open_files: convert_rvec(&val.files),
        }
    }
}

impl From<ca::ConnectResponse> for proto::CacheAsideConnectResult {
    fn from(val: ca::ConnectResponse) -> Self {
        proto::CacheAsideConnectResult {
            result_code: val.result as i32,
            file_result_codes: convert_vec(val.file_results),
        }
    }
}

impl From<&proto::FileConnectItem> for ca::OpenFile {
    fn from(val: &proto::FileConnectItem) -> Self {
        ca::OpenFile {
            key: Arc::from(val.key.as_str()),
            provider: Arc::from(val.provider.as_str()),
            extension: Arc::from(val.extension.as_str()),
            handle: val.handle,
            open_for_write: val.open_for_write,
        }
    }
}

impl From<&proto::CacheAsideOpenForReadParams> for ca::OpenForReadParams {
    fn from(val: &proto::CacheAsideOpenForReadParams) -> Self {
        ca::OpenForReadParams {
            key: Arc::from(val.key.as_str()),
            provider: Arc::from(val.provider.as_str()),
            extension: Arc::from(val.extension.as_str()),
        }
    }
}

impl From<ca::OpenResponse> for proto::CacheAsideOpenForReadResult {
    fn from(val: ca::OpenResponse) -> Self {
        proto::CacheAsideOpenForReadResult {
            result_code: val.result as i32,
            local_file: val.local_file.map(|e| e.to_string_lossy().to_string()),
            handle: val.handle,
        }
    }
}

impl From<&proto::CacheAsideOpenForWriteParams> for ca::OpenForWriteParams {
    fn from(val: &proto::CacheAsideOpenForWriteParams) -> Self {
        ca::OpenForWriteParams {
            key: Arc::from(val.key.as_str()),
            provider: Arc::from(val.provider.as_str()),
            extension: Arc::from(val.extension.as_str()),
            size: val.size,
        }
    }
}

impl From<ca::OpenResponse> for proto::CacheAsideOpenForWriteResult {
    fn from(val: ca::OpenResponse) -> Self {
        proto::CacheAsideOpenForWriteResult {
            result_code: val.result as i32,
            local_file: val.local_file.map(|e| e.to_string_lossy().to_string()),
            handle: val.handle,
        }
    }
}

impl From<&proto::CacheAsideCloseParams> for ca::CloseParams {
    fn from(val: &proto::CacheAsideCloseParams) -> Self {
        ca::CloseParams { handle: val.handle }
    }
}

impl From<ca::CloseResponse> for proto::CacheAsideCloseResult {
    fn from(val: ca::CloseResponse) -> Self {
        proto::CacheAsideCloseResult {
            result_code: val.result as i32,
        }
    }
}

impl From<&proto::CacheAsideRemoveParams> for ca::RemoveParams {
    fn from(val: &proto::CacheAsideRemoveParams) -> Self {
        ca::RemoveParams { handle: val.handle }
    }
}

impl From<ca::RemoveResponse> for proto::CacheAsideRemoveResult {
    fn from(val: ca::RemoveResponse) -> Self {
        proto::CacheAsideRemoveResult {
            result_code: val.result as i32,
        }
    }
}

impl From<&proto::BlockCacheConnectParams> for bc::ConnectParams {
    fn from(val: &proto::BlockCacheConnectParams) -> Self {
        bc::ConnectParams {
            requests: convert_rvec(&val.blocks),
        }
    }
}

impl From<&proto::BlockConnectItem> for bc::ConnectRequest {
    fn from(val: &proto::BlockConnectItem) -> Self {
        bc::ConnectRequest {
            key: Arc::from(val.key.clone()),
            extension: None,
            handle: val.handle,
            open_for_put: val.open_for_put,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Response -> proto::Response (for server)
////////////////////////////////////////////////////////////////////////////////

impl From<bc::ConnectResponse> for proto::BlockCacheConnectResult {
    fn from(val: bc::ConnectResponse) -> Self {
        proto::BlockCacheConnectResult {
            result_codes: convert_vec(val.results),
        }
    }
}

impl From<&proto::BlockCacheGetParams> for bc::GetParams {
    fn from(val: &proto::BlockCacheGetParams) -> Self {
        bc::GetParams {
            keys: val.keys.iter().map(|e| Arc::from(e.as_slice())).collect(),
            timeout_us: val.timeout_us,
        }
    }
}

impl From<bc::OpenResult> for proto::BlockGetResult {
    fn from(val: bc::OpenResult) -> Self {
        proto::BlockGetResult {
            result_code: val.result.into(),
            block: val.block.map(|e| e.into()),
        }
    }
}

impl From<bc::Block> for proto::BlockInfo {
    fn from(val: bc::Block) -> Self {
        proto::BlockInfo {
            handle: val.handle,
            location: Some(val.location.into()),
        }
    }
}

impl From<bc::Location> for proto::BlockLocation {
    fn from(val: bc::Location) -> Self {
        proto::BlockLocation {
            path: val.path.to_string_lossy().to_string(),
            id: val.id,
            offset: val.offset,
            size: val.size,
        }
    }
}

impl From<bc::GetResponse> for proto::BlockCacheGetResult {
    fn from(val: bc::GetResponse) -> Self {
        proto::BlockCacheGetResult {
            results: convert_vec(val.results),
            base_path: val.base_path.to_string_lossy().to_string(),
        }
    }
}

impl From<&proto::BlockCachePutParams> for bc::PutParams {
    fn from(val: &proto::BlockCachePutParams) -> Self {
        bc::PutParams {
            requests: convert_rvec(&val.blocks),
        }
    }
}

impl From<&proto::BlockPutItem> for bc::PutRequest {
    fn from(val: &proto::BlockPutItem) -> Self {
        bc::PutRequest {
            key: Arc::from(val.key.as_slice()),
            size: val.size,
            rank: val.rank,
            extension: None,
        }
    }
}

impl From<bc::OpenResult> for proto::BlockPutResult {
    fn from(val: bc::OpenResult) -> Self {
        proto::BlockPutResult {
            result_code: val.result.into(),
            block: val.block.map(|e| e.into()),
        }
    }
}

impl From<bc::PutResponse> for proto::BlockCachePutResult {
    fn from(val: bc::PutResponse) -> Self {
        proto::BlockCachePutResult {
            results: convert_vec(val.results),
            base_path: val.base_path.to_string_lossy().to_string(),
        }
    }
}

impl From<&proto::BlockCacheCloseParams> for bc::CloseParams {
    fn from(val: &proto::BlockCacheCloseParams) -> Self {
        bc::CloseParams {
            handles: val.handles.clone(),
        }
    }
}

impl From<bc::CloseResponse> for proto::BlockCacheCloseResult {
    fn from(val: bc::CloseResponse) -> Self {
        proto::BlockCacheCloseResult {
            result_codes: convert_vec(val.results),
        }
    }
}

impl From<&proto::BlockCacheCommitParams> for bc::CommitParams {
    fn from(val: &proto::BlockCacheCommitParams) -> Self {
        bc::CommitParams {
            handles: val.handles.clone(),
        }
    }
}

impl From<bc::CommitResponse> for proto::BlockCacheCommitResult {
    fn from(val: bc::CommitResponse) -> Self {
        proto::BlockCacheCommitResult {
            result_codes: convert_vec(val.results),
        }
    }
}

impl From<&proto::BlockCacheAbortParams> for bc::AbortParams {
    fn from(val: &proto::BlockCacheAbortParams) -> Self {
        bc::AbortParams {
            handles: val.handles.clone(),
        }
    }
}

impl From<bc::AbortResponse> for proto::BlockCacheAbortResult {
    fn from(val: bc::AbortResponse) -> Self {
        proto::BlockCacheAbortResult {
            result_codes: convert_vec(val.results),
        }
    }
}

impl From<&proto::BlockCacheStatParams> for bc::StatParams {
    fn from(val: &proto::BlockCacheStatParams) -> Self {
        bc::StatParams {
            keys: val.keys.iter().map(|e| Arc::from(e.as_slice())).collect(),
            timeout_us: val.timeout_us,
        }
    }
}

impl From<bc::StatResult> for proto::BlockStatResult {
    fn from(val: bc::StatResult) -> Self {
        proto::BlockStatResult {
            result_code: val.result.into(),
            metadata: val.metadata.map(|e| e.into()),
        }
    }
}

impl From<bc::Metadata> for proto::BlockMetadata {
    fn from(val: bc::Metadata) -> Self {
        proto::BlockMetadata {
            last_accessed: val.last_accessed,
            accesses: val.accesses,
            size: val.size,
        }
    }
}

impl From<bc::StatResponse> for proto::BlockCacheStatResult {
    fn from(val: bc::StatResponse) -> Self {
        proto::BlockCacheStatResult {
            results: convert_vec(val.results),
        }
    }
}
