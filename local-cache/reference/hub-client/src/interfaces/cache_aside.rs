// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use parking_lot::{Mutex, RwLock};
use stdext::function_name;
use tracing::{error, trace, warn};

use std::collections::HashMap;
use std::ffi::CString;
use std::fmt::Debug;
use std::os::raw::c_void;
use std::ptr::null;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};

use hub_rpc::cache_aside::{
    CloseParams, CloseResponse, ConnectParams, GetServerApiVersionParams, GetServerApiVersionResponse,
    OpenForReadParams, OpenForWriteParams, OpenResponse, RemoveParams, RemoveResponse,
};
use hub_rpc::{Interface, OperationResponse, Request, RequestParams, Response, ResultCode, VersionInfo};

use crate::client::{run_blocking_operation, Context};
use crate::{types::*, ClientResultCode, Error, Result};

static CACHE_ASIDE_VERSION: u16 = 1;
pub struct CacheAside {
    context: Arc<Context>,
}

impl CacheAside {
    pub(crate) fn new(context: Arc<Context>) -> Self {
        Self { context }
    }

    // Check the version of this client with the server version
    pub(crate) fn check_version(version_info: &VersionInfo, version_map: &RwLock<HashMap<Interface, Option<Error>>>) {
        if let Some(server_version) = version_info
            .interface_versions
            .iter()
            .find(|v| v.interface == Interface::CacheAside)
        {
            if server_version.version < CACHE_ASIDE_VERSION {
                let msg = format!(
                    "CacheAside Server version ({}) is incompatible with client version {}",
                    server_version.version, CACHE_ASIDE_VERSION
                );
                error!("{} {}", function_name!(), msg);
                version_map
                    .write()
                    .insert(Interface::CacheAside, Some(Error::Version(msg)));
            }
        } else {
            let msg = "Server does not support CacheAsideinterface";
            error!("{} {}", function_name!(), msg);
            version_map
                .write()
                .insert(Interface::CacheAside, Some(Error::Version(msg.to_owned())));
        }
    }

    // Perform startup logic for this Cache Aside interface. This calls connect with any open files in the client.
    pub(crate) async fn startup<S>(stream: &mut S, context: Arc<Context>) -> Result<()>
    where
        S: StreamExt<Item = Result<Response, std::io::Error>>
            + SinkExt<Request, Error = std::io::Error>
            + std::marker::Unpin,
    {
        // Issue the connect call to negotiate open files with the server
        let mut open_handles: Vec<u64> = Vec::new();
        let mut open_files: Vec<hub_rpc::cache_aside::OpenFile> = Vec::new();
        {
            let map = context.cache_aside.open_file_map.lock();
            for (handle, file) in &map.clone() {
                trace!(handle, ?file, pid = std::process::id(), "cache aside startup handle");
                open_handles.push(*handle);
                open_files.push(file.clone());
            }
        }

        let req = Request {
            request_id: context.next_request_id.fetch_add(1, Ordering::SeqCst),
            params: RequestParams::CacheAsideConnectParams(ConnectParams { open_files }),
        };

        match run_blocking_operation(req, stream, context.shutdown_rx.clone()).await {
            Ok(response) => {
                if let OperationResponse::CacheAsideConnectResponse(connect_repsonse) = response.result {
                    match connect_repsonse.result {
                        ResultCode::Success => {
                            // If any files failed to open on connect, just remove them from our open file map
                            for (i, result) in connect_repsonse.file_results.iter().enumerate() {
                                if *result != ResultCode::Success {
                                    warn!(
                                        "{} Failed to reopen handle {} after Hub Restart, Error: {:?}",
                                        function_name!(),
                                        open_handles[i],
                                        *result
                                    );
                                    context.cache_aside.open_file_map.lock().remove(&open_handles[i]);
                                }
                            }
                            Ok(())
                        },
                        e => Err(Error::Internal(format!("Connect returned unsucessful result: {e:?}"))),
                    }
                } else {
                    error!("Hub returned unexpected result type for Cache AsideConnect");
                    Err(Error::UnexpectedResponseType)
                }
            },
            Err(e) => Err(e),
        }
    }

