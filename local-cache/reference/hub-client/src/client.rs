// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use flume::{Receiver, Sender};
use parking_lot::{Mutex, RwLock};

use stdext::function_name;
use tracing::{error, info, warn};
use uuid::Uuid;

#[cfg(feature = "test")]
use tokio::sync::Notify;

use pin_project::pin_project;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::thread::{self, JoinHandle};
use std::{env, panic};
use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::atomic::AtomicU64,
};

use futures::{Sink, SinkExt, Stream, StreamExt};

use hub_util::settings::{Hub, Transport};

use hub_rpc::proto_codec::ProtoCodec;
use hub_rpc::*;
use hub_rpc::{
    ConnectParams, GetVersionInfoParams, Interface, OperationResponse, Request, RequestParams, Response, SemVer,
};
use tokio::net::TcpStream;
use tokio::runtime::Builder;
use tokio_serde::formats::MessagePack;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::interfaces::{BlockCache, BlockCacheContext, CacheAside, CacheAsideContext};
use crate::logging::Logger;
use crate::{launcher::Launcher, types::*, Error, Result};

/// Launch modes for the hub executable
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// launch hub exclusively, hub process lifetime tied to launching process (parent)
    Exclusive,
    /// launch hub shared, if hub isn't already running start it, if hub already running return
    Shared,
}

#[derive(Default)]
pub enum TcpCodec {
    #[default]
    MessagePack,
    Protobuf,
}

pub struct ClientBuilder {
    client_name: String,
    log_callback: Option<LogCallback>,
    log_level: Option<LogLevel>,
    mode: Option<Mode>,
    cache_path: Option<PathBuf>,
    tcp_codec: Option<TcpCodec>,
}

impl ClientBuilder {
    pub fn new(client_name: impl AsRef<str>) -> Self {
        Self {
            client_name: client_name.as_ref().into(),
            log_callback: None,
            log_level: None,
            mode: None,
            cache_path: None,
            tcp_codec: None,
        }
    }

    pub fn log_callback(mut self, log_callback: LogCallback) -> Self {
        self.log_callback = Some(log_callback);
        self
    }

    pub fn log_level(mut self, log_level: LogLevel) -> Self {
        self.log_level = Some(log_level);
        self
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = Some(mode);
        self
    }

    pub fn cache_path(mut self, path: impl AsRef<Path>) -> Self {
        self.cache_path = Some(path.as_ref().into());
        self
    }

    pub fn tcp_codec(mut self, codec: TcpCodec) -> Self {
        self.tcp_codec = Some(codec);
        self
    }

    pub fn build(self) -> Result<Client> {
        Client::new(
            self.client_name,
            self.log_callback,
            self.log_level,
            self.mode,
            self.cache_path,
            self.tcp_codec,
        )
    }
}

// NOTE: The purpose of the CodecStream abstraction is so we can support both
//       MessagePack and Protobuf protocols with the same client code. The
//       default MessagePackStream just passes the request and response types
//       through as-is, while the ProtobufStream converts to/from the
//       corresponding protobuf types on serialization/deserialization. This
//       is backed by a hefty amount of conversion code in hub-rpc.

pub trait CodecStream: Sink<Request> + Stream + Unpin {}

#[pin_project]
pub struct MessagePackStream {
    #[pin]
    framed_conn:
        tokio_serde::Framed<Framed<TcpStream, LengthDelimitedCodec>, Response, Request, MessagePack<Response, Request>>,
}

impl MessagePackStream {
    fn new(stream: TcpStream) -> Self {
        // Delimit frames using a length header
        // NOTE: The default max frame length is 8MB, which can be
        //       exceeded by Kit with only 2s of write buffering
        //       using the WriteBackDataStore. 32MB should give us
        //       a bit of breathing room.
        let length_delimited = LengthDelimitedCodec::builder()
            .max_frame_length(32 * 1_024 * 1_024)
            .new_framed(stream);
        let codec: MessagePack<Response, Request> = MessagePack::default();
        let framed_conn = tokio_serde::Framed::new(length_delimited, codec);
        Self { framed_conn }
    }
}

impl CodecStream for MessagePackStream {}

impl Sink<Request> for MessagePackStream {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Request) -> Result<(), Self::Error> {
        self.project().framed_conn.start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_close(cx)
    }
}

