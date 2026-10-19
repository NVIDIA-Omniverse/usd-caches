// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use opentelemetry::metrics::MeterProvider;
use opentelemetry::KeyValue;
use opentelemetry_sdk::{metrics::SdkMeterProvider, Resource};
use ovdc_bytes::keys::{self, Key};
use ovdc_grpc::proto::{key_value_client::KeyValueClient, put_response::PutStatus, PutRequest};
use ovdc_lru::LogUsedKeys;
use ovdc_map::{metadata::Metadata, Database};
use ovdc_trace::metrics::{collect_openmetrics, MeterReader};
use rand::{Rng, RngExt};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::{transport::Channel, Request};

use crate::embedded::{self, enable_test_certs};

pub fn update_config(op: impl FnOnce(&mut toml_edit::DocumentMut), path: &Path) {
    let path = path.join("config.json");

    let file = include_str!("./config.toml");
    let cfg_str = ovdc_settings::load_document_to_str(file, |cfg| {
        cfg["grpc"]["port"] = 0.into();
        cfg["engine"]["cf"]["max_write_buffer_number"] = 3.into();
        op(cfg);
    });

    std::fs::write(path, cfg_str.as_bytes()).unwrap();
}

pub fn prep_test_dir(op: impl FnOnce(&mut toml_edit::DocumentMut)) -> TempDir {
    let temp = tempfile::tempdir().unwrap();

    update_config(op, temp.path());

    temp
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn test_that_most_recent_key_not_removed() {
    let temp = prep_test_dir(|config| {
        config["store"]["maxSize"] = "1G".to_string().into();
    });
    let touch_key = keys::Compound::new(
        Default::default(),
        keys::Key::new(&(!0u128).to_ne_bytes()).unwrap(),
    );

    let count;
    {
        let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
            .await
            .unwrap();

        let map = &embedded.context.env;

        let mut now = now() - 120;
        let bytes = &[120u8; 10];

        {
            let cursor = map.open();
            cursor
                .put(
                    touch_key,
                    Metadata {
                        ttl_ms: None,
                        access_time_secs: 0,
                        created_secs: 0,
                    },
                    &bytes[..],
                )
                .unwrap();
        }

        embedded.context.key_log.log(&[touch_key]);

        const LEN: u64 = 10_000;
        for _ in 0..LEN {
            now -= 1;
            let id: u64 = rand::rng().random();
            let key = keys::Compound::new(
                Default::default(),
                keys::Key::new(&id.to_ne_bytes()).unwrap(),
            );

            let cursor = map.open();
            cursor
                .put(
                    key,
                    Metadata {
                        ttl_ms: None,
                        access_time_secs: now,
                        created_secs: 0,
                    },
                    &bytes[..],
                )
                .unwrap();
        }

        let cursor = map.open();
        let len = Arc::new(AtomicU64::default());
        let l = len.clone();
        cursor
            .retain(Box::new(move |_| {
                l.fetch_add(1, Ordering::Relaxed);
                true
            }))
            .unwrap();
        count = len.load(Ordering::Acquire);

        embedded.context.close().await;

        assert_eq!(count, LEN + 1);
    }

    update_config(
        |config| {
            config["store"]["maxSize"] = "1M".into();
        },
        temp.path(),
    );

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let _stat = embedded.context.env.stat().unwrap();

    embedded.context.key_log.log(&[touch_key]);

    let mut reached = false;
    let expected = ((count as f64) * 0.5).round() as u64;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let len = {
            let cursor = embedded.context.env.open();

            let len = Arc::new(AtomicU64::default());
            let l = len.clone();
            cursor
                .retain(Box::new(move |_| {
                    l.fetch_add(1, Ordering::Relaxed);
                    true
                }))
                .unwrap();
            len.load(Ordering::Acquire)
        };

        if len < expected {
            reached = true;
        }
    }

    assert!(reached, "not enough data was evicted");

    embedded.context.env.open().get(touch_key).unwrap();
}

#[tokio::test]
async fn test_that_touch_can_touch_put() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(1);

    let mut put = client
        .put(ovdc_grpc::put_request(
            key.into_inner().into(),
            vec![1, 2, 3],
        ))
        .await
        .unwrap()
        .into_inner();

    let next = put.message().await.unwrap().unwrap();
    assert!(next.status() == PutStatus::PutOk);
    assert!(put.message().await.unwrap().is_none());

    let touch = client
        .touch(ovdc_grpc::touch_request([key]))
        .await
        .unwrap()
        .into_inner()
        .touched;

    assert_eq!(touch, 1);

    let mut get = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner();

    let next = get.message().await.unwrap().unwrap();
    assert_eq!(next.value, &[1, 2, 3]);

    let mut get = client
        .get(ovdc_grpc::get_request([555.into()]))
        .await
        .unwrap()
        .into_inner();

    assert!(get.message().await.unwrap().is_none());
}

