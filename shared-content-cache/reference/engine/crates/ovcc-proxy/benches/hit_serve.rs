// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression guard for the per-response CPU envelope every served request pays
//! outside the zero-copy body: rendering the head to wire bytes, the
//! conditional-GET `If-None-Match` check, and the origin-header relay filter.
//!
//! The body itself never enters userspace on a HIT (kTLS `sendfile`), so this
//! head/relay work is the proxy's per-request compute and the right thing to
//! keep from regressing.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_proxy::{ResponseHead, if_none_match_matches, relay_headers, render_head};

fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Head serialization on the HIT path. A `200` replays the leased object's
/// headers with `Content-Length`; a `304` carries only the validator and omits
/// the length. `keep-alive` and `close` differ only in the trailing token.
fn bench_render_head(c: &mut Criterion) {
    let ok = ResponseHead {
        status: 200,
        reason: "OK",
        cache_status: Some("HIT"),
        headers: hdr(&[
            ("Accept-Ranges", "bytes"),
            ("ETag", "\"9a1e...\""),
            ("Content-Type", "application/octet-stream"),
            ("Content-Encoding", "gzip"),
            ("Last-Modified", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ]),
        content_length: Some(1_048_576),
    };
    let not_modified = ResponseHead {
        status: 304,
        reason: "Not Modified",
        cache_status: Some("HIT"),
        headers: hdr(&[("ETag", "\"9a1e...\"")]),
        content_length: None,
    };

    let mut group = c.benchmark_group("hit_serve/render_head");
    group.bench_function("hit_200_keepalive", |b| {
        b.iter(|| render_head(black_box(&ok), black_box(true)))
    });
    group.bench_function("hit_200_close", |b| {
        b.iter(|| render_head(black_box(&ok), black_box(false)))
    });
    group.bench_function("not_modified_304", |b| {
        b.iter(|| render_head(black_box(&not_modified), black_box(true)))
    });
    group.finish();
}

/// The conditional-GET check on a cached validator, across the branches a
/// client can send: an exact match (`304`), a wildcard, a non-match that serves
/// the full HIT, and an absent header.
fn bench_if_none_match(c: &mut Criterion) {
    let etag = Some("\"9a1e2f\"");
    let list = "\"deadbeef\", \"cafef00d\", \"9a1e2f\"";

    let mut group = c.benchmark_group("hit_serve/if_none_match");
    group.bench_function("match_single", |b| {
        b.iter(|| if_none_match_matches(black_box(Some("\"9a1e2f\"")), black_box(etag)))
    });
    group.bench_function("match_in_list", |b| {
        b.iter(|| if_none_match_matches(black_box(Some(list)), black_box(etag)))
    });
    group.bench_function("wildcard", |b| {
        b.iter(|| if_none_match_matches(black_box(Some("*")), black_box(etag)))
    });
    group.bench_function("no_match", |b| {
        b.iter(|| if_none_match_matches(black_box(Some("\"other\"")), black_box(etag)))
    });
    group.bench_function("absent", |b| {
        b.iter(|| if_none_match_matches(black_box(None), black_box(etag)))
    });
    group.finish();
}

/// The origin-header relay filter, dropping hop-by-hop and self-managed framing
/// headers before forwarding to the client. Fed a realistic origin response mix
/// so the per-header classification plus the clone of what survives is measured.
fn bench_relay_headers(c: &mut Criterion) {
    let origin = hdr(&[
        ("Content-Type", "application/json"),
        ("Content-Length", "4096"),
        ("Connection", "keep-alive"),
        ("Keep-Alive", "timeout=5"),
        ("Transfer-Encoding", "chunked"),
        ("ETag", "\"9a1e2f\""),
        ("Cache-Control", "max-age=3600"),
        ("Date", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ("Server", "origin/1.2"),
        ("X-Cache-Status", "MISS"),
        ("Vary", "Accept-Encoding"),
    ]);

    c.bench_function("hit_serve/relay_headers", |b| {
        b.iter(|| relay_headers(black_box(&origin)))
    });
}

fn bench_azure_optional_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("hit_serve/azure_optional_key");
    for (name, key) in [
        ("headerless", "0|azure.test|GET|/blob||etag|"),
        (
            "version",
            "0|azure.test|GET|/blob||etag|range=0:;structured=0:;md5=0:;crc64=0:;version=10:2025-01-05;upn=0:;origin=0:",
        ),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| ovcc_proxy::azure_optional_key(black_box(key)))
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_azure_optional_key,
    bench_render_head,
    bench_if_none_match,
    bench_relay_headers
);
criterion_main!(benches);
