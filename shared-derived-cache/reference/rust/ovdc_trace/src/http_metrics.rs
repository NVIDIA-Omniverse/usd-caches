// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{convert::Infallible, net::SocketAddr};

use bytes::Bytes;
use http::{Request, Response};
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::metrics::MeterReader;

async fn req_handle(
    _: Request<impl hyper::body::Body>,
    reader: MeterReader,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let value = crate::metrics::collect_openmetrics(&reader);
    Ok(Response::builder()
        .header(
            http::header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )
        .body(Full::new(Bytes::from(value)))
        .expect("static header value is always a valid response"))
}

pub(crate) fn serve(addr: SocketAddr, reader: MeterReader) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async move {
            let listener = TcpListener::bind(addr)
                .await
                .expect("failed to bind tcp address for metrics");
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    continue;
                };
                let io = TokioIo::new(tcp);

                let reader = reader.clone();
                tokio::task::spawn(async move {
                    crate::drop!(
                        hyper::server::conn::http1::Builder::new()
                            .timer(hyper_util::rt::TokioTimer::new())
                            .serve_connection(
                                io,
                                hyper::service::service_fn(move |req| {
                                    let reader = reader.clone();
                                    req_handle(req, reader)
                                }),
                            )
                            .await,
                        "Error serving connection"
                    );
                });
            }
        });
    });
}
