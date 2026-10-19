// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The metrics pillar, an env-gated OTLP metrics exporter plus the instrument
//! set the Grafana/GCM dashboards consume, recorded once per request from the
//! flat [`AccessLog`] metadata.
//!
//! # Metadata only
//!
//! [`record`] reads the already-assembled [`AccessLog`] — host, method, status,
//! cache status, upstream timings, bytes sent, the mutation flag — and never the
//! response body. The kTLS `sendfile` HIT body stays a leased fd the proxy
//! driver sends directly to the socket; nothing here observes or routes it.
//!
//! # Bounded, secret-free labels
//!
//! Request attributes use `http_host` / `request_method` / `cache_status` /
//! `http_status`; peer attributes use enum-backed tier, stage, outcome, and
//! result labels. Request URIs, keys, peer addresses, validators, query strings,
//! signatures, and credentials are never attached.
//!
//! # Names and prefix
//!
//! Instruments are named for the exact Prometheus series the dashboards query
//! (`<prefix>http_requests_total`, `<prefix>request_duration_seconds`, ...). The
//! prefix is `cc_` by default and `ucc_` under chart UCC-compat, so the exported
//! series match whichever dashboards a deployment runs. The names already carry
//! their `_total`/`_seconds`/`_bytes` suffix and no instrument unit is set, so a
//! collector exporting to Prometheus with `add_metric_suffixes: false` preserves
//! them verbatim (see the crate `AGENTS.md` for the full mapping).
//!
//! # Env-gated
//!
//! Export is enabled on exactly the same terms as span export (see
//! [`otlp_metrics_enabled`]); a run with no collector records nothing and makes
//! no network calls.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, MeterProvider as _, UpDownCounter};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_semantic_conventions::attribute::{
    ERROR_TYPE, HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, NETWORK_PROTOCOL_VERSION,
    URL_SCHEME,
};
use opentelemetry_semantic_conventions::metric::{
    HTTP_SERVER_ACTIVE_REQUESTS, HTTP_SERVER_REQUEST_BODY_SIZE, HTTP_SERVER_REQUEST_DURATION,
    HTTP_SERVER_RESPONSE_BODY_SIZE,
};
use ovcc_core::{Error, Result};

use crate::TelemetryConfig;
use crate::access::AccessLog;
use crate::{env_is_false, env_is_true};

/// Latency histogram boundaries used by the shipped dashboards.
const TIME_BUCKETS: &[f64] = &[
    0.001, 0.002, 0.003, 0.005, 0.01, 0.02, 0.03, 0.05, 0.075, 0.1, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0,
    1.5, 2.0, 3.0, 4.0, 5.0, 10.0,
];

/// OpenTelemetry's advisory explicit boundaries for HTTP server duration.
const HTTP_SERVER_DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// Size histogram bucket boundaries (bytes) for the response-size histogram.
const BYTE_BUCKETS: &[f64] = &[
    1000.0,
    2000.0,
    4000.0,
    8000.0,
    16000.0,
    32000.0,
    64000.0,
    128000.0,
    256000.0,
    1_048_576.0,
    10_485_760.0,
    104_857_600.0,
    1_000_000_000.0,
    10_000_000_000.0,
    100_000_000_000.0,
];
const SEGMENT_BUCKETS: &[f64] = &[1.0, 16.0, 256.0, 1_969.0, 10_000.0, 50_000.0];

/// Long-running write-through phase buckets. Assembly and cache seeding move an
/// entire object and routinely exceed the request/upstream histogram's 10s cap.
const LONG_TIME_BUCKETS: &[f64] = &[
    0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 180.0, 300.0, 600.0,
];
const PEER_FILL_TIME_BUCKETS: &[f64] = &[
    0.001, 0.002, 0.003, 0.005, 0.01, 0.02, 0.03, 0.05, 0.075, 0.1, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0,
    1.5, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 180.0, 300.0, 600.0,
];

#[derive(Default)]
struct PullHistogram {
    buckets: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct PullState {
    counters: BTreeMap<&'static str, BTreeMap<Vec<String>, u64>>,
    gauges: BTreeMap<&'static str, BTreeMap<Vec<String>, i64>>,
    histograms: BTreeMap<&'static str, BTreeMap<Vec<String>, PullHistogram>>,
}

/// Native Prometheus accumulator for the core request metric families.
///
/// It consumes only [`AccessLog`] metadata, independently of whether OTLP is
/// enabled. Query strings, request URIs, and authorization values never enter
/// its bounded label set.
pub struct Prometheus {
    prefix: String,
    state: Mutex<PullState>,
    peer_probe_in_flight: AtomicI64,
    peer_fill_followers_active: AtomicI64,
    peer_serves_active: AtomicI64,
}

impl Prometheus {
    /// Creates an empty accumulator using the deployment's `cc_`/`ucc_` prefix.
    pub fn new(prefix: impl Into<String>) -> Self {
        let this = Self {
            prefix: prefix.into(),
            state: Mutex::new(PullState::default()),
            peer_probe_in_flight: AtomicI64::new(0),
            peer_fill_followers_active: AtomicI64::new(0),
            peer_serves_active: AtomicI64::new(0),
        };
        add_counter(
            &mut this.state.lock().expect("Prometheus metrics poisoned"),
            "metric_poll_total",
            Vec::new(),
            0,
        );
        this
    }

    fn record(&self, access: &AccessLog) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        let request_labels = vec![
            access.host.clone(),
            access.request.clone(),
            access.upstream_cache_status.clone(),
            access.status.to_string(),
        ];
        add_counter(&mut state, "http_requests_total", request_labels.clone(), 1);
        if let Some(tier) = RequestTier::from_access(access) {
            add_counter(
                &mut state,
                "request_tier_total",
                vec![tier.as_str().to_owned()],
                1,
            );
        }
        observe(
            &mut state,
            "request_duration_seconds",
            vec![access.host.clone()],
            access.request_time,
            TIME_BUCKETS,
        );
        observe(
            &mut state,
            "response_size_bytes",
            vec![access.host.clone()],
            access.bytes_sent as f64,
            BYTE_BUCKETS,
        );
        if access.upstream_status.is_some() {
            add_counter(
                &mut state,
                "http_upstream_requests_total",
                request_labels,
                1,
            );
            for (name, value) in [
                (
                    "upstream_connect_time_seconds",
                    access.upstream_connect_time,
                ),
                ("upstream_header_time_seconds", access.upstream_header_time),
                (
                    "upstream_response_time_seconds",
                    access.upstream_response_time,
                ),
            ] {
                if let Some(value) = value {
                    observe(
                        &mut state,
                        name,
                        vec![access.host.clone()],
                        value,
                        TIME_BUCKETS,
                    );
                }
            }
        }
        if access.mutation_detected {
            add_counter(
                &mut state,
                "mutation_detection_purges_total",
                vec![access.host.clone()],
                1,
            );
        }
    }

    fn add_gauge(&self, name: &'static str, labels: Vec<String>, delta: i64) {
        add_gauge(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            name,
            labels,
            delta,
        );
    }

    fn peer_probe_in_flight_delta(&self, delta: i64) {
        self.peer_probe_in_flight
            .fetch_add(delta, Ordering::Relaxed);
    }

    fn peer_fill_followers_active_delta(&self, delta: i64) {
        self.peer_fill_followers_active
            .fetch_add(delta, Ordering::Relaxed);
    }

    fn peer_serves_active_delta(&self, delta: i64) {
        self.peer_serves_active.fetch_add(delta, Ordering::Relaxed);
    }

    fn record_access_probe(&self, event: &AccessProbe<'_>) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        let request_labels = vec![event.host.to_owned(), event.method.to_owned()];
        add_counter(
            &mut state,
            "access_requests_total",
            request_labels.clone(),
            1,
        );
        observe(
            &mut state,
            "access_upstream_request_seconds",
            vec![event.host.to_owned()],
            event.duration_seconds,
            TIME_BUCKETS,
        );
        if let Some(status) = event.status {
            let mut response_labels = request_labels;
            response_labels.push(status.to_string());
            add_counter(&mut state, "access_responses_total", response_labels, 1);
        }
    }

    fn record_write_through_result(&self, host: &str, result: WriteThroughResult) {
        add_counter(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "write_through_requests_total",
            vec![host.to_owned(), result.as_str().to_owned()],
            1,
        );
    }

    fn record_write_through_bytes(&self, host: &str, bytes: u64) {
        observe(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "write_through_upload_bytes",
            vec![host.to_owned()],
            bytes as f64,
            BYTE_BUCKETS,
        );
    }

    fn record_write_through_segments(&self, host: &str, segments: usize) {
        observe(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "write_through_segments",
            vec![host.to_owned()],
            segments as f64,
            SEGMENT_BUCKETS,
        );
    }

    fn record_write_through_duration(
        &self,
        host: &str,
        phase: WriteThroughPhase,
        duration_seconds: f64,
    ) {
        let (name, buckets) = phase.family();
        observe(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            name,
            vec![host.to_owned()],
            duration_seconds,
            buckets,
        );
    }

    fn record_peer_probe(&self, result: PeerResult, duration_seconds: f64) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        let labels = vec![result.as_str().to_owned()];
        add_counter(&mut state, "peer_probe_total", labels.clone(), 1);
        observe(
            &mut state,
            "peer_probe_seconds",
            labels,
            duration_seconds,
            TIME_BUCKETS,
        );
    }

    fn record_peer_attempt(
        &self,
        stage: PeerAttemptStage,
        outcome: PeerAttemptOutcome,
        duration_seconds: f64,
    ) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        let labels = vec![stage.as_str().to_owned(), outcome.as_str().to_owned()];
        add_counter(&mut state, "peer_attempt_total", labels.clone(), 1);
        observe(
            &mut state,
            "peer_attempt_seconds",
            labels,
            duration_seconds,
            TIME_BUCKETS,
        );
    }

    fn record_peer_probe_cancellations(&self, count: u64) {
        add_counter(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "peer_probe_cancellations_total",
            Vec::new(),
            count,
        );
    }

    fn record_peer_fill(&self, result: PeerResult, bytes: u64, duration_seconds: f64) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        add_counter(
            &mut state,
            "peer_fill_total",
            vec![result.as_str().to_owned()],
            1,
        );
        add_counter(&mut state, "peer_fill_bytes_total", Vec::new(), bytes);
        observe(
            &mut state,
            "peer_fill_seconds",
            Vec::new(),
            duration_seconds,
            PEER_FILL_TIME_BUCKETS,
        );
    }

    fn record_peer_fill_follower_terminal(&self, result: PeerFillFollowerResult) {
        add_counter(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "peer_fill_follower_requests_total",
            vec![result.as_str().to_owned()],
            1,
        );
    }

    fn record_peer_fill_follower_bytes(&self, bytes: u64) {
        add_counter(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "peer_fill_follower_bytes_total",
            Vec::new(),
            bytes,
        );
    }

    fn record_peer_serve(
        &self,
        result: PeerServeResult,
        bytes: u64,
        socket_writable_wait_seconds: f64,
    ) {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        add_counter(
            &mut state,
            "peer_serve_total",
            vec![result.as_str().to_owned()],
            1,
        );
        add_counter(&mut state, "peer_serve_bytes_total", Vec::new(), bytes);
        observe(
            &mut state,
            "peer_serve_socket_writable_wait_seconds",
            Vec::new(),
            socket_writable_wait_seconds,
            TIME_BUCKETS,
        );
    }

    fn record_downstream_close(&self, result: DownstreamCloseResult) {
        add_counter(
            &mut self.state.lock().expect("Prometheus metrics poisoned"),
            "downstream_close_total",
            vec![result.as_str().to_owned()],
            1,
        );
    }

    fn record_peer_directory(&self, peers: usize) {
        self.state
            .lock()
            .expect("Prometheus metrics poisoned")
            .gauges
            .entry("peer_directory_peers")
            .or_default()
            .insert(Vec::new(), i64::try_from(peers).unwrap_or(i64::MAX));
    }

    /// Renders Prometheus text after incrementing the canonical poll counter.
    pub fn scrape(&self) -> String {
        let mut state = self.state.lock().expect("Prometheus metrics poisoned");
        add_counter(&mut state, "metric_poll_total", Vec::new(), 1);
        state
            .gauges
            .entry("peer_probe_in_flight")
            .or_default()
            .insert(
                Vec::new(),
                self.peer_probe_in_flight.load(Ordering::Relaxed),
            );
        state
            .gauges
            .entry("peer_fill_followers_active")
            .or_default()
            .insert(
                Vec::new(),
                self.peer_fill_followers_active.load(Ordering::Relaxed),
            );
        state
            .gauges
            .entry("peer_serves_active")
            .or_default()
            .insert(Vec::new(), self.peer_serves_active.load(Ordering::Relaxed));
        render(&self.prefix, &state)
    }
}

