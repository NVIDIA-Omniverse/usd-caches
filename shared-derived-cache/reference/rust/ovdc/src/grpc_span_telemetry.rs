// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! gRPC server call metrics driven by [`tower_http::trace::TraceLayer`] hooks.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tower_http::{
    classify::GrpcFailureClass,
    trace::{OnEos, OnFailure, OnRequest, OnResponse},
};

use crate::count_connections::ConnectionState;
use crate::grpc::metrics;

use ovdc_trace::metrics::Measure;

fn grpc_method_label(path: &str) -> &'static str {
    match path {
        "/ovdc.KeyValue/Get" => "ovdc.KeyValue/Get",
        "/ovdc.KeyValue/Put" => "ovdc.KeyValue/Put",
        "/ovdc.KeyValue/Touch" => "ovdc.KeyValue/Touch",
        _ => "other",
    }
}

/// Canonical gRPC status code name per `google.rpc.Code` (A66 `grpc.status` label).
fn status_name(code: u32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        11 => "OUT_OF_RANGE",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        15 => "DATA_LOSS",
        16 => "UNAUTHENTICATED",
        _ => "UNKNOWN",
    }
}

/// Parse `grpc-status` from response trailers. Missing header is treated as OK per gRPC spec.
fn status_from_trailers(trailers: Option<&http::HeaderMap>) -> &'static str {
    let Some(t) = trailers else {
        return "OK";
    };
    let Some(v) = t.get("grpc-status") else {
        return "OK";
    };
    let code = v
        .to_str()
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    status_name(code)
}

/// Entries older than this are assumed to belong to an RPC that ended without
/// reaching either flush hook (for example a client that vanished mid-response).
/// An RPC still running after this long loses its payload-size metrics, which is
/// the right trade against growing without bound.
const MAX_ENTRY_AGE: Duration = Duration::from_secs(300);

/// Only consider sweeping once the map is at least this large.
const SWEEP_THRESHOLD: usize = 4096;

/// Minimum time between sweeps.
///
/// Size alone is not enough of a gate: a map held above the threshold by live
/// entries would otherwise run an O(n) scan on *every* byte event, and one that
/// removes nothing because the entries are younger than [`MAX_ENTRY_AGE`]. That
/// costs far more than the leak it is meant to bound.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The process-wide byte accumulator.
///
/// The measuring codec's hook is process-wide and last-writer-wins, so a process
/// running more than one server (the test suite) would otherwise record one
/// server's bytes into another's map and never find them again at flush time.
/// Sharing one map makes that harmless: attribution is by `tracing` span id,
/// which is unique across the process, so each server still flushes exactly its
/// own spans.
static SHARED: std::sync::OnceLock<GrpcByteAccumulator> = std::sync::OnceLock::new();

/// Per-RPC accumulated protobuf payload sizes (from the measuring codec), keyed by trace span id.
#[derive(Clone, Default)]
pub(crate) struct GrpcByteAccumulator {
    inner: Arc<Mutex<ByteTotals>>,
}

#[derive(Default)]
struct ByteTotals {
    by_span: HashMap<tracing::span::Id, RpcProtoBytes>,
    last_sweep: Option<Instant>,
}

#[derive(Clone, Copy)]
struct RpcProtoBytes {
    sent: u64,
    rcvd: u64,
    first_seen: Instant,
}

impl GrpcByteAccumulator {
    /// Returns the process-wide accumulator.
    pub(crate) fn shared() -> Self {
        SHARED.get_or_init(Self::default).clone()
    }

    /// Starts tracking `id` as a server request span.
    ///
    /// Only spans opened here accumulate bytes. The measuring codec's hook is
    /// installed process-wide and fires for *client* stubs too (the generated
    /// client uses the same codec), and those spans never reach a server flush
    /// hook -- so accepting them would both misattribute client traffic into
    /// server payload histograms and leak an entry per outbound call.
    pub(crate) fn begin(&self, id: tracing::span::Id) {
        self.inner.lock().by_span.insert(
            id,
            RpcProtoBytes {
                sent: 0,
                rcvd: 0,
                first_seen: Instant::now(),
            },
        );
    }

