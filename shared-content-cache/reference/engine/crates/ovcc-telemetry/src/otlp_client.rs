// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OTLP/HTTP transport on the hyper-rustls stack the proxy already links, so
//! the binary does not carry reqwest's second client for export alone.
//!
//! The SDK exports from its own batch and periodic-reader threads through
//! `futures_executor::block_on`, and the telemetry guard flushes after the
//! proxy runtime is gone. The client therefore owns a one-worker Tokio runtime
//! and hands each request to it, which is the shape reqwest's blocking client
//! had.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use opentelemetry_http::{HttpClient, HttpError, Request, Response};

/// OTLP's default export timeout when no timeout variable is set.
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Caps a collector response, which OTLP keeps to a small status message, so
/// a misbehaving collector cannot make the proxy buffer an unbounded body.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

type HttpsClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub(crate) struct OtlpHttpClient {
    client: HttpsClient,
    handle: tokio::runtime::Handle,
    // Dropped with `shutdown_background` so a guard released inside another
    // runtime, such as a `#[tokio::test]`, does not panic.
    runtime: Option<tokio::runtime::Runtime>,
    timeout: Duration,
}

impl OtlpHttpClient {
    /// Builds a client that verifies `https://` collectors against the public
    /// web PKI and bounds each export by `timeout`.
    pub(crate) fn new(timeout: Duration) -> std::io::Result<Self> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots, timeout)
    }

    fn with_roots(roots: rustls::RootCertStore, timeout: Duration) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("ovcc-otlp")
            .enable_all()
            .build()?;
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let mut http = HttpConnector::new();
        // The rustls wrapper owns scheme handling, so the TCP connector must
        // accept `https://` URIs as well as plaintext ones.
        http.enforce_http(false);
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Ok(Self {
            client,
            handle: runtime.handle().clone(),
            runtime: Some(runtime),
            timeout,
        })
    }
}

impl std::fmt::Debug for OtlpHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpHttpClient")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl Drop for OtlpHttpClient {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[async_trait::async_trait]
impl HttpClient for OtlpHttpClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let client = self.client.clone();
        let timeout = self.timeout;
        let exchange = async move {
            let response = client.request(request.map(Full::new)).await?;
            let (parts, body) = response.into_parts();
            let body = Limited::new(body, MAX_RESPONSE_BYTES)
                .collect()
                .await?
                .to_bytes();
            Ok::<_, HttpError>(Response::from_parts(parts, body))
        };
        self.handle
            .spawn(async move { tokio::time::timeout(timeout, exchange).await? })
            .await?
    }
}

/// Resolves the per-signal export timeout with OTLP's precedence, where the
/// signal variable beats `OTEL_EXPORTER_OTLP_TIMEOUT`, both in milliseconds.
pub(crate) fn export_timeout(signal_var: &str) -> Duration {
    resolve_timeout(signal_var, |var| std::env::var(var).ok())
}

