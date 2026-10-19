// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! W3C trace-context propagation over the proxy's `Vec<(String, String)>`
//! header representation.
//!
//! On the inbound path, [`set_parent_from_headers`] reads `traceparent`/`tracestate`
//! from a client request and makes the current span a child of the caller's trace.
//! On the outbound path, [`inject_current`] writes the current span's context onto an
//! upstream request's headers so the origin sees one continuous trace.

use opentelemetry::propagation::{Extractor, Injector};

/// Reads header values by case-insensitive name from the proxy's header slice.
struct HeaderSliceExtractor<'a>(&'a [(String, String)]);

impl Extractor for HeaderSliceExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// Appends propagation headers onto an outbound header list.
struct HeaderVecInjector<'a>(&'a mut Vec<(String, String)>);

impl Injector for HeaderVecInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.push((key.to_owned(), value));
    }
}

/// Extracts the W3C context carried by `headers` (empty context if none).
pub fn extract_context(headers: &[(String, String)]) -> opentelemetry::Context {
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderSliceExtractor(headers))
    })
}

/// Makes `span` a child of the trace carried by the inbound `headers`. A no-op
/// when no valid `traceparent` is present, so an un-traced client simply starts
/// a fresh root span.
pub fn set_parent_from_headers(span: &tracing::Span, headers: &[(String, String)]) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    // The result only signals whether the span carried the OTel extension; a
    // miss is fine (the span simply starts a fresh root).
    let _ = span.set_parent(extract_context(headers));
}

/// Returns the valid remote parent's span id carried by inbound trace context.
pub fn parent_span_id(headers: &[(String, String)]) -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;

    let context = extract_context(headers);
    let span = context.span();
    let span_context = span.span_context();
    (span_context.is_valid() && span_context.is_remote())
        .then(|| span_context.span_id().to_string())
}

/// Injects the current span's W3C context onto `headers` for an outbound call.
pub fn inject_current(headers: &mut Vec<(String, String)>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let context = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut HeaderVecInjector(headers));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extractor_is_case_insensitive() {
        let headers = vec![("TraceParent".to_owned(), "00-abc-def-01".to_owned())];
        let extractor = HeaderSliceExtractor(&headers);
        assert_eq!(extractor.get("traceparent"), Some("00-abc-def-01"));
        assert_eq!(extractor.get("missing"), None);
    }

    #[test]
    fn injector_appends_headers() {
        let mut headers = Vec::new();
        let mut injector = HeaderVecInjector(&mut headers);
        injector.set("traceparent", "00-abc-def-01".to_owned());
        assert_eq!(
            headers,
            vec![("traceparent".to_owned(), "00-abc-def-01".to_owned())]
        );
    }
}
