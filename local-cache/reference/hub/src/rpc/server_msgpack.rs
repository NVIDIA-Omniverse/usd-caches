// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use semver::Version;
use tokio::net::TcpStream;
use tokio_serde::formats::MessagePack;
use tokio_util::codec::LengthDelimitedCodec;
use tracing::{error, info, trace};

use hub_rpc::cache_aside::*;
use hub_rpc::*;
use hub_util::settings::Transport;

use super::dispatcher::Dispatcher;
use crate::context::Context;
use crate::Result;

lazy_static! {
    static ref VERSION_INFO: VersionInfo = VersionInfo {
        software_version: match Version::parse(env!("CARGO_PKG_VERSION")) {
            Ok(v) => SemVer {
                major: v.major,
                minor: v.minor,
                patch: v.patch,
            },
            Err(_) => SemVer {
                major: 0,
                minor: 0,
                patch: 0
            },
        },
        interface_versions: vec![
            InterfaceVersion {
                interface: Interface::CacheAside,
                version: 1
            },
            InterfaceVersion {
                interface: Interface::BlockCache,
                version: 1
            }
        ],
    };
    static ref CACHE_ASIDE_VERSION: SemVer = SemVer {
        major: 1,
        minor: 0,
        patch: 0
    };

    // Tell old clients we don't support any interfaces
    static ref VERSION_INFO_DEPRECATED: VersionInfo = VersionInfo {
        software_version: match Version::parse(env!("CARGO_PKG_VERSION")) {
            Ok(v) => SemVer {
                major: v.major,
                minor: v.minor,
                patch: v.patch,
            },
            Err(_) => SemVer {
                major: 0,
                minor: 0,
                patch: 0
            },
        },
        interface_versions: Vec::new(),
    };
}

/// Basic struct for running the MessagePack RPC server. This struct mostly exists so we can detect when it gets dropped,
/// so we can indicate to the open connections that the server side is being destructed so they shouldn't clean up.
pub struct RpcServer {
    exiting: Arc<AtomicBool>,
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        trace!("Messagepack Rpc Server dropped");
        self.exiting.store(true, Ordering::SeqCst);
    }
}

