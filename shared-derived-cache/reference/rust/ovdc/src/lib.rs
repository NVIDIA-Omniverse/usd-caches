// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Binary and library for the KV node.
#![deny(unused, missing_docs)]

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    path::Path,
    sync::Arc,
    time::Duration,
};

use context::Context;
use count_connections::ConnectionState;
use futures::{Stream, StreamExt};
use hyper::body::Incoming;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use ovdc_bytes::keys;
use ovdc_grpc::proto::key_value_server::KeyValueServer;
use ovdc_map::{environment::Rocks, Database};
use ovdc_settings::{Cfg, Snap};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
    sync::mpsc,
};
use tonic::{
    service::{Interceptor, Routes},
    transport::server::Connected,
};
use tower::ServiceExt;

pub mod context;
mod count_connections;
pub mod grpc;
mod grpc_span_telemetry;
mod jwt;

use crate::{
    context::Grpc,
    count_connections::{Counted, TcpListenerStream},
    grpc_span_telemetry::{
        GrpcSpanTelemetry, GrpcTraceOnEos, GrpcTraceOnFailure, GrpcTraceOnRequest,
        GrpcTraceOnResponse,
    },
};

#[cfg(test)]
pub mod embedded;

#[cfg(test)]
mod tests;

/// Builds an OVDC node with the given arguments.
///
/// See [Prepared::new].
pub struct Prepared {
    cx: Context<Rocks>,
    grpc_socket: TcpListener,
    grpc_cfg: Grpc,
}

impl Prepared {
    /// Prepares for the application to be served.
    ///
    /// This allows the local addresses for the sockets to obtained.
    pub async fn new(env_path: impl AsRef<Path>, settings: Cfg, _pod_ip: Option<IpAddr>) -> Self {
        tracing::info!(version = ovdc_version::VERSION_STR, "ovdc version");

        let env_path = env_path.as_ref();

        let grpc = settings.snap().subtable::<Grpc>("grpc");
        let grpc_socket = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, grpc.port))
            .await
            .expect("could not bind tcp socket for grpc");

        // Open the DB, LRU, etc.
        let cx = context::open::<Rocks>(env_path, settings)
            .await
            .expect("failed to open context, cannot start node");

        Prepared {
            cx,
            grpc_socket,
            grpc_cfg: grpc,
        }
    }

    /// Serves an OVDC node with the previously given setup information.
    pub async fn serve(self) {
        let Self {
            cx,
            grpc_socket,
            grpc_cfg,
        } = self;

        let _ = grpc::WRITE_TIMEOUT.set(
            grpc_cfg
                .write_timeout
                .dur()
                .expect("write_timeout must be non-negative"),
        );

        let keepalive = socket2::TcpKeepalive::new()
            .with_time(
                grpc_cfg
                    .tcp_keepalive_after_idle_time
                    .dur()
                    .expect("tcp_keepalive_after_idle_time must be non-negative"),
            )
            .with_interval(
                grpc_cfg
                    .tcp_keepalive_interval
                    .dur()
                    .expect("tcp_keepalive_interval must be non-negative"),
            )
            .with_retries(grpc_cfg.tcp_keepalive_retries);

        let grpc_socket = Counted::from(TcpListenerStream::new(grpc_socket, keepalive));

        tokio::select! {
            _ = serve_grpc(&cx, grpc_socket) => {},
            _ = close_on_hangup(&cx) => {},
        }
    }

    /// Returns the local socket address being used for GRPC.
    pub fn grpc_local_addr(&self) -> SocketAddr {
        self.grpc_socket.local_addr().unwrap()
    }
}

pub(crate) async fn serve_grpc<D: Database, I, IO>(cx: &Context<D>, io: I)
where
    I: Stream<Item = io::Result<IO>> + Unpin + Send + 'static,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    let cx = cx.clone();

    let grpc_tls: context::Tls = cx.settings2.snap().subtable("grpcTls");

    // Parse and enable tls from config if enabled.
    if grpc_tls.enabled {
        let ca_root = match grpc_tls.include_ca_root {
            true => Some((std::path::PathBuf::from(&grpc_tls.ca_root), true)),
            _ => None,
        };

        let (certs, handle_refresh_certs) =
            grpc::tls::certificates(grpc_tls.cert.into(), grpc_tls.key.into(), ca_root).await;

        let (send, recv) = mpsc::unbounded_channel();

        let handle_tls_accept = tokio::spawn(async move {
            let _ = grpc::tls::tcp_streams::<I, IO>(io, certs, send).await;
        });

        let io = tokio_stream::wrappers::UnboundedReceiverStream::new(recv);

        let _ = grpc_on_socket(cx.clone(), io).await;

        handle_refresh_certs.abort();
        handle_tls_accept.abort();
        return;
    }

    let _ = grpc_on_socket(cx.clone(), io).await;
}

