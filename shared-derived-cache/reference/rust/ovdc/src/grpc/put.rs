// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GRPC Put methods

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::Stream;
use ovdc_bytes::keys;
use ovdc_grpc::proto::{put_request, put_response::PutStatus, PutOpen, PutRequest, PutResponse};
use ovdc_map::{metadata::Metadata, Database};
use ovdc_settings::Snap;
use ovdc_trace::metrics::Measure;
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};
use tonic::{Code, Status, Streaming};
use tracing::Instrument;

use crate::{
    context,
    grpc::{metrics, proto_key_to_key, reply},
};

use super::require;

/// Runs one PUT stream to completion, bounded by `grpc.putTimeout`.
///
/// The bound covers the whole stream rather than any single message, because
/// what has to be bounded is how long a client can hold a write permit and a
/// half-written blob's parts -- a client that sends one byte per minute is
/// indistinguishable from a stalled one at any smaller granularity.
///
/// Timing out returns `DeadlineExceeded` and drops the stream; the parts
/// already written are left for GC to reclaim, the same as any abandoned
/// upload.
#[tracing::instrument(skip_all)]
pub async fn streaming<D>(
    tenant: keys::Tenant,
    inner: &mut Streaming<PutRequest>,
    cx: context::Context<D>,
    sender: &mut Sender<Result<PutResponse, Status>>,
) -> Result<(), Status>
where
    D: Database,
{
    let limit = cx
        .settings2
        .snap()
        .subtable::<context::Grpc>("grpc")
        .put_timeout();

    match tokio::time::timeout(limit, streaming_inner(tenant, inner, cx, sender)).await {
        Ok(result) => result,
        Err(_) => {
            metrics::API_ERRORS_TOTAL.measure(
                1,
                &ovdc_trace::label!("operation": "grpc_put", "kind": "put_timeout"),
            );
            tracing::error!(
                limit_secs = limit.as_secs(),
                "put exceeded the configured time limit; dropping the stream"
            );
            Err(Status::deadline_exceeded(format!(
                "put exceeded the configured limit of {}s",
                limit.as_secs()
            )))
        }
    }
}

