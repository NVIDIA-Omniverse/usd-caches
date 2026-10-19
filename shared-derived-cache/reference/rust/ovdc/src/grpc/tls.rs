// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod certificates;

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures::{Stream, StreamExt};
use parking_lot::RwLock;
use rustls::ServerConfig;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc::UnboundedSender,
    task::JoinHandle,
};

use tokio_rustls::{server::TlsStream, TlsAcceptor};
use tonic::transport::server::Connected;

#[derive(Clone)]
pub struct Certs {
    hash: u64,
    certs: Arc<ServerConfig>,
}

pub type Tls<T> = TlsStream<T>;
pub type CachedCerts = Arc<RwLock<Certs>>;

pub(crate) async fn tcp_streams<I, IO>(
    mut io: I,
    certificates: CachedCerts,
    sender: UnboundedSender<Result<Tls<IO>, std::io::Error>>,
) -> Result<(), std::io::Error>
where
    I: Stream<Item = std::io::Result<IO>> + Unpin + Send + 'static,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    let Certs { mut hash, certs } = certificates.read().clone();
    let mut tls = TlsAcceptor::from(certs);
    loop {
        {
            let guard = certificates.read();
            if hash != guard.hash {
                tls = TlsAcceptor::from(guard.certs.clone());
                hash = guard.hash;
            }
        }

        let io = tokio::select! {
            x = io.next() => {
                match x{ Some(x) => x, None => break  } }
            _ = sender.closed() => {
                break;
            }
        }?;

        let sender = sender.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            let result = match tls.accept(io).await {
                Ok(x) => Ok(x),
                Err(e) => match e.downcast::<rustls::Error>() {
                    Ok(x) => {
                        // The remote failed to establish a TLS connection
                        ovdc_trace::drop!(
                            Err(x) as Result<(), _>,
                            "rustls error: failed to establish tls connection"
                        );
                        return;
                    }
                    Err(_) => {
                        // The remote had some kind of communication failure
                        return;
                    }
                },
            };
            let result = ovdc_trace::err!(result, "failed to establish connection");
            let _ = sender.send(result);
        });
    }

    Ok(())
}

pub async fn certificates(
    cert_path: PathBuf,
    key_path: PathBuf,
    ca_root: Option<(PathBuf, bool)>,
) -> (CachedCerts, JoinHandle<()>) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (config, mut hash) = certificates::certificates(
        cert_path.as_ref(),
        key_path.as_ref(),
        ca_root.as_ref().map(|(x, y)| (x.as_ref(), *y)),
    )
    .await
    .expect("unable to load certificates");

    let cached: CachedCerts = Arc::new(RwLock::new(Certs {
        hash,
        certs: Arc::new(config),
    }));

    let mut _cached = cached.clone();
    let handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;

            let certs = certificates::certificates(
                cert_path.as_ref(),
                key_path.as_ref(),
                ca_root.as_ref().map(|(x, y)| (x.as_ref(), *y)),
            )
            .await;

            let Ok((new_config, new_hash)) = certs else {
                tracing::error!(
                    "invalid TLS certificates; using existing certificates; will retry"
                );
                continue;
            };

            if hash == new_hash {
                continue;
            }
            tracing::info!("new certificate chain loaded");

            hash = new_hash;
            *_cached.write() = Certs {
                hash,
                certs: Arc::new(new_config),
            };
        }
    });

    (cached, handle)
}