    fn accumulate(&self, id: tracing::span::Id, f: impl FnOnce(&mut RpcProtoBytes)) {
        let now = Instant::now();
        let mut g = self.inner.lock();

        // Not a registered server request span -- ignore rather than create an
        // entry nothing will ever flush.
        let Some(entry) = g.by_span.get_mut(&id) else {
            return;
        };
        f(entry);

        // A request whose span reaches neither flush hook still leaves its entry
        // behind, so bound the map by age as a backstop. Gated on both size and
        // elapsed time -- see SWEEP_INTERVAL for why size alone is a trap.
        if g.by_span.len() <= SWEEP_THRESHOLD {
            return;
        }
        let due = g
            .last_sweep
            .is_none_or(|last| now.duration_since(last) >= SWEEP_INTERVAL);
        if !due {
            return;
        }

        let before = g.by_span.len();
        g.by_span
            .retain(|_, v| now.duration_since(v.first_seen) < MAX_ENTRY_AGE);
        g.last_sweep = Some(now);

        let dropped = before - g.by_span.len();
        if dropped > 0 {
            tracing::debug!(dropped, "swept unflushed grpc byte accumulator entries");
        }
    }

    pub(crate) fn add_sent(&self, id: tracing::span::Id, n: u64) {
        if n == 0 {
            return;
        }
        self.accumulate(id, |e| e.sent = e.sent.saturating_add(n));
    }

    pub(crate) fn add_rcvd(&self, id: tracing::span::Id, n: u64) {
        if n == 0 {
            return;
        }
        self.accumulate(id, |e| e.rcvd = e.rcvd.saturating_add(n));
    }

    /// Emit histograms for this RPC and drop accumulated state.
    pub(crate) fn flush(&self, id: &tracing::span::Id, method: &'static str, status: &'static str) {
        let Some(t) = self.inner.lock().by_span.remove(id) else {
            return;
        };
        if t.sent > 0 {
            metrics::GRPC_SERVER_CALL_SENT_BYTES.measure(
                t.sent as f64,
                &ovdc_trace::label!("grpc.method": method, "grpc.status": status),
            );
        }
        if t.rcvd > 0 {
            metrics::GRPC_SERVER_CALL_RCVD_BYTES.measure(
                t.rcvd as f64,
                &ovdc_trace::label!("grpc.method": method, "grpc.status": status),
            );
        }
    }
}

/// Shared maps keyed by tracing span id for [`tower_http::trace::Trace`] gRPC hooks.
#[derive(Clone)]
pub(crate) struct GrpcSpanTelemetry {
    method_by_span: Arc<parking_lot::Mutex<HashMap<tracing::span::Id, &'static str>>>,
    to_header_latency: Arc<Mutex<HashMap<tracing::span::Id, Duration>>>,
    pub(crate) byte_totals: GrpcByteAccumulator,
}

impl Default for GrpcSpanTelemetry {
    fn default() -> Self {
        Self {
            method_by_span: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            to_header_latency: Arc::new(Mutex::new(HashMap::new())),
            byte_totals: GrpcByteAccumulator::shared(),
        }
    }
}

/// [`OnRequest`] hook: call started, connection fields on the span.
#[derive(Clone)]
pub(crate) struct GrpcTraceOnRequest {
    pub(crate) telemetry: GrpcSpanTelemetry,
}

