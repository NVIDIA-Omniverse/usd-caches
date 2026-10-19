// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;
use std::net::SocketAddr;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use http::Method;
use ovcc_peer::{
    Discovery, ExactValidator, ObjectIdentity, PeerDirectory, SelfIdentity, proto,
    validate_who_has_request, who_has_request,
};
use prost::Message as _;

fn identity() -> ObjectIdentity {
    ObjectIdentity {
        host: "bucket.s3.us-west-2.amazonaws.com".to_owned(),
        method: Method::GET,
        uri: "/large/object.usd".to_owned(),
        args: "partNumber=1".to_owned(),
        if_match: "\"version-123\"".to_owned(),
        range: "bytes=0-1048575".to_owned(),
    }
}

fn peers() -> Vec<SocketAddr> {
    (1..=32)
        .map(|last| {
            format!("10.0.0.{last}:14130")
                .parse()
                .expect("peer address")
        })
        .collect()
}

fn bench_codec(c: &mut Criterion) {
    let identity = identity();
    let validator = ExactValidator::Etag("\"version-123\"".to_owned());
    let request = who_has_request(&identity, &validator).expect("valid identity");
    let encoded = request.encode_to_vec();
    c.bench_function("peer/who_has_encode", |b| {
        b.iter(|| {
            who_has_request(black_box(&identity), black_box(&validator))
                .map(|request| request.encode_to_vec())
        })
    });
    c.bench_function("peer/who_has_decode", |b| {
        b.iter(|| {
            proto::WhoHasRequest::decode(black_box(encoded.as_slice()))
                .map_err(|error| error.to_string())
                .and_then(|request| {
                    validate_who_has_request(request).map_err(|error| error.to_string())
                })
        })
    });
}

fn bench_directory(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let directory = PeerDirectory::new(
        Discovery::Explicit(peers()),
        Duration::from_secs(30),
        Duration::from_secs(1),
        SelfIdentity {
            pod_ip: Some("10.0.0.1".parse().expect("self address")),
            ..SelfIdentity::default()
        },
    );
    runtime.block_on(directory.refresh()).expect("refresh");
    c.bench_function("peer/directory_refresh_32", |b| {
        b.iter(|| runtime.block_on(directory.refresh()))
    });
    c.bench_function("peer/directory_select_all_31", |b| {
        b.iter(|| runtime.block_on(directory.select_all()))
    });
}

criterion_group!(benches, bench_codec, bench_directory);
criterion_main!(benches);
