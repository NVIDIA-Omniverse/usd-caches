// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_cachekey::{
    BurstyKeyParts, KeyParts, STRIPPED_PARAMS, STRIPPED_PARAMS_AZURE, filter_query_args,
    filter_query_args_with, if_match_for_seed,
};

fn bench_filter_query_args(c: &mut Criterion) {
    let query = "list-type=2&prefix=foo/bar&X-Amz-Algorithm=AWS4-HMAC-SHA256\
                 &X-Amz-Credential=AKID&X-Amz-Date=20250101T000000Z&X-Amz-Expires=3600\
                 &X-Amz-SignedHeaders=host&X-Amz-Signature=deadbeef&x-id=GetObject";
    c.bench_function("filter_query_args/presigned", |b| {
        b.iter(|| filter_query_args(black_box(query)))
    });

    let azure_query = "sv=2025-11-05&ss=bf&srt=co&sp=r&sig=deadbeef\
                       &rscc=public%2C+max-age%3D60&rscd=attachment&rsce=identity\
                       &rscl=en-US&rsct=application%2Fzip";
    c.bench_function("filter_query_args/azure_overrides", |b| {
        b.iter(|| filter_query_args_with(black_box(azure_query), STRIPPED_PARAMS_AZURE))
    });
}

fn bench_render(c: &mut Criterion) {
    c.bench_function("KeyParts::render", |b| {
        b.iter(|| {
            KeyParts {
                generation: black_box("7"),
                host: black_box("bucket.s3.us-west-2.amazonaws.com"),
                method: black_box("GET"),
                uri: black_box("/path/to/object.usd"),
                filtered_args: black_box("list-type=2&prefix=foo/bar"),
                if_match: black_box("\"etag123\""),
                range: black_box(""),
            }
            .render()
        })
    });
}

fn bench_if_match_for_seed(c: &mut Criterion) {
    c.bench_function("if_match_for_seed/versioned", |b| {
        b.iter(|| if_match_for_seed(black_box("\"etag123\""), black_box(true)))
    });
    c.bench_function("if_match_for_seed/unversioned", |b| {
        b.iter(|| if_match_for_seed(black_box("\"etag123\""), black_box(false)))
    });
}

// The full per-PUT seed-key pipeline filters the (presigned/SAS) query, derives
// the If-Match segment from the origin ETag, and renders the canonical key. This
// must produce the byte-identical key the consumer GET computes.
fn bench_seed_key(c: &mut Criterion) {
    let s3_query = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Date=20250101T000000Z\
                    &X-Amz-Expires=3600&X-Amz-Signature=deadbeef&x-id=PutObject";
    c.bench_function("seed_key/s3_presigned", |b| {
        b.iter(|| {
            let filtered = filter_query_args_with(black_box(s3_query), STRIPPED_PARAMS);
            let if_match = if_match_for_seed(black_box("\"v1\""), true);
            KeyParts {
                generation: black_box("0"),
                host: black_box("bucket.s3.us-west-2.amazonaws.com"),
                method: "GET",
                uri: black_box("/parrot/42"),
                filtered_args: &filtered,
                if_match,
                range: "",
            }
            .render()
        })
    });

    let azure_query = "sp=r&ss=bf&srt=co&st=2026-06-23T00:00:00Z\
                       &se=2026-06-23T01:00:00Z&spr=https&sv=2024-11-04&sig=deadbeef";
    c.bench_function("seed_key/azure_sas", |b| {
        b.iter(|| {
            let filtered = filter_query_args_with(black_box(azure_query), STRIPPED_PARAMS_AZURE);
            let if_match = if_match_for_seed(black_box("\"v1\""), true);
            KeyParts {
                generation: black_box("0"),
                host: black_box("acct.blob.core.windows.net"),
                method: "GET",
                uri: black_box("/parrot/42"),
                filtered_args: &filtered,
                if_match,
                range: "",
            }
            .render()
        })
    });
}

fn bench_bursty_render(c: &mut Criterion) {
    c.bench_function("bursty_key/render", |b| {
        b.iter(|| {
            BurstyKeyParts {
                generation: black_box("0"),
                adaptive_id: black_box("fn-alpha|v2.0"),
                host: black_box("bucket.s3.us-west-2.amazonaws.com"),
                method: "GET",
                request_uri: black_box("/?list-type=2&prefix=foo/bar"),
            }
            .render()
        })
    });
}

fn bench_optional_expansion(c: &mut Criterion) {
    use ovcc_cachekey::{CacheKey, KeyPart, LogicalKey};
    for count in [0, 1, 4] {
        let parts = (0..count)
            .map(|i| KeyPart {
                name: format!("part{i}"),
                value: Some("value".into()),
                optional: true,
            })
            .collect();
        let key = LogicalKey::new("bench.v1".into(), parts).unwrap();
        c.bench_function(&format!("logical_key/optional_parts/{count}"), |b| {
            b.iter(|| {
                black_box(&key)
                    .physical_keys(|parts| {
                        CacheKey::from_raw(
                            parts
                                .iter()
                                .map(|(name, value)| format!("{name}={}", value.unwrap_or("")))
                                .collect::<Vec<_>>()
                                .join("|"),
                        )
                    })
                    .unwrap()
            })
        });
    }
}

criterion_group!(
    benches,
    bench_filter_query_args,
    bench_render,
    bench_if_match_for_seed,
    bench_seed_key,
    bench_bursty_render,
    bench_optional_expansion
);
criterion_main!(benches);