impl<B> OnRequest<B> for GrpcTraceOnRequest {
    fn on_request(&mut self, request: &http::Request<B>, span: &tracing::Span) {
        let path = request.uri().path();
        let method = grpc_method_label(path);
        if let Some(id) = span.id() {
            self.telemetry
                .method_by_span
                .lock()
                .insert(id.clone(), method);
            // Marks this span as server-side, so the process-wide codec hook
            // accumulates its bytes and ignores everything else.
            self.telemetry.byte_totals.begin(id);
        }
        metrics::GRPC_SERVER_CALL_STARTED.measure(1, &ovdc_trace::label!("grpc.method": method));

        let connection_state = request.extensions().get::<ConnectionState>().unwrap();
        span.record("c", connection_state.c_id().as_ref());
        span.record("r", connection_state.r_id());

        tracing::trace!("request.begin");
    }
}

/// [`OnResponse`] hook: record time until the response is ready (headers).
#[derive(Clone)]
pub(crate) struct GrpcTraceOnResponse {
    pub(crate) telemetry: GrpcSpanTelemetry,
}

impl<B> OnResponse<B> for GrpcTraceOnResponse {
    fn on_response(self, _response: &http::Response<B>, latency: Duration, span: &tracing::Span) {
        if let Some(id) = span.id() {
            self.telemetry.to_header_latency.lock().insert(id, latency);
        }
    }
}

/// [`OnEos`] hook: full RPC duration for successful streams.
#[derive(Clone)]
pub(crate) struct GrpcTraceOnEos {
    pub(crate) telemetry: GrpcSpanTelemetry,
}

impl OnEos for GrpcTraceOnEos {
    fn on_eos(
        self,
        trailers: Option<&http::HeaderMap>,
        stream_duration: Duration,
        span: &tracing::Span,
    ) {
        tracing::trace!(?stream_duration, "request.end_of_stream");
        let Some(id) = span.id() else {
            return;
        };
        let method = self
            .telemetry
            .method_by_span
            .lock()
            .remove(&id)
            .unwrap_or("other");
        let status = status_from_trailers(trailers);
        self.telemetry.byte_totals.flush(&id, method, status);
        let to_header = self.telemetry.to_header_latency.lock().remove(&id);
        let total = to_header
            .map(|h| h + stream_duration)
            .unwrap_or(stream_duration);
        metrics::GRPC_SERVER_CALL_DURATION.measure(
            total.as_secs_f64(),
            &ovdc_trace::label!("grpc.method": method, "grpc.status": status),
        );
    }
}

/// [`OnFailure`] hook: RPC duration when the request fails at the HTTP layer.
#[derive(Clone)]
pub(crate) struct GrpcTraceOnFailure {
    pub(crate) telemetry: GrpcSpanTelemetry,
}

