// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Weak;
use std::time::Duration;

use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::{
    error::{OTelSdkError, OTelSdkResult},
    logs::{
        BatchConfigBuilder as LogBatchConfigBuilder, BatchLogProcessor, LogProcessor, SdkLogRecord,
    },
    metrics::{
        data::ResourceMetrics, reader::MetricReader, InstrumentKind, PeriodicReader, Pipeline,
        Temporality,
    },
    trace::{BatchConfigBuilder, BatchSpanProcessor, Span, SpanData, SpanProcessor},
    Resource,
};

use crate::{ExporterKind, LogConfig, MetricConfig, OtlpProtocol, OtlpVars, TraceConfig};

/// Wraps a boxed log processor, forwarding all [`LogProcessor`] methods.
pub(crate) struct Logs(pub Box<dyn LogProcessor + Send + Sync>);

impl std::fmt::Debug for Logs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logs")
    }
}

impl LogProcessor for Logs {
    fn emit(&self, data: &mut SdkLogRecord, instrumentation: &opentelemetry::InstrumentationScope) {
        self.0.emit(data, instrumentation);
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn shutdown(&self) -> OTelSdkResult {
        self.0.shutdown()
    }
    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        self.0.event_enabled(level, target, name)
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

/// Wraps a boxed span processor, forwarding all [`SpanProcessor`] methods.
pub(crate) struct Spans(pub Box<dyn SpanProcessor + Send + Sync>);

impl std::fmt::Debug for Spans {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Spans")
    }
}

impl SpanProcessor for Spans {
    fn on_start(&self, span: &mut Span, cx: &opentelemetry::Context) {
        self.0.on_start(span, cx);
    }
    fn on_end(&self, span: SpanData) {
        self.0.on_end(span);
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }
    fn shutdown(&self) -> OTelSdkResult {
        self.0.shutdown()
    }
    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

/// Wraps a boxed metric reader, forwarding all [`MetricReader`] methods.
pub(crate) struct Metrics(pub Box<dyn MetricReader + Send + Sync>);

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Metrics")
    }
}

impl MetricReader for Metrics {
    fn register_pipeline(&self, pipe: Weak<Pipeline>) {
        self.0.register_pipeline(pipe);
    }
    fn collect(&self, rm: &mut ResourceMetrics) -> Result<(), OTelSdkError> {
        self.0.collect(rm)
    }
    fn force_flush(&self) -> Result<(), OTelSdkError> {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, timeout: Duration) -> Result<(), OTelSdkError> {
        self.0.shutdown_with_timeout(timeout)
    }
    fn shutdown(&self) -> Result<(), OTelSdkError> {
        self.0.shutdown()
    }
    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

/// Build a log processor from config, or return `None` to clear.
pub(crate) fn build_log_processor(cfg: &LogConfig) -> Option<Box<dyn LogProcessor + Send + Sync>> {
    let log_batch_config = LogBatchConfigBuilder::default()
        .with_scheduled_delay(*cfg.processor.schedule_delay)
        .with_max_queue_size(cfg.processor.max_queue_size as usize)
        .with_max_export_batch_size(cfg.processor.max_batch_size as usize)
        .build();

    match cfg.exporter {
        ExporterKind::Otlp => {
            let export_config = ex_cfg(cfg.exporter, &cfg.otlp)?;
            match cfg.otlp.protocol {
                OtlpProtocol::Grpc => {
                    let exporter = apply_export_config(
                        opentelemetry_otlp::LogExporter::builder().with_tonic(),
                        export_config,
                    )
                    .build()
                    .expect("invalid otlp log exporter settings");
                    let processor = BatchLogProcessor::builder(exporter)
                        .with_batch_config(log_batch_config)
                        .build();
                    tracing::info!("reloaded OTLP gRPC log processor");
                    Some(Box::new(processor))
                }
                OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
                    tracing::warn!("OTLP HTTP log exporter is not available in this build");
                    None
                }
                _ => None,
            }
        }
        ExporterKind::Console => {
            tracing::warn!("console log exporter is not available in this build");
            None
        }
        _ => None,
    }
}

