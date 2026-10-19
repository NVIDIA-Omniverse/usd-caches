// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! The per-request access log, one flat JSON object per request, written to
//! stdout under the stable `cloud_backend_log` contract.
//!
//! It is a flat object on its own stdout sink — deliberately not routed through
//! the `tracing` JSON envelope — so there is no `{"timestamp":..,"fields":{..}}`
//! wrapper to defeat the contract. Diagnostics and spans go to stderr instead
//! (see crate docs). The sink is pluggable so the in-process e2e can capture
//! and assert the raw emitted line.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

/// Bounded inactivity stage that terminated a proxied response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutStage {
    OriginConnect,
    OriginResponseHeader,
    OriginResponseBody,
    DownstreamWrite,
}

/// One access-log record with a stable field set, including OTel correlation.
///
/// Every field is always serialized: absent strings render `""`, flags render
/// `false`, and unavailable optional fields render `null`. Downstream
/// parsers therefore never have to handle a missing key.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AccessLog {
    /// Request completion time as `DD/Mon/YYYY:HH:MM:SS +0000`.
    pub access_time: String,
    /// Client IP, no port (`$remote_addr`).
    pub remote_addr: String,
    /// HTTP method serialized under the contract's `request` field.
    pub request: String,
    pub scheme: String,
    /// Client HTTP version (`HTTP/1.1` / `HTTP/2`).
    pub http_protocol: String,
    pub status: u16,
    pub request_uri: String,
    pub host: String,
    pub http_user_agent: String,
    /// Process-wide connection serial.
    pub connection: u64,
    /// One-based request serial on this connection.
    pub connection_requests: u64,
    /// Negotiated TLS version (`$ssl_protocol`).
    pub ssl_protocol: String,
    /// Request `If-Match` validator serialized under `etag`.
    pub etag: String,
    /// ETag of the cached entry served or checked; `null` when unavailable.
    pub cached_etag: Option<String>,
    /// ETag from the upstream fetch or HIT check; `null` when unavailable.
    pub upstream_etag: Option<String>,
    /// Request `Range` header.
    pub request_range: String,
    /// Client `X-Request-Id`; legacy correlation, superseded by `trace_id`/`span_id`.
    pub http_x_request_id: String,
    /// Upstream address; empty when no origin was contacted (a HIT).
    pub upstream: String,
    /// Origin status; `null` when no origin was contacted (a HIT).
    pub upstream_status: Option<u16>,
    /// Origin `Location` response header; empty when absent or no origin was contacted.
    pub upstream_http_location: String,
    pub upstream_connect_time: Option<f64>,
    pub upstream_header_time: Option<f64>,
    pub upstream_response_time: Option<f64>,
    /// Timeout stage that terminated the request, or `null` on normal completion.
    pub timeout_stage: Option<TimeoutStage>,
    pub upstream_cache_status: String,
    /// Outcome of local storage after a cache MISS.
    ///
    /// Empty when no cache-store operation applied (for example, a HIT).
    pub cache_store_result: String,
    /// Cacheable MISS source (`origin` or `peer`); empty on other responses.
    pub cache_tier: String,
    /// Total request wall-clock seconds (the proxy measures with `Instant`).
    pub request_time: f64,
    pub bytes_sent: u64,
    /// Request head bytes through the terminating CRLF pair.
    pub request_length: u64,
    /// Whether the request carried an `Authorization` header (never the token).
    pub has_auth_header: bool,
    /// Bounded terminal result of presigned authorization; empty when no check ran.
    pub presigned_auth_outcome: String,
    pub bursty_cache: bool,
    pub mutation_detected: bool,
    pub disable_cache: bool,
    /// Serving pod hostname (`$hostname`).
    pub pod_name: String,
    /// Worker process id (`$pid`).
    pub pid: u32,
    /// Engine version.
    pub version: String,
    /// W3C ids of the request span and its remote parent.
    pub otel_trace_id: Option<String>,
    pub otel_span_id: Option<String>,
    pub otel_parent_id: Option<String>,
}

