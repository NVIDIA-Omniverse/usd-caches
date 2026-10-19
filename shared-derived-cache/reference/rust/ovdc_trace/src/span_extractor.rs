// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use http::HeaderMap;
use opentelemetry::global;
use opentelemetry::propagation::Extractor;
use tracing_opentelemetry::{OpenTelemetrySpanExt, SetParentError};

struct HeaderExtractor<'a>(&'a HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }

    fn get_all(&self, key: &str) -> Option<Vec<&str>> {
        let values = self
            .0
            .get_all(key)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect::<Vec<_>>();

        (!values.is_empty()).then_some(values)
    }
}

pub fn apply_distributed_trace(headers: &HeaderMap, span: tracing::Span) -> tracing::Span {
    let parent_context =
        global::get_text_map_propagator(|prop| prop.extract(&HeaderExtractor(headers)));

    match span.set_parent(parent_context) {
        Ok(()) | Err(SetParentError::LayerNotFound) => {}
        Err(error_message) => {
            tracing::debug!(%error_message, "failed to set trace parent to external trace id");
        }
    }

    span
}
