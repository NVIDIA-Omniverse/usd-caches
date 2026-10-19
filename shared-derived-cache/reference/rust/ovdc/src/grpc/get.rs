// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GRPC methods to handle get of key.

use std::{io::ErrorKind, time::Duration};

use ovdc_bytes::keys;
use ovdc_grpc::proto::GetUnorderedResponse;
use ovdc_lru::LogUsedKeys;
use ovdc_map::{Database, Entry, Value};
use ovdc_trace::metrics::Measure;
use tokio::sync::mpsc::error::SendTimeoutError;

use crate::context::Context;

use super::{metrics, WRITE_TIMEOUT};

#[tracing::instrument(skip_all)]
pub async fn handle_get<D: Database>(
    cx: Context<D>,
    keys: Vec<keys::Compound>,
    send: &tokio::sync::mpsc::Sender<Result<GetUnorderedResponse, tonic::Status>>,
) -> Result<(), tonic::Status> {
    for (index, key) in keys.into_iter().enumerate() {
        let cursor = cx.env.open();

        tracing::info_span!("log_key_use").in_scope(|| {
            cx.key_log.log(&[key]);
        });

        if let Ok(entry) = cursor.get_non_blocking(key) {
            handle_match(&cx.env, send, Ok(&entry), index as u32, key).await?;
            continue;
        }

        // perform blocking operation to find the key pair
        let get_span = tracing::info_span!("get_key");
        let db = cx.env.clone();
        let entry = tokio::task::spawn_blocking(move || {
            let _span = get_span.entered();
            let cursor = db.open();
            cursor.get(key)
        })
        .await
        .unwrap();

        let store;
        let new_result = match entry {
            Ok(x) => {
                store = Some(x);
                Ok(store.as_ref().unwrap() as &dyn Entry)
            }
            Err(x) => Err(x),
        };

        handle_match(&cx.env, send, new_result, index as u32, key).await?;
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn queue_as_vec(
    send: &tokio::sync::mpsc::Sender<Result<GetUnorderedResponse, tonic::Status>>,
    index: u32,
    total_size: u64,
    value: Vec<u8>,
) -> Result<(), tonic::Status> {
    ovdc_trace::err!(
        queue(
            send,
            GetUnorderedResponse {
                tag: index,
                size: total_size,
                value
            },
        )
        .await,
        "failed to write back"
    )?;

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn handle_match(
    env: &impl Database,
    send: &tokio::sync::mpsc::Sender<Result<GetUnorderedResponse, tonic::Status>>,
    entry: Result<&dyn Entry, ovdc_map::Error>,
    index: u32,
    key: keys::Compound,
) -> Result<(), tonic::Status> {
    if is_not_found(&entry) {
        return Ok(());
    }

    if is_write_in_progress(&entry) {
        tracing::warn!(?key, "concurrent write and read");
        return Err(tonic::Status::aborted("value changed"));
    }

    let Ok(entry) = ovdc_trace::err!(entry, "failed to get") else {
        return Err(tonic::Status::internal("internal db error"));
    };

    let value = match entry.value() {
        Value::Blob {
            size,
            tag: blob_tag,
            ..
        } => {
            return recall_multi_part(env, index, *size, key, *blob_tag, send).await;
        }
        Value::Vec(x) => x,
    };

    queue_as_vec(send, index, entry.value().len(), value.as_ref().to_vec()).await?;

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn recall_multi_part(
    env: &impl Database,
    tag: u32,
    size: u64,
    key: keys::Compound,
    blob_tag: u128,
    send: &tokio::sync::mpsc::Sender<Result<GetUnorderedResponse, tonic::Status>>,
) -> Result<(), tonic::Status> {
    let mut read_cx = env.blobs().begin_read(key, blob_tag);

    let mut read = 0;

    while read < size {
        tracing::trace!(tag, size, "read part");
        let _span_guard = tracing::info_span!("get_key").entered();

        match env.blobs().poll_read(&mut read_cx) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                tracing::trace!(tag, ?key, "blob read");
                return Err(tonic::Status::internal("failed to fetch value"));
            }
            x => {
                ovdc_trace::drop!(x, "error reading blob part");
                return Err(tonic::Status::internal("failed to fetch value"));
            }
        }

        drop(_span_guard);

        while let Some(next) = env.blobs().next(&mut read_cx) {
            tracing::trace!(tag, ?key, size = { next.len() }, "blob to be sent");
            queue_as_vec(send, tag, size, next.to_vec()).await?;
            read += next.len() as u64;
        }
    }

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn queue<T>(
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

#[inline]
fn is_not_found<T>(value: &Result<T, ovdc_map::Error>) -> bool {
    matches!(value, Err(ovdc_map::Error::NotFound))
}

#[inline]
fn is_write_in_progress<T>(value: &Result<T, ovdc_map::Error>) -> bool {
    matches!(value, Err(ovdc_map::Error::WriteInProgress))
}
