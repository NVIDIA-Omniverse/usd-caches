// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression guard for the peer-fill preflight, the crate's pure per-request
//! work. Planning rewrites the client header set and inspects the presigned
//! query, and metadata extraction walks the origin headers case-insensitively;
//! both run before any socket is touched.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_upstream::{
    OriginResponse, PreflightMetadata, PreflightValidator, UpstreamTiming, peer_preflight_metadata,
    plan_peer_preflight,
};

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// A client request as it reaches the origin path, large enough that the
/// header filter and clone dominate rather than the allocation of a stub.
fn client_headers() -> Vec<(String, String)> {
    headers(&[
        ("Host", "bucket.s3.us-west-2.amazonaws.com"),
        ("User-Agent", "aws-sdk-rust/1.0 os/linux lang/rust/1.90"),
        ("Accept", "*/*"),
        ("Accept-Encoding", "identity"),
        ("X-Amz-Date", "20260101T000000Z"),
        (
            "X-Amz-Content-Sha256",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        ("Range", "bytes=1048576-2097151"),
    ])
}

fn origin_headers(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut all = headers(&[
        ("Date", "Thu, 01 Jan 2026 00:00:00 GMT"),
        ("Server", "AmazonS3"),
        ("x-amz-request-id", "8E4C5B1F2A3D9017"),
        ("x-amz-id-2", "kZbY1s9m0rQe6TtLpNcVxHgWdJfAuOiRyXbEnMlK"),
        ("Accept-Ranges", "bytes"),
        ("Content-Type", "application/octet-stream"),
        ("Last-Modified", "Wed, 31 Dec 2025 23:00:00 GMT"),
    ]);
    all.extend(headers(extra));
    all
}

fn origin_response(status: u16, extra: &[(&str, &str)]) -> OriginResponse {
    OriginResponse {
        status,
        etag: Some("\"v1\"".to_owned()),
        headers: origin_headers(extra),
        body: Vec::new(),
        timing: UpstreamTiming::default(),
    }
}

fn range_header(plan: &ovcc_upstream::PeerPreflight) -> Option<&str> {
    plan.headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.as_str())
}

fn bench_plan(c: &mut Criterion) {
    let request = client_headers();

    // The signature covers `range`, so the query must be parsed and the client's
    // own range validated as single and finite instead of replaced.
    let signed_range = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
                        &X-Amz-Credential=AKID%2F20260101%2Fus-west-2%2Fs3%2Faws4_request\
                        &X-Amz-Date=20260101T000000Z&X-Amz-Expires=3600\
                        &X-Amz-SignedHeaders=host%3Brange&X-Amz-Signature=deadbeef&x-id=GetObject";

    let public = plan_peer_preflight("", &request, false).expect("public plan");
    let authenticated = plan_peer_preflight("", &request, true).expect("authenticated plan");
    let presigned = plan_peer_preflight(signed_range, &request, true).expect("signed-range plan");
    assert_eq!(public.method.as_str(), "HEAD");
    assert_eq!(range_header(&public), None);
    assert_eq!(authenticated.method.as_str(), "GET");
    assert_eq!(range_header(&authenticated), Some("bytes=0-0"));
    assert_eq!(presigned.method.as_str(), "GET");
    assert_eq!(range_header(&presigned), Some("bytes=1048576-2097151"));

    let mut group = c.benchmark_group("preflight_plan");
    group.bench_function("public", |b| {
        b.iter(|| plan_peer_preflight(black_box(""), black_box(&request), black_box(false)))
    });
    group.bench_function("authenticated", |b| {
        b.iter(|| plan_peer_preflight(black_box(""), black_box(&request), black_box(true)))
    });
    group.bench_function("presigned_signed_range", |b| {
        b.iter(|| {
            plan_peer_preflight(
                black_box(signed_range),
                black_box(&request),
                black_box(true),
            )
        })
    });
    group.finish();
}

fn bench_metadata(c: &mut Criterion) {
    let head = plan_peer_preflight("", &client_headers(), false).expect("public plan");
    let ranged = plan_peer_preflight("", &client_headers(), true).expect("authenticated plan");

    let head_200 = origin_response(200, &[("Content-Length", "10485760"), ("ETag", "\"v1\"")]);
    let get_206 = origin_response(
        206,
        &[
            ("Content-Length", "1"),
            ("Content-Range", "bytes 0-0/10485760"),
            ("ETag", "\"v1\""),
        ],
    );
    // No `ETag`, so the walk runs to the end and falls back to `Last-Modified`.
    let last_modified = origin_response(206, &[("Content-Range", "bytes 0-0/10485760")]);

    // Guards the fixtures: a shape change that made these unauthorized would
    // leave the benchmark timing an early return instead of the real walk.
    assert_eq!(
        peer_preflight_metadata(&head, &head_200),
        Some(PreflightMetadata {
            total_size: 10_485_760,
            validator: PreflightValidator::Etag("\"v1\"".to_owned()),
        })
    );
    assert_eq!(
        peer_preflight_metadata(&ranged, &get_206),
        Some(PreflightMetadata {
            total_size: 10_485_760,
            validator: PreflightValidator::Etag("\"v1\"".to_owned()),
        })
    );
    assert_eq!(
        peer_preflight_metadata(&ranged, &last_modified),
        Some(PreflightMetadata {
            total_size: 10_485_760,
            validator: PreflightValidator::LastModified("Wed, 31 Dec 2025 23:00:00 GMT".to_owned()),
        })
    );
    assert!(peer_preflight_metadata(&head, &get_206).is_none());

    let mut group = c.benchmark_group("preflight_metadata");
    group.bench_function("head_200", |b| {
        b.iter(|| peer_preflight_metadata(black_box(&head), black_box(&head_200)))
    });
    group.bench_function("get_206", |b| {
        b.iter(|| peer_preflight_metadata(black_box(&ranged), black_box(&get_206)))
    });
    group.bench_function("last_modified", |b| {
        b.iter(|| peer_preflight_metadata(black_box(&ranged), black_box(&last_modified)))
    });
    // A `HEAD` plan answered with `206` is an unusable shape, and rejection is
    // the cost the peer path pays before giving up.
    group.bench_function("rejected", |b| {
        b.iter(|| peer_preflight_metadata(black_box(&head), black_box(&get_206)))
    });
    group.finish();
}

fn bench_nameserver_scope(c: &mut Criterion) {
    // Models the large inline Nucleus future without network or executor noise.
    c.bench_function("nameserver_scope/box_9600_byte_future", |b| {
        b.iter(|| {
            let payload = black_box([0_u8; 9600]);
            let future =
                ovcc_upstream::with_nameserver_override(None, async move { black_box(payload) });
            drop(black_box(Box::pin(future)));
        });
    });
}

criterion_group!(benches, bench_plan, bench_metadata, bench_nameserver_scope);
criterion_main!(benches);