    ///
    /// Rust API
    ///
    async fn send_request(&self, params: RequestParams) -> Result<OperationResponse> {
        match self.context.send_request(params.clone(), Interface::CacheAside).await {
            Ok(result) => match self.context.cache_aside.update_open_file_map(&result, &params) {
                Ok(_) => Ok(result),
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn get_server_api_version(&self) -> Result<GetServerApiVersionResponse> {
        match self
            .send_request(RequestParams::CacheAsideGetServerApiVersionParams(
                GetServerApiVersionParams {},
            ))
            .await
        {
            Ok(result) => match result {
                OperationResponse::CacheAsideGetServerApiVersionResponse(r) => Ok(r),
                _ => Err(Error::UnexpectedResponseType),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn cache_open_for_read(
        &self,
        key: impl AsRef<str> + Debug,
        provider: impl AsRef<str> + Debug,
        extension: impl AsRef<str> + Debug,
    ) -> Result<OpenResponse> {
        trace!(?key, ?provider, ?extension, "{}", function_name!());
        match self
            .send_request(RequestParams::CacheAsideOpenForReadParams(OpenForReadParams {
                key: key.as_ref().into(),
                provider: provider.as_ref().into(),
                extension: extension.as_ref().into(),
            }))
            .await
        {
            Ok(result) => match result {
                OperationResponse::CacheAsideOpenForReadResponse(r) => Ok(r),
                _ => Err(Error::UnexpectedResponseType),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn cache_open_for_write(
        &self,
        key: impl AsRef<str> + Debug,
        provider: impl AsRef<str> + Debug,
        extension: impl AsRef<str> + Debug,
        size: u64,
    ) -> Result<OpenResponse> {
        trace!(?key, ?provider, ?extension, ?size, "{}", function_name!());
        match self
            .send_request(RequestParams::CacheAsideOpenForWriteParams(OpenForWriteParams {
                key: key.as_ref().into(),
                provider: provider.as_ref().into(),
                extension: extension.as_ref().into(),
                size,
            }))
            .await
        {
            Ok(result) => match result {
                OperationResponse::CacheAsideOpenForWriteResponse(r) => Ok(r),
                _ => Err(Error::UnexpectedResponseType),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn close(&self, handle: u64) -> Result<CloseResponse> {
        trace!(?handle, "{}", function_name!());
        match self
            .send_request(RequestParams::CacheAsideCloseParams(CloseParams { handle }))
            .await
        {
            Ok(result) => match result {
                OperationResponse::CacheAsideCloseResponse(r) => Ok(r),
                _ => Err(Error::UnexpectedResponseType),
            },
            Err(e) => Err(e),
        }
    }

    pub async fn remove(&self, handle: u64) -> Result<RemoveResponse> {
        trace!(?handle, "{}", function_name!());
        match self
            .send_request(RequestParams::CacheAsideRemoveParams(RemoveParams { handle }))
            .await
        {
            Ok(result) => match result {
                OperationResponse::CacheAsideRemoveResponse(r) => Ok(r),
                _ => Err(Error::UnexpectedResponseType),
            },
            Err(e) => Err(e),
        }
    }

    ///
    /// C API
    ///
    fn send_c_request(&self, params: RequestParams, c_callback: ClientCallbackC, timer: CallbackTimer) -> Result<()> {
        let params_clone = params.clone();
        let context_clone = self.context.clone();
        let response_callback = move |resp: Result<Response>| {
            timer.end();
            match resp {
                Ok(r) => match context_clone.cache_aside.update_open_file_map(&r.result, &params_clone) {
                    Ok(_) => {
                        Self::handle_c_callback(r, c_callback);
                    },
                    Err(e) => Self::handle_c_callback_error(c_callback.error_callback, c_callback.context, e),
                },
                Err(e) => Self::handle_c_callback_error(c_callback.error_callback, c_callback.context, e),
            }
        };

        self.context
            .send_callback_request(params, Interface::CacheAside, response_callback)
    }

    pub fn get_server_api_version_c(
        &self,
        context: *mut c_void,
        callback: GetServerApiVersionCallback,
        error_callback: ErrorCallback,
    ) -> Result<()> {
        let params = RequestParams::CacheAsideGetServerApiVersionParams(GetServerApiVersionParams {});

        let c_callback = ClientCallbackC {
            callback: Callbacks::GetServerApiVersion(callback),
            error_callback,
            context,
        };

        self.send_c_request(
            params,
            c_callback,
            CallbackTimer::new("get_server_api_version_callback"),
        )
    }

    pub fn cache_open_for_read_c(
        &self,
        key: impl AsRef<str> + Debug,
        provider: impl AsRef<str> + Debug,
        extension: impl AsRef<str> + Debug,
        context: *mut c_void,
        callback: CacheOpenForReadCallback,
        error_callback: ErrorCallback,
    ) -> Result<()> {
        trace!(?key, ?provider, ?extension, "{}", function_name!());
        let params = RequestParams::CacheAsideOpenForReadParams(OpenForReadParams {
            key: key.as_ref().into(),
            provider: provider.as_ref().into(),
            extension: extension.as_ref().into(),
        });

        let c_callback = ClientCallbackC {
            callback: Callbacks::CacheOpenForRead(callback),
            error_callback,
            context,
        };

        self.send_c_request(params, c_callback, CallbackTimer::new("cache_open_for_read_callback"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cache_open_for_write_c(
        &self,
        key: impl AsRef<str> + Debug,
        provider: impl AsRef<str> + Debug,
        extension: impl AsRef<str> + Debug,
        size: u64,
        context: *mut c_void,
        callback: CacheOpenForWriteCallback,
        error_callback: ErrorCallback,
    ) -> Result<()> {
        trace!(?key, ?provider, ?extension, ?size, "{}", function_name!());
        let params = RequestParams::CacheAsideOpenForWriteParams(OpenForWriteParams {
            key: key.as_ref().into(),
            provider: provider.as_ref().into(),
            extension: extension.as_ref().into(),
            size,
        });

        let c_callback = ClientCallbackC {
            callback: Callbacks::CacheOpenForWrite(callback),
            error_callback,
            context,
        };

        self.send_c_request(params, c_callback, CallbackTimer::new("cache_open_for_write_callback"))
    }

    pub fn close_c(
        &self,
        handle: u64,
        context: *mut c_void,
        callback: CloseCallback,
        error_callback: ErrorCallback,
    ) -> Result<()> {
        trace!(?handle, "{}", function_name!());
        let params = RequestParams::CacheAsideCloseParams(CloseParams { handle });

        let c_callback = ClientCallbackC {
            callback: Callbacks::Close(callback),
            error_callback,
            context,
        };

        self.send_c_request(params, c_callback, CallbackTimer::new("cache_close_callback"))
    }

    pub fn remove_c(
        &self,
        handle: u64,
        context: *mut c_void,
        callback: RemoveCallback,
        error_callback: ErrorCallback,
    ) -> Result<()> {
        trace!(?handle, "{}", function_name!());
        let params = RequestParams::CacheAsideRemoveParams(RemoveParams { handle });

        let c_callback = ClientCallbackC {
            callback: Callbacks::Remove(callback),
            error_callback,
            context,
        };

        self.send_c_request(params, c_callback, CallbackTimer::new("cache_remove_callback"))
    }

    // Call the c callback with the provided response
    fn handle_c_callback(resp: Response, client_callback: ClientCallbackC) {
        // If the request returned an error response, call the error callback
        if let OperationResponse::ErrorResponse(r) = resp.result {
            Self::handle_c_callback_error(
                client_callback.error_callback,
                client_callback.context,
                Error::Internal(format!("{:?}", r.message)),
            );
            return;
        }

        match client_callback.callback {
            Callbacks::GetServerApiVersion(call) => {
                if let OperationResponse::CacheAsideGetServerApiVersionResponse(r) = resp.result {
                    (call)(r, client_callback.context)
                } else {
                    Self::handle_c_callback_error(
                        client_callback.error_callback,
                        client_callback.context,
                        Error::UnexpectedResponseType,
                    );
                }
            },
            Callbacks::CacheOpenForRead(call) => {
                if let OperationResponse::CacheAsideOpenForReadResponse(r) = resp.result {
                    match r.result {
                        ResultCode::Success => {
                            if let (Some(local_file), Some(handle)) = (r.local_file, r.handle) {
                                match CString::new(local_file.to_string_lossy().as_bytes()) {
                                    Ok(path) => {
                                        let local_file_c = LocalFile {
                                            handle,
                                            path: path.as_ptr(),
                                        };
                                        let result = CacheOpenResponse {
                                            result: r.result,
                                            local_file: &local_file_c,
                                        };
                                        (call)(result, client_callback.context);
                                    },
                                    Err(e) => {
                                        Self::handle_c_callback_error(
                                            client_callback.error_callback,
                                            client_callback.context,
                                            Error::Internal(format!("Failed to allocate CString: {e:?}")),
                                        );
                                    },
                                }
                            } else {
                                Self::handle_c_callback_error(
                                    client_callback.error_callback,
                                    client_callback.context,
                                    Error::Internal(
                                        "Server did not provide local file info with successful open call".to_owned(),
                                    ),
                                );
                            }
                        },
                        _ => {
                            let result = CacheOpenResponse {
                                result: r.result,
                                local_file: null(),
                            };
                            (call)(result, client_callback.context);
                        },
                    };
                } else {
                    Self::handle_c_callback_error(
                        client_callback.error_callback,
                        client_callback.context,
                        Error::UnexpectedResponseType,
                    );
                }
            },
            Callbacks::CacheOpenForWrite(call) => {
                if let OperationResponse::CacheAsideOpenForWriteResponse(r) = resp.result {
                    match r.result {
                        ResultCode::Success => {
                            if let (Some(local_file), Some(handle)) = (r.local_file, r.handle) {
                                match CString::new(local_file.to_string_lossy().as_bytes()) {
                                    Ok(path) => {
                                        let local_file_c = LocalFile {
                                            handle,
                                            path: path.as_ptr(),
                                        };
                                        let result = CacheOpenResponse {
                                            result: r.result,
                                            local_file: &local_file_c,
                                        };
                                        (call)(result, client_callback.context);
                                    },
                                    Err(e) => {
                                        Self::handle_c_callback_error(
                                            client_callback.error_callback,
                                            client_callback.context,
                                            Error::Internal(format!("Failed to allocate CString: {e:?}")),
                                        );
                                    },
                                }
                            } else {
                                Self::handle_c_callback_error(
                                    client_callback.error_callback,
                                    client_callback.context,
                                    Error::Internal(
                                        "Server did not provide local file info with successful open call".to_owned(),
                                    ),
                                );
                            }
                        },
                        _ => {
                            let result = CacheOpenResponse {
                                result: r.result,
                                local_file: null(),
                            };
                            (call)(result, client_callback.context);
                        },
                    };
                } else {
                    Self::handle_c_callback_error(
                        client_callback.error_callback,
                        client_callback.context,
                        Error::UnexpectedResponseType,
                    );
                }
            },
            Callbacks::Close(call) => {
                if let OperationResponse::CacheAsideCloseResponse(r) = resp.result {
                    (call)(r, client_callback.context)
                } else {
                    Self::handle_c_callback_error(
                        client_callback.error_callback,
                        client_callback.context,
                        Error::UnexpectedResponseType,
                    );
                }
            },
            Callbacks::Remove(call) => {
                if let OperationResponse::CacheAsideRemoveResponse(r) = resp.result {
                    (call)(r, client_callback.context)
                } else {
                    Self::handle_c_callback_error(
                        client_callback.error_callback,
                        client_callback.context,
                        Error::UnexpectedResponseType,
                    );
                }
            },
        }
    }

    pub fn handle_c_callback_error(callback: ErrorCallback, context: *mut c_void, error: Error) {
        if let Ok(error_message) = CString::new(format!("{error:?}")) {
            (callback)(ClientResultCode::from(&error), error_message.as_ptr(), context);
        } else {
            (callback)(ClientResultCode::from(&error), FAILED_TO_ALLOCATE_STRING, context);
        }
    }

    #[cfg(feature = "test")]
    pub fn clear_open_files(&self) {
        self.context.cache_aside.open_file_map.lock().clear();
    }
}

/// Context information for the cache aside interface
pub(crate) struct CacheAsideContext {
    pub open_file_map: Mutex<HashMap<u64, hub_rpc::cache_aside::OpenFile>>,
}

impl CacheAsideContext {
    pub fn new() -> Self {
        Self {
            open_file_map: Mutex::new(HashMap::new()),
        }
    }

    fn update_open_file_map(&self, response: &OperationResponse, request_params: &RequestParams) -> Result<()> {
        match response {
            OperationResponse::CacheAsideOpenForReadResponse(r) => {
                if r.result == ResultCode::Success {
                    if let Some(handle) = r.handle {
                        if let RequestParams::CacheAsideOpenForReadParams(params) = request_params {
                            let existing = self.open_file_map.lock().insert(
                                handle,
                                hub_rpc::cache_aside::OpenFile {
                                    key: params.key.clone(),
                                    provider: params.provider.clone(),
                                    extension: params.extension.clone(),
                                    handle,
                                    open_for_write: false,
                                },
                            );
                            if existing.is_some() {
                                return Err(Error::Internal(format!(
                                    "Previous entry in open file map for {}:{}",
                                    params.provider, params.key
                                )));
                            }
                            Ok(())
                        } else {
                            Err(Error::UnexpectedResponseType)
                        }
                    } else {
                        Err(Error::Internal(
                            "Server did not provide handle with successful open call".to_owned(),
                        ))
                    }
                } else {
                    Ok(())
                }
            },
            OperationResponse::CacheAsideOpenForWriteResponse(r) => {
                if r.result == ResultCode::Success {
                    if let Some(handle) = r.handle {
                        if let RequestParams::CacheAsideOpenForWriteParams(params) = request_params {
                            let existing = self.open_file_map.lock().insert(
                                handle,
                                hub_rpc::cache_aside::OpenFile {
                                    key: params.key.clone(),
                                    provider: params.provider.clone(),
                                    extension: params.extension.clone(),
                                    handle,
                                    open_for_write: true,
                                },
                            );
                            if existing.is_some() {
                                return Err(Error::Internal(format!(
                                    "Previous entry in open file map for {}:{}",
                                    params.provider, params.key
                                )));
                            }
                            Ok(())
                        } else {
                            Err(Error::UnexpectedResponseType)
                        }
                    } else {
                        Err(Error::Internal(
                            "Server did not provide handle with successful open call".to_owned(),
                        ))
                    }
                } else {
                    Ok(())
                }
            },
            OperationResponse::CacheAsideCloseResponse(r) => {
                if r.result == ResultCode::Success {
                    if let RequestParams::CacheAsideCloseParams(params) = request_params {
                        let entry = self.open_file_map.lock().remove(&params.handle);
                        if entry.is_none() {
                            return Err(Error::Internal(format!(
                                "Unknown handle {} in open file map",
                                params.handle
                            )));
                        }
                        Ok(())
                    } else {
                        Err(Error::UnexpectedResponseType)
                    }
                } else {
                    Ok(())
                }
            },
            OperationResponse::CacheAsideRemoveResponse(r) => {
                if r.result == ResultCode::Success {
                    if let RequestParams::CacheAsideRemoveParams(params) = request_params {
                        let entry = self.open_file_map.lock().remove(&params.handle);
                        if entry.is_none() {
                            return Err(Error::Internal(format!(
                                "Unknown handle {} in open file map",
                                params.handle
                            )));
                        }
                        Ok(())
                    } else {
                        Err(Error::UnexpectedResponseType)
                    }
                } else {
                    Ok(())
                }
            },
            _ => Ok(()),
        }
    }
}

// Wrapper struct for the information for a C callback
#[derive(Clone)]
struct ClientCallbackC {
    pub callback: Callbacks,
    pub error_callback: ErrorCallback,
    pub context: *mut c_void,
}

unsafe impl Send for ClientCallbackC {}
unsafe impl Sync for ClientCallbackC {}
