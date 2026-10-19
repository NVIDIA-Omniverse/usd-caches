// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_proxy::classify_put_body;

fn hdr(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The per-PUT body-framing decision, across every branch. Pure and on the hot
/// write-through path (runs once per upload before any I/O).
fn bench_classify(c: &mut Criterion) {
    let fixed = hdr(&[
        ("Content-Type", "text/plain"),
        ("Content-Length", "1048576"),
    ]);
    let aws_chunked = hdr(&[
        ("Content-Encoding", "aws-chunked"),
        ("Content-Length", "5"),
        ("x-amz-decoded-content-length", "5"),
    ]);
    let te_chunked = hdr(&[("Transfer-Encoding", "chunked")]);
    let missing = hdr(&[("Content-Type", "text/plain")]);

    let mut group = c.benchmark_group("classify");
    group.bench_function("fixed", |b| {
        b.iter(|| classify_put_body(black_box(&fixed), black_box(false)))
    });
    group.bench_function("aws_chunked", |b| {
        b.iter(|| classify_put_body(black_box(&aws_chunked), black_box(false)))
    });
    group.bench_function("te_chunked", |b| {
        b.iter(|| classify_put_body(black_box(&te_chunked), black_box(false)))
    });
    group.bench_function("missing_length", |b| {
        b.iter(|| classify_put_body(black_box(&missing), black_box(false)))
    });
    group.finish();
}

criterion_group!(benches, bench_classify);
criterion_main!(benches);