async fn grpc_on_socket<I, IO, D: Database>(
    cx: Context<D>,
    mut io: I,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    I: Stream<Item = io::Result<IO>> + Unpin + Send + 'static,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    let mut http =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());

    let grpc_config: Grpc = cx.settings2.snap().subtable("grpc");
    let grpc_jwt: context::Jwt = cx.settings2.snap().subtable("grpcJwt");

    {
        let mut builder = http.http2();
        builder
            .timer(hyper_util::rt::TokioTimer::new())
            .initial_stream_window_size(grpc_config.initial_stream_window_size)
            .initial_connection_window_size(grpc_config.initial_connection_window_size)
            //.concurrency_limit_per_connection(grpc_config.connection_concurrency_limit as usize)
            .max_concurrent_streams(
                (grpc_config.max_concurrent_streams > 0)
                    .then_some(grpc_config.max_concurrent_streams),
            )
            .max_frame_size((grpc_config.max_frame_size != 0).then_some(grpc_config.max_frame_size))
            .keep_alive_interval(grpc_config.http2_keepalive_interval.dur())
            .keep_alive_timeout(
                grpc_config
                    .http2_keepalive_timeout
                    .dur()
                    .expect("http2_keepalive_timeout must be non-negative"),
            );
    }

    let grpc = KeyValueServer::new(grpc::GrpcService::new(cx.clone()))
        .max_decoding_message_size(grpc_config.max_decoding_message_size as usize);

    let verify_jwk = cx.verify_jwk.clone();

    let grpc_telemetry = GrpcSpanTelemetry::default();
    ovdc_grpc::measuring_codec::init_byte_counter({
        let acc = grpc_telemetry.byte_totals.clone();
        Arc::new(move |sent, n| {
            let Some(id) = tracing::Span::current().id() else {
                return;
            };
            if sent {
                acc.add_sent(id, n as u64);
            } else {
                acc.add_rcvd(id, n as u64);
            }
        })
    });

    let stack = tower::ServiceBuilder::new()
        .layer(
            tower_http::trace::TraceLayer::new_for_grpc()
                .make_span_with(|request: &http::Request<tonic::body::Body>| {
                    ovdc_trace::span_extractor::apply_distributed_trace(
                        request.headers(),
                        tracing::info_span!(
                            "req",
                            r = tracing::field::Empty,
                            c = tracing::field::Empty,
                            path = %request.uri().path(),
                        ),
                    )
                })
                .on_request(GrpcTraceOnRequest {
                    telemetry: grpc_telemetry.clone(),
                })
                .on_response(GrpcTraceOnResponse {
                    telemetry: grpc_telemetry.clone(),
                })
                .on_failure(GrpcTraceOnFailure {
                    telemetry: grpc_telemetry.clone(),
                })
                .on_body_chunk(
                    |chunk: &bytes::Bytes, duration: Duration, _span: &tracing::Span| {
                        tracing::trace!(len = { chunk.len() }, ?duration, "response.body_chunk");
                    },
                )
                .on_eos(GrpcTraceOnEos {
                    telemetry: grpc_telemetry,
                }),
        )
        .layer(tonic::service::InterceptorLayer::new(ServiceInterceptor {
            verify_jwk,
            read_tenant_metadata: grpc_config.tenant_from_metadata,
            require_jwt: grpc_jwt.require,
        }));

    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<KeyValueServer<grpc::GrpcService<D>>>()
        .await;
    health_reporter
        .set_service_status(LEGACY_KEY_VALUE_SERVICE, tonic_health::ServingStatus::Serving)
        .await;

    let svc = stack
        .service(
            Routes::default()
                .add_service(health_service)
                .add_service(grpc),
        )
        .map_request(|mut req: http::Request<Incoming>| {
            rewrite_legacy_package(&mut req);
            req.map(tonic::body::Body::new)
        });

    while let Some(io) = io.next().await {
        let io = io?;
        let builder = http.clone();

        let svc = tower::ServiceBuilder::default()
            .layer(tower_http::add_extension::AddExtensionLayer::new(
                ConnectionState::default(),
            ))
            .service(svc.clone());

        tokio::spawn(async move {
            ovdc_trace::drop!(
                builder
                    .serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(svc))
                    .await,
                "connection error"
            );
        });
    }

    Ok(())
}