static PROMETHEUS: OnceLock<Arc<Prometheus>> = OnceLock::new();

/// Installs or returns the process Prometheus accumulator.
///
/// A process has one configured metric prefix, matching one OVCC deployment.
pub fn prometheus(prefix: &str) -> Arc<Prometheus> {
    Arc::clone(PROMETHEUS.get_or_init(|| Arc::new(Prometheus::new(prefix))))
}

fn add_counter(state: &mut PullState, name: &'static str, labels: Vec<String>, amount: u64) {
    *state
        .counters
        .entry(name)
        .or_default()
        .entry(labels)
        .or_default() += amount;
}

fn add_gauge(state: &mut PullState, name: &'static str, labels: Vec<String>, delta: i64) {
    *state
        .gauges
        .entry(name)
        .or_default()
        .entry(labels)
        .or_default() += delta;
}

fn observe(
    state: &mut PullState,
    name: &'static str,
    labels: Vec<String>,
    value: f64,
    boundaries: &[f64],
) {
    let histogram = state
        .histograms
        .entry(name)
        .or_default()
        .entry(labels)
        .or_insert_with(|| PullHistogram {
            buckets: vec![0; boundaries.len() + 1],
            ..PullHistogram::default()
        });
    let index = boundaries
        .iter()
        .position(|boundary| value <= *boundary)
        .unwrap_or(boundaries.len());
    histogram.buckets[index] += 1;
    histogram.sum += value;
    histogram.count += 1;
}

struct Family {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
    labels: &'static [&'static str],
    buckets: &'static [f64],
}

const FAMILIES: &[Family] = &[
    Family {
        name: "http_requests_total",
        help: "Count of requests with additional labels",
        kind: "counter",
        labels: &["http_host", "request_method", "cache_status", "http_status"],
        buckets: &[],
    },
    Family {
        name: "http_upstream_requests_total",
        help: "Count of upstream requests with additional labels",
        kind: "counter",
        labels: &["http_host", "request_method", "cache_status", "http_status"],
        buckets: &[],
    },
    Family {
        name: "request_tier_total",
        help: "Cacheable MISS requests by bounded source tier.",
        kind: "counter",
        labels: &["cache_tier"],
        buckets: &[],
    },
    Family {
        name: "mutation_detection_purges_total",
        help: "Cache entries purged due to stale Last-Modified",
        kind: "counter",
        labels: &["http_host"],
        buckets: &[],
    },
    Family {
        name: "metric_poll_total",
        help: "Total number of requests to the http metrics endpoint.",
        kind: "counter",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "access_requests_total",
        help: "Total number of upstream access probes",
        kind: "counter",
        labels: &["http_host", "http_method"],
        buckets: &[],
    },
    Family {
        name: "access_responses_total",
        help: "Total number of responses from upstream access probes",
        kind: "counter",
        labels: &["http_host", "http_method", "http_status"],
        buckets: &[],
    },
    Family {
        name: "write_through_requests_total",
        help: "Write-through PUT attempts by final result.",
        kind: "counter",
        labels: &["http_host", "result"],
        buckets: &[],
    },
    Family {
        name: "peer_probe_total",
        help: "Peer availability probes by bounded result.",
        kind: "counter",
        labels: &["result"],
        buckets: &[],
    },
    Family {
        name: "peer_attempt_total",
        help: "Peer attempts by terminal stage and bounded outcome.",
        kind: "counter",
        labels: &["stage", "outcome"],
        buckets: &[],
    },
    Family {
        name: "peer_probe_cancellations_total",
        help: "Remaining peer probe tasks cancelled after winner selection.",
        kind: "counter",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_fill_total",
        help: "Peer cache fills by bounded result.",
        kind: "counter",
        labels: &["result"],
        buckets: &[],
    },
    Family {
        name: "peer_fill_bytes_total",
        help: "Bytes received through peer cache fills.",
        kind: "counter",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_fill_follower_requests_total",
        help: "Same-replica peer-fill follower requests by bounded terminal result.",
        kind: "counter",
        labels: &["result"],
        buckets: &[],
    },
    Family {
        name: "peer_fill_follower_bytes_total",
        help: "Bytes sent from in-progress peer-fill staging files.",
        kind: "counter",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_serve_total",
        help: "Peer body requests by bounded terminal result.",
        kind: "counter",
        labels: &["result"],
        buckets: &[],
    },
    Family {
        name: "peer_serve_bytes_total",
        help: "Peer body bytes written to requesting sockets.",
        kind: "counter",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "downstream_close_total",
        help: "Proxy-initiated client connection closes after a response, by bounded result.",
        kind: "counter",
        labels: &["result"],
        buckets: &[],
    },
    Family {
        name: "nginx_http_connections",
        help: "Number of HTTP connections",
        kind: "gauge",
        labels: &["state"],
        buckets: &[],
    },
    Family {
        name: "write_through_active_uploads",
        help: "Current in-flight write-through uploads.",
        kind: "gauge",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_directory_peers",
        help: "Current peers in the refreshed directory.",
        kind: "gauge",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_probe_in_flight",
        help: "Current in-flight peer probe tasks.",
        kind: "gauge",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_fill_followers_active",
        help: "Current same-replica peer-fill follower requests.",
        kind: "gauge",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "peer_serves_active",
        help: "Current peer body serves.",
        kind: "gauge",
        labels: &[],
        buckets: &[],
    },
    Family {
        name: "request_duration_seconds",
        help: "HTTP request latency",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "peer_serve_socket_writable_wait_seconds",
        help: "Socket-writability wait per peer body serve.",
        kind: "histogram",
        labels: &[],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "upstream_connect_time_seconds",
        help: "Time spent connecting to upstream services",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "upstream_header_time_seconds",
        help: "Time to read headers from upstream",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "upstream_response_time_seconds",
        help: "Time to receive all bytes from upstream",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "response_size_bytes",
        help: "Size of HTTP responses",
        kind: "histogram",
        labels: &["http_host"],
        buckets: BYTE_BUCKETS,
    },
    Family {
        name: "access_upstream_request_seconds",
        help: "Time spent performing upstream access probes.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "write_through_upload_bytes",
        help: "Size of completed write-through upload bodies.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: BYTE_BUCKETS,
    },
    Family {
        name: "peer_fill_seconds",
        help: "Peer cache-fill transfer latency in seconds.",
        kind: "histogram",
        labels: &[],
        buckets: PEER_FILL_TIME_BUCKETS,
    },
    Family {
        name: "peer_probe_seconds",
        help: "Completed peer probe latency in seconds.",
        kind: "histogram",
        labels: &["result"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "peer_attempt_seconds",
        help: "Peer attempt latency by terminal stage and bounded outcome.",
        kind: "histogram",
        labels: &["stage", "outcome"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "write_through_upload_seconds",
        help: "Time streaming a client body to storage and staging.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
    Family {
        name: "write_through_segments",
        help: "Logical segment count per accepted Azure block-list publication.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: SEGMENT_BUCKETS,
    },
    Family {
        name: "write_through_seed_seconds",
        help: "Time seeding a completed upload into the cache.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: LONG_TIME_BUCKETS,
    },
    Family {
        name: "write_through_adopt_seconds",
        help: "Time assembling staged Azure blocks.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: LONG_TIME_BUCKETS,
    },
    Family {
        name: "write_through_blocklist_seconds",
        help: "Total time handling an Azure block-list commit.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: LONG_TIME_BUCKETS,
    },
    Family {
        name: "write_through_block_seconds",
        help: "Time streaming one Azure block to storage and staging.",
        kind: "histogram",
        labels: &["http_host"],
        buckets: TIME_BUCKETS,
    },
];

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn render_labels(names: &[&str], values: &[String], le: Option<String>) -> String {
    let mut pairs: Vec<String> = names
        .iter()
        .zip(values)
        .map(|(name, value)| format!("{name}=\"{}\"", escape_label(value)))
        .collect();
    if let Some(le) = le {
        pairs.push(format!("le=\"{}\"", escape_label(&le)));
    }
    if pairs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", pairs.join(","))
    }
}

fn render(prefix: &str, state: &PullState) -> String {
    let mut output = String::new();
    for family in FAMILIES {
        let name = format!("{prefix}{}", family.name);
        let help = family.help.replace('\\', "\\\\").replace('\n', "\\n");
        let _ = writeln!(output, "# HELP {name} {help}");
        let _ = writeln!(output, "# TYPE {name} {}", family.kind);
        if family.kind == "counter" {
            if let Some(series) = state.counters.get(family.name) {
                for (values, value) in series {
                    let _ = writeln!(
                        output,
                        "{name}{} {value}",
                        render_labels(family.labels, values, None)
                    );
                }
            }
        } else if family.kind == "gauge" {
            if let Some(series) = state.gauges.get(family.name) {
                for (values, value) in series {
                    let _ = writeln!(
                        output,
                        "{name}{} {value}",
                        render_labels(family.labels, values, None)
                    );
                }
            }
        } else if let Some(series) = state.histograms.get(family.name) {
            for (values, histogram) in series {
                let mut cumulative = 0;
                for (index, count) in histogram.buckets.iter().enumerate() {
                    cumulative += count;
                    let le = family
                        .buckets
                        .get(index)
                        .map_or_else(|| "+Inf".to_owned(), ToString::to_string);
                    let _ = writeln!(
                        output,
                        "{name}_bucket{} {cumulative}",
                        render_labels(family.labels, values, Some(le))
                    );
                }
                let labels = render_labels(family.labels, values, None);
                let _ = writeln!(output, "{name}_sum{labels} {}", histogram.sum);
                let _ = writeln!(output, "{name}_count{labels} {}", histogram.count);
            }
        }
    }
    output
}

/// The dashboard-consumed instruments, built once when export is enabled. Only
/// the families the shipped Grafana/GCM dashboards actually query are emitted;
/// the rest are documented in the crate `AGENTS.md` as intentionally skipped.
struct Metrics {
    /// Official HTTP server request duration in seconds.
    http_server_request_duration: Histogram<f64>,
    /// Official active HTTP server requests.
    http_server_active_requests: UpDownCounter<i64>,
    /// Official HTTP server request payload size in bytes.
    http_server_request_body_size: Histogram<u64>,
    /// Official HTTP server response payload size in bytes.
    http_server_response_body_size: Histogram<u64>,
    /// `<prefix>http_requests_total{http_host,request_method,cache_status,http_status}`.
    requests_total: Counter<u64>,
    /// `<prefix>http_upstream_requests_total{...}` — only when an origin was contacted.
    upstream_requests_total: Counter<u64>,
    /// `<prefix>request_tier_total{cache_tier}`.
    request_tier_total: Counter<u64>,
    /// `<prefix>mutation_detection_purges_total{http_host}`.
    mutation_purges_total: Counter<u64>,
    /// `<prefix>request_duration_seconds{http_host}`.
    request_duration_seconds: Histogram<f64>,
    /// `<prefix>upstream_connect_time_seconds{http_host}`.
    upstream_connect_seconds: Histogram<f64>,
    /// `<prefix>upstream_header_time_seconds{http_host}`.
    upstream_header_seconds: Histogram<f64>,
    /// `<prefix>upstream_response_time_seconds{http_host}`.
    upstream_response_seconds: Histogram<f64>,
    /// `<prefix>response_size_bytes{http_host}`.
    response_size_bytes: Histogram<f64>,
    /// `<prefix>nginx_http_connections{state}`.
    connections: UpDownCounter<i64>,
    /// `<prefix>access_requests_total{http_host,http_method}`.
    access_requests_total: Counter<u64>,
    /// `<prefix>access_responses_total{http_host,http_method,http_status}`.
    access_responses_total: Counter<u64>,
    /// `<prefix>access_upstream_request_seconds{http_host}`.
    access_upstream_request_seconds: Histogram<f64>,
    /// `<prefix>write_through_requests_total{http_host,result}`.
    write_through_requests_total: Counter<u64>,
    /// `<prefix>write_through_active_uploads`.
    write_through_active_uploads: UpDownCounter<i64>,
    /// `<prefix>write_through_upload_bytes{http_host}`.
    write_through_upload_bytes: Histogram<f64>,
    /// `<prefix>write_through_segments{http_host}`.
    write_through_segments: Histogram<f64>,
    /// Write-through phase histograms, indexed by [`WriteThroughPhase`].
    write_through_upload_seconds: Histogram<f64>,
    write_through_seed_seconds: Histogram<f64>,
    write_through_adopt_seconds: Histogram<f64>,
    write_through_blocklist_seconds: Histogram<f64>,
    write_through_block_seconds: Histogram<f64>,
    /// `<prefix>peer_probe_total{result}`.
    peer_probe_total: Counter<u64>,
    /// `<prefix>peer_probe_seconds{result}`.
    peer_probe_seconds: Histogram<f64>,
    /// `<prefix>peer_probe_in_flight`.
    peer_probe_in_flight: UpDownCounter<i64>,
    /// `<prefix>peer_probe_cancellations_total`.
    peer_probe_cancellations_total: Counter<u64>,
    /// `<prefix>peer_attempt_total{stage,outcome}`.
    peer_attempt_total: Counter<u64>,
    /// `<prefix>peer_attempt_seconds{stage,outcome}`.
    peer_attempt_seconds: Histogram<f64>,
    /// `<prefix>peer_fill_total{result}`.
    peer_fill_total: Counter<u64>,
    /// `<prefix>peer_fill_bytes_total`.
    peer_fill_bytes_total: Counter<u64>,
    /// `<prefix>peer_fill_seconds`.
    peer_fill_seconds: Histogram<f64>,
    /// `<prefix>peer_fill_follower_requests_total{result}`.
    peer_fill_follower_requests_total: Counter<u64>,
    /// `<prefix>peer_fill_follower_bytes_total`.
    peer_fill_follower_bytes_total: Counter<u64>,
    /// `<prefix>peer_fill_followers_active`.
    peer_fill_followers_active: UpDownCounter<i64>,
    /// `<prefix>peer_serve_total{result}`.
    peer_serve_total: Counter<u64>,
    /// `<prefix>peer_serve_bytes_total`.
    peer_serve_bytes_total: Counter<u64>,
    /// `<prefix>peer_serves_active`.
    peer_serves_active: UpDownCounter<i64>,
    /// `<prefix>peer_serve_socket_writable_wait_seconds`.
    peer_serve_socket_writable_wait_seconds: Histogram<f64>,
    /// `<prefix>peer_directory_peers`.
    peer_directory_peers: Gauge<u64>,
    /// `<prefix>downstream_close_total{result}`.
    downstream_close_total: Counter<u64>,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Whether OTLP metric export should be enabled. Same precedence as spans:
/// `OTEL_SDK_DISABLED` / `OVCC_OTLP=false` force off; `OVCC_OTLP=true` forces
/// on; otherwise on when any OTLP endpoint is configured (metrics-specific env,
/// general env, or the config endpoint).
pub fn otlp_metrics_enabled(config: &TelemetryConfig) -> bool {
    if env_is_true("OTEL_SDK_DISABLED") || env_is_false("OVCC_OTLP") {
        return false;
    }
    if env_is_true("OVCC_OTLP") {
        return true;
    }
    std::env::var_os("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || !config.otlp.endpoint.is_empty()
}

/// Builds the OTLP/HTTP-protobuf metric exporter and a periodic-reader meter
/// provider with the `service.name` / `service.version` resource. The endpoint
/// comes from `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT`
/// when set, else from `config.otlp.endpoint` (env wins, matching the rest).
pub fn build_meter_provider(config: &TelemetryConfig) -> Result<SdkMeterProvider> {
    use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};

    let client = crate::otlp_client::OtlpHttpClient::new(crate::otlp_client::export_timeout(
        opentelemetry_otlp::OTEL_EXPORTER_OTLP_METRICS_TIMEOUT,
    ))
    .map_err(|err| Error::internal(format!("OTLP metric client init failed: {err}")))?;
    let mut builder = opentelemetry_otlp::MetricExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary);
    let env_endpoint = std::env::var_os("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT")
        .or_else(|| std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .is_some();
    if !env_endpoint && !config.otlp.endpoint.is_empty() {
        builder = builder.with_endpoint(config.otlp.endpoint.clone());
    }
    let exporter = builder
        .build()
        .map_err(|err| Error::internal(format!("OTLP metric exporter init failed: {err}")))?;

    let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();

    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| config.service_name.clone());
    let service_version = std::env::var("OVCC_SERVICE_VERSION")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| config.service_version.clone());
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            service_version,
        ))
        .build();

    Ok(SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader)
        .build())
}

