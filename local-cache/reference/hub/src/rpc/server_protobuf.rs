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
use tokio_util::codec::LengthDelimitedCodec;
use tracing::{error, info, trace};

use hub_rpc::proto::request_params::Params as ParamsEnum;
use hub_rpc::proto::response_result::Result as ResultEnum;
use hub_rpc::proto_codec::ProtoCodec;
use hub_rpc::*;
use hub_util::settings::Transport;

use super::dispatcher::Dispatcher;
use crate::context::Context;
use crate::Result;

lazy_static! {
    static ref VERSION_INFO: proto::VersionInfo = proto::VersionInfo {
        software_version: match Version::parse(env!("CARGO_PKG_VERSION")) {
            Ok(v) => Some(proto::SemVer {
                major: v.major,
                minor: v.minor,
                patch: v.patch,
            }),
            Err(_) => Some(proto::SemVer {
                major: 0,
                minor: 0,
                patch: 0
            }),
        },
        interface_versions: vec![
            proto::InterfaceVersion {
                interface: proto::Interface::CacheAside as i32,
                version: 1
            },
            proto::InterfaceVersion {
                interface: proto::Interface::BlockCache as i32,
                version: 1
            }
        ],
    };
    static ref CACHE_ASIDE_VERSION: proto::SemVer = proto::SemVer {
        major: 1,
        minor: 0,
        patch: 0
    };
}

/// Basic struct for running the Protobuf RPC server. This struct mostly exists so we can detect when it gets dropped,
/// so we can indicate to the open connections that the server side is being destructed so they shouldn't clean up.
pub struct ProtobufServer {
    exiting: Arc<AtomicBool>,
}

impl Drop for ProtobufServer {
    fn drop(&mut self) {
        trace!("Protobuf Rpc Server dropped");
        self.exiting.store(true, Ordering::SeqCst);
    }
}

impl ProtobufServer {
    pub fn new() -> Self {
        Self {
            exiting: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn run_server(&self, context: Arc<Context>) -> Result<()> {
        let addr = match &context.settings.read().hub.transport {
            Transport::Tcp { host, ext_port, .. } => SocketAddr::new(IpAddr::from_str(host.as_str())?, *ext_port),
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
                Transport::Tcp { host, rpc_port, .. } => {
                    set.hub.transport = Transport::Tcp {
                        host: host.to_string(),
                        rpc_port: *rpc_port,
                        ext_port: assigned_port,
                    };
                },
                _ => todo!("Implement other transports"),
            }
        }

        // Signal that we've bound to the port
        context.protobuf_ready.notify_one();

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
    let codec: ProtoCodec<proto::Request, proto::Response> = ProtoCodec::default();
    let mut framed_conn = tokio_serde::Framed::new(length_delimited, codec);

    loop {
        // Read from socket and process the request
        match framed_conn.next().await {
            Some(Ok(msg)) => {
                trace!("Received request {:?} from {remote_addr}", msg);
                let response = process_request(msg, &mut dispatcher);
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

fn process_request(request: proto::Request, dispatcher: &mut Dispatcher) -> proto::Response {
    let params = try_get_params(&request);
    if params.is_none() {
        return generate_response(
            &request,
            ResultEnum::ErrorInfo(proto::ErrorInfoResult {
                message: "unrecognized message type".to_string(),
            }),
        );
    }
    let params = &params.unwrap();
    let result = process_params(params, dispatcher);
    generate_response(&request, result)
}

fn try_get_params(request: &proto::Request) -> Option<&ParamsEnum> {
    // NOTE: The way prost deals with oneof types is kind of annoying. It adds
    //       an additional layer using the oneof field name, which translates
    //       to a "params.params" syntax in proto::Request. However, the
    //       RequestParams wrapper makes it easier to add new parameter sets
    //       later, so this _is_ the correct design from a protocol
    //       standpoint. It's just unfortunate that it looks bad in Rust.
    if let Some(params) = &request.params {
        if let Some(params) = &params.params {
            return Some(params);
        }
    }
    None
}

fn process_params(params: &ParamsEnum, dispatcher: &mut Dispatcher) -> ResultEnum {
    match params {
        // ================================ Hub ===============================
        ParamsEnum::ClientConnect(p) => match ConnectParams::try_from(p) {
            Ok(p) => match dispatcher.connect(p) {
                Ok(_) => ResultEnum::ClientConnect(proto::ClientConnectResult {}),
                Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
            },
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::ServiceVersionInfo(_) => ResultEnum::ServiceVersionInfo(proto::ServiceVersionInfoResult {
            version_info: Some(VERSION_INFO.clone()),
        }),
        // ============================ Cache Aside ===========================
        ParamsEnum::CacheAsideVersionInfo(_) => ResultEnum::CacheAsideVersionInfo(proto::CacheAsideVersionInfoResult {
            version: Some(*CACHE_ASIDE_VERSION),
        }),
        ParamsEnum::CacheAsideConnect(p) => match dispatcher.cache_connect(p.into()) {
            Ok(r) => ResultEnum::CacheAsideConnect(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::CacheAsideOpenForRead(p) => match dispatcher.cache_open_for_read(p.into()) {
            Ok(r) => ResultEnum::CacheAsideOpenForRead(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::CacheAsideOpenForWrite(p) => match dispatcher.cache_open_for_write(p.into()) {
            Ok(r) => ResultEnum::CacheAsideOpenForWrite(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::CacheAsideClose(p) => match dispatcher.cache_close(p.into()) {
            Ok(r) => ResultEnum::CacheAsideClose(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::CacheAsideRemove(p) => match dispatcher.cache_remove(p.into()) {
            Ok(r) => ResultEnum::CacheAsideRemove(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        // ============================ Block Cache ===========================
        ParamsEnum::BlockCacheConnect(p) => match dispatcher.block_connect(p.into()) {
            Ok(r) => ResultEnum::BlockCacheConnect(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCacheGet(p) => match dispatcher.block_get(p.into()) {
            Ok(r) => ResultEnum::BlockCacheGet(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCachePut(p) => match dispatcher.block_put(p.into()) {
            Ok(r) => ResultEnum::BlockCachePut(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCacheClose(p) => match dispatcher.block_close(p.into()) {
            Ok(r) => ResultEnum::BlockCacheClose(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCacheCommit(p) => match dispatcher.block_commit(p.into()) {
            Ok(r) => ResultEnum::BlockCacheCommit(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCacheAbort(p) => match dispatcher.block_abort(p.into()) {
            Ok(r) => ResultEnum::BlockCacheAbort(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
        ParamsEnum::BlockCacheStat(p) => match dispatcher.block_stat(p.into()) {
            Ok(r) => ResultEnum::BlockCacheStat(r.into()),
            Err(e) => ResultEnum::ErrorInfo(proto::ErrorInfoResult { message: e.to_string() }),
        },
    }
}

fn generate_response(request: &proto::Request, result: ResultEnum) -> proto::Response {
    proto::Response {
        request_id: request.request_id,
        result: Some(proto::ResponseResult { result: Some(result) }),
    }
}
