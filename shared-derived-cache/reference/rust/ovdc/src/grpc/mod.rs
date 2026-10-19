// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Methods for handling GRPC operations on the server.

mod get;
pub(crate) mod metrics;
mod put;
pub(crate) mod tls;

use std::time::Duration;

use std::sync::OnceLock;

use ovdc_bytes::keys;
use ovdc_grpc::proto::{
    GetRequest, GetUnorderedResponse, PutRequest, PutResponse, TouchRequest, TouchResponse,
};
use ovdc_lru::LogUsedKeys;
use ovdc_map::Database;
use tokio::{sync::mpsc::error::SendTimeoutError, task::JoinHandle};
use tonic::{Code, Response, Status};
use tracing::Instrument;

use ovdc_trace::metrics::{timer, Measure};

use crate::context::Context;

use self::put::StreamAutoDropFut;

type PutStream = StreamAutoDropFut<Result<PutResponse, Status>>;

/// Set write timeout for replying to queue
pub static WRITE_TIMEOUT: OnceLock<Duration> = OnceLock::new();

/// Serves a GRPC protocol version of the application.
#[derive(Clone)]
pub struct GrpcService<D: Database> {
    #[allow(unused)]
    cx: Context<D>,
}

impl<D: Database> GrpcService<D> {
    /// Create a new GRPC service around a [Context]
    pub fn new(cx: Context<D>) -> Self {
        Self { cx }
    }
}

#[tonic::async_trait]
impl<D: Database> ovdc_grpc::proto::key_value_server::KeyValue for GrpcService<D> {
    type GetStream =
        tokio_stream::wrappers::ReceiverStream<Result<GetUnorderedResponse, tonic::Status>>;
    type PutStream = PutStream;

    #[tracing::instrument(skip_all, keys = tracing::field::Empty)]
    async fn get(
        &self,
        request: tonic::Request<GetRequest>,
    ) -> std::result::Result<tonic::Response<Self::GetStream>, tonic::Status> {
        let current_span = tracing::span::Span::current();

        let tenant = request.extensions().get::<keys::Tenant>().copied();

        let timer = timer(
            &metrics::API_REQUEST_TIME,
            ovdc_trace::label!("operation": "grpc_get"),
        );
        metrics::API_REQUEST_TOTAL.measure(1, &ovdc_trace::label!("operation": "grpc_get"));

        let get_op = request.into_inner();

        let keys: Vec<_> =
            convert_proto_keys(tenant.unwrap_or_default(), get_op.keys.iter()).collect();

        metrics::API_KEYS_TOTAL.measure(
            keys.len() as u64,
            &ovdc_trace::label!("operation": "grpc_get"),
        );
        current_span.record("keys", keys.len());
        tracing::trace!(len = keys.len(), "begin get request");

        let cx = self.cx.clone();

        let (send, recv) =
            tokio::sync::mpsc::channel::<Result<GetUnorderedResponse, tonic::Status>>(2);
        tokio::spawn(
            async move {
                let result = get::handle_get(cx, keys, &send).await;
                if let Err(err) = result {
                    tracing::trace!("error: {err:?}");
                    _reply_err(&send, err, "grpc_get").await;
                }

                drop(timer);
            }
            .in_current_span(),
        );

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            recv,
        )))
    }

    #[tracing::instrument(skip_all)]
    async fn put(
        &self,
        request: tonic::Request<tonic::Streaming<PutRequest>>,
    ) -> std::result::Result<tonic::Response<Self::PutStream>, tonic::Status> {
        let tenant = request.extensions().get::<keys::Tenant>().copied();

        let timer = timer(
            &metrics::API_REQUEST_TIME,
            ovdc_trace::label!("operation": "grpc_put"),
        );
        metrics::API_REQUEST_TOTAL.measure(1, &ovdc_trace::label!("operation": "grpc_put"));

        let (mut sender, recv) = tokio::sync::mpsc::channel::<Result<PutResponse, Status>>(2);

        let cx = self.cx.clone();

        let task = tokio::spawn(
            async move {
                let mut stream = request.into_inner();
                let result =
                    put::streaming(tenant.unwrap_or_default(), &mut stream, cx, &mut sender).await;
                tracing::trace!("streaming put ended");

                if let Err(err) = result {
                    tracing::trace!("error: {err:?}");
                    _reply_err(&sender, err, "grpc_put").await;
                }

                drop(timer);
            }
            .in_current_span(),
        );

        Ok(Response::new(StreamAutoDropFut::new(recv, task)))
    }

    #[tracing::instrument(skip_all)]
    async fn touch(
        &self,
        request: tonic::Request<TouchRequest>,
    ) -> std::result::Result<tonic::Response<TouchResponse>, tonic::Status> {
        let tenant = request.extensions().get::<keys::Tenant>().copied();

        metrics::API_REQUEST_TOTAL.measure(1, &ovdc_trace::label!("operation": "grpc_touch"));
        let _timer = timer(
            &metrics::API_REQUEST_TIME,
            ovdc_trace::label!("operation": "grpc_touch"),
        );

        let request = request.into_inner();

        let keys = request.keys;
        let keys: Vec<_> = convert_proto_keys(tenant.unwrap_or_default(), keys.iter()).collect();

        metrics::API_KEYS_TOTAL.measure(
            keys.len() as u64,
            &ovdc_trace::label!("operation": "grpc_touch"),
        );

        let mut background: OnceLock<(
            tokio::sync::mpsc::UnboundedSender<(u32, keys::Compound)>,
            JoinHandle<Vec<u32>>,
        )> = OnceLock::new();

        let mut exists = vec![];
        for (i, key) in keys.into_iter().enumerate() {
            let cursor = self.cx.env.open();

            if cursor.exists_non_blocking(key).is_ok() {
                exists.push(i as u32);
                self.cx.key_log.log(&[key]);
                continue;
            }

            let bg_task = background.get_or_init(|| {
                let db = self.cx.env.clone();
                let (send, mut recv) = tokio::sync::mpsc::unbounded_channel();

                let key_log = self.cx.key_log.clone();
                let span = tracing::info_span!("touch_background");

                let mut exists = vec![];

                let task = tokio::task::spawn_blocking(move || {
                    let _g = span.entered();

                    let cursor = db.open();
                    while let Some((i, next)) = recv.blocking_recv() {
                        if cursor.exists(next).is_ok() {
                            key_log.log(&[next]);
                            exists.push(i);
                        }
                    }

                    exists
                });

                (send, task)
            });

            bg_task.0.send((i as u32, key)).unwrap();
        }

        let touched = match background.take() {
            Some((sender, task)) => {
                drop(sender);
                let other_touched = task
                    .instrument(tracing::info_span!("wait_for_background"))
                    .await
                    .unwrap_or_default();

                exists.extend_from_slice(&other_touched);
                exists
            }
            _ => exists,
        };

        Ok(Response::new(TouchResponse {
            touched: touched.len() as u32,
            matched: touched,
        }))
    }
}