impl Stream for MessagePackStream {
    type Item = Result<Response, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.project().framed_conn.poll_next(cx)
    }
}

#[pin_project]
pub struct ProtobufStream {
    #[pin]
    framed_conn: tokio_serde::Framed<
        Framed<TcpStream, LengthDelimitedCodec>,
        proto::Response,
        proto::Request,
        ProtoCodec<proto::Response, proto::Request>,
    >,
}

impl ProtobufStream {
    fn new(stream: TcpStream) -> Self {
        // Delimit frames using a length header
        // NOTE: The default max frame length is 8MB, which can be
        //       exceeded by Kit with only 2s of write buffering
        //       using the WriteBackDataStore. 32MB should give us
        //       a bit of breathing room.
        let length_delimited = LengthDelimitedCodec::builder()
            .max_frame_length(32 * 1_024 * 1_024)
            .new_framed(stream);
        let codec: ProtoCodec<proto::Response, proto::Request> = ProtoCodec::default();
        let framed_conn = tokio_serde::Framed::new(length_delimited, codec);
        Self { framed_conn }
    }
}

impl CodecStream for ProtobufStream {}

impl Sink<Request> for ProtobufStream {
    type Error = std::io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Request) -> Result<(), Self::Error> {
        // convert to proto::Request here
        self.project().framed_conn.start_send(item.into())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().framed_conn.poll_close(cx)
    }
}

