// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
use ovcc_telemetry::access::AccessLog;
use ovcc_telemetry::metrics::{
    HttpServerErrorType, HttpServerRequestGuard, standalone_http_server_metrics,
};
use ovcc_telemetry::redact::redact_url;

fn hit_log() -> AccessLog {
    AccessLog {
        access_time: "2026-06-28T09:00:00Z".to_owned(),
        remote_addr: "10.0.0.5".to_owned(),
        request: "GET".to_owned(),
        scheme: "https".to_owned(),
        status: 200,
        request_uri: "/assets/scene/props/table.usd".to_owned(),
        host: "bucket.s3.us-west-2.amazonaws.com".to_owned(),
        upstream_cache_status: "HIT".to_owned(),
        request_time: 0.0008,
        bytes_sent: 1_048_576,
        has_auth_header: true,
        cached_etag: Some("\"e7bb2a59d983ee85\"".to_owned()),
        upstream_etag: Some("\"e7bb2a59d983ee85\"".to_owned()),
        ..Default::default()
    }
}

fn miss_log() -> AccessLog {
    AccessLog {
        upstream: "https://bucket.s3.us-west-2.amazonaws.com".to_owned(),
        upstream_status: Some(200),
        upstream_connect_time: Some(0.004),
        upstream_header_time: Some(0.011),
        upstream_response_time: Some(0.180),
        upstream_cache_status: "MISS".to_owned(),
        cached_etag: None,
        ..hit_log()
    }
}

// Serializing the access-log record runs once per request on the serve path, so
// keep it cheap. Bench both a HIT (no upstream fields) and a MISS (full upstream
// timing set), since the omitted-field count differs.
fn bench_access_log_serialize(c: &mut Criterion) {
    let hit = hit_log();
    let miss = miss_log();
    let mut group = c.benchmark_group("access_log_serialize");
    group.bench_function("hit", |b| {
        b.iter(|| serde_json::to_string(black_box(&hit)).unwrap())
    });
    group.bench_function("miss", |b| {
        b.iter(|| serde_json::to_string(black_box(&miss)).unwrap())
    });
    group.finish();
}

// URL redaction runs per request to build span attributes from request targets
// that may carry presigned signatures.
fn bench_redact_url(c: &mut Criterion) {
    let mut group = c.benchmark_group("redact_url");
    group.bench_function("presigned", |b| {
        b.iter(|| {
            redact_url(black_box(
                "https://bucket.s3.amazonaws.com/obj.usd?X-Amz-Signature=deadbeefcafef00d&X-Amz-Date=20260628T090000Z&X-Amz-Expires=900",
            ))
        })
    });
    group.bench_function("plain_path", |b| {
        b.iter(|| redact_url(black_box("/assets/scene/props/table.usd")))
    });
    group.finish();
}

// Metric recording runs once per request in the telemetry Layer. Bench the
// disabled no-op (the cost every request pays when OTLP metrics are off — it
// must be ~free) and the enabled path (label assembly + instrument updates) via
// a standalone recorder so the measurement does not depend on process-global
// initialization.
fn bench_metrics_record(c: &mut Criterion) {
    let access = miss_log();
    let mut group = c.benchmark_group("metrics_record");

    group.bench_function("disabled", |b| {
        b.iter(|| ovcc_telemetry::metrics::record(black_box(&access)))
    });

    let recorder = ovcc_telemetry::metrics::standalone_recorder("cc_");
    group.bench_function("enabled", |b| b.iter(|| recorder(black_box(&access))));

    group.finish();
}

// The HTTP server guard runs around every routed request independently of its
// cache outcome. Measure the complete disabled, successful, and failed
// lifecycles so attribute assembly and terminal recording stay visible.
fn bench_http_server_request_guard(c: &mut Criterion) {
    let mut group = c.benchmark_group("http_server_request_guard");

    group.bench_function("disabled", |b| {
        b.iter(|| {
            HttpServerRequestGuard::start(
                black_box("GET"),
                black_box("https"),
                black_box("HTTP/1.1"),
                black_box(Some(0)),
            )
            .finish(black_box(Some(200)), black_box(1_048_576), None);
        })
    });

    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    let metrics = standalone_http_server_metrics(provider, "cc_");
    group.bench_function("enabled_success", |b| {
        b.iter(|| {
            metrics
                .start(
                    black_box("GET"),
                    black_box("https"),
                    black_box("HTTP/1.1"),
                    black_box(Some(0)),
                )
                .finish(black_box(Some(200)), black_box(1_048_576), None);
        })
    });
    group.bench_function("enabled_failure", |b| {
        b.iter(|| {
            metrics
                .start(
                    black_box("GET"),
                    black_box("https"),
                    black_box("HTTP/1.1"),
                    black_box(Some(0)),
                )
                .finish(
                    black_box(None),
                    black_box(512),
                    Some(HttpServerErrorType::DownstreamWrite),
                );
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_access_log_serialize,
    bench_redact_url,
    bench_metrics_record,
    bench_http_server_request_guard
);
criterion_main!(benches);