fn proto_key_to_key(tenant: keys::Tenant, key: ovdc_grpc::proto::Key) -> Option<keys::Compound> {
    match key.inner.as_ref()? {
        ovdc_grpc::proto::key::Inner::FixedKey(sized_32_key) => {
            let buf: [u8; 32] = sized_32_key.into();
            Some(keys::Compound::new(tenant, keys::Key::from(buf)))
        }
        ovdc_grpc::proto::key::Inner::Key(var_key) => {
            Some(keys::Compound::new(tenant, keys::Key::new(&var_key[..])?))
        }
    }
}

fn convert_proto_keys<'a>(
    tenant: keys::Tenant,
    keys: impl Iterator<Item = &'a ovdc_grpc::proto::Key> + 'a,
) -> impl Iterator<Item = keys::Compound> + 'a {
    keys.flat_map(|y| y.inner.as_ref().into_iter())
        .filter_map(|raw_key| match raw_key {
            ovdc_grpc::proto::key::Inner::FixedKey(sized_32_key) => {
                let buf: [u8; 32] = sized_32_key.into();
                Some(keys::Key::from(buf))
            }
            ovdc_grpc::proto::key::Inner::Key(var_key) => match keys::Key::new(&var_key[..]) {
                Some(x) => Some(x),
                _ => {
                    tracing::trace!("could not use input key");
                    None
                }
            },
        })
        .map(move |key| keys::Compound::new(tenant, key))
}

#[inline]
fn require<T>(value: Option<T>, err_reason: &'static str) -> Result<T, tonic::Status> {
    value.ok_or_else(|| tonic::Status::new(Code::FailedPrecondition, err_reason))
}

async fn reply<T>(
    send: &tokio::sync::mpsc::Sender<Result<T, tonic::Status>>,
    value: T,
) -> Result<(), tonic::Status> {
    let duration = WRITE_TIMEOUT
        .get()
        .cloned()
        .unwrap_or(Duration::from_secs(60));

    send.send_timeout(Ok(value), duration)
        .await
        .map_err(|e| match e {
            SendTimeoutError::Closed(_) => tonic::Status::aborted("channel closed"),
            SendTimeoutError::Timeout(_) => tonic::Status::aborted("write timeout"),
        })
}

async fn _reply_err<T>(
    send: &tokio::sync::mpsc::Sender<Result<T, tonic::Status>>,
    status: tonic::Status,
    operation: &'static str,
) {
    tracing::trace!(?status, "request.stream.error");
    metrics::API_ERRORS_TOTAL.measure(1, &ovdc_trace::label!("operation": operation));

    let duration = WRITE_TIMEOUT
        .get()
        .cloned()
        .unwrap_or(Duration::from_secs(60));
    let _ = send.send_timeout(Err(status), duration).await;
}