#[tokio::test]
async fn test_that_get_value_is_put_value() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = Key::from_u64_be(123);
    let data = vec![0u8; 10_240];

    {
        let put_res = client
            .put(ovdc_grpc::put_request(key, data.clone()))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(put_res.status(), PutStatus::PutOk);
    }

    let get_res = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(get_res.value.len(), data.len());
    assert_eq!(&get_res.value[..], &data);
}

/// A refused value is refused once, not acknowledged and then refused.
///
/// The blob-table backend stores a value in at most 254 parts, and a value
/// needing more is rejected up front rather than failing partway through. That
/// path still has to read the body the client is streaming, or the entries after
/// it would be parsed out of the middle of this one -- but draining is not
/// accepting, and replying `PutOk` before returning `InvalidArgument` tells the
/// client the value was stored a moment before the same entry fails.
///
/// A small chunk size puts the 254-part ceiling within reach of a test.
#[tokio::test]
async fn test_oversized_value_is_refused_without_being_acknowledged() {
    let temp = prep_test_dir(|cfg| {
        cfg["store"]["mode"] = "legacy_and_discrete".into();
        cfg["store"]["blobChunkSize"] = 1024.into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    // 254 parts of 1 KiB is the ceiling; this needs more than twice that.
    let value = vec![3u8; 600 * 1024];
    let key = keys::Key::from_u64_be(77);

    let mut responses = client
        .put(ovdc_grpc::put_request(key, value))
        .await
        .unwrap()
        .into_inner();

    let mut seen = vec![];
    let status = loop {
        match responses.message().await {
            Ok(Some(response)) => seen.push(response.status()),
            Ok(None) => break None,
            Err(status) => break Some(status),
        }
    };

    assert!(
        !seen.contains(&PutStatus::PutOk),
        "a value the server refused was acknowledged as stored: {seen:?}"
    );

    let status = status.expect("the oversized value should be refused with an error");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    // Refused, so nothing should have been stored under the key.
    let mut got = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner();

    while let Some(next) = got.message().await.unwrap() {
        assert!(
            next.value.is_empty(),
            "a refused value was readable afterwards"
        );
    }
}

#[tokio::test]
async fn test_large_multi_part_put() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(0);

    let val = vec![0u8; 64_000_000];

    let put_status = client
        .put(ovdc_grpc::put_request(key, val))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(put_status.status(), PutStatus::PutOk);

    let mut responses = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner();

    let mut total = 0;
    while let Some(next) = responses.message().await.unwrap() {
        assert_eq!(next.size, 64_000_000);
        assert_eq!(next.tag, 0);
        const ZERO: [u8; 64] = [0u8; 64];
        assert!(next.value.chunks(64).all(|y| y == ZERO));
        total += next.value.len();
    }

    assert_eq!(total, 64_000_000);
}

#[test]
fn test_latency_memory() {
    let extern_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    let embedded = extern_rt.block_on(async move {
        let temp = prep_test_dir(|config| {
            config["store"]["maxSize"] = "265G".to_string().into();
        });

        embedded::Embedded::new(temp.path().to_str().unwrap())
            .await
            .unwrap()
    });

    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();

    client_rt.block_on(async move {
        let client = embedded.client2().await;
        mesaure_latency2(client).await;
    });
}

async fn mesaure_latency2(mut client: KeyValueClient<Channel>) {
    let mut data = vec![0u8; 5000];
    rand::rng().fill_bytes(&mut data);

    for i in 0..1000 {
        client
            .put(ovdc_grpc::put_request(
                keys::Key::from_u64_be(i),
                data.clone(),
            ))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap();
    }

    let mut hist0: hdrhistogram::SyncHistogram<u64> =
        hdrhistogram::Histogram::new_with_max(10_000_000, 2)
            .unwrap()
            .into_sync();

    let mut hist0_get: hdrhistogram::SyncHistogram<u64> =
        hdrhistogram::Histogram::new_with_max(10_000_000, 2)
            .unwrap()
            .into_sync();

    let mut hist1: hdrhistogram::SyncHistogram<u64> =
        hdrhistogram::Histogram::new_with_max(10_000_000, 2)
            .unwrap()
            .into_sync();

    let mut tasks = vec![];
    for _ in 0..1 {
        let mut client = client.clone();
        let mut hist0 = hist0.recorder();
        let mut hist0_get = hist0_get.recorder();
        let mut hist1 = hist1.recorder();
        tasks.push(tokio::task::spawn(async move {
            for _ in 0..2000 {
                let key = keys::Key::from_u64_be(rand::rng().random_range(0..1000u64));

                let now = Instant::now();
                for _ in 0..25 {
                    let now = Instant::now();

                    let mut get_stream = client
                        .get(ovdc_grpc::get_request([key]))
                        .await
                        .unwrap()
                        .into_inner();
                    while get_stream.message().await.unwrap().is_some() {}

                    let t = now.elapsed();
                    let _ = hist0_get.record(t.as_nanos() as u64);
                }
                let t = now.elapsed();
                let _ = hist0.record(t.as_nanos() as u64);
            }

            for _ in 0..2000 {
                let all: Vec<_> = (0..25)
                    .map(|_| keys::Key::from_u64_be(rand::rng().random_range(0..1000)))
                    .collect();

                let now = Instant::now();
                let mut get_stream = client
                    .get(ovdc_grpc::get_request(all))
                    .await
                    .unwrap()
                    .into_inner();

                while get_stream.message().await.unwrap().is_some() {}
                let t = now.elapsed();

                let _ = hist1.record(t.as_nanos() as u64);
            }
        }));
    }

    for task in tasks {
        let _ = task.await;
        hist0.refresh();
        hist1.refresh();
        hist0_get.refresh();
    }

    println!(
        "1x25 p95 {}us",
        hist0_get.value_at_quantile(0.99) as f64 / 1000.0
    );
    println!(
        "1x25 p50 {}us",
        hist0_get.value_at_quantile(0.5) as f64 / 1000.0
    );
    println!(
        "1x25 batch p95 {}us",
        hist0.value_at_quantile(0.99) as f64 / 1000.0
    );
    println!(
        "1x25 batch p50 {}us",
        hist0.value_at_quantile(0.5) as f64 / 1000.0
    );

    println!("--- --- --- --- ---");

    println!(
        "25x1 batch p95 {}us",
        hist1.value_at_quantile(0.99) as f64 / 1000.0
    );
    println!(
        "25x1 batch p50 {}us",
        hist1.value_at_quantile(0.5) as f64 / 1000.0
    );
}

#[tokio::test]
async fn test_tenant_set() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(0);

    let val = vec![0u8; 5000];

    let req = ovdc_grpc::set_tenant("0xDEAD", Request::new(ovdc_grpc::put_request(key, val)));

    // Place a new key in the cache.
    let put_status = client
        .put(req)
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(put_status.status(), PutStatus::PutOk);

    // Ensure that the key exists.
    let mut responses = client
        .get(ovdc_grpc::set_tenant(
            "0xDEAD",
            ovdc_grpc::get_request([key]),
        ))
        .await
        .unwrap()
        .into_inner();
    let next = responses.message().await.unwrap().unwrap();
    assert_eq!(next.size, 5000);
    assert_eq!(next.tag, 0);

    // The key must not exist if a different tenant id is used.
    let mut responses = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner();
    assert!(responses.message().await.unwrap().is_none());

    // The key must not exist if a different tenant id is used.
    let mut responses = client
        .get(ovdc_grpc::set_tenant(
            "0xBEEF",
            ovdc_grpc::get_request([key]),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(responses.message().await.unwrap().is_none());
}

#[tokio::test]
async fn test_tenant_from_jwt() {
    let temp = prep_test_dir(|cfg| {
        cfg["grpcJwt"]["enabled"] = true.into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(0);

    let val = vec![0u8; 5000];

    let req = ovdc_grpc::set_jwt(
        ovdc_jwt::_DEBUG_JWT_0,
        Request::new(ovdc_grpc::put_request(key, val)),
    );

    // Place a new key in the cache.
    let put_status = client
        .put(req)
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(put_status.status(), PutStatus::PutOk);

    // Ensure that the key exists.
    let mut responses = client
        .get(ovdc_grpc::set_jwt(
            ovdc_jwt::_DEBUG_JWT_0,
            ovdc_grpc::get_request([key]),
        ))
        .await
        .unwrap()
        .into_inner();
    let next = responses.message().await.unwrap().unwrap();
    assert_eq!(next.size, 5000);
    assert_eq!(next.tag, 0);

    // The key must not exist if a different tenant id is used.
    let mut responses = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner();
    assert!(responses.message().await.unwrap().is_none());

    // The key must not exist if a different tenant id is used.
    let mut responses = client
        .get(ovdc_grpc::set_jwt(
            ovdc_jwt::_DEBUG_JWT_1,
            ovdc_grpc::get_request([key]),
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(responses.message().await.unwrap().is_none());
}

#[tokio::test]
async fn test_jwt_no_nid() {
    let temp = prep_test_dir(|cfg| {
        cfg["grpcJwt"]["enabled"] = true.into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(0);

    let val = vec![0u8; 5000];

    let req = ovdc_grpc::set_jwt(
        ovdc_jwt::_DEBUG_BAD_JWT_MISSING_NID,
        Request::new(ovdc_grpc::put_request(key, val)),
    );

    assert!(client.put(req).await.is_err())
}

#[tokio::test]
async fn test_jwt_expired() {
    let temp = prep_test_dir(|cfg| {
        cfg["grpcJwt"]["enabled"] = true.into();
        cfg["grpcJwt"]["verifyExp"] = true.into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = keys::Key::from_u64_be(0);

    let val = vec![0u8; 5000];

    let req = ovdc_grpc::set_jwt(
        ovdc_jwt::_DEBUG_BAD_JWT_EXPIRED,
        Request::new(ovdc_grpc::put_request(key, val)),
    );

    assert!(client.put(req).await.is_err())
}

#[tokio::test]
async fn test_get_with_missing() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = Key::from_u64_be(123);
    let key_missing = Key::from_u64_be(456);
    let data = vec![0u8; 10_240];

    {
        let put_res = client
            .put(ovdc_grpc::put_request(key, data.clone()))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(put_res.status(), PutStatus::PutOk);
    }

    let get_res = client
        .get(ovdc_grpc::get_request([key_missing, key]))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(get_res.value.len(), data.len());
    assert_eq!(&get_res.value[..], &data);
}

#[tokio::test]
async fn test_missing_keys() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let data = vec![0u8; 10_240];
    for id in 0..8 {
        let key = Key::from_u64_be(id);

        let put_res = client
            .put(ovdc_grpc::put_request(key, data.clone()))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(put_res.status(), PutStatus::PutOk);
    }

    let keys = [
        Key::from_u64_be(1),
        Key::from_u64_be(2),
        Key::from_u64_be(88),
        Key::from_u64_be(4),
    ];

    let mut get_res = client
        .get(ovdc_grpc::get_request(keys))
        .await
        .unwrap()
        .into_inner();

    for _ in 0..3 {
        let next = get_res.message().await.unwrap().unwrap();
        assert_eq!(next.value.len(), data.len());
        assert_eq!(&next.value[..], &data);
    }
}

#[tokio::test]
async fn test_that_get_value_is_put_value_chunked_large() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    for k in 1..10 {
        let key = Key::from_u64_be(k);
        let data = vec![0u8; k as usize * 1_000_000];

        let put_res = client
            .put(ovdc_grpc::put_request_split_by(key, data.clone(), 4_000))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(put_res.status(), PutStatus::PutOk);

        let mut responses = client
            .get(ovdc_grpc::get_request([key]))
            .await
            .unwrap()
            .into_inner();

        let mut total = 0;
        while let Some(next) = responses.message().await.unwrap() {
            assert_eq!(next.size, data.len() as u64);
            assert_eq!(next.tag, 0);
            const ZERO: [u8; 64] = [0u8; 64];
            assert!(next.value.chunks(64).all(|y| y == ZERO));
            total += next.value.len();
        }

        assert_eq!(total, data.len());
    }
}

#[tokio::test]
async fn test_that_get_value_is_put_value_chunked() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    for k in 1..10 {
        let key = Key::from_u64_be(k);
        let data = vec![0u8; k as usize * 2_000];

        let put_res = client
            .put(ovdc_grpc::put_request_split_by(key, data.clone(), 4_000))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(put_res.status(), PutStatus::PutOk);

        let get_res = client
            .get(ovdc_grpc::get_request([key]))
            .await
            .unwrap()
            .into_inner()
            .message()
            .await
            .unwrap()
            .unwrap();

        assert_eq!(get_res.value.len(), data.len());
        assert_eq!(&get_res.value[..], &data);
    }
}

#[tokio::test]
async fn test_last_write_wins() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    {
        let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
        let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);
        let in_flight = client.put(recv_stream).await.unwrap();

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
                    ovdc_grpc::proto::PutOpen {
                        ttl: None,
                        durability: None,
                    },
                )),
            })
            .await;

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
                    ovdc_grpc::proto::PutDeclare {
                        key: Some(keys::Key::from_u64_be(123).into()),
                        size: 8_000_001,
                        value: vec![],
                    },
                )),
            })
            .await;

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![
                    0u8;
                    4_000_000
                ])),
            })
            .await;

        {
            let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
            let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);
            let in_flight = client.put(recv_stream).await.unwrap();

            let _ = send
                .send(PutRequest {
                    kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
                        ovdc_grpc::proto::PutOpen {
                            ttl: None,
                            durability: None,
                        },
                    )),
                })
                .await;

            let _ = send
                .send(PutRequest {
                    kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
                        ovdc_grpc::proto::PutDeclare {
                            key: Some(keys::Key::from_u64_be(123).into()),
                            size: 8_000_001,
                            value: vec![],
                        },
                    )),
                })
                .await;

            let _ = send
                .send(PutRequest {
                    kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![
                        1u8;
                        4_000_000
                    ])),
                })
                .await;

            let _ = send
                .send(PutRequest {
                    kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![
                        1u8;
                        4_000_001
                    ])),
                })
                .await;

            let mut stream = in_flight.into_inner();
            let next = stream.message().await.unwrap().unwrap();
            assert!(next.status() == PutStatus::PutOk);
            assert!(next.index.unwrap() == 0);
        }

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![
                    0u8;
                    4_000_001
                ])),
            })
            .await;

        let put_res = in_flight.into_inner().message().await.unwrap().unwrap();
        assert_eq!(put_res.status(), PutStatus::PutOk);
    }

    let mut responses = client
        .get(ovdc_grpc::get_request([keys::Key::from_u64_be(123)]))
        .await
        .unwrap()
        .into_inner();

    let mut total = 0;
    while let Some(next) = responses.message().await.unwrap() {
        assert_eq!(next.size, 8_000_001);
        assert_eq!(next.tag, 0);
        assert!(next.value.iter().all(|y| *y == 0));
        total += next.value.len();
    }

    assert_eq!(total, 8_000_001);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn test_threaded_concurrent_put() {
    let temp = prep_test_dir(|cfg| {
        cfg["store"]["maxSize"] = "1G".to_owned().into();
        cfg["store"]["blobChunkSize"] = 250.into();
        cfg["store"]["blobCutoff"] = 100.into();
        cfg["engine"]["cf"]["min_blob_size"] = "200".to_owned().into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let keys = (0..8).map(keys::Key::from_u64_be);
    for key in keys {
        let mut client = embedded.client2().await;
        let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
        let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);
        let in_flight = client.put(recv_stream).await.unwrap();
        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
                    ovdc_grpc::proto::PutOpen {
                        ttl: None,
                        durability: None,
                    },
                )),
            })
            .await;

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
                    ovdc_grpc::proto::PutDeclare {
                        key: Some(key.into()),
                        size: 600,
                        value: vec![],
                    },
                )),
            })
            .await;

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![0u8; 300])),
            })
            .await;
        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![0u8; 300])),
            })
            .await;

        let mut stream = in_flight.into_inner();
        let next = stream.message().await.unwrap().unwrap();
        assert!(next.status() == PutStatus::PutOk);
    }

    // While writing, the only acceptable errors are failure for concurrent put OR
    // putok.
    for _ in 0..3 {
        let mut client = embedded.client2().await;

        tokio::spawn(async move {
            let mut keys = std::iter::repeat(0..8)
                .step_by(2)
                .flatten()
                .map(keys::Key::from_u64_be);
            loop {
                let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
                let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);
                let in_flight = client.put(recv_stream).await.unwrap();
                let _ = send
                    .send(PutRequest {
                        kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
                            ovdc_grpc::proto::PutOpen {
                                ttl: None,
                                durability: None,
                            },
                        )),
                    })
                    .await;

                let _ = send
                    .send(PutRequest {
                        kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
                            ovdc_grpc::proto::PutDeclare {
                                key: keys.next().map(|y| y.into()),
                                size: 600,
                                value: vec![],
                            },
                        )),
                    })
                    .await;

                let _ = send
                    .send(PutRequest {
                        kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![0u8; 300])),
                    })
                    .await;
                let _ = send
                    .send(PutRequest {
                        kind: Some(ovdc_grpc::proto::put_request::Kind::Value(vec![0u8; 300])),
                    })
                    .await;

                let mut stream = in_flight.into_inner();
                let next = stream.message().await.unwrap().unwrap();
                assert!(
                    next.status() == PutStatus::ErrorConcurrentWrite
                        || next.status() == PutStatus::PutOk
                );
            }
        });
    }

    let mut keys = std::iter::repeat(0..8)
        .flatten()
        .map(keys::Key::from_u64_be);

    let mut client = embedded.client2().await;

    // while reading, we must either get an entire value back, or an error.
    for count in 1..5 {
        let for_count = 800 / count;
        for _ in 0..for_count {
            let mut responses = client
                .get(ovdc_grpc::get_request(keys.by_ref().take(count)))
                .await
                .unwrap()
                .into_inner();

            let mut is_err = false;
            let mut counts = [0u64, 0, 0, 0];
            while let Some(next) = {
                let next = responses.message().await;
                match next {
                    Ok(x) => x,
                    Err(e) if e.message() == "value changed" => {
                        is_err = true;
                        None
                    }
                    Err(e) => {
                        panic!("failed with err {e}");
                    }
                }
            } {
                counts[next.tag as usize] += next.value.len() as u64;
                assert_eq!(next.size, 600u64);
            }

            let expected = 600 * count as u64;
            let sum: u64 = counts[..count].iter().sum();

            let is_both_recv = sum == expected;
            let is_both_empty = sum == 0;
            let is_any_partial = sum > 0 && !is_both_empty;

            if is_both_recv {
                assert!(!is_err);
            } else if is_both_empty || is_any_partial {
                assert!(
                    is_err,
                    "expected error, empty={is_both_empty} partial={is_any_partial}, looking for {count} {counts:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn test_chunked_put() {
    let temp = prep_test_dir(|cfg| {
        cfg["engine"]["cacheSize"] = "1K".to_owned().into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let buffer = vec![0u8; 2_000_000];

    let trials: &[(bool, usize, &[usize])] = &[
        (
            false,
            8_000_002,
            &[
                1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000,
                1_000_000, 2,
            ],
        ),
        (
            true,
            8_000_002,
            &[
                1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000,
                1_000_000,
            ],
        ),
        (
            false,
            4_156_944,
            &[1_048_576, 1_048_576, 1_048_576, 1_011_216],
        ),
        (true, 4_194_304, &[1_048_576, 1_048_576, 1_048_576]),
    ];

    for (fails, total_size, chunks) in trials {
        let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
        let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);

        let in_flight = client.put(recv_stream).await.unwrap();

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
                    ovdc_grpc::proto::PutOpen {
                        ttl: None,
                        durability: None,
                    },
                )),
            })
            .await;

        let _ = send
            .send(PutRequest {
                kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
                    ovdc_grpc::proto::PutDeclare {
                        key: Some(keys::Key::from_u64_be(123).into()),
                        size: *total_size as u64,
                        value: vec![],
                    },
                )),
            })
            .await;

        for size in chunks.iter().copied() {
            let _ = send
                .send(PutRequest {
                    kind: Some(ovdc_grpc::proto::put_request::Kind::Value(
                        buffer[..size].to_vec(),
                    )),
                })
                .await;
        }
        drop(send);

        if !fails {
            let put_res = in_flight.into_inner().message().await.unwrap().unwrap();
            assert_eq!(put_res.status(), PutStatus::PutOk);
            continue;
        }

        let put_res = in_flight.into_inner().message().await;
        assert!(put_res.is_err());
    }
}