#[cfg(unix)]
async fn close_on_hangup<D: Database>(context: &Context<D>) {
    let ctrl_c_signal = tokio::signal::ctrl_c();

    let mut sigint =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = ctrl_c_signal => {
            tracing::info!("ctrl-c signal...");
        },
        _ = sigterm.recv() => {
            tracing::info!("terminate signal...");
        }
        _ = sigint.recv() => {
            tracing::info!("interrupt signal...");
        },
    }
    context.close().await;
    tracing::info!("closed");
    std::process::exit(0);
}

#[cfg(not(unix))]
async fn close_on_hangup<D: Database>(_context: &Context<D>) {
    let _: () = std::future::pending().await;
}

const LEGACY_KEY_VALUE_SERVICE: &str = "ddcs.KeyValue";
const LEGACY_KEY_VALUE_PREFIX: &str = "/ddcs.KeyValue/";
const KEY_VALUE_PREFIX: &str = "/ovdc.KeyValue/";

/// Existing clients compiled against a proto with `package ddcs` call
/// `/ddcs.KeyValue/*`. The wire format does not depend on the package name, so
/// rewrite the path to `/ovdc.KeyValue/*` before routing.
fn rewrite_legacy_package<B>(req: &mut http::Request<B>) {
    let uri = req.uri();
    let Some(rest) = uri.path().strip_prefix(LEGACY_KEY_VALUE_PREFIX) else {
        return;
    };
    let mut path_and_query = format!("{KEY_VALUE_PREFIX}{rest}");
    if let Some(query) = uri.query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = path_and_query.parse().ok();
    if let Ok(new_uri) = http::Uri::from_parts(parts) {
        *req.uri_mut() = new_uri;
    }
}

#[derive(Clone)]
struct ServiceInterceptor {
    verify_jwk: Option<Arc<ovdc_jwt::Verify>>,
    read_tenant_metadata: bool,
    require_jwt: bool,
}

impl Interceptor for ServiceInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        // Only read and set tenant from metadata if enabled.
        if self.read_tenant_metadata {
            handle_metadata_tenant(&mut request)?;
        }

        // Only verify JWK and use the tenant from the claim, when enabled.
        // A JWK will superceed any tenant set in the metadata
        if let Some(verify_jwk) = &self.verify_jwk {
            jwt::handle(verify_jwk, self.require_jwt, &mut request)?;
        }

        Ok(request)
    }
}

fn handle_metadata_tenant(request: &mut tonic::Request<()>) -> Result<(), tonic::Status> {
    // Tenant can be provided as a utf-8 string
    if let Some(tenant_raw) = request.metadata().get("tenant") {
        let tenant_str = tenant_raw
            .to_str()
            .map_err(|_| tonic::Status::invalid_argument("tenant: not a utf8 string"))?;

        if tenant_str.len() > keys::TENANT_KEY_SIZE {
            return Err(tonic::Status::invalid_argument(
                "tenant: invalid tenant length",
            ));
        }

        let mut key = [0u8; keys::TENANT_KEY_SIZE];
        key[..tenant_str.len()].copy_from_slice(tenant_str.as_bytes());
        request.extensions_mut().insert(keys::Tenant::new(key));
    }

    Ok(())
}

#[cfg(test)]
mod legacy_package_tests {
    use super::rewrite_legacy_package;

    fn rewritten(uri: &str) -> String {
        let mut req = http::Request::builder().uri(uri).body(()).unwrap();
        rewrite_legacy_package(&mut req);
        req.uri().to_string()
    }

    #[test]
    fn rewrites_ddcs_key_value() {
        assert_eq!(rewritten("/ddcs.KeyValue/Get"), "/ovdc.KeyValue/Get");
        assert_eq!(
            rewritten("http://host:1/ddcs.KeyValue/Put"),
            "http://host:1/ovdc.KeyValue/Put"
        );
    }

    #[test]
    fn leaves_other_paths_alone() {
        assert_eq!(rewritten("/ovdc.KeyValue/Get"), "/ovdc.KeyValue/Get");
        assert_eq!(
            rewritten("/grpc.health.v1.Health/Check"),
            "/grpc.health.v1.Health/Check"
        );
    }
}