/// Installs the meter provider (when enabled) and builds the instrument set so
/// later [`record`] calls are cheap lookups. Returns the provider so the
/// telemetry guard can flush + shut it down at exit; returns `None` (a no-op)
/// when metric export is disabled.
pub(crate) fn init_metrics(config: &TelemetryConfig) -> Result<Option<SdkMeterProvider>> {
    if !otlp_metrics_enabled(config) {
        return Ok(None);
    }
    let provider = build_meter_provider(config)?;
    opentelemetry::global::set_meter_provider(provider.clone());
    let meter = provider.meter("ovcc");
    // `set` fails only if a second init raced us; either set of instruments is
    // equivalent, so ignore the loser.
    let _ = METRICS.set(build_instruments(&meter, &config.metric_prefix));
    Ok(Some(provider))
}

/// Builds the dashboard instruments under `prefix`. Histograms carry the
/// dashboards' explicit bucket boundaries; counters and histograms are named
/// for the exact Prometheus series, with no unit so the names survive export.
fn build_instruments(meter: &Meter, prefix: &str) -> Metrics {
    Metrics {
        http_server_request_duration: meter
            .f64_histogram(HTTP_SERVER_REQUEST_DURATION)
            .with_description("Duration of HTTP server requests.")
            .with_unit("s")
            .with_boundaries(HTTP_SERVER_DURATION_BUCKETS.to_vec())
            .build(),
        http_server_active_requests: meter
            .i64_up_down_counter(HTTP_SERVER_ACTIVE_REQUESTS)
            .with_description("Number of active HTTP server requests.")
            .with_unit("{request}")
            .build(),
        http_server_request_body_size: meter
            .u64_histogram(HTTP_SERVER_REQUEST_BODY_SIZE)
            .with_description("Size of HTTP server request bodies.")
            .with_unit("By")
            .with_boundaries(BYTE_BUCKETS.to_vec())
            .build(),
        http_server_response_body_size: meter
            .u64_histogram(HTTP_SERVER_RESPONSE_BODY_SIZE)
            .with_description("Size of HTTP server response bodies.")
            .with_unit("By")
            .with_boundaries(BYTE_BUCKETS.to_vec())
            .build(),
        requests_total: meter
            .u64_counter(format!("{prefix}http_requests_total"))
            .with_description("Client requests, by host, method, cache status, and HTTP status.")
            .build(),
        upstream_requests_total: meter
            .u64_counter(format!("{prefix}http_upstream_requests_total"))
            .with_description("Requests that contacted an origin, by host, method, cache status, and HTTP status.")
            .build(),
        request_tier_total: meter
            .u64_counter(format!("{prefix}request_tier_total"))
            .with_description("Cacheable MISS requests by bounded source tier.")
            .build(),
        mutation_purges_total: meter
            .u64_counter(format!("{prefix}mutation_detection_purges_total"))
            .with_description("Cache entries purged after an origin mutation was detected.")
            .build(),
        request_duration_seconds: meter
            .f64_histogram(format!("{prefix}request_duration_seconds"))
            .with_description("End-to-end request wall-clock time in seconds.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        upstream_connect_seconds: meter
            .f64_histogram(format!("{prefix}upstream_connect_time_seconds"))
            .with_description("Origin TCP/TLS connect time in seconds (fresh connections only).")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        upstream_header_seconds: meter
            .f64_histogram(format!("{prefix}upstream_header_time_seconds"))
            .with_description("Origin time-to-first-byte (header) time in seconds.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        upstream_response_seconds: meter
            .f64_histogram(format!("{prefix}upstream_response_time_seconds"))
            .with_description("Origin full-response time in seconds.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        response_size_bytes: meter
            .f64_histogram(format!("{prefix}response_size_bytes"))
            .with_description("Response body bytes sent to the client.")
            .with_boundaries(BYTE_BUCKETS.to_vec())
            .build(),
        connections: meter
            .i64_up_down_counter(format!("{prefix}nginx_http_connections"))
            .with_description("Current client connections by proxy state.")
            .build(),
        access_requests_total: meter
            .u64_counter(format!("{prefix}access_requests_total"))
            .with_description("Upstream access probes by host and method.")
            .build(),
        access_responses_total: meter
            .u64_counter(format!("{prefix}access_responses_total"))
            .with_description("Completed upstream access probes by host, method, and status.")
            .build(),
        access_upstream_request_seconds: meter
            .f64_histogram(format!("{prefix}access_upstream_request_seconds"))
            .with_description("Upstream access probe latency in seconds.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        write_through_requests_total: meter
            .u64_counter(format!("{prefix}write_through_requests_total"))
            .with_description("Write-through PUT attempts by final result.")
            .build(),
        write_through_active_uploads: meter
            .i64_up_down_counter(format!("{prefix}write_through_active_uploads"))
            .with_description("Current in-flight write-through uploads.")
            .build(),
        write_through_upload_bytes: meter
            .f64_histogram(format!("{prefix}write_through_upload_bytes"))
            .with_description("Completed write-through upload body bytes.")
            .with_boundaries(BYTE_BUCKETS.to_vec())
            .build(),
        write_through_segments: meter
            .f64_histogram(format!("{prefix}write_through_segments"))
            .with_description("Logical segment count per Azure block-list publication.")
            .with_boundaries(SEGMENT_BUCKETS.to_vec())
            .build(),
        write_through_upload_seconds: write_through_histogram(
            meter,
            prefix,
            WriteThroughPhase::Upload,
        ),
        write_through_seed_seconds: write_through_histogram(
            meter,
            prefix,
            WriteThroughPhase::Seed,
        ),
        write_through_adopt_seconds: write_through_histogram(
            meter,
            prefix,
            WriteThroughPhase::Adopt,
        ),
        write_through_blocklist_seconds: write_through_histogram(
            meter,
            prefix,
            WriteThroughPhase::BlockList,
        ),
        write_through_block_seconds: write_through_histogram(
            meter,
            prefix,
            WriteThroughPhase::Block,
        ),
        peer_probe_total: meter
            .u64_counter(format!("{prefix}peer_probe_total"))
            .with_description("Peer availability probes by bounded result.")
            .build(),
        peer_probe_seconds: meter
            .f64_histogram(format!("{prefix}peer_probe_seconds"))
            .with_description("Completed peer probe latency in seconds.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        peer_probe_in_flight: meter
            .i64_up_down_counter(format!("{prefix}peer_probe_in_flight"))
            .with_description("Current in-flight peer probe tasks.")
            .build(),
        peer_probe_cancellations_total: meter
            .u64_counter(format!("{prefix}peer_probe_cancellations_total"))
            .with_description("Remaining peer probe tasks cancelled after winner selection.")
            .build(),
        peer_attempt_total: meter
            .u64_counter(format!("{prefix}peer_attempt_total"))
            .with_description("Peer attempts by terminal stage and bounded outcome.")
            .build(),
        peer_attempt_seconds: meter
            .f64_histogram(format!("{prefix}peer_attempt_seconds"))
            .with_description("Peer attempt latency by terminal stage and bounded outcome.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        peer_fill_total: meter
            .u64_counter(format!("{prefix}peer_fill_total"))
            .with_description("Peer cache fills by bounded result.")
            .build(),
        peer_fill_bytes_total: meter
            .u64_counter(format!("{prefix}peer_fill_bytes_total"))
            .with_description("Bytes received through peer cache fills.")
            .build(),
        peer_fill_seconds: meter
            .f64_histogram(format!("{prefix}peer_fill_seconds"))
            .with_description("Peer cache-fill transfer latency in seconds.")
            .with_boundaries(PEER_FILL_TIME_BUCKETS.to_vec())
            .build(),
        peer_fill_follower_requests_total: meter
            .u64_counter(format!("{prefix}peer_fill_follower_requests_total"))
            .with_description(
                "Same-replica peer-fill follower requests by bounded terminal result.",
            )
            .build(),
        peer_fill_follower_bytes_total: meter
            .u64_counter(format!("{prefix}peer_fill_follower_bytes_total"))
            .with_description("Bytes sent from in-progress peer-fill staging files.")
            .build(),
        peer_fill_followers_active: meter
            .i64_up_down_counter(format!("{prefix}peer_fill_followers_active"))
            .with_description("Current same-replica peer-fill follower requests.")
            .build(),
        peer_serve_total: meter
            .u64_counter(format!("{prefix}peer_serve_total"))
            .with_description("Peer body requests by bounded terminal result.")
            .build(),
        peer_serve_bytes_total: meter
            .u64_counter(format!("{prefix}peer_serve_bytes_total"))
            .with_description("Peer body bytes written to requesting sockets.")
            .build(),
        peer_serves_active: meter
            .i64_up_down_counter(format!("{prefix}peer_serves_active"))
            .with_description("Current peer body serves.")
            .build(),
        peer_serve_socket_writable_wait_seconds: meter
            .f64_histogram(format!(
                "{prefix}peer_serve_socket_writable_wait_seconds"
            ))
            .with_description("Socket-writability wait per peer body serve.")
            .with_boundaries(TIME_BUCKETS.to_vec())
            .build(),
        peer_directory_peers: meter
            .u64_gauge(format!("{prefix}peer_directory_peers"))
            .with_description("Current peers in the refreshed directory.")
            .build(),
        downstream_close_total: meter
            .u64_counter(format!("{prefix}downstream_close_total"))
            .with_description(
                "Proxy-initiated client connection closes after a response, by bounded result.",
            )
            .build(),
    }
}

fn write_through_histogram(
    meter: &Meter,
    prefix: &str,
    phase: WriteThroughPhase,
) -> Histogram<f64> {
    let (family, buckets) = phase.family();
    meter
        .f64_histogram(format!("{prefix}{family}"))
        .with_description(phase.description())
        .with_boundaries(buckets.to_vec())
        .build()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTier {
    Origin,
    Peer,
}

impl RequestTier {
    fn from_access(access: &AccessLog) -> Option<Self> {
        match access.cache_tier.as_str() {
            "origin" => Some(Self::Origin),
            "peer" => Some(Self::Peer),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Origin => "origin",
            Self::Peer => "peer",
        }
    }
}

const DEFAULT_HTTP_KNOWN_METHODS: &[&str] = &[
    "CONNECT", "DELETE", "GET", "HEAD", "OPTIONS", "PATCH", "POST", "PUT", "QUERY", "TRACE",
];

/// Bounded terminal failures for HTTP requests without a status-derived error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpServerErrorType {
    Timeout,
    DownstreamWrite,
    Other,
}

impl HttpServerErrorType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::DownstreamWrite => "downstream_write",
            Self::Other => "_OTHER",
        }
    }
}

/// One completed HTTP server request projected onto semantic-convention metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpServerMetricEvent {
    pub method: String,
    pub scheme: String,
    pub protocol_version: Option<&'static str>,
    pub request_body_size: Option<u64>,
    pub response_status: Option<u16>,
    pub response_body_size: u64,
    pub duration_seconds: f64,
    pub error_type: Option<String>,
}

fn configured_http_known_methods() -> &'static [String] {
    static KNOWN_METHODS: OnceLock<Vec<String>> = OnceLock::new();
    KNOWN_METHODS.get_or_init(|| {
        std::env::var("OTEL_INSTRUMENTATION_HTTP_KNOWN_METHODS").map_or_else(
            |_| {
                DEFAULT_HTTP_KNOWN_METHODS
                    .iter()
                    .map(|method| (*method).to_owned())
                    .collect()
            },
            |configured| parse_http_known_methods(&configured),
        )
    })
}

fn parse_http_known_methods(configured: &str) -> Vec<String> {
    configured
        .split(',')
        .map(str::trim)
        .filter(|method| !method.is_empty())
        .map(str::to_owned)
        .collect()
}

fn canonical_http_method(method: &str, known_methods: &[String]) -> String {
    if known_methods.iter().any(|known| known == method) {
        method.to_owned()
    } else {
        "_OTHER".to_owned()
    }
}

fn canonical_http_scheme(scheme: &str) -> String {
    match scheme {
        "http" | "https" => scheme.to_owned(),
        _ => "_OTHER".to_owned(),
    }
}

fn normalize_http_protocol_version(version: &str) -> Option<&'static str> {
    match version {
        "HTTP/1.0" | "1.0" => Some("1.0"),
        "HTTP/1.1" | "1.1" => Some("1.1"),
        "HTTP/2" | "HTTP/2.0" | "2" | "2.0" => Some("2"),
        "HTTP/3" | "HTTP/3.0" | "3" | "3.0" => Some("3"),
        _ => None,
    }
}

