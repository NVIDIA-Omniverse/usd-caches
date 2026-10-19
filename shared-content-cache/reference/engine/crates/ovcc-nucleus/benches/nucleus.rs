// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_nucleus::{Location, classify, lft_cache_key, transform_request_headers};

fn headers() -> Vec<(String, String)> {
    [
        ("host", "nucleus.mock.local"),
        ("nvcf-nca-id", "tenant-1"),
        ("omniverse-content-uid", "uid-abc-123"),
        ("omni-tenant-token", "a.jwt.token"),
        ("x-ov-uri", "omniverse://server/path"),
        ("omniverse-proxy-scheme", "https"),
        ("range", "bytes=0-16777215"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

// Path classification runs on every Nucleus request.
fn bench_classify(c: &mut Criterion) {
    c.bench_function("classify_lft", |b| {
        b.iter(|| classify(black_box("/omni/lft/some/object")))
    });
}

// The generic request-header transform (strip Omni-Tenant-Token) runs on every
// generic forward-proxy request that reaches the origin.
fn bench_transform(c: &mut Criterion) {
    let headers = headers();
    c.bench_function("transform_generic", |b| {
        b.iter(|| transform_request_headers(black_box(Location::Generic), black_box(&headers)))
    });
}

// LFT cache-key assembly runs on every LFT lookup and fill.
fn bench_lft_key(c: &mut Criterion) {
    let headers = headers();
    c.bench_function("lft_cache_key", |b| {
        b.iter(|| lft_cache_key(black_box("0"), black_box("GET"), black_box(&headers)))
    });
}

criterion_group!(benches, bench_classify, bench_transform, bench_lft_key);
criterion_main!(benches);
