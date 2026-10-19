// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_bursty::{BurstyRule, adaptive_id, expires_at, is_expired, matches};

fn rules() -> Vec<BurstyRule> {
    vec![
        BurstyRule {
            method: "GET".to_owned(),
            uri_starts_with: "/?list-type=".to_owned(),
        },
        BurstyRule {
            method: "HEAD".to_owned(),
            uri_starts_with: "/".to_owned(),
        },
    ]
}

// The per-request bursty-eligibility decision (runs on every GET/HEAD when
// bursty is enabled for the backend).
fn bench_matches(c: &mut Criterion) {
    let rules = rules();
    let mut group = c.benchmark_group("matches");
    group.bench_function("list_get", |b| {
        b.iter(|| {
            matches(
                black_box(&rules),
                black_box("GET"),
                black_box("/?list-type=2&prefix=a/"),
            )
        })
    });
    group.bench_function("non_list_get", |b| {
        b.iter(|| {
            matches(
                black_box(&rules),
                black_box("GET"),
                black_box("/object.usd"),
            )
        })
    });
    group.finish();
}

// The per-tenant adaptive id, across the priority chain.
fn bench_adaptive_id(c: &mut Criterion) {
    let mut group = c.benchmark_group("adaptive_id");
    group.bench_function("fn_id_version", |b| {
        b.iter(|| {
            adaptive_id(
                black_box(Some("fn-alpha")),
                black_box(Some("v2.0")),
                black_box(None),
                black_box(None),
            )
        })
    });
    group.bench_function("nca_id", |b| {
        b.iter(|| {
            adaptive_id(
                black_box(None),
                black_box(None),
                black_box(Some("nca-tenant-1")),
                black_box(None),
            )
        })
    });
    group.bench_function("auth_hash", |b| {
        b.iter(|| {
            adaptive_id(
                black_box(None),
                black_box(None),
                black_box(None),
                black_box(Some("Bearer token-abc-123")),
            )
        })
    });
    group.finish();
}

// The TTL expiry policy is computed on every bursty fill and checked on every
// bursty hit, so it sits on the hot serve path.
fn bench_ttl(c: &mut Criterion) {
    let mut group = c.benchmark_group("ttl");
    group.bench_function("expires_at", |b| {
        b.iter(|| expires_at(black_box(300), black_box(1_700_000_000)))
    });
    group.bench_function("is_expired", |b| {
        b.iter(|| is_expired(black_box(Some(1_700_000_300)), black_box(1_700_000_000)))
    });
    group.finish();
}

criterion_group!(benches, bench_matches, bench_adaptive_id, bench_ttl);
criterion_main!(benches);