impl RpcServer {
    pub fn new() -> Self {
        Self {
            exiting: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn run_server(&self, context: Arc<Context>) -> Result<()> {
        let addr = match &context.settings.read().hub.transport {
            Transport::Tcp { host, rpc_port, .. } => SocketAddr::new(IpAddr::from_str(host.as_str())?, *rpc_port),
            _ => todo!("Implement other transports"),
        };

        let listener = tokio::net::TcpListener::bind(&addr).await?;

        // The provided port might have been 0, making the OS assign us a port, so update port in the settings if so
        if addr.port() == 0 {
            let assigned_port = listener.local_addr()?.port();
            info!("Messagepack Tcp transport bound to port {}", assigned_port);

            // Update the RPC port in the settings
            let mut set = context.settings.write();
            match &set.hub.transport {
                Transport::Tcp { host, ext_port, .. } => {
                    set.hub.transport = Transport::Tcp {
                        host: host.to_string(),
                        rpc_port: assigned_port,
                        ext_port: *ext_port,
                    };
                },
                _ => todo!("Implement other transports"),
            }
        }

        // Signal that we've bound to the port
        context.msgpack_ready.notify_one();

        loop {
            // Listen for new connections
            match listener.accept().await {
                Ok((socket, remote_addr)) => {
                    socket.set_nodelay(true)?;
                    // Spawn task to handle connection
                    tokio::spawn(handle_connection(
                        context.clone(),
                        self.exiting.clone(),
                        socket,
                        remote_addr,
                    ));
                },
                Err(e) => {
                    error!("Error accepting client connection: {:?}", e);
                },
            }
        }
    }
}

async fn handle_connection(
    context: Arc<Context>,
    exiting: Arc<AtomicBool>,
    socket: TcpStream,
    remote_addr: SocketAddr,
) {
    info!("Received new connection from {remote_addr}");

    let mut dispatcher = Dispatcher::new(context.clone(), exiting);

    // Delimit frames using a length header
    // NOTE: The default max frame length is 8MB, which can be exceeded by Kit
    //       with only 2s of write buffering using the WriteBackDataStore.
    //       32MB should give us a bit of breathing room.
    let length_delimited = LengthDelimitedCodec::builder()
        .max_frame_length(32 * 1_024 * 1_024)
        .new_framed(socket);
    let codec: MessagePack<Request, Response> = MessagePack::default();
    let mut framed_conn = tokio_serde::Framed::new(length_delimited, codec);

    loop {
        // Read from socket and process the request
        match framed_conn.next().await {
            Some(Ok(msg)) => {
                trace!("Received request {:?} from {remote_addr}", msg);
                let response = process_request(msg, &mut dispatcher).await;
                if let Err(e) = framed_conn.send(response).await {
                    error!("Error sending response to client {:?}: {:?}", remote_addr, e);
                }
            },
            Some(Err(e)) => {
                error!("Error reading message from client {:?}: {:?}", remote_addr, e);
            },
            None => {
                info!("Connection from {remote_addr} closed");
                break;
            },
        }
    }
}

async fn process_request(request: Request, dispatcher: &mut Dispatcher) -> Response {
    // TODO: could probably explore using proc macro for this match
    match request.params {
        // If an old client connects, indicate we don't support any of its interfaces
        RequestParams::GetVersionInfoParamsDeprecated(_) => Response {
            request_id: request.request_id,
            result: OperationResponse::GetVersionInfoResponse(GetVersionInfoResponse {
                version_info: VERSION_INFO_DEPRECATED.clone(),
            }),
        },
        RequestParams::GetVersionInfoParams(_) => Response {
            request_id: request.request_id,
            result: OperationResponse::GetVersionInfoResponse(GetVersionInfoResponse {
                version_info: VERSION_INFO.clone(),
            }),
        },
        RequestParams::ConnectParams(p) => match dispatcher.connect(p) {
            Ok(_) => Response {
                request_id: request.request_id,
                result: OperationResponse::ConnectResponse,
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::CacheAsideGetServerApiVersionParams(_) => Response {
            request_id: request.request_id,
            result: OperationResponse::CacheAsideGetServerApiVersionResponse(GetServerApiVersionResponse {
                version: *CACHE_ASIDE_VERSION,
            }),
        },
        RequestParams::CacheAsideOpenForReadParams(p) => match dispatcher.cache_open_for_read(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::CacheAsideOpenForReadResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::CacheAsideOpenForWriteParams(p) => match dispatcher.cache_open_for_write(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::CacheAsideOpenForWriteResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::CacheAsideCloseParams(p) => match dispatcher.cache_close(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::CacheAsideCloseResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::CacheAsideRemoveParams(p) => match dispatcher.cache_remove(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::CacheAsideRemoveResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::CacheAsideConnectParams(p) => match dispatcher.cache_connect(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::CacheAsideConnectResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockConnectParams(p) => match dispatcher.block_connect(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockConnectResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockGetOpenParams(p) => match dispatcher.block_get(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockGetOpenResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockGetCloseParams(p) => match dispatcher.block_close(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockGetCloseResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockPutOpenParams(p) => match dispatcher.block_put(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockPutOpenResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockPutCommitParams(p) => match dispatcher.block_commit(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockPutCommitResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockPutAbortParams(p) => match dispatcher.block_abort(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockPutAbortResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
        RequestParams::BlockStatParams(p) => match dispatcher.block_stat(p) {
            Ok(r) => Response {
                request_id: request.request_id,
                result: OperationResponse::BlockStatResponse(r),
            },
            Err(e) => Response {
                request_id: request.request_id,
                result: OperationResponse::ErrorResponse(ErrorResponse { message: e.to_string() }),
            },
        },
    }
}