#[tracing::instrument(skip_all)]
async fn streaming_inner<D>(
    tenant: keys::Tenant,
    inner: &mut Streaming<PutRequest>,
    cx: context::Context<D>,
    sender: &mut Sender<Result<PutResponse, Status>>,
) -> Result<(), Status>
where
    D: Database,
{
    tracing::trace!("waiting for put beginning message");

    let open = wait_for_open(inner).await?;

    let store_cfg = cx.settings2.snap().subtable::<context::Store>("store");
    let mut count = 0;

    while let Some(declared) = wait_for_declare(tenant, inner).await? {
        if declared.size > *store_cfg.blob_cutoff {
            write_declared_entry(open, declared, inner, cx.clone(), sender, count).await?;
        } else {
            write_declared_entry_single(open, declared, inner, cx.clone(), sender, count).await?;
        }
        count += 1;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn wait_for_open(streaming: &mut Streaming<PutRequest>) -> Result<PutOpen, tonic::Status> {
    let next = streaming
        .message()
        .instrument(tracing::info_span!("recv_from_stream"))
        .await?;

    let kind = require(
        require(next, "invalid: no message")?.kind,
        "invalid: missing kind",
    )?;
    let put_request::Kind::Begin(open) = kind else {
        return Err(tonic::Status::failed_precondition(
            "stream must begin with valid `Begin` enum kind",
        ));
    };
    Ok(open)
}

struct Declared {
    size: u64,
    value: Vec<u8>,
    key: keys::Compound,
}

#[tracing::instrument(skip_all)]
async fn wait_for_declare(
    tenant: keys::Tenant,
    streaming: &mut Streaming<PutRequest>,
) -> Result<Option<Declared>, tonic::Status> {
    let Some(next) = streaming
        .message()
        .instrument(tracing::info_span!("recv_from_stream"))
        .await?
    else {
        return Ok(None);
    };

    let kind = require(next.kind, "invalid: declare missing kind")?;
    let put_request::Kind::Declare(declare) = kind else {
        return Err(tonic::Status::failed_precondition(
            "stream must begin with `Begin` enum kind",
        ));
    };

    let key = require(
        proto_key_to_key(tenant, require(declare.key, "key not found")?),
        "invalid key",
    )?;

    metrics::API_KEYS_TOTAL.measure(1, &ovdc_trace::label!("operation": "grpc_put"));

    let size = declare.size;

    tracing::trace!(?key, size, "pair declared");

    Ok(Some(Declared {
        key,
        size,
        value: declare.value,
    }))
}

#[tracing::instrument(skip_all)]
async fn wait_for_part(
    streaming: &mut Streaming<PutRequest>,
) -> Result<Option<Vec<u8>>, tonic::Status> {
    tracing::trace!("wait for part");
    let next = streaming
        .message()
        .instrument(tracing::info_span!("recv_from_stream"))
        .await?;

    // Look for the next part, if there is any other type or if the stream ended, then
    // break the current loop.
    let Some(PutRequest {
        kind: Some(put_request::Kind::Value(next_value)),
    }) = next
    else {
        return Ok(None);
    };

    Ok(Some(next_value))
}

#[tracing::instrument(skip_all, fields(count = count, size, key))]
async fn write_declared_entry_single<D>(
    PutOpen { ttl, durability }: PutOpen,
    Declared {
        size,
        mut value,
        key,
    }: Declared,
    inner: &mut Streaming<PutRequest>,
    cx: context::Context<D>,
    sender: &mut Sender<Result<PutResponse, Status>>,
    count: u32,
) -> Result<(), Status>
where
    D: Database,
{
    while value.len() < size as usize {
        if let Some(x) = wait_for_part(inner).await? {
            value.extend_from_slice(&x);
        }
    }

    if value.len() as u64 != size {
        tracing::warn!(
            "client did not send correct number of bytes {}/{size}",
            value.len()
        );
        return Err(tonic::Status::new(
            Code::Aborted,
            "incorrect number of bytes received",
        ));
    }

    let cursor = cx.env.open();

    let mut flags = 0;
    if durability.unwrap_or(0) == 1 {
        flags = ovdc_map::FLAGS_HIGHER_DURABILITY;
    }

    let metadata = Metadata {
        ttl_ms: ttl,
        ..Default::default()
    };

    ovdc_trace::err!(
        db_write(move || {
            let _guard = ovdc_map::thread_local_flags(flags);
            cursor.put(key, metadata, &value)
        })
        .await,
        "failed to put"
    )
    .map_err(|_| tonic::Status::new(Code::Aborted, "put failed"))?;

    tracing::trace!("pair put fnished");

    ovdc_trace::err!(
        reply(
            sender,
            PutResponse {
                status: PutStatus::PutOk.into(),
                index: Some(count),
            },
        )
        .instrument(tracing::info_span!("buffer_response"))
        .await,
        "failed to write"
    )?;

    Ok(())
}

/// Reads and discards the rest of the value, without answering the client.
///
/// The stream carries every entry in the request, so a body left unread is not
/// skipped -- it is read as the next entry's frames. Draining is what keeps the
/// entries after a refused one aligned.
///
/// Separate from [`sink_value`] because draining and accepting are different
/// decisions: a caller that is about to fail the entry has to do the first
/// without the second.
async fn drain_value(
    inner: &mut Streaming<PutRequest>,
    mut remainder: usize,
) -> Result<(), Status> {
    while remainder > 0 {
        let Some(value) = wait_for_part(inner).await? else {
            return Err(tonic::Status::aborted("not enough bytes received"));
        };

        remainder -= value.len();
    }

    tracing::trace!("finished sinking remainder of bytes");
    Ok(())
}

/// Throws away the remaining bytes and tells the client the entry was stored.
///
/// For an entry that is deliberately null-routed: the client asked to write a
/// key another request is already writing, and the agreed behaviour is that the
/// first wins and the rest report success.
async fn sink_value(
    inner: &mut Streaming<PutRequest>,
    sender: &mut Sender<Result<PutResponse, Status>>,
    remainder: usize,
    count: u32,
) -> Result<(), Status> {
    drain_value(inner, remainder).await?;

    ovdc_trace::err!(
        reply(
            sender,
            PutResponse {
                status: PutStatus::PutOk.into(),
                index: Some(count),
            },
        )
        .instrument(tracing::info_span!("buffer_response"))
        .await,
        "failed to write"
    )?;
    Ok(())
}

#[tracing::instrument(skip_all, fields(count = count, size, key))]
async fn write_declared_entry<D>(
    PutOpen { ttl, .. }: PutOpen,
    Declared { size, value, key }: Declared,
    inner: &mut Streaming<PutRequest>,
    cx: context::Context<D>,
    sender: &mut Sender<Result<PutResponse, Status>>,
    count: u32,
) -> Result<(), Status>
where
    D: Database,
{
    let metadata = Metadata {
        ttl_ms: ttl,
        ..Default::default()
    };

    let chunk_size = {
        let store_cfg = cx.settings2.snap().subtable::<context::Store>("store");
        *store_cfg.blob_chunk_size
    };

    let mut parts = (size / chunk_size) + (size % chunk_size != 0) as u64;

    // Refuse an oversized value before reading any of it. The engine would
    // otherwise fail on the part that crosses its limit, after the client had
    // streamed most of the body, and report it as a write failure rather than
    // as the client asking for something the backend cannot store.
    let max_parts = cx.env.blobs().max_parts() as u64;
    if parts > max_parts {
        tracing::warn!(
            ?key,
            size,
            parts,
            max_parts,
            "value needs more parts than the storage backend supports"
        );
        // Refused outright, with nothing drained first. The body was read here
        // once so "the entries after it still line up" -- but the `Err` below is
        // propagated by `streaming` with `?`, which ends the whole put stream,
        // so there are no entries after it to line up. Draining only made the
        // client upload a value the server had already decided to reject before
        // telling it so, which is exactly what the comment above promises not to
        // do. Answering now stops the transfer at the point the decision is made.
        return Err(tonic::Status::new(
            Code::InvalidArgument,
            format!(
                "value of {size} bytes needs {parts} parts of {chunk_size} bytes, \
                 but this storage mode supports at most {max_parts}"
            ),
        ));
    }

    let Some((mut w_cx, _tag)) = cx.env.blobs().begin_write(key, metadata, size) else {
        tracing::trace!("value is ignored, the key is being written");

        // The value is already being written and therefore cannot be written by this request. The
        // current behavior expected by the client is that the FIRST request is accepted and latter
        // requests are null routed.
        sink_value(
            inner,
            sender,
            (size as usize).saturating_sub(value.len()),
            count,
        )
        .await?;

        return Ok(());
    };

    let mut written = 0u64;
    let mut buffer = bytes::BytesMut::from(bytes::Bytes::from(value));
    let chunk_size = chunk_size as usize;

    loop {
        // Write the part if there is enough data buffered.
        tracing::trace!(?key, size, written, "write value");

        let mut remainder = (size - written) as usize;

        while buffer.len() >= chunk_size || (parts == 1 && buffer.len() == remainder) {
            let split = buffer.split_to(remainder.min(chunk_size));
            let len = split.len();

            if !db_write_blob(|| cx.env.blobs().schedule(&mut w_cx, split.as_ref())).await {
                tracing::error!("timeout while trying to write to DB");
                return Err(tonic::Status::internal("write failure"));
            }

            written += len as u64;
            remainder -= len;
            tracing::trace!(len, written, remainder, parts, "write part");
            parts -= 1;
        }

        // Enough data was written, break out.
        if remainder == 0 {
            break;
        }

        // Not enough data is buffered, wait for more data.
        let Some(value) = wait_for_part(inner).await? else {
            return Err(tonic::Status::aborted("not enough bytes received"));
        };
        buffer.extend_from_slice(&value);
    }

    if written != size {
        tracing::warn!("client did not send correct number of bytes {written}/{size}");
        return Err(tonic::Status::new(
            Code::Aborted,
            "incorrect number of bytes received",
        ));
    }

    // The body is already on disk at this point; this write is what makes it
    // readable. Retry on back-pressure rather than failing at the last step,
    // and if it will not land, report the failure -- until this succeeds the
    // parts exist with nothing pointing at them, so answering PutOk would be a
    // silent loss. Dropping `w_cx` unsatisfied lets the guard reclaim them.
    ovdc_trace::err!(
        db_write(|| cx.env.blobs().done(&mut w_cx)).await,
        "failed to complete blob"
    )
    .map_err(|_| tonic::Status::new(Code::Aborted, "put failed"))?;

    tracing::trace!("pair put fnished");

    ovdc_trace::err!(
        reply(
            sender,
            PutResponse {
                status: PutStatus::PutOk.into(),
                index: Some(count),
            },
        )
        .instrument(tracing::info_span!("buffer_response"))
        .await,
        "failed to write"
    )?;

    // Release the key. The guard is satisfied now, so this only removes the
    // in-progress entry -- but holding it to the end of the request would keep
    // refusing other writers for no reason.
    drop(w_cx);

    Ok(())
}

/// A wrapper around [`tokio::sync::mpsc::Receiver`] that implements [`Stream`].
#[derive(Debug)]
pub struct StreamAutoDropFut<T> {
    inner: Receiver<T>,
    fut: JoinHandle<()>,
}

impl<T> StreamAutoDropFut<T> {
    /// Create a new `ReceiverStream`.
    pub fn new(recv: Receiver<T>, fut: JoinHandle<()>) -> Self {
        Self { inner: recv, fut }
    }
}

impl<T> Stream for StreamAutoDropFut<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_recv(cx)
    }
}

