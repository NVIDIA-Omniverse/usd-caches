// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Provides a simple interface for embedding a KV store.

use std::{io, path::PathBuf};

use hyper_util::rt::TokioIo;
use ovdc_map::{environment::Rocks, Database};
use tokio::{io::DuplexStream, sync::mpsc::Sender, task::JoinHandle};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Uri};

use crate::context::Context;

/// An embedded version of [Context].
pub struct Embedded {
    /// The inner context.
    pub context: Context<Rocks>,

    sender: Sender<io::Result<DuplexStream>>,

    task: JoinHandle<()>,
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Embedded {
    /// Opens a database in process.
    pub async fn new(path: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = PathBuf::from(path);
        let settings_path = path.join("config.json");

        let settings = ovdc_settings::Settings::new(settings_path).await;
        let context = crate::context::open(path.clone(), settings).await?;

        tracing::info!("opened an embedded DB at {path:?}");

        let (sender, task) = embedded_grpc(context.clone());

        Ok(Self {
            sender,
            context,
            task,
        })
    }

    /// Creates a client for the embedded database.
    pub async fn client2(&self) -> ovdc_grpc::proto::key_value_client::KeyValueClient<Channel> {
        let sender = self.sender.clone();

        let channel = tonic::transport::Endpoint::from_static("http://x.url")
            .initial_stream_window_size(Some(512_000))
            .initial_connection_window_size(Some(512_000))
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let sender = sender.clone();
                let (left, right) = tokio::io::duplex(1_000_000);
                async move {
                    sender.send(Ok(left)).await.unwrap();
                    Ok(TokioIo::new(right)) as io::Result<TokioIo<DuplexStream>>
                }
            }))
            .await
            .unwrap();

        ovdc_grpc::proto::key_value_client::KeyValueClient::new(channel)
    }

    /// Creates a client for the embedded database.
    pub async fn client_tls(&self) -> ovdc_grpc::proto::key_value_client::KeyValueClient<Channel> {
        let sender = self.sender.clone();

        let channel = tonic::transport::Endpoint::from_static("https://localhost")
            .initial_stream_window_size(Some(512_000))
            .initial_connection_window_size(Some(512_000))
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(include_bytes!(
                        "../../tests/test_certs/rootCA.crt"
                    )))
                    .domain_name("localhost"),
            )
            .unwrap()
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let sender = sender.clone();
                let (left, right) = tokio::io::duplex(1_000_000);
                async move {
                    sender.send(Ok(left)).await.unwrap();
                    Ok(TokioIo::new(right)) as io::Result<TokioIo<DuplexStream>>
                }
            }))
            .await
            .unwrap();

        ovdc_grpc::proto::key_value_client::KeyValueClient::new(channel)
    }

    /// Creates a raw stream channel
    pub async fn raw_channel(&self) -> DuplexStream {
        let (left, right) = tokio::io::duplex(1_000_000);
        self.sender.send(Ok(left)).await.unwrap();
        right
    }
}

#[allow(unused)]
pub(crate) fn enable_test_certs(cfg: &mut toml_edit::DocumentMut) {
    let mut tls_cfg = &mut cfg["grpcTls"];
    tls_cfg["cert"] = "../tests/test_certs/localhost.crt".into();
    tls_cfg["key"] = "../tests/test_certs/localhost.key".into();
    tls_cfg["enabled"] = true.into();
}

fn embedded_grpc<D: Database>(
    cx: Context<D>,
) -> (Sender<io::Result<DuplexStream>>, JoinHandle<()>) {
    let (send, recv) = tokio::sync::mpsc::channel::<io::Result<DuplexStream>>(1);

    let task = tokio::spawn(async move {
        crate::serve_grpc(&cx, tokio_stream::wrappers::ReceiverStream::new(recv)).await;
    });

    (send, task)
}