fn resolve_timeout(signal_var: &str, lookup: impl Fn(&str) -> Option<String>) -> Duration {
    [signal_var, opentelemetry_otlp::OTEL_EXPORTER_OTLP_TIMEOUT]
        .into_iter()
        .find_map(|var| lookup(var)?.parse::<u64>().ok())
        .map_or(DEFAULT_EXPORT_TIMEOUT, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

    fn post(url: String) -> Request<Bytes> {
        Request::post(url)
            .header("content-type", "application/x-protobuf")
            .body(Bytes::from_static(b"payload"))
            .unwrap()
    }

    /// Serves one TLS connection for `localhost` on its own runtime thread and
    /// returns the listening port plus the certificate a client must trust.
    fn spawn_tls_server(response: &'static [u8]) -> (u16, rustls::RootCertStore) {
        let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(issued.cert.der().clone()).unwrap();
        let key =
            rustls::pki_types::PrivateKeyDer::Pkcs8(issued.signing_key.serialize_der().into());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![issued.cert.der().clone()], key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(tcp).await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = tls.read(&mut buf).await.unwrap();
                tls.write_all(response).await.unwrap();
                tls.shutdown().await.unwrap();
            });
        });
        (port, roots)
    }

    #[test]
    fn otlp_client_posts_over_https_from_a_non_tokio_thread() {
        let (port, roots) = spawn_tls_server(OK_RESPONSE);
        let client = OtlpHttpClient::with_roots(roots, Duration::from_secs(10)).unwrap();

        // The SDK drives exports with `futures_executor::block_on` on a plain
        // thread, so the test does the same rather than using a Tokio runtime.
        let response = futures_executor::block_on(
            client.send_bytes(post(format!("https://localhost:{port}/v1/traces"))),
        )
        .expect("HTTPS export");

        assert_eq!(response.status(), 200);
        assert_eq!(response.body().as_ref(), b"ok");
    }

    #[test]
    fn otlp_client_rejects_an_untrusted_collector_certificate() {
        let (port, _roots) = spawn_tls_server(OK_RESPONSE);
        let client =
            OtlpHttpClient::with_roots(rustls::RootCertStore::empty(), Duration::from_secs(10))
                .unwrap();

        let result = futures_executor::block_on(
            client.send_bytes(post(format!("https://localhost:{port}/v1/traces"))),
        );

        assert!(
            result.is_err(),
            "an unverified collector must not receive telemetry"
        );
    }

    #[test]
    fn otlp_client_times_out_a_silent_collector() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold the connection without ever responding.
        std::thread::spawn(move || {
            let held = listener.accept();
            std::thread::sleep(Duration::from_secs(30));
            drop(held);
        });
        let client = OtlpHttpClient::new(Duration::from_millis(200)).unwrap();

        let started = Instant::now();
        let result =
            futures_executor::block_on(client.send_bytes(post(format!("http://{addr}/v1/traces"))));

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn otlp_client_rejects_an_oversized_collector_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let (mut sock, _) = listener.accept().unwrap();
            // Drain the whole request so closing the socket cannot reset the
            // connection and fail the export for an unrelated reason.
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.ends_with(b"payload") {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "client closed before sending its body");
                request.extend_from_slice(&buf[..n]);
            }
            let len = MAX_RESPONSE_BYTES + 1;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n");
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&vec![b'x'; len]);
        });
        let client = OtlpHttpClient::new(Duration::from_secs(10)).unwrap();

        let result =
            futures_executor::block_on(client.send_bytes(post(format!("http://{addr}/v1/traces"))));

        assert!(
            result.is_err(),
            "a response over the cap must not be buffered"
        );
    }

    #[test]
    fn otlp_timeout_prefers_signal_variable_then_generic_then_default() {
        let signal = "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT";
        let both = |var: &str| match var {
            "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT" => Some("250".to_owned()),
            "OTEL_EXPORTER_OTLP_TIMEOUT" => Some("4000".to_owned()),
            _ => None,
        };
        let generic_only =
            |var: &str| (var == "OTEL_EXPORTER_OTLP_TIMEOUT").then(|| "4000".to_owned());
        let malformed_signal = |var: &str| match var {
            "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT" => Some("soon".to_owned()),
            "OTEL_EXPORTER_OTLP_TIMEOUT" => Some("4000".to_owned()),
            _ => None,
        };

        assert_eq!(resolve_timeout(signal, both), Duration::from_millis(250));
        assert_eq!(
            resolve_timeout(signal, generic_only),
            Duration::from_millis(4000)
        );
        assert_eq!(
            resolve_timeout(signal, malformed_signal),
            Duration::from_millis(4000)
        );
        assert_eq!(resolve_timeout(signal, |_| None), DEFAULT_EXPORT_TIMEOUT);
    }

    #[test]
    fn otlp_client_drops_inside_another_runtime_without_panicking() {
        let client = OtlpHttpClient::new(Duration::from_secs(1)).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async move { drop(client) });
    }
}