impl<T> AsRef<Receiver<T>> for StreamAutoDropFut<T> {
    fn as_ref(&self) -> &Receiver<T> {
        &self.inner
    }
}

impl<T> AsMut<Receiver<T>> for StreamAutoDropFut<T> {
    fn as_mut(&mut self) -> &mut Receiver<T> {
        &mut self.inner
    }
}

impl<T> Drop for StreamAutoDropFut<T> {
    fn drop(&mut self) {
        tracing::trace!("drop called on put response stream");
        self.fut.abort();
    }
}

#[tracing::instrument(skip_all, fields(busy = tracing::field::Empty))]
async fn db_write_blob(mut f: impl FnMut() -> bool) -> bool {
    let _w_guard = metrics::writes_waiting_guard();
    let _t_guard = ovdc_trace::metrics::timer(&metrics::API_WRITE_QUEUE_TIME, []);

    let mut busy_count = 0;
    loop {
        let put_result = f();

        match put_result {
            true => return true,
            false if busy_count == 60 => {
                tracing::error!("write failed because the database is busy");
                return false;
            }
            // Rocks db cannot currently accept the data.
            false => {
                busy_count += 1;
                if busy_count == 0 {
                    let span = tracing::Span::current();
                    span.record("busy", true);
                }
                tracing::trace!("waiting for database to be ready");
                tokio::time::sleep(std::time::Duration::from_millis(2u64.pow(busy_count % 12)))
                    .await;
                continue;
            }
        }
    }
}

