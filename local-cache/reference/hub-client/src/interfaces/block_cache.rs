// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use futures::{SinkExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use stdext::function_name;
use tracing::{error, trace, warn};

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::os::raw::c_void;
use std::path::Path;
use std::ptr::null;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use hub_rpc::block_cache::{
    AbortParams, CloseParams, CommitParams, ConnectParams, ConnectRequest, GetParams, PutParams, PutRequest, StatParams,
};
use hub_rpc::{Interface, OperationResponse, Request, RequestParams, Response, ResultCode, VersionInfo};

use crate::client::{run_blocking_operation, Context};
use crate::error::ClientResultCode;
use crate::types::{
    BlockLocation, BlockMetadata, BlockResult, CallbackTimer, CloseResult, GetCloseCallback, GetOpenCallback,
    MetadataResult, OpenResult, PutAbortCallback, PutCommitCallback, PutOpenCallback, StatCallback, StatResult,
};
use crate::{Error, Result};

static BLOCK_CACHE_VERSION: u16 = 1;

pub struct BlockCache {
    context: Arc<Context>,
}

impl BlockCache {
    pub(crate) fn new(context: Arc<Context>) -> Self {
        Self { context }
    }

    // Check the version of this client with the server version
    pub(crate) fn check_version(version_info: &VersionInfo, version_map: &RwLock<HashMap<Interface, Option<Error>>>) {
        if let Some(server_version) = version_info
            .interface_versions
            .iter()
            .find(|v| v.interface == Interface::BlockCache)
        {
            if server_version.version < BLOCK_CACHE_VERSION {
                let msg = format!(
                    "BlockCache Server version ({}) is incompatible with client version {}",
                    server_version.version, BLOCK_CACHE_VERSION
                );
                error!("{} {}", function_name!(), msg);
                version_map
                    .write()
                    .insert(Interface::BlockCache, Some(Error::Version(msg)));
            }
        } else {
            let msg = "Server does not support BlockCache interface";
            error!("{} {}", function_name!(), msg);
            version_map
                .write()
                .insert(Interface::BlockCache, Some(Error::Version(msg.to_owned())));
        }
    }

    pub(crate) async fn startup<S>(stream: &mut S, context: Arc<Context>) -> Result<()>
    where
        S: StreamExt<Item = Result<Response, std::io::Error>>
            + SinkExt<Request, Error = std::io::Error>
            + std::marker::Unpin,
    {
        // Issue the connect call to negotiate open files with the server
        // Gather all open blocks
        let mut open_handles: Vec<u64> = Vec::new();
        let mut open_blocks: Vec<ConnectRequest> = Vec::new();
        {
            let map = context.block_cache.open_block_map.lock();
            for (handle, block) in map.iter() {
                open_handles.push(*handle);
                open_blocks.push(block.clone());
            }
        }

        let req = Request {
            request_id: context.next_request_id.fetch_add(1, Ordering::SeqCst),
            params: RequestParams::BlockConnectParams(ConnectParams { requests: open_blocks }),
        };

        match run_blocking_operation(req, stream, context.shutdown_rx.clone()).await {
            Ok(response) => {
                if let OperationResponse::BlockConnectResponse(connect_repsonse) = response.result {
                    for (i, result) in connect_repsonse.results.iter().enumerate() {
                        if *result != ResultCode::Success {
                            warn!(
                                "{} Failed to reopen handle {} after Hub restart. Error: {:?}",
                                function_name!(),
                                open_handles[i],
                                *result
                            );
                        }
                    }
                    Ok(())
                } else {
                    error!("Hub returned unexpected result type for Block Connect");
                    Err(Error::UnexpectedResponseType)
                }
            },
            Err(e) => Err(e),
        }
    }

