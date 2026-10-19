// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    io,
    pin::Pin,
    sync::{atomic::AtomicU32, Arc},
};

use futures::Stream;
use ovdc_bytes::ident::ConnectionId;
use ovdc_jwt::VerifiedToken;
use ovdc_trace::metrics::Measure;
use socket2::TcpKeepalive;
use std::task::{Context, Poll};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};
use tonic::transport::server::Connected;

use crate::grpc::metrics;

pub struct Wrapper<IO> {
    stream: IO,
    state: ConnectionState,
}

impl<IO> Drop for Wrapper<IO> {
    fn drop(&mut self) {
        metrics::API_CONNECTIONS.measure(-1, &[]);
        tracing::info!(c = ?{ self.state.c_id() }, "connection drop");
    }
}

pub struct Counted<I> {
    inner: I,
    state: ConnectionState,
}

impl<I> From<I> for Counted<I> {
    fn from(value: I) -> Self {
        Self {
            inner: value,
            state: Default::default(),
        }
    }
}

#[derive(Default, Clone)]
pub struct ConnectionState {
    c: ConnectionId,
    request_count: Arc<AtomicU32>,

    last_verified_token: Arc<parking_lot::RwLock<Option<VerifiedToken>>>,
}

impl ConnectionState {
    pub fn c_id(&self) -> ConnectionId {
        self.c
    }

    pub fn r_id(&self) -> u32 {
        self.request_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn try_set_last_verified_token(&self, token: impl FnOnce() -> VerifiedToken) {
        if let Some(mut guard) = self.last_verified_token.try_write() {
            *guard = Some(token());
        }
    }

    pub fn with_verififed_token<F, O>(&self, with_fn: F) -> Option<O>
    where
        F: FnOnce(&VerifiedToken) -> O,
    {
        let token = self.last_verified_token.read();
        if let Some(token) = &*token {
            return Some(with_fn(token));
        }

        None
    }
}

impl<I, IO> Stream for Counted<I>
where
    I: Stream<Item = io::Result<IO>> + Unpin + Send + 'static,
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    IO::ConnectInfo: Clone + Send + Sync + 'static,
{
    type Item = io::Result<Wrapper<IO>>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(io))) => {
                metrics::API_CONNECTIONS.measure(1, &[]);
                tracing::trace!(c = ?{ self.state.c_id() }, "connection");
                std::task::Poll::Ready(Some(Ok(Wrapper {
                    stream: io,
                    state: self.state.clone(),
                })))
            }
            std::task::Poll::Ready(Some(Err(e))) => std::task::Poll::Ready(Some(Err(e))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<IO> Connected for Wrapper<IO>
where
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
{
    type ConnectInfo = ConnectionState;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.state.clone()
    }
}

impl<IO> AsyncRead for Wrapper<IO>
where
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let len = buf.filled().len();
        let poll_result = Pin::new(&mut self.stream).poll_read(cx, buf);

        if let std::task::Poll::Ready(Ok(..)) = &poll_result {
            let post_len = buf.filled().len();
            metrics::API_TCP_RX_TOTAL.measure((post_len - len) as u64, &[]);
        }

        poll_result
    }
}

impl<IO> AsyncWrite for Wrapper<IO>
where
    IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        let poll_result = Pin::new(&mut self.stream).poll_write(cx, buf);

        if let std::task::Poll::Ready(Ok(x)) = &poll_result {
            metrics::API_TCP_TX_TOTAL.measure(*x as u64, &[]);
        }

        poll_result
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub struct TcpListenerStream {
    inner: TcpListener,
    keepalive: socket2::TcpKeepalive,
}

impl TcpListenerStream {
    pub fn new(listener: TcpListener, keepalive: TcpKeepalive) -> Self {
        Self {
            inner: listener,
            keepalive,
        }
    }
}

impl Stream for TcpListenerStream {
    type Item = io::Result<TcpStream>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<TcpStream>>> {
        match self.inner.poll_accept(cx) {
            Poll::Ready(Ok((stream, _))) => {
                stream.set_nodelay(true).expect("could not set SO_NODELAY");
                let sock_ref = socket2::SockRef::from(&stream);
                sock_ref
                    .set_tcp_keepalive(&self.keepalive)
                    .expect("can not set socket timeout on this platform");
                sock_ref.set_linger(None).expect("unable to set no linger");

                Poll::Ready(Some(Ok(stream)))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Some(Err(err))),
            Poll::Pending => Poll::Pending,
        }
    }
}