fn terminal_http_attributes(event: &HttpServerMetricEvent) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new(HTTP_REQUEST_METHOD, event.method.clone()),
        KeyValue::new(URL_SCHEME, event.scheme.clone()),
    ];
    if let Some(version) = event.protocol_version {
        attributes.push(KeyValue::new(NETWORK_PROTOCOL_VERSION, version));
    }
    if let Some(status) = event.response_status {
        attributes.push(KeyValue::new(HTTP_RESPONSE_STATUS_CODE, i64::from(status)));
    }
    if let Some(error_type) = event.error_type.as_ref() {
        attributes.push(KeyValue::new(ERROR_TYPE, error_type.clone()));
    }
    attributes
}

fn record_http_server_event(metrics: &Metrics, event: &HttpServerMetricEvent) {
    let attributes = terminal_http_attributes(event);
    metrics
        .http_server_request_duration
        .record(event.duration_seconds, &attributes);
    if let Some(request_body_size) = event.request_body_size {
        metrics
            .http_server_request_body_size
            .record(request_body_size, &attributes);
    }
    metrics
        .http_server_response_body_size
        .record(event.response_body_size, &attributes);
}

fn http_server_error_type(
    response_status: Option<u16>,
    failure: Option<HttpServerErrorType>,
) -> Option<String> {
    failure.map(|error| error.as_str().to_owned()).or_else(|| {
        response_status
            .filter(|status| *status >= 500)
            .map(|status| status.to_string())
    })
}

/// Cancellation-safe active-request guard and terminal HTTP metric recorder.
pub struct HttpServerRequestGuard<'a> {
    metrics: Option<&'a Metrics>,
    event: Option<HttpServerMetricEvent>,
    active_attributes: Vec<KeyValue>,
    started: Option<Instant>,
    active: bool,
}

impl HttpServerRequestGuard<'static> {
    /// Starts one HTTP server request at the tower-service boundary.
    pub fn start(
        method: &str,
        scheme: &str,
        protocol_version: &str,
        request_body_size: Option<u64>,
    ) -> Self {
        Self::from_metrics(
            METRICS.get(),
            method,
            scheme,
            protocol_version,
            request_body_size,
            configured_http_known_methods(),
        )
    }
}

impl<'a> HttpServerRequestGuard<'a> {
    fn from_metrics(
        metrics: Option<&'a Metrics>,
        method: &str,
        scheme: &str,
        protocol_version: &str,
        request_body_size: Option<u64>,
        known_methods: &[String],
    ) -> Self {
        let Some(metrics) = metrics else {
            return Self {
                metrics: None,
                event: None,
                active_attributes: Vec::new(),
                started: None,
                active: false,
            };
        };
        let started = Instant::now();
        let method = canonical_http_method(method, known_methods);
        let scheme = canonical_http_scheme(scheme);
        let active_attributes = vec![
            KeyValue::new(HTTP_REQUEST_METHOD, method.clone()),
            KeyValue::new(URL_SCHEME, scheme.clone()),
        ];
        metrics
            .http_server_active_requests
            .add(1, &active_attributes);
        Self {
            metrics: Some(metrics),
            event: Some(HttpServerMetricEvent {
                method,
                scheme,
                protocol_version: normalize_http_protocol_version(protocol_version),
                request_body_size,
                response_status: None,
                response_body_size: 0,
                duration_seconds: 0.0,
                error_type: None,
            }),
            active_attributes,
            started: Some(started),
            active: true,
        }
    }

    /// Records terminal duration and body sizes, then balances the active count.
    pub fn finish(
        mut self,
        response_status: Option<u16>,
        response_body_size: u64,
        failure: Option<HttpServerErrorType>,
    ) {
        if let (Some(metrics), Some(event), Some(started)) =
            (self.metrics, self.event.as_mut(), self.started)
        {
            event.response_status = response_status;
            event.response_body_size = response_body_size;
            event.duration_seconds = started.elapsed().as_secs_f64();
            event.error_type = http_server_error_type(response_status, failure);
            record_http_server_event(metrics, event);
        }
        self.deactivate();
    }

    fn deactivate(&mut self) {
        if self.active {
            if let Some(metrics) = self.metrics {
                metrics
                    .http_server_active_requests
                    .add(-1, &self.active_attributes);
            }
            self.active = false;
        }
    }
}

impl Drop for HttpServerRequestGuard<'_> {
    fn drop(&mut self) {
        self.deactivate();
    }
}

/// Owns an isolated HTTP metric pipeline for benchmarks and tests.
#[doc(hidden)]
pub struct StandaloneHttpServerMetrics {
    _provider: SdkMeterProvider,
    metrics: Metrics,
}

impl StandaloneHttpServerMetrics {
    /// Starts a request guard against the isolated metric pipeline.
    pub fn start(
        &self,
        method: &str,
        scheme: &str,
        protocol_version: &str,
        request_body_size: Option<u64>,
    ) -> HttpServerRequestGuard<'_> {
        HttpServerRequestGuard::from_metrics(
            Some(&self.metrics),
            method,
            scheme,
            protocol_version,
            request_body_size,
            configured_http_known_methods(),
        )
    }
}

/// Builds an isolated enabled HTTP metric pipeline for benchmarks and tests.
#[doc(hidden)]
pub fn standalone_http_server_metrics(
    provider: SdkMeterProvider,
    prefix: &str,
) -> StandaloneHttpServerMetrics {
    let metrics = build_instruments(&provider.meter("ovcc"), prefix);
    StandaloneHttpServerMetrics {
        _provider: provider,
        metrics,
    }
}

/// The bounded connection states queried by the shipped dashboards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Reading,
    Writing,
    Waiting,
    Active,
}

impl ConnectionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reading => "reading",
            Self::Writing => "writing",
            Self::Waiting => "waiting",
            Self::Active => "active",
        }
    }
}

/// Bounded peer outcomes shared by probe, fill, and serve instruments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerResult {
    Hit,
    Miss,
    Unauthorized,
    Stale,
    Timeout,
    Refused,
    Truncated,
}

impl PeerResult {
    /// Every result label accepted by peer metric recording APIs.
    pub const ALL: [Self; 7] = [
        Self::Hit,
        Self::Miss,
        Self::Unauthorized,
        Self::Stale,
        Self::Timeout,
        Self::Refused,
        Self::Truncated,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Unauthorized => "unauthorized",
            Self::Stale => "stale",
            Self::Timeout => "timeout",
            Self::Refused => "refused",
            Self::Truncated => "truncated",
        }
    }
}

/// Bounded terminal outcomes for source-side peer body requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerServeResult {
    Complete,
    NotFound,
    Unauthorized,
    ClientAbort,
    IoFailure,
}

impl PeerServeResult {
    /// Every result label accepted by the peer-serve recorder.
    pub const ALL: [Self; 5] = [
        Self::Complete,
        Self::NotFound,
        Self::Unauthorized,
        Self::ClientAbort,
        Self::IoFailure,
    ];

    /// Label value written to the `result` dimension of the peer-serve series.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::NotFound => "not_found",
            Self::Unauthorized => "unauthorized",
            Self::ClientAbort => "client_abort",
            Self::IoFailure => "io_failure",
        }
    }
}

/// Bounded outcomes of a proxy-initiated client close after a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownstreamCloseResult {
    /// The client ended the connection during the lingering close.
    PeerClosed,
    /// The linger ended gracefully because the client held every byte, or on
    /// Windows stopped sending, but kept the connection open.
    Closed,
    /// The client acknowledged nothing for the write stall bound before
    /// `close_notify` could be queued, or Windows could not flush it, so the
    /// connection was reset.
    AlertTimeout,
    /// The client acknowledged nothing for the write stall bound while bytes
    /// still waited for it, so the connection was reset.
    LingerTimeout,
    /// A downstream write timeout, or a failed body whose head carried no
    /// framing, requested an abortive close.
    Reset,
    /// The close failed because the client connection was already broken.
    Error,
}

impl DownstreamCloseResult {
    /// Every result label accepted by the downstream-close recorder.
    pub const ALL: [Self; 6] = [
        Self::PeerClosed,
        Self::Closed,
        Self::AlertTimeout,
        Self::LingerTimeout,
        Self::Reset,
        Self::Error,
    ];

    /// Label value written to the `result` dimension of the close series.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PeerClosed => "peer_closed",
            Self::Closed => "closed",
            Self::AlertTimeout => "alert_timeout",
            Self::LingerTimeout => "linger_timeout",
            Self::Reset => "reset",
            Self::Error => "error",
        }
    }
}

/// Bounded terminal outcomes for same-replica requests that joined an active
/// file-backed peer fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerFillFollowerResult {
    Complete,
    Partial,
    PrebyteFallback,
    ClientAbort,
    IoFailure,
}

impl PeerFillFollowerResult {
    /// Every result label accepted by the follower terminal recorder.
    pub const ALL: [Self; 5] = [
        Self::Complete,
        Self::Partial,
        Self::PrebyteFallback,
        Self::ClientAbort,
        Self::IoFailure,
    ];