/// Build a span processor from config, or return `None` to clear.
pub(crate) fn build_span_processor(
    cfg: &TraceConfig,
) -> Option<Box<dyn SpanProcessor + Send + Sync>> {
    let trace_batch_config = BatchConfigBuilder::default()
        .with_scheduled_delay(*cfg.processor.schedule_delay)
        .with_max_queue_size(cfg.processor.max_queue_size as usize)
        .with_max_export_batch_size(cfg.processor.max_batch_size as usize)
        .build();

    match cfg.exporter {
        ExporterKind::Otlp => {
            let export_config = ex_cfg(cfg.exporter, &cfg.otlp)?;
            match cfg.otlp.protocol {
                OtlpProtocol::Grpc => {
                    let exporter = apply_export_config(
                        opentelemetry_otlp::SpanExporter::builder().with_tonic(),
                        export_config,
                    )
                    .build()
                    .expect("failed to build span exporter");
                    let processor = BatchSpanProcessor::builder(exporter)
                        .with_batch_config(trace_batch_config)
                        .build();
                    tracing::info!("reloaded OTLP gRPC trace processor");
                    Some(Box::new(processor))
                }
                OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
                    tracing::warn!("OTLP HTTP trace exporter is not available in this build");
                    None
                }
                _ => None,
            }
        }
        ExporterKind::Console => {
            tracing::warn!("console trace exporter is not available in this build");
            None
        }
        _ => None,
    }
}

/// Build a metric reader from config, or return `None` to clear.
pub(crate) fn build_metric_reader(
    cfg: &MetricConfig,
) -> Option<Box<dyn MetricReader + Send + Sync>> {
    match cfg.exporter {
        ExporterKind::Otlp => {
            let export_config = ex_cfg(cfg.exporter, &cfg.otlp)?;
            match cfg.otlp.protocol {
                OtlpProtocol::Grpc => {
                    let exporter = apply_export_config(
                        opentelemetry_otlp::MetricExporter::builder().with_tonic(),
                        export_config,
                    )
                    .build()
                    .expect("invalid metric exporter config");
                    let reader = PeriodicReader::builder(exporter)
                        .with_interval(*cfg.processor.interval)
                        .build();
                    tracing::info!("reloaded OTLP gRPC metric reader");
                    Some(Box::new(reader))
                }
                OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => {
                    tracing::warn!("OTLP HTTP metric exporter is not available in this build");
                    None
                }
                _ => None,
            }
        }
        ExporterKind::Console => {
            tracing::warn!("console metric exporter is not available in this build");
            None
        }
        _ => None,
    }
}

/// Resolved OTLP export settings, decoupled from any specific signal's builder.
///
/// Replaces the removed `opentelemetry_otlp::ExportConfig` struct; the values are
/// applied to each exporter builder via [`apply_export_config`].
struct ExportConfig {
    endpoint: Option<String>,
    protocol: Protocol,
    timeout: Duration,
}

fn ex_cfg(kind: ExporterKind, otlp: &OtlpVars) -> Option<ExportConfig> {
    if kind != ExporterKind::Otlp {
        return None;
    }
    Some(ExportConfig {
        endpoint: otlp.endpoint.clone(),
        protocol: match otlp.protocol {
            OtlpProtocol::Unknown => return None,
            OtlpProtocol::Grpc => Protocol::Grpc,
            OtlpProtocol::HttpProtobuf | OtlpProtocol::HttpJson => return None,
        },
        timeout: *otlp.timeout,
    })
}

/// Apply resolved [`ExportConfig`] settings to any OTLP exporter builder.
///
/// The `with_export_config` convenience was removed in opentelemetry-otlp 0.32 in
/// favor of the granular `WithExportConfig` methods used here.
fn apply_export_config<B: WithExportConfig>(builder: B, cfg: ExportConfig) -> B {
    let builder = builder
        .with_protocol(cfg.protocol)
        .with_timeout(cfg.timeout);
    match cfg.endpoint {
        Some(endpoint) => builder.with_endpoint(endpoint),
        None => builder,
    }
}
