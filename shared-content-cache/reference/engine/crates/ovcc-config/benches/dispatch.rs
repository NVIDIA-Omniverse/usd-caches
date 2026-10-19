// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regression guard for per-request dispatch: [`Backend::resolve_upstream`]
//! picks the origin for a request `Host` on every request,
//! [`Backend::registry_match`] resolves a package-manager virtual host to its
//! upstream on the registry fill path, and [`RegistryMatch::is_cacheable_uri`]
//! decides whether a registry URI may use the cache. All run before any I/O.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ovcc_config::{Backend, load};
use ovcc_registry::{CacheableUri, Matching, RegistryConfig, RegistryMatch, RegistryUpstream};

fn load_backend(values: &str) -> Backend {
    let path = std::env::temp_dir().join(format!("ovcc-config-bench-{}.yaml", std::process::id()));
    std::fs::write(&path, values).expect("writes benchmark config");
    let mut config = load(&path).expect("loads benchmark config");
    std::fs::remove_file(path).expect("removes benchmark config");
    config.backends.remove(0)
}

fn configured_backend() -> Backend {
    load_backend(
        r#"
nginx:
  backends:
    s3:
      include: true
      serverName: '~^(?<bucket>[^.]+)\.s3\.[^.]+\.amazonaws\.com$'
      proxyPass: 'https://$host'
      proxyAuthPass: 'https://$host'
      allowHttp2Clients: true
tls:
  enabled: true
cache:
  root: /tmp/ovcc-cache
"#,
    )
}

/// The expression, built-in, and exact host routing modes plus a non-match.
fn bench_resolve_upstream(c: &mut Criterion) {
    let configured = configured_backend();
    let cloud = Backend {
        name: "s3".to_owned(),
        ..Default::default()
    };
    let exact = Backend {
        name: "mock".to_owned(),
        host: "s3.mock.local".to_owned(),
        upstream: "http://127.0.0.1:9000".to_owned(),
        ..Default::default()
    };

    let mut group = c.benchmark_group("dispatch/resolve_upstream");
    group.bench_function("configured_regex", |b| {
        b.iter(|| configured.resolve_upstream(black_box("bucket.s3.us-west-2.amazonaws.com")))
    });
    group.bench_function("cloud_pattern", |b| {
        b.iter(|| cloud.resolve_upstream(black_box("bucket.s3.us-west-2.amazonaws.com")))
    });
    group.bench_function("exact_host", |b| {
        b.iter(|| exact.resolve_upstream(black_box("s3.mock.local:14128")))
    });
    group.bench_function("no_match", |b| {
        b.iter(|| exact.resolve_upstream(black_box("example.com")))
    });
    group.finish();
}

/// Registry virtual-host resolution on the fill path: a suffixed vhost mapped to
/// its real upstream, versus a host the registry does not list.
fn bench_registry_match(c: &mut Criterion) {
    let registry = Backend {
        name: "registry".to_owned(),
        registry: Some(RegistryConfig {
            matching: Matching::Suffix,
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
                    name: "pip".to_owned(),
                    hosts: vec!["pypi.org".to_owned(), "files.pythonhosted.org".to_owned()],
                    cacheable_uri: None,
                    content_rewrite: Vec::new(),
                },
            ],
        }),
        ..Default::default()
    };

    let mut group = c.benchmark_group("dispatch/registry_match");
    group.bench_function("hit", |b| {
        b.iter(|| {
            registry.registry_match(black_box("files.pythonhosted.org.cache.svc.cluster.local"))
        })
    });
    group.bench_function("miss", |b| {
        b.iter(|| registry.registry_match(black_box("example.com")))
    });
    group.finish();
}

/// Registry URI eligibility for a route without rules and for the chart's
/// example and the fixture's Artifactory rules, each against a URI its regex
/// matches and one it does not.
fn bench_cacheable_uri(c: &mut Criterion) {
    let policy = load_backend(&format!(
        "tls:\n  enabled: false\ncache:\n  root: /tmp/ovcc-cache\n{}",
        include_str!("../../../fixtures/registry-cacheable-uri.values.yaml")
    ));
    let unrestricted = policy.registry_match("pypi.org").expect("pip route");
    let complex = policy
        .registry_match("docker.artifactory.example.com")
        .expect("Artifactory route");
    let simple_rule =
        CacheableUri::try_from(r"!~ ^/v2/token(?:\?|$)".to_owned()).expect("example rule compiles");
    let simple = RegistryMatch {
        upstream_url: String::new(),
        content_rewrite: &[],
        cacheable_uri: Some(&simple_rule),
        upstream_cacheable_uri: None,
    };
    // A query-less blob GET with a full digest keeps the Artifactory rule
    // scanning to its last byte, the cost every cacheable blob request pays.
    let blob = concat!(
        "/v2/team/image/blobs/sha256:",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );

    let mut group = c.benchmark_group("dispatch/cacheable_uri");
    for (name, route, uri, cacheable) in [
        ("no_rule", &unrestricted, "/simple/requests/", true),
        (
            "simple_match",
            &simple,
            "/v2/token?service=registry&scope=repository:team/image:pull",
            false,
        ),
        ("simple_no_match", &simple, blob, true),
        (
            "complex_match",
            &complex,
            "/artifactory/api/security/token?expires_in=3600",
            false,
        ),
        ("complex_no_match", &complex, blob, true),
    ] {
        assert_eq!(route.is_cacheable_uri(uri), cacheable, "{name}");
        group.bench_function(name, |b| b.iter(|| route.is_cacheable_uri(black_box(uri))));
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_resolve_upstream,
    bench_registry_match,
    bench_cacheable_uri
);
criterion_main!(benches);