    /// Label value written to the `result` dimension of the follower series.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::PrebyteFallback => "prebyte_fallback",
            Self::ClientAbort => "client_abort",
            Self::IoFailure => "io_failure",
        }
    }
}

/// Bounded stages where a peer attempt can terminate before body transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAttemptStage {
    Preflight,
    Fanout,
    FillLock,
    PeerOpen,
}

impl PeerAttemptStage {
    /// Label value written to the `stage` dimension of the peer-attempt series.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Fanout => "fanout",
            Self::FillLock => "fill_lock",
            Self::PeerOpen => "peer_open",
        }
    }
}

/// Bounded terminal outcomes for a peer attempt stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAttemptOutcome {
    Success,
    Failed,
    Timeout,
    Ineligible,
    Miss,
    Shared,
    Bypass,
    Refused,
    Rejected,
}

impl PeerAttemptOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Ineligible => "ineligible",
            Self::Miss => "miss",
            Self::Shared => "shared",
            Self::Bypass => "bypass",
            Self::Refused => "refused",
            Self::Rejected => "rejected",
        }
    }
}

/// Records a connection-state delta at an exact accept/read/write/wait/close
/// transition. Labels are enum-backed so callers cannot introduce cardinality.
pub fn connection_delta(state: ConnectionState, delta: i64) {
    let labels = [KeyValue::new("state", state.as_str())];
    if let Some(metrics) = METRICS.get() {
        metrics.connections.add(delta, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.add_gauge(
            "nginx_http_connections",
            vec![state.as_str().to_owned()],
            delta,
        );
    }
}

/// One outbound access-check event. A transport failure has no response status,
/// matching the legacy counter while still recording request count and latency.
pub struct AccessProbe<'a> {
    pub host: &'a str,
    pub method: &'static str,
    pub status: Option<u16>,
    pub duration_seconds: f64,
}

/// Records one exact access-check request/response/duration event.
pub fn record_access_probe(event: &AccessProbe<'_>) {
    if let Some(metrics) = METRICS.get() {
        let request_labels = [
            KeyValue::new("http_host", event.host.to_owned()),
            KeyValue::new("http_method", event.method),
        ];
        metrics.access_requests_total.add(1, &request_labels);
        metrics.access_upstream_request_seconds.record(
            event.duration_seconds,
            &[KeyValue::new("http_host", event.host.to_owned())],
        );
        if let Some(status) = event.status {
            let response_labels = [
                KeyValue::new("http_host", event.host.to_owned()),
                KeyValue::new("http_method", event.method),
                KeyValue::new("http_status", status.to_string()),
            ];
            metrics.access_responses_total.add(1, &response_labels);
        }
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_access_probe(event);
    }
}

/// Bounded write-through outcomes exposed to metrics consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThroughResult {
    Rejected,
    UpstreamError,
    Cancelled,
    SeedError,
    Success,
    Block,
    BlockError,
}

impl WriteThroughResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::UpstreamError => "upstream_error",
            Self::Cancelled => "cancelled",
            Self::SeedError => "seed_error",
            Self::Success => "success",
            Self::Block => "block",
            Self::BlockError => "block_error",
        }
    }
}

/// Write-through phases with source timings in the current proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThroughPhase {
    Upload,
    Seed,
    Adopt,
    BlockList,
    Block,
}

impl WriteThroughPhase {
    fn family(self) -> (&'static str, &'static [f64]) {
        match self {
            Self::Upload => ("write_through_upload_seconds", TIME_BUCKETS),
            Self::Seed => ("write_through_seed_seconds", LONG_TIME_BUCKETS),
            Self::Adopt => ("write_through_adopt_seconds", LONG_TIME_BUCKETS),
            Self::BlockList => ("write_through_blocklist_seconds", LONG_TIME_BUCKETS),
            Self::Block => ("write_through_block_seconds", TIME_BUCKETS),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Upload => "Write-through upload and staging latency in seconds.",
            Self::Seed => "Write-through cache seed latency in seconds.",
            Self::Adopt => "Atomic Azure segmented-cache adoption latency in seconds.",
            Self::BlockList => "Azure block-list commit latency in seconds.",
            Self::Block => "Azure block upload and staging latency in seconds.",
        }
    }

    fn instrument(self, metrics: &Metrics) -> &Histogram<f64> {
        match self {
            Self::Upload => &metrics.write_through_upload_seconds,
            Self::Seed => &metrics.write_through_seed_seconds,
            Self::Adopt => &metrics.write_through_adopt_seconds,
            Self::BlockList => &metrics.write_through_blocklist_seconds,
            Self::Block => &metrics.write_through_block_seconds,
        }
    }
}

/// RAII active-upload gauge guard. Every return path decrements the gauge.
pub struct WriteThroughActiveGuard;

impl WriteThroughActiveGuard {
    /// Counts one upload as active until the returned guard drops.
    pub fn start() -> Self {
        if let Some(metrics) = METRICS.get() {
            metrics.write_through_active_uploads.add(1, &[]);
        }
        if let Some(prometheus) = PROMETHEUS.get() {
            prometheus.add_gauge("write_through_active_uploads", Vec::new(), 1);
        }
        Self
    }
}

impl Drop for WriteThroughActiveGuard {
    fn drop(&mut self) {
        if let Some(metrics) = METRICS.get() {
            metrics.write_through_active_uploads.add(-1, &[]);
        }
        if let Some(prometheus) = PROMETHEUS.get() {
            prometheus.add_gauge("write_through_active_uploads", Vec::new(), -1);
        }
    }
}

/// RAII in-flight probe gauge guard. Task cancellation drops the guard.
pub struct PeerProbeInFlightGuard {
    metrics: Option<&'static Metrics>,
    prometheus: Option<Arc<Prometheus>>,
}

impl PeerProbeInFlightGuard {
    /// Counts one probe as in flight until the returned guard drops.
    pub fn start() -> Self {
        Self::from_sinks(METRICS.get(), PROMETHEUS.get().map(Arc::clone))
    }

    fn from_sinks(metrics: Option<&'static Metrics>, prometheus: Option<Arc<Prometheus>>) -> Self {
        if let Some(metrics) = metrics {
            metrics.peer_probe_in_flight.add(1, &[]);
        }
        if let Some(prometheus) = prometheus.as_ref() {
            prometheus.peer_probe_in_flight_delta(1);
        }
        Self {
            metrics,
            prometheus,
        }
    }
}

impl Drop for PeerProbeInFlightGuard {
    fn drop(&mut self) {
        if let Some(metrics) = self.metrics {
            metrics.peer_probe_in_flight.add(-1, &[]);
        }
        if let Some(prometheus) = self.prometheus.as_ref() {
            prometheus.peer_probe_in_flight_delta(-1);
        }
    }
}

/// RAII active follower gauge guard. Task cancellation and panic both drop the
/// guard and return the gauge to its prior value.
pub struct PeerFillFollowerActiveGuard {
    metrics: Option<&'static Metrics>,
    prometheus: Option<Arc<Prometheus>>,
}

impl PeerFillFollowerActiveGuard {
    /// Counts one follower as active until the returned guard drops.
    pub fn start() -> Self {
        Self::from_sinks(METRICS.get(), PROMETHEUS.get().map(Arc::clone))
    }

    fn from_sinks(metrics: Option<&'static Metrics>, prometheus: Option<Arc<Prometheus>>) -> Self {
        if let Some(metrics) = metrics {
            metrics.peer_fill_followers_active.add(1, &[]);
        }
        if let Some(prometheus) = prometheus.as_ref() {
            prometheus.peer_fill_followers_active_delta(1);
        }
        Self {
            metrics,
            prometheus,
        }
    }
}

impl Drop for PeerFillFollowerActiveGuard {
    fn drop(&mut self) {
        if let Some(metrics) = self.metrics {
            metrics.peer_fill_followers_active.add(-1, &[]);
        }
        if let Some(prometheus) = self.prometheus.as_ref() {
            prometheus.peer_fill_followers_active_delta(-1);
        }
    }
}

/// RAII active peer-body gauge guard. Cancellation and panic both decrement it.
pub struct PeerServeActiveGuard {
    metrics: Option<&'static Metrics>,
    prometheus: Option<Arc<Prometheus>>,
}

impl PeerServeActiveGuard {
    /// Counts one peer body serve as active until the returned guard drops.
    pub fn start() -> Self {
        Self::from_sinks(METRICS.get(), PROMETHEUS.get().map(Arc::clone))
    }

    fn from_sinks(metrics: Option<&'static Metrics>, prometheus: Option<Arc<Prometheus>>) -> Self {
        if let Some(metrics) = metrics {
            metrics.peer_serves_active.add(1, &[]);
        }
        if let Some(prometheus) = prometheus.as_ref() {
            prometheus.peer_serves_active_delta(1);
        }
        Self {
            metrics,
            prometheus,
        }
    }
}

impl Drop for PeerServeActiveGuard {
    fn drop(&mut self) {
        if let Some(metrics) = self.metrics {
            metrics.peer_serves_active.add(-1, &[]);
        }
        if let Some(prometheus) = self.prometheus.as_ref() {
            prometheus.peer_serves_active_delta(-1);
        }
    }
}