impl OnFailure<GrpcFailureClass> for GrpcTraceOnFailure {
    fn on_failure(&mut self, failure: GrpcFailureClass, latency: Duration, span: &tracing::Span) {
        tracing::trace!(?latency, ?failure, "request.failure");
        let status = match &failure {
            GrpcFailureClass::Status(s) => status_name(s.code_raw() as u32),
            GrpcFailureClass::Error(_) => "UNKNOWN",
            // `GrpcFailureClass` is `#[non_exhaustive]` as of tower-http 0.7.
            _ => "UNKNOWN",
        };
        let method = if let Some(id) = span.id() {
            let m = self
                .telemetry
                .method_by_span
                .lock()
                .remove(&id)
                .unwrap_or("other");
            self.telemetry.byte_totals.flush(&id, m, status);
            m
        } else {
            "other"
        };
        if let Some(id) = span.id() {
            self.telemetry.to_header_latency.lock().remove(&id);
        }
        metrics::GRPC_SERVER_CALL_DURATION.measure(
            latency.as_secs_f64(),
            &ovdc_trace::label!("grpc.method": method, "grpc.status": status),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u64) -> tracing::span::Id {
        tracing::span::Id::from_u64(n)
    }

    fn totals(acc: &GrpcByteAccumulator, id: &tracing::span::Id) -> Option<(u64, u64)> {
        acc.inner.lock().by_span.get(id).map(|t| (t.sent, t.rcvd))
    }

    fn is_empty(acc: &GrpcByteAccumulator) -> bool {
        acc.inner.lock().by_span.is_empty()
    }

    #[test]
    fn bytes_for_an_unregistered_span_are_dropped() {
        // The codec's hook is process-wide and fires for generated *client*
        // stubs too. Those spans never reach a server flush hook, so accepting
        // them would both pollute the server histograms and leak one entry per
        // outbound call.
        let acc = GrpcByteAccumulator::default();

        acc.add_sent(id(1), 100);
        acc.add_rcvd(id(1), 200);

        assert_eq!(totals(&acc, &id(1)), None);
        assert!(is_empty(&acc));
    }

    #[test]
    fn bytes_for_a_registered_span_accumulate() {
        let acc = GrpcByteAccumulator::default();
        acc.begin(id(1));

        acc.add_sent(id(1), 100);
        acc.add_sent(id(1), 50);
        acc.add_rcvd(id(1), 7);

        assert_eq!(totals(&acc, &id(1)), Some((150, 7)));
    }

    #[test]
    fn concurrent_spans_do_not_cross_contaminate() {
        // The defect this guards: under concurrent RPCs one call's bytes were
        // attributed to whichever span decoded most recently.
        let acc = GrpcByteAccumulator::default();
        acc.begin(id(1));
        acc.begin(id(2));

        acc.add_sent(id(1), 10);
        acc.add_sent(id(2), 999);

        assert_eq!(totals(&acc, &id(1)), Some((10, 0)));
        assert_eq!(totals(&acc, &id(2)), Some((999, 0)));
    }

    #[test]
    fn flush_removes_the_entry_and_is_idempotent() {
        let acc = GrpcByteAccumulator::default();
        acc.begin(id(1));
        acc.add_sent(id(1), 10);

        acc.flush(&id(1), "ovdc.KeyValue/Touch", "OK");
        assert_eq!(totals(&acc, &id(1)), None);

        acc.flush(&id(1), "ovdc.KeyValue/Touch", "OK");
        assert!(is_empty(&acc));
    }

    #[test]
    fn bytes_arriving_after_flush_are_dropped() {
        // Ordering matters: a late byte event must not resurrect an entry that
        // no hook will ever remove again.
        let acc = GrpcByteAccumulator::default();
        acc.begin(id(1));
        acc.flush(&id(1), "ovdc.KeyValue/Touch", "OK");

        acc.add_sent(id(1), 10);

        assert!(is_empty(&acc));
    }

    #[test]
    fn sweep_waits_for_its_interval_then_removes_expired_entries() {
        let acc = GrpcByteAccumulator::default();
        for n in 1..=SWEEP_THRESHOLD + 1 {
            acc.begin(id(n as u64));
        }

        let expired = id(1);
        let live = id(2);
        {
            let mut totals = acc.inner.lock();
            totals.by_span.get_mut(&expired).unwrap().first_seen = Instant::now() - MAX_ENTRY_AGE;
            totals.last_sweep = Some(Instant::now());
        }

        // An expired entry remains until the sweep interval elapses.
        acc.add_sent(live.clone(), 1);
        assert!(totals(&acc, &expired).is_some());

        acc.inner.lock().last_sweep = Some(Instant::now() - SWEEP_INTERVAL);
        acc.add_sent(live.clone(), 1);

        assert_eq!(totals(&acc, &expired), None);
        assert_eq!(totals(&acc, &live), Some((2, 0)));
    }

    #[test]
    fn shared_hands_out_one_process_wide_map() {
        // The codec's hook is last-writer-wins, so in a process running more
        // than one server, one server's bytes routinely arrive through
        // another's hook. Sharing the map is what makes that harmless.
        let a = GrpcByteAccumulator::shared();
        let b = GrpcByteAccumulator::shared();

        assert!(Arc::ptr_eq(&a.inner, &b.inner));
    }
}