impl Stream for ProtobufStream {
    type Item = Result<Response, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.project().framed_conn.poll_next(cx) {
            Poll::Ready(item) => match item {
                Some(result) => match result {
                    // convert from proto::Response here
                    Ok(response) => Poll::Ready(Some(Ok(response.into()))),
                    Err(e) => Poll::Ready(Some(Err(e))),
                },
                None => Poll::Ready(None),
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The Hub Client
///
/// This struct manages launches the Hub server process and connecting to it. It contains the various interfaces
/// that actually implement the Hub API
pub struct Client {
    shutdown_tx: Sender<()>,
    runtime_thread: Mutex<Option<JoinHandle<()>>>,
    context: Arc<Context>,
    pub cache_aside: CacheAside,
    pub block_cache: BlockCache,
    logger: Logger,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Client {
    pub fn builder(client_name: impl AsRef<str>) -> ClientBuilder {
        ClientBuilder::new(client_name)
    }

    fn new(
        client_name: String,
        log_callback: Option<LogCallback>,
        log_level: Option<LogLevel>,
        mode: Option<Mode>,
        cache_path: Option<PathBuf>,
        tcp_codec: Option<TcpCodec>,
    ) -> Result<Self, Error> {
        // Initialize logging first
        let mut builder = Logger::builder();
        if let Some(cb) = log_callback {
            builder = builder.log_callback(cb);
        }
        if let Some(level) = log_level {
            builder = builder.log_level(level);
        }
        let logger = builder.build()?;

        // Check for mode or cache path environment variables
        let mode = match mode {
            Some(m) => m,
            None => Client::determine_mode()?,
        };

        let cache_path = match cache_path {
            Some(p) => Some(p),
            None => Client::check_cache_path()?,
        };

        let cache_size = Client::check_cache_size()?;

        if mode == Mode::Exclusive && cache_path.is_none() {
            error!(
                "{} Using exclusive mode requires providing a cache directory via OMNICLIENT_HUB_CACHE_DIR",
                function_name!()
            );
            return Err(Error::Config(
                "Using exclusive mode requires providing a cache directory via OMNICLIENT_HUB_CACHE_DIR".to_owned(),
            ));
        }

        let codec = match tcp_codec {
            Some(c) => c,
            None => TcpCodec::MessagePack,
        };

        info!(?mode, ?cache_path, ?cache_size, "{} Launching client", function_name!());

        // Signals the client thread to shutdown
        let (shutdown_tx, shutdown_rx): (Sender<()>, Receiver<()>) = flume::bounded(1);

        // Send a request for the client to perform
        let (request_tx, request_rx): (Sender<ClientRequest>, Receiver<ClientRequest>) = flume::unbounded();

        let context = Arc::new(Context {
            request_tx,
            request_rx,
            shutdown_rx,
            runtime_error: RwLock::new(None),
            software_version: RwLock::new(SemVer {
                major: 0,
                minor: 0,
                patch: 0,
            }),
            version_errors: RwLock::new(HashMap::from([
                (Interface::CacheAside, None),
                (Interface::BlockCache, None),
            ])),
            keep_running: AtomicBool::new(true),
            next_request_id: AtomicU64::new(0),
            client_name,
            client_uuid: Uuid::new_v4(),
            mode,
            retries: 40, // 40 retries, with 10ms linear backoff delay, ~8 seconds before giving up on hub
            retry_delay: AtomicU64::new(10),
            cache_aside: CacheAsideContext::new(),
            block_cache: BlockCacheContext::new(),
            cache_path,
            cache_size,
            hub_process: RwLock::new(None),
            #[cfg(feature = "test")]
            on_reconnect_notify: Notify::new(),
            #[cfg(feature = "test")]
            wait_on_reconnect: AtomicBool::new(false),
            hub_http_port: AtomicU16::new(0),
            tcp_codec: codec,
        });

        let mut client = Self {
            shutdown_tx,
            runtime_thread: Mutex::new(None),
            context: context.clone(),
            cache_aside: CacheAside::new(context.clone()),
            block_cache: BlockCache::new(context),
            logger,
        };

        client.run()?;

        Ok(client)
    }

    pub fn shutdown(&self) {
        // HUB-900: temporary remove - info!("{} Shutting down Hub Client", function_name!());
        self.context
            .set_fatal_runtime_error(Error::Shutdown("Client object shutdown".to_owned()));
        let _ = self.shutdown_tx.send(());

        if let Some(runtime) = self.runtime_thread.lock().take() {
            if cfg!(windows) {
                if !thread::panicking() {
                    // On Windows this join can panic if the runtime thread has already been killed.
                    // Similar to this issue https://github.com/rust-lang/rust/issues/112521. There's nothing we can do to detect
                    // that case before calling join, so wrap this in catch_unwind so we don't crash the program on shutdown.
                    // If this were to panic, then the thread is already stopped, so we can continue on.

                    // Install a panic hook that does nothing to prevent the panic message from printing to the console.
                    let prev_hook = panic::take_hook();
                    panic::set_hook(Box::new(|_| {}));

                    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                        let _ = runtime.join();
                    }));

                    // Restore the previous panic hook
                    panic::set_hook(prev_hook);
                }
            } else {
                let _ = runtime.join();
            }
        }
    }

    pub fn get_last_error(&self) -> Option<Error> {
        self.context.runtime_error.read().clone()
    }

    pub fn set_log_callback(&self, callback: LogCallback) -> Result<()> {
        self.logger.set_log_callback(callback)
    }

    pub fn disable_log_callback(&self) -> Result<()> {
        self.logger.disable_log_callback()
    }

    pub fn set_log_level(&self, level: LogLevel) -> Result<()> {
        self.logger.set_log_level(level)
    }

    pub fn get_software_version(&self) -> SemVer {
        *self.context.software_version.read()
    }

    pub fn get_hub_http_port(&self) -> u16 {
        self.context.hub_http_port.load(Ordering::Relaxed)
    }

    // Starts the runner thread for the client
    fn run(&mut self) -> Result<()> {
        // Signal the client that hub was discovered or configured for use
        // Otherwise client should continue on without hub (for now hub is not required)
        let (hub_found_tx, hub_found_rx): (Sender<Result<()>>, Receiver<Result<()>>) = flume::bounded(1);

        let context = self.context.clone();
        let mut launcher = Launcher::new(context.mode)?;

        *self.runtime_thread.lock() = Some(thread::spawn(move || {
            // Block SIGPIPE for this thread in case the Hub server dies while we're writing.
            // Rust programs by default block SIGPIPE, but this library
            // can be used by other languages which do not do that. The SO_NOSIGPIPE socket option is
            // BSD only, and Rust libraries don't have the option to provide MSG_NOSIGNAL to the underlying
            // send call. Therefore are last resort is to block SIGPIPE for this thread.
            #[cfg(target_os = "linux")]
            {
                unsafe {
                    let mut mask: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut mask);
                    libc::sigaddset(&mut mask, libc::SIGPIPE);
                    libc::pthread_sigmask(libc::SIG_BLOCK, &mask as *const libc::sigset_t, std::ptr::null_mut());
                }
            }

            let runtime = match Builder::new_current_thread()
                .thread_name("Hub Client")
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    context.set_fatal_runtime_error(Error::from(&e));
                    // Let the launching thread know we've hit a fatal error
                    let _ = hub_found_tx.send(Err(Error::from(e)));
                    return;
                },
            };

            let mut callback_map: HashMap<u64, ClientRequest> = HashMap::new();

            let mut retry_count = 0;
            let mut retry_reason = String::new();

            while context.keep_running.load(Ordering::SeqCst) {
                retry_count += 1;

                runtime.block_on(async {
                    // OMPE-2707: only try so many times before giving up. Otherwise the hub client
                    // can hang by infinitely retrying here...
                    if retry_count > context.retries {
                        let err = Error::Shutdown(retry_reason.clone());
                        context.set_fatal_runtime_error(err.clone());
                        let _ = hub_found_tx.send(Err(err));
                        return;
                    }

                    if retry_count > 1 {
                        warn!(retry_reason, "Hub encountered error. Trying to reconnect to Hub.");
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            context.retry_delay.load(Ordering::SeqCst) * retry_count,
                        ))
                        .await;
                    }

                    // Tests may want to control reconnect logic
                    #[cfg(feature = "test")]
                    {
                        if context.wait_on_reconnect.load(Ordering::Relaxed) {
                            context.on_reconnect_notify.notified().await;
                            context.wait_on_reconnect.store(false, Ordering::Relaxed);
                        }
                    }

                    // Attempt to launch Hub. If Hub is already running this will give us the connection information of
                    // the currently running executable.
                    let search_path = env::var_os("OMNICLIENT_HUB_EXE").map(PathBuf::from);

                    let settings = match launcher.launch(search_path.as_deref(), context.cache_path.as_deref(), context.cache_size.as_deref()) {
                        Ok((child, config)) => {
                            *context.hub_process.write() = Some(child);
                            serde_json::from_str::<Hub>(&config).unwrap()
                        },
                        Err(e) => match e {
                            // If Hub is not found, signal the launching thread and exit immediately.
                            Error::HubNotFound => {
                                context.set_fatal_runtime_error(Error::HubNotFound);
                                let _ = hub_found_tx.send(Err(Error::HubNotFound));
                                return;
                            }
                            _ => {
                                retry_reason = format!("Hub failed to launch: {e:?}");
                                return;
                            }
                        }
                    };

                    context.hub_http_port.store(settings.http.port, Ordering::Relaxed);

                    // NOTE: Doing things this way instead of retrieving the
                    //       port based on the intended codec means that we
                    //       can't support old versions of Hub with a new Hub
                    //       Client, but this seems like a reasonable
                    //       requirement anyway.
                    let (host, port) = match settings.transport {
                        Transport::Tcp {
                            host,
                            rpc_port,
                            ext_port,
                            ..
                        } => {
                            match context.tcp_codec {
                                TcpCodec::MessagePack => (host, rpc_port),
                                TcpCodec::Protobuf => (host, ext_port),
                            }
                        },
                        _ => todo!("Implement other transports"),
                    };

                    let addr = match IpAddr::from_str(&host) {
                        Ok(h) => SocketAddr::new(h, port),
                        Err(e) => {
                            context.set_fatal_runtime_error(Error::AddrParse(e));
                            return;
                        },
                    };

                    let stream = match TcpStream::connect(&addr).await {
                        Ok(t) => t,
                        Err(e) => {
                            retry_reason = format!("Tcp connect failed: {e:?}");
                            return;
                        },
                    };

                    if let Err(e) = stream.set_nodelay(true) {
                        // Debateable if this should be a fatal error or not, but loop back and retry for now.
                        retry_reason = format!("Could not set nodelay: {e:?}");
                        return;
                    }

                    // NOTE: This could probably be done better, by moving the types into CodecStream somehow.
                    type ErrorType = std::io::Error;
                    type ItemType = Result<Response, std::io::Error>;
                    let mut framed_conn: Box<dyn CodecStream<Item = ItemType, Error = ErrorType>> = match context.tcp_codec {
                        TcpCodec::MessagePack => {
                            Box::new(MessagePackStream::new(stream))
                        },
                        TcpCodec::Protobuf => {
                            Box::new(ProtobufStream::new(stream))
                        },
                    };

                    // Gather interface versions from the server
                    if let Err(e) = Self::initialize_interfaces(&mut framed_conn, context.clone()).await {
                        match e {
                            Error::Shutdown(err) => {
                                context.set_fatal_runtime_error(Error::Shutdown(err));
                                return;
                            },
                            _ => {
                                retry_reason = format!("{e:?}");
                                return;
                            }
                        }
                    }

                    // Reissue outstanding requests
                    for (id, request) in &callback_map {
                        let request = Request {
                            request_id: *id,
                            params: request.params.clone(),
                        };
                        if let Err(e) = framed_conn.send(request).await {
                            // Failed to send a request, exit and retry
                            retry_reason = format!("Error sending outstanding request: {e:?}");
                            return;
                        }
                    }

                    // We've successfully connected to Hub. Reset the retry_count so we reset the linear backoff start point.
                    retry_count = 0;

                    // Also let the client know that we have connected
                    let _ = hub_found_tx.send(Ok(()));

                    loop {
                        tokio::select! {
                            // Response from the server
                            read_result = framed_conn.next() => {
                                match read_result {
                                    Some(Ok(resp)) => {
                                        if let Some(client_request) = callback_map.remove(&resp.request_id) {
                                            (client_request.callback)(Ok(resp));
                                        }
                                    }
                                    Some(Err(e)) => {
                                        retry_reason = format!("Error receiving response: {e:?}");
                                        break;
                                    },
                                    None => {
                                        retry_reason = "Connection to server closed".to_owned();
                                        break;
                                    },
                                }
                            }
                            // New request from the client
                            req_result = context.request_rx.recv_async() => {
                                match req_result {
                                    Ok(req) => {
                                        // Check if the provided interface is supported
                                        match context.version_errors.read().get(&req.interface) {
                                            // No error, continue
                                            Some(None) => {},
                                            // Version error, exit immediately
                                            Some(Some(e)) => {
                                                (req.callback)(Err(e.clone()));
                                                break;
                                            },
                                            None => {
                                                (req.callback)(Err(Error::Internal("Unknown interface type requested".to_owned())));
                                                break;
                                            }
                                        }
                                        let req_id = context.next_request_id.fetch_add(1, Ordering::SeqCst);
                                        let request = Request {
                                            request_id: req_id,
                                            params: req.params.clone(),
                                        };
                                        callback_map.insert(req_id, req);
                                        if let Err(e) = framed_conn.send(request).await {
                                            retry_reason = format!("Error sending request: {e:?}");
                                            break;
                                        }
                                    },
                                    Err(_) => {
                                        // Getting a receive error here means all senders have been dropped, which would
                                        // only happen if the client is shutdown
                                        context.set_fatal_runtime_error(Error::Shutdown("All request senders dropped".to_owned()));
                                        break;
                                    }
                                }
                            }

                            // Shutdown signal
                            _ = &mut context.shutdown_rx.recv_async() => {
                                context.set_fatal_runtime_error(Error::Shutdown("Client object shutdown".to_owned()));
                                break;
                            }
                        }
                    }
                });
            }
        }));

        // if the hub binary was not specified (e.g. by env var) and we were unable to find
        // a hub binary on disk, this should return hub-not-found and allow the client to continue
        // without hub (at least until hub is required)
        hub_found_rx.recv()?
    }

    // Performs the necessary operations to initialize the interfaces. This includes checking supported versions
    // and performing interface specific startup
    async fn initialize_interfaces<S>(stream: &mut S, context: Arc<Context>) -> Result<()>
    where
        S: StreamExt<Item = Result<Response, std::io::Error>>
            + SinkExt<Request, Error = std::io::Error>
            + std::marker::Unpin,
    {
        // Gather interface versions from the server
        Self::check_versions(stream, context.clone()).await?;

        // Call the connect method, giving the server info about this client
        Self::connect_client(stream, context.clone()).await?;

        // Startup the CacheAside interface if it is supported
        if context.version_errors.read()[&Interface::CacheAside].is_none() {
            CacheAside::startup(stream, context.clone()).await?;
        }

        // Startup the BlockCache interface if it is supported
        if context.version_errors.read()[&Interface::BlockCache].is_none() {
            BlockCache::startup(stream, context.clone()).await?;
        }

        Ok(())
    }

    /// Get's version information from the server and checks the interface versions
    async fn check_versions<S>(stream: &mut S, context: Arc<Context>) -> Result<()>
    where
        S: StreamExt<Item = Result<Response, std::io::Error>>
            + SinkExt<Request, Error = std::io::Error>
            + std::marker::Unpin,
    {
        let req = Request {
            request_id: context.next_request_id.fetch_add(1, Ordering::SeqCst),
            params: RequestParams::GetVersionInfoParams(GetVersionInfoParams {}),
        };

        match run_blocking_operation(req, stream, context.shutdown_rx.clone()).await {
            Ok(response) => {
                if let OperationResponse::GetVersionInfoResponse(version_response) = response.result {
                    *context.software_version.write() = version_response.version_info.software_version;
                    CacheAside::check_version(&version_response.version_info, &context.version_errors);
                    BlockCache::check_version(&version_response.version_info, &context.version_errors);
                    Ok(())
                } else {
                    Err(Error::Internal(
                        "GetVersionInfo response wasn't GetVersionInfoResponse".to_owned(),
                    ))
                }
            },
            Err(e) => Err(e),
        }
    }

    /// Get's version information from the server and checks the interface versions
    async fn connect_client<S>(stream: &mut S, context: Arc<Context>) -> Result<()>
    where
        S: StreamExt<Item = Result<Response, std::io::Error>>
            + SinkExt<Request, Error = std::io::Error>
            + std::marker::Unpin,
    {
        let req = Request {
            request_id: context.next_request_id.fetch_add(1, Ordering::SeqCst),
            params: RequestParams::ConnectParams(ConnectParams {
                client_name: context.client_name.clone(),
                client_uuid: context.client_uuid,
            }),
        };

        match run_blocking_operation(req, stream, context.shutdown_rx.clone()).await {
            Ok(response) => match response.result {
                OperationResponse::ConnectResponse => Ok(()),
                _ => Err(Error::Internal(
                    "Client did not return ConnectResponse for Connect API call".to_owned(),
                )),
            },
            Err(e) => Err(e),
        }
    }

    // Checks for OMNICLIENT_HUB_MODE environment variable, or returns Mode::Shared if that variable is not set
    fn determine_mode() -> Result<Mode> {
        match env::var("OMNICLIENT_HUB_MODE") {
            Ok(val) => match val.to_lowercase().as_str() {
                "shared" => Ok(Mode::Shared),
                "exclusive" => Ok(Mode::Exclusive),
                _ => Err(Error::Config(format!(
                    "Unknown value provided for OMNICLIENT_HUB_MODE: {val}"
                ))),
            },
            Err(env::VarError::NotPresent) => Ok(Mode::Shared),
            Err(env::VarError::NotUnicode(s)) => Err(Error::Config(format!(
                "Could not parse OMNICLIENT_HUB_MODE value: {s:?}"
            ))),
        }
    }

    // Checks for OMNICLIENT_HUB_CACHE_DIR environment variable
    fn check_cache_path() -> Result<Option<PathBuf>> {
        match env::var("OMNICLIENT_HUB_CACHE_DIR") {
            Ok(val) => {
                // Hub server will create the cache dir if it doesn't exist, so we don't need to check for existence here
                Ok(Some(PathBuf::from(val)))
            },
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(s)) => Err(Error::Config(format!(
                "Could not parse OMNICLIENT_HUB_CACHE_DIR value: {s:?}"
            ))),
        }
    }

    // Checks for OMNICLIENT_HUB_CACHE_SIZE environment variable
    fn check_cache_size() -> Result<Option<String>> {
        match env::var("OMNICLIENT_HUB_CACHE_SIZE") {
            Ok(val) => Ok(Some(val)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(s)) => Err(Error::Config(format!(
                "Could not parse OMNICLIENT_HUB_CACHE_SIZE value: {s:?}"
            ))),
        }
    }

    #[cfg(feature = "test")]
    pub fn get_hub_process(&self) -> Option<Child> {
        self.context.hub_process.write().take()
    }

    #[cfg(feature = "test")]
    pub fn set_wait_on_reconnect(&self) {
        self.context.wait_on_reconnect.store(true, Ordering::Relaxed);
    }

    #[cfg(feature = "test")]
    pub fn notify_on_reconnect(&self) {
        self.context.on_reconnect_notify.notify_one();
    }
}