#[tracing::instrument(skip_all, err, fields(busy = tracing::field::Empty))]
async fn db_write<T>(
    mut f: impl FnMut() -> Result<T, ovdc_map::Error>,
) -> Result<T, ovdc_map::Error> {
    let _w_guard = metrics::writes_waiting_guard();
    let _t_guard = ovdc_trace::metrics::timer(&metrics::API_WRITE_QUEUE_TIME, []);

    let mut busy_count = 0;
    loop {
        let put_result = f();

        match put_result {
            Ok(t) => return Ok(t),
            e @ Err(ovdc_map::Error::Exists) | e @ Err(ovdc_map::Error::WriteInProgress) => {
                return e;
            }
            Err(ovdc_map::Error::Busy) if busy_count == 60 => {
                tracing::error!("write failed because the database is busy");
                return Err(ovdc_map::Error::Busy);
            }
            // Rocks db cannot currently accept the data.
            Err(ovdc_map::Error::Busy) => {
                busy_count += 1;
                if busy_count == 0 {
                    let span = tracing::Span::current();
                    span.record("busy", true);
                }
                tracing::trace!("waiting for database to be ready");
                tokio::time::sleep(std::time::Duration::from_millis(2u64.pow(busy_count % 12)))
                    .await;
                continue;
            }
            Err(e) => return ovdc_trace::err!(Err(e), "failed to write"),
        }
    }
}