/// The shipped `config.toml` must carry a `putTimeout` the binary accepts, and
/// it must mean one hour.
///
/// This is the only test tying the file's `"1h"` to the value `put_timeout()`
/// hands `streaming`; the unit tests in `context::config` cover the clamp but
/// start from an in-memory struct, so a typo in the TOML would escape them.
#[tokio::test]
async fn test_shipped_config_put_timeout_is_one_hour() {
    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let grpc: crate::context::Grpc =
        ovdc_settings::Snap::subtable(&embedded.context.settings2.snap(), "grpc");

    assert_eq!(
        grpc.put_timeout(),
        Duration::from_secs(3600),
        "shipped config.toml should set grpc.putTimeout to 1h"
    );
}

/// A PUT that stops sending is cut off at `grpc.putTimeout` rather than holding
/// its write permit until the client gives up.
///
/// The stream declares a value above `blobCutoff` so it takes the multi-part
/// path that waits for further messages, then sends nothing while keeping the
/// sender alive -- a half-open upload, not a closed stream. Without the limit
/// the server would wait indefinitely and this test would hang until the
/// harness killed it.
#[tokio::test]
async fn test_put_is_cut_off_at_the_configured_time_limit() {
    let temp = prep_test_dir(|cfg| {
        cfg["grpc"]["putTimeout"] = "300ms".to_owned().into();
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();
    let mut client = embedded.client2().await;

    let (send, recv) = tokio::sync::mpsc::channel::<PutRequest>(10);
    let recv_stream = tokio_stream::wrappers::ReceiverStream::new(recv);

    let in_flight = client.put(recv_stream).await.unwrap();

    send.send(PutRequest {
        kind: Some(ovdc_grpc::proto::put_request::Kind::Begin(
            ovdc_grpc::proto::PutOpen {
                ttl: None,
                durability: None,
            },
        )),
    })
    .await
    .unwrap();

    send.send(PutRequest {
        kind: Some(ovdc_grpc::proto::put_request::Kind::Declare(
            ovdc_grpc::proto::PutDeclare {
                key: Some(keys::Key::from_u64_be(9001).into()),
                // Above blobCutoff, so the server waits for value messages.
                size: 4_000_000,
                value: vec![],
            },
        )),
    })
    .await
    .unwrap();

    // Hold the sender open for the whole wait: dropping it would end the stream
    // and the server would fail for that reason instead, which is not what this
    // test is about.
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(20), in_flight.into_inner().message())
        .await
        .expect("server did not answer; the put time limit did not fire");
    drop(send);

    let err = result.expect_err("a stalled put should be refused, not accepted");
    assert_eq!(
        err.code(),
        tonic::Code::DeadlineExceeded,
        "expected DeadlineExceeded, got {err:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the limit fired, but far later than the configured 300ms: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn test_client_can_connect_tls() {
    let temp = prep_test_dir(|cfg| {
        cfg["engine"]["cacheSize"] = "1K".to_owned().into();
        enable_test_certs(cfg);
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    {
        let mut _client0 = embedded.client_tls().await;
        let mut _client1 = embedded.client_tls().await;
    }

    {
        let mut _client0 = embedded.client_tls().await;
        let mut _client1 = embedded.client_tls().await;
    }
}

#[tokio::test]
async fn test_tls_service_accepts_garbage() {
    let temp = prep_test_dir(|cfg| {
        cfg["engine"]["cacheSize"] = "1K".to_owned().into();
        enable_test_certs(cfg);
    });

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client_tls().await;

    client
        .get(ovdc_grpc::get_request(
            [keys::Key::from_u64_ne(0)].into_iter(),
        ))
        .await
        .unwrap();

    {
        let _channel = embedded.raw_channel().await;
    }

    {
        let mut channel = embedded.raw_channel().await;

        channel.write_all(b"dead beef\n").await.unwrap();
        channel.flush().await.unwrap();

        let mut buff = vec![0u8; 4096];
        let _ = channel.read(&mut buff[..]).await.unwrap();
        assert_eq!(channel.read(&mut buff[..]).await.unwrap(), 0)
    }
    let mut client2 = embedded.client_tls().await;

    client
        .get(ovdc_grpc::get_request(
            [keys::Key::from_u64_ne(0)].into_iter(),
        ))
        .await
        .unwrap();
    client2
        .get(ovdc_grpc::get_request(
            [keys::Key::from_u64_ne(0)].into_iter(),
        ))
        .await
        .unwrap();
}

/// Extracts the `config.toml` block from `helm template` output.
///
/// The ConfigMap uses a YAML block scalar indented by 4 spaces; this strips
/// that leading indent to recover the raw TOML.
///
/// The TOML is otherwise passed through verbatim, so this test exercises
/// exactly the bytes the container receives. The chart templates each setting
/// explicitly rather than piping `.Values.settings` through `toToml`, which
/// would render every number as a float (`8000.0`, `4e+06`) and fail to
/// deserialise into the `HumanByteSize` / `HumanDuration` fields.
fn extract_helm_config_toml(yaml: &str) -> Option<String> {
    for doc in yaml.split("\n---") {
        if !doc.contains("kind: ConfigMap") {
            continue;
        }
        let marker = "config.toml: |\n";
        let start = doc.find(marker)? + marker.len();
        let toml = doc[start..]
            .lines()
            // Strip 4-space block-scalar indent.
            .map(|line| line.strip_prefix("    ").unwrap_or("").to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        return Some(toml);
    }
    None
}

/// Renders the Helm chart's default values into a TOML config, starts an
/// embedded database with that config, and verifies a round-trip put/get.
///
/// This is the only test covering the chart/application config contract: every
/// `subtable()` the binary reads must deserialize from what the chart renders.
///
/// If `helm` is not installed the test skips, so local runs without helm still
/// pass. Set `OVDC_REQUIRE_HELM=1` to turn that skip into a failure — CI does
/// this so a broken helm install cannot silently drop the coverage.
#[tokio::test]
async fn test_helm_chart_config() {
    let helm_chart = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/helm");
    let require_helm = std::env::var_os("OVDC_REQUIRE_HELM").is_some();

    let output = match std::process::Command::new("helm")
        .args(["template", "ovdc-test"])
        .arg(&helm_chart)
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => panic!(
            "helm template failed: {}",
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !require_helm => {
            eprintln!("skipping test_helm_chart_config: helm is not installed");
            return;
        }
        Err(e) => panic!("failed to run helm ({helm_chart:?}): {e}"),
    };

    let yaml = String::from_utf8(output.stdout).unwrap();
    let toml = extract_helm_config_toml(&yaml).expect("config.toml not found in helm output");

    let temp = tempfile::tempdir().unwrap();

    let cfg_str = ovdc_settings::load_document_to_str(&toml, |cfg| {
        cfg["grpc"]["port"] = 0.into();
        cfg["engine"]["cf"]["max_write_buffer_number"] = 3.into();
    });

    std::fs::write(temp.path().join("config.json"), cfg_str.as_bytes()).unwrap();

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = Key::from_u64_be(1);
    let data = b"hello from helm config".to_vec();

    let put_res = client
        .put(ovdc_grpc::put_request(key, data.clone()))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(put_res.status(), PutStatus::PutOk);

    let get_res = client
        .get(ovdc_grpc::get_request([key]))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap()
        .unwrap();

    assert_eq!(&get_res.value, &data);
}

/// Serializes tests that replace the global OTEL metrics provider (`ovdc_trace::metrics::set`).
static GRPC_PAYLOAD_METRICS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn openmetrics_histogram_sum_for_method(
    out: &str,
    metric_name_contains: &str,
    grpc_method: &str,
) -> f64 {
    let needle = format!("{}_sum", metric_name_contains);
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if line.contains(&needle) && line.contains(grpc_method) {
            if let Some(v) = line.split_whitespace().last() {
                if let Ok(x) = v.parse::<f64>() {
                    return x;
                }
            }
        }
    }
    0.0
}

#[tokio::test]
async fn test_grpc_touch_increments_payload_byte_histograms() {
    let _lock = GRPC_PAYLOAD_METRICS_LOCK.lock().await;

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();

    let reader = MeterReader::default();
    let resource = Resource::builder_empty()
        .with_attributes([KeyValue::new("service.name", "ovdc")])
        .build();
    let provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader.clone())
        .build();
    ovdc_trace::metrics::set(provider.meter("ovdc"), provider);

    let before = collect_openmetrics(&reader);
    let sent_before = openmetrics_histogram_sum_for_method(
        &before,
        "sent_total_compressed_message_size",
        "ovdc.KeyValue/Touch",
    );
    let rcvd_before = openmetrics_histogram_sum_for_method(
        &before,
        "rcvd_total_compressed_message_size",
        "ovdc.KeyValue/Touch",
    );

    let temp = prep_test_dir(|_| {});

    let embedded = embedded::Embedded::new(temp.path().to_str().unwrap())
        .await
        .unwrap();

    let mut client = embedded.client2().await;

    let key = Key::from_u64_be(991_234);

    // Put a value so Touch returns touched > 0. Proto3 omits default scalar fields, so an
    // all-default TouchResponse encodes to 0 bytes and the codec records no "sent" observation.
    let mut put = client
        .put(ovdc_grpc::put_request(key, vec![7]))
        .await
        .unwrap()
        .into_inner();
    let put_ok = put.message().await.unwrap().unwrap();
    assert_eq!(put_ok.status(), PutStatus::PutOk);
    assert!(put.message().await.unwrap().is_none());

    client.touch(ovdc_grpc::touch_request([key])).await.unwrap();

    let after = collect_openmetrics(&reader);
    let sent_after = openmetrics_histogram_sum_for_method(
        &after,
        "sent_total_compressed_message_size",
        "ovdc.KeyValue/Touch",
    );
    let rcvd_after = openmetrics_histogram_sum_for_method(
        &after,
        "rcvd_total_compressed_message_size",
        "ovdc.KeyValue/Touch",
    );

    assert!(
        sent_after > sent_before,
        "sent payload sum should increase (before {sent_before}, after {sent_after}):\n{after}"
    );
    assert!(
        rcvd_after > rcvd_before,
        "recv payload sum should increase (before {rcvd_before}, after {rcvd_after}):\n{after}"
    );
    assert!(sent_after > 0.0, "expected non-zero sent bytes");
    assert!(rcvd_after > 0.0, "expected non-zero recv bytes");
}