/// Runtime inforamation for the Hub Client
pub(crate) struct Context {
    request_tx: Sender<ClientRequest>,
    request_rx: Receiver<ClientRequest>,
    pub shutdown_rx: Receiver<()>,
    runtime_error: RwLock<Option<Error>>,
    software_version: RwLock<SemVer>,
    version_errors: RwLock<HashMap<Interface, Option<Error>>>,
    pub cache_aside: CacheAsideContext,
    pub block_cache: BlockCacheContext,
    pub keep_running: AtomicBool,
    pub next_request_id: AtomicU64,
    pub client_name: String,
    pub client_uuid: Uuid,
    pub mode: Mode,
    pub retries: u64,
    pub retry_delay: AtomicU64,
    pub cache_path: Option<PathBuf>,
    pub cache_size: Option<String>,
    pub hub_process: RwLock<Option<Child>>,
    // Notify to wait on on reconnect
    #[cfg(feature = "test")]
    on_reconnect_notify: tokio::sync::Notify,
    // Set to true if you want the client to wait on reconnect
    #[cfg(feature = "test")]
    wait_on_reconnect: AtomicBool,
    hub_http_port: AtomicU16,
    tcp_codec: TcpCodec,
}

impl Context {
    // Set a fatal error in the client. This will error out all outstanding requests and prevent any new requests from
    // being issued.
    fn set_fatal_runtime_error(&self, err: Error) {
        *self.runtime_error.write() = Some(err.clone());
        self.keep_running.store(false, Ordering::SeqCst);

        // At this point no more requests can be issued because we got the write lock on the runtime error, so drain
        // any outstanding requests
        for client_request in self.request_rx.drain() {
            (client_request.callback)(Err(err.clone()));
        }
    }