/// Counts one finished write-through request under its host and outcome.
pub fn record_write_through_result(host: &str, result: WriteThroughResult) {
    let labels = [
        KeyValue::new("http_host", host.to_owned()),
        KeyValue::new("result", result.as_str()),
    ];
    if let Some(metrics) = METRICS.get() {
        metrics.write_through_requests_total.add(1, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_write_through_result(host, result);
    }
}

/// Records the uploaded byte count for one write-through request.
pub fn record_write_through_bytes(host: &str, bytes: u64) {
    let labels = [KeyValue::new("http_host", host.to_owned())];
    if let Some(metrics) = METRICS.get() {
        metrics
            .write_through_upload_bytes
            .record(bytes as f64, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_write_through_bytes(host, bytes);
    }
}

/// Records how many fragments a segmented write-through upload carried.
pub fn record_write_through_segments(host: &str, segments: usize) {
    let labels = [KeyValue::new("http_host", host.to_owned())];
    if let Some(metrics) = METRICS.get() {
        metrics
            .write_through_segments
            .record(segments as f64, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_write_through_segments(host, segments);
    }
}

/// Records elapsed time for one write-through phase against that phase's histogram.
pub fn record_write_through_duration(host: &str, phase: WriteThroughPhase, duration_seconds: f64) {
    let labels = [KeyValue::new("http_host", host.to_owned())];
    if let Some(metrics) = METRICS.get() {
        phase.instrument(metrics).record(duration_seconds, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_write_through_duration(host, phase, duration_seconds);
    }
}

/// Records one completed peer probe and its latency with an enum-backed result.
pub fn record_peer_probe(result: PeerResult, duration_seconds: f64) {
    let labels = [KeyValue::new("result", result.as_str())];
    if let Some(metrics) = METRICS.get() {
        metrics.peer_probe_total.add(1, &labels);
        metrics.peer_probe_seconds.record(duration_seconds, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_probe(result, duration_seconds);
    }
}

/// Records one peer attempt at its bounded terminal stage and outcome.
pub fn record_peer_attempt(
    stage: PeerAttemptStage,
    outcome: PeerAttemptOutcome,
    duration_seconds: f64,
) {
    let labels = [
        KeyValue::new("stage", stage.as_str()),
        KeyValue::new("outcome", outcome.as_str()),
    ];
    if let Some(metrics) = METRICS.get() {
        metrics.peer_attempt_total.add(1, &labels);
        metrics
            .peer_attempt_seconds
            .record(duration_seconds, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_attempt(stage, outcome, duration_seconds);
    }
}

/// Records the loser tasks still pending when a peer winner is selected.
pub fn record_peer_probe_cancellations(count: usize) {
    if count == 0 {
        return;
    }
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    if let Some(metrics) = METRICS.get() {
        metrics.peer_probe_cancellations_total.add(count, &[]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_probe_cancellations(count);
    }
}

/// Records one completed or failed peer fill and its observed transfer.
pub fn record_peer_fill(result: PeerResult, bytes: u64, duration_seconds: f64) {
    let labels = [KeyValue::new("result", result.as_str())];
    if let Some(metrics) = METRICS.get() {
        metrics.peer_fill_total.add(1, &labels);
        metrics.peer_fill_bytes_total.add(bytes, &[]);
        metrics.peer_fill_seconds.record(duration_seconds, &[]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_fill(result, bytes, duration_seconds);
    }
}

/// Records one terminal same-replica follower request with an enum-bounded
/// result. This is distinct from [`record_peer_fill`], which owns peer ingress.
pub fn record_peer_fill_follower_terminal(result: PeerFillFollowerResult) {
    let labels = [KeyValue::new("result", result.as_str())];
    if let Some(metrics) = METRICS.get() {
        metrics.peer_fill_follower_requests_total.add(1, &labels);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_fill_follower_terminal(result);
    }
}

/// Records bytes actually sent from an in-progress local staging file. A
/// zero-byte send has no event and therefore creates no counter series.
pub fn record_peer_fill_follower_bytes(bytes: u64) {
    if bytes == 0 {
        return;
    }
    if let Some(metrics) = METRICS.get() {
        metrics.peer_fill_follower_bytes_total.add(bytes, &[]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_fill_follower_bytes(bytes);
    }
}

/// Records one terminal source-side peer body serve from measured socket writes.
pub fn record_peer_serve(result: PeerServeResult, bytes: u64, socket_writable_wait_seconds: f64) {
    let labels = [KeyValue::new("result", result.as_str())];
    if let Some(metrics) = METRICS.get() {
        metrics.peer_serve_total.add(1, &labels);
        metrics.peer_serve_bytes_total.add(bytes, &[]);
        metrics
            .peer_serve_socket_writable_wait_seconds
            .record(socket_writable_wait_seconds, &[]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_serve(result, bytes, socket_writable_wait_seconds);
    }
}

/// Records the current bounded peer-directory membership after a refresh.
pub fn record_peer_directory(peers: usize) {
    let peers_u64 = u64::try_from(peers).unwrap_or(u64::MAX);
    if let Some(metrics) = METRICS.get() {
        metrics.peer_directory_peers.record(peers_u64, &[]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_peer_directory(peers);
    }
}

/// Records how one proxy-initiated client close ended.
pub fn record_downstream_close(result: DownstreamCloseResult) {
    if let Some(metrics) = METRICS.get() {
        metrics
            .downstream_close_total
            .add(1, &[KeyValue::new("result", result.as_str())]);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record_downstream_close(result);
    }
}

/// Records one request's metrics from the access-log metadata. OTLP remains a
/// no-op when disabled; the pull accumulator records after its listener installs
/// it. Reads metadata only, so the kTLS `sendfile` HIT path is untouched.
pub fn record(access: &AccessLog) {
    if let Some(metrics) = METRICS.get() {
        record_into(metrics, access);
    }
    if let Some(prometheus) = PROMETHEUS.get() {
        prometheus.record(access);
    }
}

/// Builds a standalone instrument set (no exporter, no global state) and returns
/// a recorder closure over it. Lets benches and tests measure the real
/// enabled-path recording cost without the process-global `OnceLock`. Hidden
/// from the public docs; not part of the supported surface.
#[doc(hidden)]
pub fn standalone_recorder(prefix: &str) -> impl Fn(&AccessLog) {
    let provider = SdkMeterProvider::builder().build();
    let metrics = build_instruments(&provider.meter("ovcc"), prefix);
    move |access: &AccessLog| {
        // Holding `provider` keeps the instrument pipeline alive for the closure.
        let _ = &provider;
        record_into(&metrics, access);
    }
}

/// The recording body against an explicit instrument set, so tests can drive it
/// without the process-global `OnceLock`.
fn record_into(metrics: &Metrics, access: &AccessLog) {
    let http_status = access.status.to_string();
    let request_labels = [
        KeyValue::new("http_host", access.host.clone()),
        KeyValue::new("request_method", access.request.clone()),
        KeyValue::new("cache_status", access.upstream_cache_status.clone()),
        KeyValue::new("http_status", http_status),
    ];
    let host_label = [KeyValue::new("http_host", access.host.clone())];

    metrics.requests_total.add(1, &request_labels);
    if let Some(tier) = RequestTier::from_access(access) {
        metrics
            .request_tier_total
            .add(1, &[KeyValue::new("cache_tier", tier.as_str())]);
    }
    metrics
        .request_duration_seconds
        .record(access.request_time, &host_label);
    metrics
        .response_size_bytes
        .record(access.bytes_sent as f64, &host_label);

    // Upstream families count only when an origin was actually contacted (a HIT
    // never touches an origin), matching the dashboards' upstream panels.
    if access.upstream_status.is_some() {
        metrics.upstream_requests_total.add(1, &request_labels);
        if let Some(connect) = access.upstream_connect_time {
            metrics
                .upstream_connect_seconds
                .record(connect, &host_label);
        }
        if let Some(header) = access.upstream_header_time {
            metrics.upstream_header_seconds.record(header, &host_label);
        }
        if let Some(response) = access.upstream_response_time {
            metrics
                .upstream_response_seconds
                .record(response, &host_label);
        }
    }

    if access.mutation_detected {
        metrics.mutation_purges_total.add(1, &host_label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

    #[test]
    fn metrics_disabled_by_default() {
        let _env = crate::lock_test_env();
        // No env, empty config endpoint -> off (mirrors the span gating).
        assert!(!otlp_metrics_enabled(&TelemetryConfig::default()));
    }

    #[test]
    fn metrics_enabled_when_config_endpoint_set() {
        let _env = crate::lock_test_env();
        let mut config = TelemetryConfig::default();
        config.otlp.endpoint = "http://localhost:4318".to_owned();
        assert!(otlp_metrics_enabled(&config));
    }

    #[test]
    fn record_is_noop_when_uninitialized() {
        // The global instrument set is never built in this test; `record` must
        // simply return without panicking so a collector-less run is safe.
        record(&AccessLog::default());
    }

    /// Collects the exported metric names from an in-memory exporter snapshot.
    fn exported_names(snapshot: &[ResourceMetrics]) -> Vec<String> {
        snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .map(|metric| metric.name().to_owned())
            .collect()
    }

    /// The `u64` sum (counter) data point value for `name`, if present.
    fn counter_value(snapshot: &[ResourceMetrics], name: &str) -> Option<u64> {
        snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .find(|metric| metric.name() == name)
            .and_then(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    Some(sum.data_points().map(|dp| dp.value()).sum())
                }
                _ => None,
            })
    }

    fn miss_with_upstream() -> AccessLog {
        AccessLog {
            host: "bucket.example.com".to_owned(),
            request: "GET".to_owned(),
            status: 200,
            upstream_cache_status: "MISS".to_owned(),
            cache_tier: "origin".to_owned(),
            request_time: 0.012,
            bytes_sent: 4096,
            upstream_status: Some(200),
            upstream_connect_time: Some(0.001),
            upstream_header_time: Some(0.004),
            upstream_response_time: Some(0.009),
            mutation_detected: true,
            ..Default::default()
        }
    }

    fn hit_no_upstream() -> AccessLog {
        AccessLog {
            host: "bucket.example.com".to_owned(),
            request: "GET".to_owned(),
            status: 200,
            upstream_cache_status: "HIT".to_owned(),
            request_time: 0.0003,
            bytes_sent: 4096,
            ..Default::default()
        }
    }

    // Isolation test for the real OTLP/HTTP metric exporter (GCP-independent):
    // point `build_meter_provider` at a local TCP listener, record, force-flush,
    // and assert an OTLP POST actually leaves the process. This reproduces the
    // on-cluster path where traces exported but metrics did not.
    #[test]
    fn otlp_metric_exporter_posts_on_flush() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        use std::sync::mpsc;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock otlp");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::channel::<usize>();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                let _ = tx.send(n);
            }
        });

        let mut config = TelemetryConfig::default();
        config.otlp.endpoint = format!("http://{addr}");
        let provider = build_meter_provider(&config).expect("build meter provider");
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        record_into(&metrics, &miss_with_upstream());
        provider.force_flush().expect("force flush");

        let received = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("OTLP metric exporter never POSTed on force_flush");
        assert!(received > 0, "mock OTLP endpoint received an empty request");
        let _ = provider.shutdown();
    }

    /// Builds an in-memory meter provider, records `access`, flushes, and returns
    /// the exported snapshot plus the exporter for assertions.
    fn record_and_collect(prefix: &str, access: &AccessLog) -> Vec<ResourceMetrics> {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let meter = provider.meter("ovcc");
        let metrics = build_instruments(&meter, prefix);
        record_into(&metrics, access);
        provider.force_flush().expect("force flush");
        exporter.get_finished_metrics().expect("finished metrics")
    }

    #[test]
    fn record_emits_prefixed_dashboard_series_on_miss() {
        let snapshot = record_and_collect("cc_", &miss_with_upstream());
        let names = exported_names(&snapshot);
        for expected in [
            "cc_http_requests_total",
            "cc_http_upstream_requests_total",
            "cc_request_tier_total",
            "cc_mutation_detection_purges_total",
            "cc_request_duration_seconds",
            "cc_upstream_connect_time_seconds",
            "cc_upstream_header_time_seconds",
            "cc_upstream_response_time_seconds",
            "cc_response_size_bytes",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected} in {names:?}"
            );
        }
        assert_eq!(counter_value(&snapshot, "cc_http_requests_total"), Some(1));
        assert_eq!(
            counter_value(&snapshot, "cc_http_upstream_requests_total"),
            Some(1)
        );
        assert_eq!(
            counter_value(&snapshot, "cc_mutation_detection_purges_total"),
            Some(1)
        );
    }

    #[test]
    fn record_labels_carry_request_dimensions() {
        let snapshot = record_and_collect("cc_", &miss_with_upstream());
        let labels: Vec<(String, String)> = snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .filter(|metric| metric.name() == "cc_http_requests_total")
            .flat_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .flat_map(|dp| dp.attributes())
                    .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        for key in ["http_host", "request_method", "cache_status", "http_status"] {
            assert!(labels.iter().any(|(k, _)| k == key), "missing label {key}");
        }
        // No request URI, query, or auth token ever becomes a label.
        for key in ["request_uri", "authorization", "url.path"] {
            assert!(
                labels.iter().all(|(k, _)| k != key),
                "unexpected label {key}"
            );
        }
    }

    #[test]
    fn http_server_semconv_instruments_match_contract() {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        let event = HttpServerMetricEvent {
            method: "GET".to_owned(),
            scheme: "https".to_owned(),
            protocol_version: Some("1.1"),
            request_body_size: Some(12),
            response_status: Some(503),
            response_body_size: 7,
            duration_seconds: 0.125,
            error_type: Some("503".to_owned()),
        };
        let active_attributes = [
            KeyValue::new(HTTP_REQUEST_METHOD, "GET"),
            KeyValue::new(URL_SCHEME, "https"),
        ];

        // Act.
        metrics
            .http_server_active_requests
            .add(1, &active_attributes);
        record_http_server_event(&metrics, &event);
        metrics
            .http_server_active_requests
            .add(-1, &active_attributes);
        provider.force_flush().expect("force flush");
        let snapshot = exporter.get_finished_metrics().expect("finished metrics");

        // Assert.
        let find_metric = |name: &str| {
            snapshot
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        let duration = find_metric(HTTP_SERVER_REQUEST_DURATION);
        assert_eq!(duration.unit(), "s");
        let AggregatedMetrics::F64(MetricData::Histogram(duration)) = duration.data() else {
            panic!("HTTP duration must be an f64 histogram");
        };
        let duration_point = duration.data_points().next().expect("duration point");
        assert_eq!(duration_point.count(), 1);
        assert_eq!(duration_point.sum(), 0.125);
        assert_eq!(
            duration_point.bounds().collect::<Vec<_>>(),
            HTTP_SERVER_DURATION_BUCKETS
        );
        let attributes = duration_point
            .attributes()
            .map(|attribute| (attribute.key.to_string(), attribute.value.to_string()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            attributes.get(HTTP_REQUEST_METHOD).map(String::as_str),
            Some("GET")
        );
        assert_eq!(
            attributes.get(URL_SCHEME).map(String::as_str),
            Some("https")
        );
        assert_eq!(
            attributes.get(NETWORK_PROTOCOL_VERSION).map(String::as_str),
            Some("1.1")
        );
        assert_eq!(
            attributes
                .get(HTTP_RESPONSE_STATUS_CODE)
                .map(String::as_str),
            Some("503")
        );
        assert_eq!(attributes.get(ERROR_TYPE).map(String::as_str), Some("503"));
        assert_eq!(attributes.len(), 5);

        for (name, unit, expected_sum) in [
            (HTTP_SERVER_REQUEST_BODY_SIZE, "By", 12_u64),
            (HTTP_SERVER_RESPONSE_BODY_SIZE, "By", 7_u64),
        ] {
            let metric = find_metric(name);
            assert_eq!(metric.unit(), unit);
            let AggregatedMetrics::U64(MetricData::Histogram(histogram)) = metric.data() else {
                panic!("{name} must be a u64 histogram");
            };
            let point = histogram.data_points().next().expect("body-size point");
            assert_eq!(point.count(), 1);
            assert_eq!(point.sum(), expected_sum);
            assert_eq!(point.bounds().collect::<Vec<_>>(), BYTE_BUCKETS);
        }

        let active = find_metric(HTTP_SERVER_ACTIVE_REQUESTS);
        assert_eq!(active.unit(), "{request}");
        let AggregatedMetrics::I64(MetricData::Sum(active)) = active.data() else {
            panic!("active requests must be an i64 sum");
        };
        assert!(!active.is_monotonic());
        assert_eq!(
            active.data_points().map(|point| point.value()).sum::<i64>(),
            0
        );
    }

    #[test]
    fn http_known_methods_are_case_sensitive_and_env_is_a_full_override() {
        let defaults = DEFAULT_HTTP_KNOWN_METHODS
            .iter()
            .map(|method| (*method).to_owned())
            .collect::<Vec<_>>();
        assert_eq!(canonical_http_method("GET", &defaults), "GET");
        assert_eq!(canonical_http_method("get", &defaults), "_OTHER");
        assert_eq!(canonical_http_method("BREW", &defaults), "_OTHER");

        let configured = parse_http_known_methods("BREW, CUSTOM");
        assert_eq!(canonical_http_method("BREW", &configured), "BREW");
        assert_eq!(canonical_http_method("GET", &configured), "_OTHER");
        assert!(parse_http_known_methods("").is_empty());
    }

    #[test]
    fn http_error_type_uses_status_and_bounded_transport_failures() {
        assert_eq!(
            http_server_error_type(Some(500), None).as_deref(),
            Some("500")
        );
        assert_eq!(http_server_error_type(Some(404), None), None);
        assert_eq!(
            http_server_error_type(Some(200), Some(HttpServerErrorType::Timeout)).as_deref(),
            Some("timeout")
        );
        assert_eq!(
            http_server_error_type(None, Some(HttpServerErrorType::DownstreamWrite)).as_deref(),
            Some("downstream_write")
        );
        assert_eq!(
            http_server_error_type(None, Some(HttpServerErrorType::Other)).as_deref(),
            Some("_OTHER")
        );
    }

    #[test]
    fn telemetry_http_request_guard_balances_active() {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        let known = vec!["GET".to_owned()];
        let guard = HttpServerRequestGuard::from_metrics(
            Some(&metrics),
            "GET",
            "https",
            "HTTP/1.1",
            Some(0),
            &known,
        );

        // Act.
        guard.finish(Some(200), 7, None);
        provider.force_flush().expect("force flush");
        let snapshot = exporter.get_finished_metrics().expect("finished metrics");

        // Assert.
        let find_metric = |name: &str| {
            snapshot
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };
        let AggregatedMetrics::I64(MetricData::Sum(active)) =
            find_metric(HTTP_SERVER_ACTIVE_REQUESTS).data()
        else {
            panic!("active requests must be an i64 sum");
        };
        assert_eq!(
            active.data_points().map(|point| point.value()).sum::<i64>(),
            0
        );
        let AggregatedMetrics::F64(MetricData::Histogram(duration)) =
            find_metric(HTTP_SERVER_REQUEST_DURATION).data()
        else {
            panic!("HTTP duration must be an f64 histogram");
        };
        assert_eq!(
            duration
                .data_points()
                .map(|point| point.count())
                .sum::<u64>(),
            1
        );
    }

    #[test]
    fn http_active_request_guard_balances_cancellation_and_panic() {
        use std::future::Future as _;
        use std::task::{Context, Poll, Waker};

        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        let known = vec!["GET".to_owned()];
        let mut task = Box::pin(async {
            let _guard = HttpServerRequestGuard::from_metrics(
                Some(&metrics),
                "GET",
                "https",
                "HTTP/1.1",
                Some(0),
                &known,
            );
            std::future::pending::<()>().await;
        });
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);

        // Act: dropping the pending future models task cancellation.
        drop(task);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = HttpServerRequestGuard::from_metrics(
                Some(&metrics),
                "GET",
                "https",
                "HTTP/1.1",
                Some(0),
                &known,
            );
            panic!("injected request panic");
        }));
        provider.force_flush().expect("force flush");
        let snapshot = exporter.get_finished_metrics().expect("finished metrics");

        // Assert.
        let active = snapshot
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|metric| metric.name() == HTTP_SERVER_ACTIVE_REQUESTS)
            .expect("active request metric");
        let AggregatedMetrics::I64(MetricData::Sum(active)) = active.data() else {
            panic!("active requests must be an i64 sum");
        };
        assert_eq!(
            active.data_points().map(|point| point.value()).sum::<i64>(),
            0
        );
        assert!(
            snapshot
                .iter()
                .flat_map(|resource| resource.scope_metrics())
                .flat_map(|scope| scope.metrics())
                .all(|metric| metric.name() != HTTP_SERVER_REQUEST_DURATION),
            "cancelled and panicked requests do not fabricate completion durations"
        );
    }

    #[test]
    fn hit_does_not_emit_upstream_or_mutation_series() {
        let snapshot = record_and_collect("cc_", &hit_no_upstream());
        let names = exported_names(&snapshot);
        assert!(names.iter().any(|n| n == "cc_http_requests_total"));
        assert!(names.iter().any(|n| n == "cc_request_duration_seconds"));
        for absent in [
            "cc_http_upstream_requests_total",
            "cc_request_tier_total",
            "cc_upstream_connect_time_seconds",
            "cc_upstream_header_time_seconds",
            "cc_upstream_response_time_seconds",
            "cc_mutation_detection_purges_total",
        ] {
            assert!(
                names.iter().all(|n| n != absent),
                "unexpected {absent} on a HIT in {names:?}"
            );
        }
    }

    #[test]
    fn ucc_prefix_renames_series() {
        let snapshot = record_and_collect("ucc_", &hit_no_upstream());
        let names = exported_names(&snapshot);
        assert!(names.iter().any(|n| n == "ucc_http_requests_total"));
        assert!(names.iter().all(|n| !n.starts_with("cc_")));
    }

    #[test]
    fn prometheus_exposes_core_counter_histogram_and_escaped_labels() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        let mut access = miss_with_upstream();
        access.host = "bucket\\\"name\n.example".to_owned();
        metrics.record(&access);

        // Act.
        let body = metrics.scrape();

        // Assert.
        assert!(body.contains("# HELP cc_http_requests_total"));
        assert!(body.contains("# TYPE cc_http_requests_total counter"));
        assert!(body.contains(
            "cc_http_requests_total{http_host=\"bucket\\\\\\\"name\\n.example\",request_method=\"GET\",cache_status=\"MISS\",http_status=\"200\"} 1"
        ));
        assert!(body.contains(
            "cc_request_duration_seconds_bucket{http_host=\"bucket\\\\\\\"name\\n.example\",le=\"0.02\"} 1"
        ));
        assert!(body.contains(
            "cc_request_duration_seconds_sum{http_host=\"bucket\\\\\\\"name\\n.example\"} 0.012"
        ));
        assert!(body.contains(
            "cc_request_duration_seconds_count{http_host=\"bucket\\\\\\\"name\\n.example\"} 1"
        ));
    }

    #[test]
    fn prometheus_poll_counter_changes_every_scrape() {
        // Arrange.
        let metrics = Prometheus::new("cc_");

        // Act.
        let first = metrics.scrape();
        let second = metrics.scrape();

        // Assert.
        assert!(first.contains("cc_metric_poll_total 1"));
        assert!(second.contains("cc_metric_poll_total 2"));
        assert_ne!(first, second);
    }

    #[test]
    fn prometheus_records_dashboard_event_families() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        metrics.add_gauge("nginx_http_connections", vec!["active".to_owned()], 1);
        metrics.add_gauge("write_through_active_uploads", Vec::new(), 1);
        metrics.record_access_probe(&AccessProbe {
            host: "bucket.example.com",
            method: "HEAD",
            status: Some(200),
            duration_seconds: 0.004,
        });

        // Act.
        metrics.record_write_through_result("bucket.example.com", WriteThroughResult::Success);
        metrics.record_write_through_bytes("bucket.example.com", 4096);
        metrics.record_write_through_duration(
            "bucket.example.com",
            WriteThroughPhase::Upload,
            0.012,
        );
        metrics.add_gauge("write_through_active_uploads", Vec::new(), -1);
        let body = metrics.scrape();

        // Assert.
        for series in [
            "cc_nginx_http_connections{state=\"active\"} 1",
            "cc_write_through_active_uploads 0",
            "cc_access_requests_total{http_host=\"bucket.example.com\",http_method=\"HEAD\"} 1",
            "cc_access_responses_total{http_host=\"bucket.example.com\",http_method=\"HEAD\",http_status=\"200\"} 1",
            "cc_access_upstream_request_seconds_count{http_host=\"bucket.example.com\"} 1",
            "cc_write_through_requests_total{http_host=\"bucket.example.com\",result=\"success\"} 1",
            "cc_write_through_upload_bytes_count{http_host=\"bucket.example.com\"} 1",
            "cc_write_through_upload_seconds_count{http_host=\"bucket.example.com\"} 1",
        ] {
            assert!(body.contains(series), "missing {series}");
        }
    }

    #[test]
    fn request_tier_uses_only_access_log_origin_and_peer_values() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        let mut origin = miss_with_upstream();
        let mut peer = miss_with_upstream();
        peer.cache_tier = "peer".to_owned();
        let mut unknown = miss_with_upstream();
        unknown.cache_tier = "unexpected".to_owned();

        // Act.
        metrics.record(&origin);
        metrics.record(&peer);
        metrics.record(&unknown);
        origin.cache_tier.clear();
        metrics.record(&origin);
        let body = metrics.scrape();

        // Assert.
        assert!(body.contains("cc_request_tier_total{cache_tier=\"origin\"} 1"));
        assert!(body.contains("cc_request_tier_total{cache_tier=\"peer\"} 1"));
        assert!(!body.contains("cache_tier=\"unexpected\""));
        assert!(!body.contains("cache_tier=\"\""));
    }

    #[test]
    fn peer_attempt_metrics_use_terminal_stage_and_outcome_labels() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        let events = [
            (
                PeerAttemptStage::Preflight,
                PeerAttemptOutcome::Failed,
                0.003,
            ),
            (PeerAttemptStage::Fanout, PeerAttemptOutcome::Miss, 0.006),
            (
                PeerAttemptStage::FillLock,
                PeerAttemptOutcome::Shared,
                0.008,
            ),
            (
                PeerAttemptStage::PeerOpen,
                PeerAttemptOutcome::Success,
                0.012,
            ),
        ];

        // Act.
        for (stage, outcome, duration) in events {
            metrics.record_peer_attempt(stage, outcome, duration);
        }
        metrics.record_peer_probe_cancellations(2);
        let body = metrics.scrape();

        // Assert.
        for (stage, outcome) in [
            ("preflight", "failed"),
            ("fanout", "miss"),
            ("fill_lock", "shared"),
            ("peer_open", "success"),
        ] {
            assert!(body.contains(&format!(
                "cc_peer_attempt_total{{stage=\"{stage}\",outcome=\"{outcome}\"}} 1"
            )));
            assert!(body.contains(&format!(
                "cc_peer_attempt_seconds_count{{stage=\"{stage}\",outcome=\"{outcome}\"}} 1"
            )));
        }
        assert!(body.contains("cc_peer_probe_cancellations_total 2"));
    }

    #[test]
    fn peer_probe_in_flight_guard_decrements_when_dropped() {
        // Arrange.
        let metrics = Arc::new(Prometheus::new("cc_"));

        // Act.
        let active = {
            let _guard = PeerProbeInFlightGuard::from_sinks(None, Some(Arc::clone(&metrics)));
            metrics.scrape()
        };
        let inactive = metrics.scrape();

        // Assert.
        assert!(active.contains("cc_peer_probe_in_flight 1"));
        assert!(inactive.contains("cc_peer_probe_in_flight 0"));
    }

    #[test]
    fn peer_fill_follower_active_guard_decrements_on_drop_and_panic() {
        // Arrange.
        let metrics = Arc::new(Prometheus::new("cc_"));

        // Act.
        let active = {
            let _guard = PeerFillFollowerActiveGuard::from_sinks(None, Some(Arc::clone(&metrics)));
            metrics.scrape()
        };
        let inactive = metrics.scrape();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let metrics = Arc::clone(&metrics);
            move || {
                let _guard = PeerFillFollowerActiveGuard::from_sinks(None, Some(metrics));
                panic!("injected follower panic");
            }
        }));
        let after_panic = metrics.scrape();

        // Assert.
        assert!(active.contains("cc_peer_fill_followers_active 1"));
        assert!(inactive.contains("cc_peer_fill_followers_active 0"));
        assert!(after_panic.contains("cc_peer_fill_followers_active 0"));
    }

    #[test]
    fn peer_serve_active_guard_decrements_on_drop_and_panic() {
        // Arrange.
        let metrics = Arc::new(Prometheus::new("cc_"));

        // Act.
        let active = {
            let _guard = PeerServeActiveGuard::from_sinks(None, Some(Arc::clone(&metrics)));
            metrics.scrape()
        };
        let inactive = metrics.scrape();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe({
            let metrics = Arc::clone(&metrics);
            move || {
                let _guard = PeerServeActiveGuard::from_sinks(None, Some(metrics));
                panic!("injected peer serve panic");
            }
        }));
        let after_panic = metrics.scrape();

        // Assert.
        assert!(active.contains("cc_peer_serves_active 1"));
        assert!(inactive.contains("cc_peer_serves_active 0"));
        assert!(after_panic.contains("cc_peer_serves_active 0"));
    }

    #[test]
    fn peer_serve_active_guard_decrements_on_cancellation() {
        use std::future::Future as _;
        use std::task::{Context, Poll, Waker};

        // Arrange.
        let metrics = Arc::new(Prometheus::new("cc_"));
        let task_metrics = Arc::clone(&metrics);
        let mut task = Box::pin(async move {
            let _guard = PeerServeActiveGuard::from_sinks(None, Some(task_metrics));
            std::future::pending::<()>().await;
        });
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(task.as_mut().poll(&mut context), Poll::Pending);
        assert!(metrics.scrape().contains("cc_peer_serves_active 1"));

        // Act.
        drop(task);

        // Assert.
        assert!(metrics.scrape().contains("cc_peer_serves_active 0"));
    }

    #[test]
    fn peer_metrics_use_exact_bounded_results() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        for result in PeerResult::ALL {
            metrics.record_peer_probe(result, 0.004);
        }
        for result in PeerServeResult::ALL {
            metrics.record_peer_serve(result, 1024, 0.002);
        }
        metrics.record_peer_fill(PeerResult::Hit, 4096, 0.012);
        for result in PeerFillFollowerResult::ALL {
            metrics.record_peer_fill_follower_terminal(result);
        }
        metrics.record_peer_fill_follower_bytes(2048);
        metrics.record_peer_directory(3);

        // Act.
        let body = metrics.scrape();

        // Assert.
        for result in [
            "hit",
            "miss",
            "unauthorized",
            "stale",
            "timeout",
            "refused",
            "truncated",
        ] {
            assert!(
                body.contains(&format!("cc_peer_probe_total{{result=\"{result}\"}} 1")),
                "missing bounded probe result {result}"
            );
        }
        for result in [
            "complete",
            "not_found",
            "unauthorized",
            "client_abort",
            "io_failure",
        ] {
            assert!(
                body.contains(&format!("cc_peer_serve_total{{result=\"{result}\"}} 1")),
                "missing bounded serve result {result}"
            );
        }
        for series in [
            "cc_peer_fill_total{result=\"hit\"} 1",
            "cc_peer_fill_bytes_total 4096",
            "cc_peer_fill_follower_bytes_total 2048",
            "cc_peer_fill_seconds_count 1",
            "cc_peer_probe_seconds_count{result=\"hit\"} 1",
            "cc_peer_serve_bytes_total 5120",
            "cc_peer_serve_socket_writable_wait_seconds_count 5",
            "cc_peer_directory_peers 3",
        ] {
            assert!(body.contains(series), "missing {series}");
        }
        for result in [
            "complete",
            "partial",
            "prebyte_fallback",
            "client_abort",
            "io_failure",
        ] {
            assert!(body.contains(&format!(
                "cc_peer_fill_follower_requests_total{{result=\"{result}\"}} 1"
            )));
        }
        assert!(
            body.contains("cc_peer_fill_bytes_total 4096"),
            "local follower bytes must not inflate peer ingress"
        );
    }

    #[test]
    fn peer_fill_follower_counters_are_not_zero_filled_without_events() {
        // Arrange / act.
        let body = Prometheus::new("cc_").scrape();

        // Assert. HELP/TYPE inventory is present, but no counter observation is
        // fabricated before a follower terminal or byte-send event.
        assert!(!body.contains("\ncc_peer_fill_follower_requests_total{"));
        assert!(!body.contains("\ncc_peer_fill_follower_bytes_total "));
    }

    #[test]
    fn peer_metrics_preserve_ucc_prefix() {
        // Arrange.
        let metrics = Prometheus::new("ucc_");
        let mut access = miss_with_upstream();
        access.cache_tier = "peer".to_owned();
        metrics.record(&access);
        metrics.record_peer_probe(PeerResult::Miss, 0.004);
        metrics.record_peer_attempt(PeerAttemptStage::Fanout, PeerAttemptOutcome::Miss, 0.008);
        metrics.record_peer_probe_cancellations(2);
        metrics.peer_probe_in_flight_delta(1);
        metrics.record_peer_fill(PeerResult::Timeout, 0, 0.05);
        metrics.record_peer_fill_follower_terminal(PeerFillFollowerResult::Complete);
        metrics.record_peer_fill_follower_bytes(1024);
        metrics.peer_fill_followers_active_delta(1);
        metrics.peer_serves_active_delta(1);
        metrics.record_peer_serve(PeerServeResult::Complete, 4096, 0.003);
        metrics.record_peer_directory(2);

        // Act.
        let body = metrics.scrape();

        // Assert.
        assert!(body.contains("\nucc_request_tier_total{cache_tier=\"peer\"} 1\n"));
        assert!(body.contains("\nucc_peer_probe_total{result=\"miss\"} 1\n"));
        assert!(body.contains("\nucc_peer_probe_seconds_count{result=\"miss\"} 1\n"));
        assert!(body.contains("\nucc_peer_attempt_total{stage=\"fanout\",outcome=\"miss\"} 1\n"));
        assert!(body.contains("\nucc_peer_probe_cancellations_total 2\n"));
        assert!(body.contains("\nucc_peer_probe_in_flight 1\n"));
        assert!(body.contains("\nucc_peer_fill_total{result=\"timeout\"} 1\n"));
        assert!(body.contains("\nucc_peer_fill_follower_requests_total{result=\"complete\"} 1\n"));
        assert!(body.contains("\nucc_peer_fill_follower_bytes_total 1024\n"));
        assert!(body.contains("\nucc_peer_fill_followers_active 1\n"));
        assert!(body.contains("\nucc_peer_serves_active 1\n"));
        assert!(body.contains("\nucc_peer_serve_total{result=\"complete\"} 1\n"));
        assert!(body.contains("\nucc_peer_serve_bytes_total 4096\n"));
        assert!(body.contains("\nucc_peer_serve_socket_writable_wait_seconds_count 1\n"));
        assert!(body.contains("\nucc_peer_directory_peers 2\n"));
        assert!(!body.contains("\ncc_peer_"));
    }

    #[test]
    fn peer_instruments_emit_prefixed_otlp_series()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        let result = [KeyValue::new("result", PeerResult::Hit.as_str())];
        metrics.peer_probe_total.add(1, &result);
        metrics.peer_probe_seconds.record(0.004, &result);
        metrics.peer_probe_in_flight.add(1, &[]);
        metrics.peer_probe_cancellations_total.add(2, &[]);
        let attempt = [
            KeyValue::new("stage", PeerAttemptStage::PeerOpen.as_str()),
            KeyValue::new("outcome", PeerAttemptOutcome::Success.as_str()),
        ];
        metrics.peer_attempt_total.add(1, &attempt);
        metrics.peer_attempt_seconds.record(0.012, &attempt);
        metrics.peer_fill_total.add(1, &result);
        metrics.peer_fill_bytes_total.add(4096, &[]);
        metrics.peer_fill_seconds.record(0.012, &[]);
        for result in PeerFillFollowerResult::ALL {
            metrics
                .peer_fill_follower_requests_total
                .add(1, &[KeyValue::new("result", result.as_str())]);
        }
        metrics.peer_fill_follower_bytes_total.add(2048, &[]);
        metrics.peer_fill_followers_active.add(1, &[]);
        for result in PeerServeResult::ALL {
            metrics
                .peer_serve_total
                .add(1, &[KeyValue::new("result", result.as_str())]);
        }
        metrics.peer_serve_bytes_total.add(4096, &[]);
        metrics.peer_serves_active.add(1, &[]);
        metrics
            .peer_serve_socket_writable_wait_seconds
            .record(0.003, &[]);
        metrics.peer_directory_peers.record(3, &[]);

        // Act.
        provider.force_flush()?;
        let snapshot = exporter.get_finished_metrics()?;
        let names = exported_names(&snapshot);

        // Assert.
        for expected in [
            "cc_peer_probe_total",
            "cc_peer_probe_seconds",
            "cc_peer_probe_in_flight",
            "cc_peer_probe_cancellations_total",
            "cc_peer_attempt_total",
            "cc_peer_attempt_seconds",
            "cc_peer_fill_total",
            "cc_peer_fill_bytes_total",
            "cc_peer_fill_seconds",
            "cc_peer_fill_follower_requests_total",
            "cc_peer_fill_follower_bytes_total",
            "cc_peer_fill_followers_active",
            "cc_peer_serve_total",
            "cc_peer_serve_bytes_total",
            "cc_peer_serves_active",
            "cc_peer_serve_socket_writable_wait_seconds",
            "cc_peer_directory_peers",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected} in {names:?}"
            );
        }
        let mut follower_results = snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .filter(|metric| metric.name() == "cc_peer_fill_follower_requests_total")
            .flat_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .flat_map(|point| point.attributes())
                    .filter(|attribute| attribute.key.as_str() == "result")
                    .map(|attribute| attribute.value.to_string())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        follower_results.sort();
        assert_eq!(
            follower_results,
            vec![
                "client_abort",
                "complete",
                "io_failure",
                "partial",
                "prebyte_fallback"
            ]
        );
        let mut serve_results = snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .filter(|metric| metric.name() == "cc_peer_serve_total")
            .flat_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .flat_map(|point| point.attributes())
                    .filter(|attribute| attribute.key.as_str() == "result")
                    .map(|attribute| attribute.value.to_string())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        serve_results.sort();
        assert_eq!(
            serve_results,
            vec![
                "client_abort",
                "complete",
                "io_failure",
                "not_found",
                "unauthorized"
            ]
        );
        assert_eq!(
            counter_value(&snapshot, "cc_peer_fill_bytes_total"),
            Some(4096),
            "follower sends must not alter the peer-ingress counter"
        );
        assert_eq!(
            counter_value(&snapshot, "cc_peer_fill_follower_bytes_total"),
            Some(2048)
        );
        Ok(())
    }

    #[test]
    fn downstream_close_metrics_use_exact_bounded_results() {
        // Arrange.
        let metrics = Prometheus::new("cc_");
        for result in DownstreamCloseResult::ALL {
            metrics.record_downstream_close(result);
        }

        // Act.
        let body = metrics.scrape();

        // Assert.
        for result in [
            "peer_closed",
            "closed",
            "alert_timeout",
            "linger_timeout",
            "reset",
            "error",
        ] {
            assert!(
                body.contains(&format!(
                    "cc_downstream_close_total{{result=\"{result}\"}} 1"
                )),
                "missing bounded close result {result}"
            );
        }
    }

    #[test]
    fn downstream_close_instrument_emits_prefixed_otlp_series()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Arrange.
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider = SdkMeterProvider::builder().with_reader(reader).build();
        let metrics = build_instruments(&provider.meter("ovcc"), "cc_");
        for result in DownstreamCloseResult::ALL {
            metrics
                .downstream_close_total
                .add(1, &[KeyValue::new("result", result.as_str())]);
        }

        // Act.
        provider.force_flush()?;
        let snapshot = exporter.get_finished_metrics()?;

        // Assert.
        let mut results = snapshot
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .filter(|metric| metric.name() == "cc_downstream_close_total")
            .flat_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .flat_map(|point| point.attributes())
                    .filter(|attribute| attribute.key.as_str() == "result")
                    .map(|attribute| attribute.value.to_string())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        results.sort();
        assert_eq!(
            results,
            vec![
                "alert_timeout",
                "closed",
                "error",
                "linger_timeout",
                "peer_closed",
                "reset"
            ]
        );
        Ok(())
    }

    #[test]
    fn prometheus_ucc_prefix_is_exact() {
        // Arrange.
        let metrics = Prometheus::new("ucc_");
        metrics.record(&hit_no_upstream());

        // Act.
        let body = metrics.scrape();

        // Assert.
        assert!(body.contains("\nucc_http_requests_total{"));
        assert!(body.contains("\nucc_metric_poll_total 1\n"));
        assert!(!body.contains("\ncc_"));
    }
}