    /// C API
    #[tracing::instrument(level = "debug", skip(self, keys, context, callback), fields(key_count=keys.len(), ?first_key=keys[0]))]
    pub fn get_open_c(
        &self,
        keys: &[Arc<[u8]>],
        timeout_us: u64,
        context: *mut c_void,
        callback: GetOpenCallback,
    ) -> Result<()> {
        trace!(?keys, ?timeout_us, "{}", function_name!());
        let ctx = CallbackContext { context };
        let callback_timer = CallbackTimer::new_with_key("get_open_callback", keys.first().cloned());
        let mut key_copy = VecDeque::from(keys.to_vec());
        let client_context = self.context.clone();
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockGetOpenResponse(g) => Self::call_open_callback(
                        &mut key_copy,
                        client_context,
                        false,
                        g.results,
                        &g.base_path,
                        ctx,
                        callback,
                    ),
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(OpenResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for get_open");
                        callback(
                            OpenResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for get_open call: {:?}", e);
                    callback(OpenResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockGetOpenParams(GetParams {
                keys: keys.to_vec(),
                timeout_us,
            }),
            Interface::BlockCache,
            response_callback,
        )
    }

    #[tracing::instrument(level = "debug", skip(self, handles, context, callback), fields(handle_count=handles.len(), first_handle=handles[0]))]
    pub fn get_close_c(&self, handles: &[u64], context: *mut c_void, callback: GetCloseCallback) -> Result<()> {
        trace!(?handles, "{}", function_name!());
        let ctx = CallbackContext { context };
        let callback_timer = CallbackTimer::new_with_handle("get_close_callback", handles.first());
        let client_context = self.context.clone();
        let owned_handles = handles.to_vec();
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockGetCloseResponse(p) => {
                        client_context.block_cache.remove_blocks(&owned_handles);
                        callback(
                            CloseResult::new(ClientResultCode::Success, p.results.as_ptr(), p.results.len() as u64),
                            ctx.context(),
                        );
                    },
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(CloseResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for get_close");
                        callback(
                            CloseResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for get_close call: {:?}", e);
                    callback(CloseResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockGetCloseParams(CloseParams {
                handles: handles.to_vec(),
            }),
            Interface::BlockCache,
            response_callback,
        )
    }

    #[tracing::instrument(level = "debug", skip(self, blocks, context, callback), fields(key_count=blocks.len(), ?first_key=blocks[0].key))]
    pub fn put_open_c(&self, blocks: Vec<PutRequest>, context: *mut c_void, callback: PutOpenCallback) -> Result<()> {
        trace!(?blocks, "{}", function_name!());
        let params = PutParams { requests: blocks };

        let callback_timer =
            CallbackTimer::new_with_key("put_open_callback", params.requests.first().map(|b| b.key.clone()));
        let ctx = CallbackContext { context };
        let mut key_copy: VecDeque<Arc<[u8]>> = params.requests.iter().map(|b| b.key.clone()).collect();
        let client_context = self.context.clone();
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockPutOpenResponse(p) => Self::call_open_callback(
                        &mut key_copy,
                        client_context,
                        true,
                        p.results,
                        &p.base_path,
                        ctx,
                        callback,
                    ),
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(OpenResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for put_open");
                        callback(
                            OpenResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for put_open call: {:?}", e);
                    callback(OpenResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockPutOpenParams(params),
            Interface::BlockCache,
            response_callback,
        )
    }

    #[tracing::instrument(level = "debug", skip(self, handles, context, callback), fields(handle_count=handles.len(), first_handle=handles[0]))]
    pub fn put_commit_c(&self, handles: &[u64], context: *mut c_void, callback: PutCommitCallback) -> Result<()> {
        trace!(?handles, "{}", function_name!());
        let ctx = CallbackContext { context };
        let callback_timer = CallbackTimer::new_with_handle("put_commit_callback", handles.first());
        let client_context = self.context.clone();
        let owned_handles = handles.to_vec();
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockPutCommitResponse(p) => {
                        client_context.block_cache.remove_blocks(&owned_handles);
                        callback(
                            CloseResult::new(ClientResultCode::Success, p.results.as_ptr(), p.results.len() as u64),
                            ctx.context(),
                        );
                    },
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(CloseResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for put_commit");
                        callback(
                            CloseResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for put_commit call: {:?}", e);
                    callback(CloseResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockPutCommitParams(CommitParams {
                handles: handles.to_vec(),
            }),
            Interface::BlockCache,
            response_callback,
        )
    }

    #[tracing::instrument(level = "debug", skip(self, handles, context, callback), fields(handle_count=handles.len(), first_handle=handles[0]))]
    pub fn put_abort_c(&self, handles: &[u64], context: *mut c_void, callback: PutAbortCallback) -> Result<()> {
        trace!(?handles, "{}", function_name!());
        let ctx = CallbackContext { context };
        let callback_timer = CallbackTimer::new_with_handle("put_abort_callback", handles.first());
        let client_context = self.context.clone();
        let owned_handles = handles.to_vec();
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockPutAbortResponse(p) => {
                        client_context.block_cache.remove_blocks(&owned_handles);
                        callback(
                            CloseResult::new(ClientResultCode::Success, p.results.as_ptr(), p.results.len() as u64),
                            ctx.context(),
                        );
                    },
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(CloseResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for put_abort");
                        callback(
                            CloseResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for put_abort call: {:?}", e);
                    callback(CloseResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockPutAbortParams(AbortParams {
                handles: handles.to_vec(),
            }),
            Interface::BlockCache,
            response_callback,
        )
    }

    #[tracing::instrument(level = "debug", skip(self, keys, context, callback), fields(key_count=keys.len(), ?first_key=keys[0]))]
    pub fn stat_c(
        &self,
        keys: &[Arc<[u8]>],
        timeout_us: u64,
        context: *mut c_void,
        callback: StatCallback,
    ) -> Result<()> {
        trace!(?keys, ?timeout_us, "{}", function_name!());
        let ctx = CallbackContext { context };
        let callback_timer = CallbackTimer::new_with_key("stat_callback", keys.first().cloned());
        let response_callback = move |resp: Result<Response>| {
            callback_timer.end();
            match resp {
                Ok(r) => match r.result {
                    OperationResponse::BlockStatResponse(s) => {
                        let mut metadata = Vec::with_capacity(s.results.len());
                        let results: Vec<MetadataResult> = s
                            .results
                            .iter()
                            .map(|r| match r.result {
                                ResultCode::Success => match &r.metadata {
                                    Some(m) => {
                                        metadata.push(BlockMetadata::new(m.size));
                                        MetadataResult::new(r.result, metadata.last().unwrap())
                                    },
                                    None => {
                                        error!("None handle returned for successful get_open result");
                                        MetadataResult::new(ResultCode::ErrorInternal, null())
                                    },
                                },
                                _ => MetadataResult::new(r.result, null()),
                            })
                            .collect();

                        callback(
                            StatResult::new(ClientResultCode::Success, results.as_ptr(), results.len() as u64),
                            ctx.context(),
                        );
                    },
                    OperationResponse::ErrorResponse(e) => {
                        error!("Hub returned error: {:?}", e.message);
                        callback(StatResult::from(ClientResultCode::InternalError), ctx.context());
                    },
                    _ => {
                        error!("Hub returned unexpected result type for stat");
                        callback(
                            StatResult::from(ClientResultCode::UnexpectedResponseType),
                            ctx.context(),
                        );
                    },
                },
                Err(e) => {
                    error!("Error result for stat call: {:?}", e);
                    callback(StatResult::from(&e), ctx.context());
                },
            }
        };

        self.context.send_callback_request(
            RequestParams::BlockStatParams(StatParams {
                keys: keys.to_vec(),
                timeout_us,
            }),
            Interface::BlockCache,
            response_callback,
        )
    }

    fn call_open_callback(
        keys: &mut VecDeque<Arc<[u8]>>,
        client_context: Arc<Context>,
        open_for_put: bool,
        results: Vec<hub_rpc::block_cache::OpenResult>,
        base_path: &Path,
        ctx: CallbackContext,
        callback: extern "C" fn(OpenResult, *mut c_void),
    ) {
        if results.len() != keys.len() {
            error!(
                "Different number of provided keys ({}) and returned results {})",
                keys.len(),
                results.len()
            );
            callback(OpenResult::from(ClientResultCode::InternalError), ctx.context());
            return;
        }
        let mut paths = Vec::with_capacity(results.len());
        let mut locations = Vec::with_capacity(results.len());
        let block_results: Vec<BlockResult> = results
            .iter()
            .map(|r| match r.result {
                ResultCode::Success => match &r.block {
                    Some(b) => match CString::new(base_path.join(&b.location.path).to_string_lossy().as_bytes()) {
                        Ok(c) => {
                            client_context.block_cache.add_open_block(
                                b.handle,
                                keys.pop_front().unwrap(),
                                None, // TODO: Fix extension?
                                open_for_put,
                            );
                            paths.push(c);
                            locations.push(BlockLocation::new(
                                paths.last().unwrap().as_ptr(),
                                b.location.offset,
                                b.location.size,
                                b.handle,
                            ));
                            BlockResult::new(r.result, locations.last().unwrap())
                        },
                        Err(_) => {
                            error!("Failed to allocate path string");
                            BlockResult::new(ResultCode::ErrorInternal, null())
                        },
                    },
                    None => {
                        error!("None handle returned for successful get_open result");
                        BlockResult::new(ResultCode::ErrorInternal, null())
                    },
                },
                _ => BlockResult::new(r.result, null()),
            })
            .collect();
        callback(
            OpenResult::new(
                ClientResultCode::Success,
                block_results.as_ptr(),
                block_results.len() as u64,
            ),
            ctx.context(),
        );
    }

    #[cfg(feature = "test")]
    pub fn clear_open_blocks(&self) {
        self.context.block_cache.open_block_map.lock().clear();
    }
}

/// Context information for the cache aside interface
pub(crate) struct BlockCacheContext {
    // TODO: convert to Dashmap?
    open_block_map: Mutex<HashMap<u64, hub_rpc::block_cache::ConnectRequest>>,
}

impl BlockCacheContext {
    pub fn new() -> Self {
        Self {
            open_block_map: Mutex::new(HashMap::new()),
        }
    }

    pub fn add_open_block(&self, handle: u64, key: Arc<[u8]>, extension: Option<Arc<str>>, open_for_put: bool) {
        let mut locked = self.open_block_map.lock();
        locked.insert(
            handle,
            ConnectRequest {
                key,
                extension,
                handle,
                open_for_put,
            },
        );
    }

    pub fn remove_blocks(&self, handles: &[u64]) {
        let mut locked = self.open_block_map.lock();
        for handle in handles {
            if locked.remove(handle).is_none() {
                warn!("Attempted to remove handle {handle} which doesn't exist in open block map");
            }
        }
    }
}

#[derive(Clone)]
struct CallbackContext {
    context: *mut c_void,
}

impl CallbackContext {
    pub fn context(&self) -> *mut c_void {
        self.context
    }
}

unsafe impl Send for CallbackContext {}
unsafe impl Sync for CallbackContext {}
