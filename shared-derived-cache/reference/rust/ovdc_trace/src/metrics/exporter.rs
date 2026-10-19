// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use opentelemetry_sdk::metrics::{reader::MetricReader, ManualReader, Temporality};

#[derive(Default, Debug, Clone)]
pub struct MeterReader {
    inner: Arc<ManualReader>,
}

impl MetricReader for MeterReader {
    fn register_pipeline(&self, pipeline: std::sync::Weak<opentelemetry_sdk::metrics::Pipeline>) {
        self.inner.register_pipeline(pipeline);
    }

    fn collect(
        &self,
        rm: &mut opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        self.inner.collect(rm)
    }

    fn force_flush(&self) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        self.inner.force_flush()
    }

    fn shutdown(&self) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        Ok(())
    }

    fn temporality(&self, kind: opentelemetry_sdk::metrics::InstrumentKind) -> Temporality {
        self.inner.temporality(kind)
    }
}