    pub fn send_callback_request<F>(&self, params: RequestParams, interface: Interface, callback: F) -> Result<()>
    where
        F: FnOnce(Result<Response>) + Send + 'static,
    {
        let tx = self.request_tx.clone();
        let runtime_error = self.runtime_error.read();
        if let Some(err) = runtime_error.as_ref() {
            return Err(err.clone());
        }
        tx.send(ClientRequest {
            params,
            callback: Box::new(callback),
            interface,
        })
        .map_err(Error::from)
    }

    pub async fn send_request(&self, params: RequestParams, interface: Interface) -> Result<OperationResponse> {
        let (resp_tx, resp_rx): (Sender<Result<Response>>, Receiver<Result<Response>>) = flume::bounded(1);

        let callback = move |resp: Result<Response>| {
            let _ = resp_tx.send(resp);
        };

        {
            let runtime_error = self.runtime_error.read();
            if let Some(err) = runtime_error.as_ref() {
                return Err(err.clone());
            }

            self.request_tx.send(ClientRequest {
                params,
                callback: Box::new(callback),
                interface,
            })?;
        }

        match resp_rx.recv_async().await {
            Ok(r) => match r {
                Ok(r) => Ok(r.result),
                Err(e) => Err(e),
            },
            Err(e) => Err(Error::ChannelRecv(e)),
        }
    }
}

/// Performs a blocking operation to the server
pub(crate) async fn run_blocking_operation<S>(
    req: Request,
    stream: &mut S,
    shutdown_rx: Receiver<()>,
) -> Result<Response>
where
    S: StreamExt<Item = Result<Response, std::io::Error>>
        + SinkExt<Request, Error = std::io::Error>
        + std::marker::Unpin,
{
    if let Err(e) = stream.send(req).await {
        return Err(Error::Io(e.to_string()));
    }

    tokio::select! {
        // Wait for response
        result = stream.next() => {
            match result {
                Some(Ok(response)) => {
                    Ok(response)
                },
                Some(Err(e)) => {
                    Err(Error::Io(e.to_string()))
                }
                None => {
                    Err(Error::Internal("No response from request".to_owned()))
                }
            }
        }
        // Timeout on request
        _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
            Err(Error::Internal("Timeout waiting for response".to_owned()))
        }
        // Client gets shutdown
        _ = shutdown_rx.recv_async() => {
            Err(Error::Shutdown("Client object shutdown".to_owned()))
        }
    }
}
