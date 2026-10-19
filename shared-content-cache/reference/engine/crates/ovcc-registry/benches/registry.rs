// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_registry::{CacheableUri, Matching, RegistryConfig, RegistryUpstream};

fn config() -> RegistryConfig {
    RegistryConfig {
        matching: Matching::Any,
        suffix: "cache.svc.cluster.local".to_owned(),
        cacheable_uri: None,
        upstreams: vec![
            RegistryUpstream {
                name: "npm".to_owned(),
                hosts: vec!["registry.npmjs.org".to_owned()],
                cacheable_uri: None,
                content_rewrite: Vec::new(),
            },
            RegistryUpstream {
                name: "conan".to_owned(),
                hosts: vec!["center2.conan.io".to_owned()],
                cacheable_uri: None,
                content_rewrite: Vec::new(),
            },
            RegistryUpstream {
                name: "pip".to_owned(),
                hosts: vec!["pypi.org".to_owned(), "files.pythonhosted.org".to_owned()],
                cacheable_uri: None,
                content_rewrite: Vec::new(),
            },
        ],
    }
}

// Host -> upstream resolution runs once per request on a registry backend.
fn bench_resolve(c: &mut Criterion) {
    let cfg = config();
    let mut group = c.benchmark_group("resolve");
    // A suffixed host near the end of the upstream list (the costliest hit).
    group.bench_function("suffixed_hit", |b| {
        b.iter(|| black_box(&cfg).resolve(black_box("pypi.org.cache.svc.cluster.local")))
    });
    // A bare host hit.
    group.bench_function("bare_hit", |b| {
        b.iter(|| black_box(&cfg).resolve(black_box("registry.npmjs.org")))
    });
    // A miss scans every upstream/host before failing.
    group.bench_function("miss", |b| {
        b.iter(|| black_box(&cfg).resolve(black_box("example.com")))
    });
    group.finish();
}

fn bench_cacheable_uri(c: &mut Criterion) {
    let mut cfg = config();
    cfg.cacheable_uri = Some(CacheableUri::try_from(r"!~ ^/v2/token(?:\?|$)".to_owned()).unwrap());
    cfg.upstreams[0].cacheable_uri = Some(CacheableUri::try_from("^/v2/".to_owned()).unwrap());
    let route = cfg.resolve("registry.npmjs.org").unwrap();
    let mut group = c.benchmark_group("cacheable_uri");
    for (name, uri) in [
        (
            "excluded_token",
            "/v2/token?service=registry&scope=repository:test:pull",
        ),
        ("allowed_blob", "/v2/test/blobs/sha256:0123456789abcdef"),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| black_box(&route).is_cacheable_uri(black_box(uri)))
        });
    }
    group.finish();
}

criterion_group!(benches, bench_resolve, bench_cacheable_uri);
criterion_main!(benches);