/// Renders UTC as `DD/Mon/YYYY:HH:MM:SS +0000`.
///
/// OVCC containers run in UTC. Rendering directly avoids a process-global local
/// timezone lookup on every request.
pub fn now_access_time() -> String {
    let now = time::OffsetDateTime::now_utc();
    let month = match now.month() {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    };
    format!(
        "{:02}/{month}/{:04}:{:02}:{:02}:{:02} +0000",
        now.day(),
        now.year(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

static SINK: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();

fn sink() -> &'static Mutex<Box<dyn Write + Send>> {
    SINK.get_or_init(|| Mutex::new(Box::new(std::io::stdout())))
}

/// Replaces the access-log sink (stdout by default). Used by tests to capture.
pub fn set_writer(writer: Box<dyn Write + Send>) {
    *sink().lock().expect("access-log sink poisoned") = writer;
}

/// Serializes `log` (filling OTel trace/span ids from the current span when
/// absent) and writes it as one JSON line to the sink.
pub fn emit(log: &AccessLog) {
    let mut record = log.clone();
    if record.otel_trace_id.is_none()
        && let Some((trace_id, span_id)) = current_trace_ids()
    {
        record.otel_trace_id = Some(trace_id);
        record.otel_span_id = Some(span_id);
    }
    if let Ok(line) = serde_json::to_string(&record) {
        let mut sink = sink().lock().expect("access-log sink poisoned");
        let _ = writeln!(sink, "{line}");
        let _ = sink.flush();
    }
}

/// The current span's `(trace_id, span_id)` when a valid OTel context exists.
fn current_trace_ids() -> Option<(String, String)> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let context = tracing::Span::current().context();
    let span = context.span();
    let span_context = span.span_context();
    if span_context.is_valid() {
        Some((
            span_context.trace_id().to_string(),
            span_context.span_id().to_string(),
        ))
    } else {
        None
    }
}

/// Test-only capture sink, installed with [`install_capture`], then read via
/// [`CaptureHandle::lines`] after driving requests.
#[derive(Clone)]
pub struct CaptureHandle(Arc<Mutex<Vec<u8>>>);

impl CaptureHandle {
    /// The emitted access-log lines so far (one JSON object each).
    pub fn lines(&self) -> Vec<String> {
        let buf = self.0.lock().expect("capture buffer poisoned");
        String::from_utf8_lossy(&buf)
            .lines()
            .map(|line| line.to_owned())
            .collect()
    }
}

struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("capture buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Redirects the access-log sink into an in-memory buffer and returns a handle
/// to read it. For in-process tests only.
pub fn install_capture() -> CaptureHandle {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    set_writer(Box::new(SharedWriter(buffer.clone())));
    CaptureHandle(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_backend_log_serializes_complete_contract() {
        // Arrange.
        let log = AccessLog {
            access_time: "27/Jun/2026:00:00:00 +0000".to_owned(),
            remote_addr: "10.0.0.1".to_owned(),
            request: "GET".to_owned(),
            scheme: "https".to_owned(),
            http_protocol: "HTTP/1.1".to_owned(),
            status: 200,
            request_uri: "/objects/a.usd?sig=abc".to_owned(),
            host: "bucket.example.com".to_owned(),
            http_user_agent: "ovcc-test".to_owned(),
            connection: 7,
            connection_requests: 2,
            ssl_protocol: "TLSv1.3".to_owned(),
            upstream: "10.0.0.2:443".to_owned(),
            upstream_status: Some(200),
            upstream_http_location: "/redirect".to_owned(),
            upstream_connect_time: Some(0.001),
            upstream_header_time: Some(0.002),
            upstream_response_time: Some(0.003),
            timeout_stage: Some(TimeoutStage::OriginResponseBody),
            upstream_cache_status: "MISS".to_owned(),
            cache_store_result: "STORED".to_owned(),
            cache_tier: "origin".to_owned(),
            request_time: 0.012,
            bytes_sent: 1024,
            etag: "\"etag\"".to_owned(),
            cached_etag: Some("W/\"cached\"".to_owned()),
            upstream_etag: Some("\"origin\"".to_owned()),
            request_range: "bytes=0-9".to_owned(),
            has_auth_header: true,
            presigned_auth_outcome: "authorized_uncached".to_owned(),
            http_x_request_id: "request-id".to_owned(),
            pod_name: "ovcc-0".to_owned(),
            pid: 123,
            request_length: 256,
            version: "4.3.0".to_owned(),
            bursty_cache: false,
            disable_cache: false,
            mutation_detected: false,
            otel_trace_id: Some("00000000000000000000000000000001".to_owned()),
            otel_span_id: Some("0000000000000002".to_owned()),
            otel_parent_id: Some("0000000000000003".to_owned()),
        };

        // Act.
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();

        // Assert.
        let expected_keys = [
            "access_time",
            "remote_addr",
            "request",
            "scheme",
            "status",
            "request_uri",
            "host",
            "http_user_agent",
            "connection",
            "connection_requests",
            "ssl_protocol",
            "http_protocol",
            "upstream",
            "upstream_status",
            "upstream_cache_status",
            "cache_store_result",
            "cache_tier",
            "upstream_http_location",
            "upstream_connect_time",
            "upstream_header_time",
            "upstream_response_time",
            "timeout_stage",
            "request_time",
            "bytes_sent",
            "etag",
            "cached_etag",
            "upstream_etag",
            "request_range",
            "has_auth_header",
            "presigned_auth_outcome",
            "http_x_request_id",
            "pod_name",
            "pid",
            "request_length",
            "version",
            "bursty_cache",
            "disable_cache",
            "mutation_detected",
            "otel_trace_id",
            "otel_span_id",
            "otel_parent_id",
        ];
        let actual_keys = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actual_keys,
            expected_keys.into_iter().collect(),
            "cloud_backend_log key set changed"
        );
        assert_eq!(json["request"], "GET");
        assert_eq!(json["status"], 200);
        assert_eq!(json["connection"], 7);
        assert_eq!(json["connection_requests"], 2);
        assert_eq!(json["request_length"], 256);
        assert_eq!(json["pid"], 123);
        assert_eq!(json["cache_store_result"], "STORED");
        assert_eq!(json["cache_tier"], "origin");
        assert_eq!(json["etag"], "\"etag\"");
        assert_eq!(json["cached_etag"], "W/\"cached\"");
        assert_eq!(json["upstream_etag"], "\"origin\"");
        assert_eq!(json["timeout_stage"], "origin_response_body");
        assert!(json["request_time"].is_f64());
        assert!(json["has_auth_header"].is_boolean());
        assert_eq!(json["presigned_auth_outcome"], "authorized_uncached");
        assert!(json.get("authorization").is_none());
        assert!(json.get("http_authorization").is_none());
    }

    #[test]
    fn access_get_validator_fields_preserve_absent_and_empty() -> Result<(), serde_json::Error> {
        // Arrange.
        let absent = AccessLog::default();
        let empty = AccessLog {
            cached_etag: Some(String::new()),
            upstream_etag: Some(String::new()),
            ..AccessLog::default()
        };

        // Act.
        let absent = serde_json::to_value(absent)?;
        let empty = serde_json::to_value(empty)?;

        // Assert.
        for field in ["cached_etag", "upstream_etag"] {
            assert_eq!(absent.get(field), Some(&serde_json::Value::Null));
            assert_eq!(empty[field], "");
        }
        Ok(())
    }

    #[test]
    fn access_time_parses_contract_format() {
        // Arrange.
        let format = time::macros::format_description!(
            "[day]/[month repr:short]/[year]:[hour]:[minute]:[second] \
             [offset_hour sign:mandatory][offset_minute]"
        );

        // Act.
        let parsed = time::OffsetDateTime::parse(&now_access_time(), format);

        // Assert.
        assert!(parsed.is_ok(), "access_time must parse its contract format");
    }

    #[test]
    fn capture_sink_collects_emitted_lines() {
        // Arrange.
        let capture = install_capture();

        // Act.
        emit(&AccessLog {
            access_time: now_access_time(),
            request: "GET".to_owned(),
            status: 200,
            request_uri: "/x".to_owned(),
            host: "h".to_owned(),
            upstream_cache_status: "MISS".to_owned(),
            cache_store_result: "STORAGE_ERROR".to_owned(),
            ..Default::default()
        });
        let lines = capture.lines();

        // Assert.
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(parsed["upstream_cache_status"], "MISS");
        assert_eq!(parsed["cache_store_result"], "STORAGE_ERROR");
    }

    #[test]
    fn optional_diagnostic_strings_default_to_empty() {
        // Arrange.
        let log = AccessLog::default();

        // Act.
        let json = serde_json::to_value(log);

        // Assert.
        assert_eq!(
            json.as_ref()
                .ok()
                .and_then(|value| value.get("cache_tier"))
                .and_then(serde_json::Value::as_str),
            Some("")
        );
        assert_eq!(
            json.as_ref()
                .ok()
                .and_then(|value| value.get("presigned_auth_outcome"))
                .and_then(serde_json::Value::as_str),
            Some("")
        );
    }
}
