// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables Open Telemetry tracing for OVDC.

pub mod config;
mod filter;
mod http_metrics;
mod layer;
mod macros;
pub mod metrics;
mod processors;
pub mod span_extractor;
mod swap_processors;
#[cfg(test)]
mod tests_util;

pub use config::*;

#[allow(unused)]
pub use macros::*;
use ovdc_settings::{Mapping, Snap};

use opentelemetry::metrics::MeterProvider;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
pub use pastey::paste as ___paste;
pub use swap_processors::{Logs, Metrics, Traces};

use filter::{parse_level_or_warn, parse_rust_log_level, FilterGatedLayer, FilterSpec};

use std::{net::SocketAddr, sync::Arc};

use tracing::metadata::LevelFilter;

use opentelemetry::{trace::TracerProvider, KeyValue};
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_sdk::Resource;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::Layer as TracingLayer;
use tracing_subscriber::{fmt, Registry};

use crate::metrics::MeterReader;

type ReloadHandle = tracing_subscriber::reload::Handle<OtelCompositeLayer, Registry>;

type BoxedLayer = Box<dyn tracing_subscriber::Layer<Registry> + Send + Sync + 'static>;

/// A composite layer that groups all OpenTelemetry layers together.
///
/// This allows us to reload all OTEL layers as a single unit.
struct OtelCompositeLayer {
    pub console_logs: Option<FilteredStdLogLayer>,
    pub logs: Option<FilteredLogLayer>,
    pub traces: Option<FilteredTraceLayer>,
}

impl tracing_subscriber::Layer<Registry> for OtelCompositeLayer {
    fn on_layer(&mut self, subscriber: &mut Registry) {
        if let Some(logs) = &mut self.logs {
            logs.on_layer(subscriber);
        }
        if let Some(traces) = &mut self.traces {
            traces.on_layer(subscriber);
        }
        if let Some(metrics) = &mut self.console_logs {
            metrics.on_layer(subscriber);
        }
    }

    fn register_callsite(
        &self,
        metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        use tracing::subscriber::Interest;

        // TODO: this allocation is stupid
        // Combine interests from all layers - return the most permissive
        let mut interests = vec![];

        if let Some(logs) = &self.logs {
            interests.push(logs.register_callsite(metadata));
        }
        if let Some(traces) = &self.traces {
            interests.push(traces.register_callsite(metadata));
        }
        if let Some(metrics) = &self.console_logs {
            interests.push(metrics.register_callsite(metadata));
        }

        // If any layer is always interested, return always
        // If any layer is sometimes interested, return sometimes
        // Otherwise, return never
        if interests.iter().any(|i| i.is_always()) {
            Interest::always()
        } else if interests.iter().any(|i| i.is_sometimes()) {
            Interest::sometimes()
        } else {
            Interest::never()
        }
    }

    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) -> bool {
        (self
            .logs
            .as_ref()
            .is_some_and(|l| l.enabled(metadata, ctx.clone())))
            || (self
                .traces
                .as_ref()
                .is_some_and(|l| l.enabled(metadata, ctx.clone())))
            || (self
                .console_logs
                .as_ref()
                .is_some_and(|l| l.enabled(metadata, ctx)))
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_new_span(attrs, id, ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_new_span(attrs, id, ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_new_span(attrs, id, ctx);
        }
    }

    fn on_record(
        &self,
        span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_record(span, values, ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_record(span, values, ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_record(span, values, ctx);
        }
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_event(event, ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_event(event, ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_event(event, ctx);
        }
    }

    fn on_enter(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_enter(id, ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_enter(id, ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_enter(id, ctx);
        }
    }

    fn on_exit(
        &self,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_exit(id, ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_exit(id, ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_exit(id, ctx);
        }
    }

    fn on_close(
        &self,
        id: tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, Registry>,
    ) {
        if let Some(logs) = &self.logs {
            logs.on_close(id.clone(), ctx.clone());
        }
        if let Some(traces) = &self.traces {
            traces.on_close(id.clone(), ctx.clone());
        }
        if let Some(metrics) = &self.console_logs {
            metrics.on_close(id, ctx);
        }
    }
}

/// Manages hot-swappable OpenTelemetry processors for logs, traces, and metrics.
#[derive(Clone)]
pub struct ProcessorManager {
    /// Pull-based reader for the Prometheus scrape endpoint. Registered on every
    /// `SdkMeterProvider` build so it coexists alongside the OTLP push reader.
    pub pull_reader: MeterReader,
    layer_handle: Arc<ReloadHandle>,
}

impl ProcessorManager {
    pub fn meter_reader(&self) -> &MeterReader {
        &self.pull_reader
    }

    pub fn reload(&self, cfg: &ovdc_settings::Cfg) {
        let mut layer = OtelCompositeLayer {
            console_logs: None,
            logs: None,
            traces: None,
        };
        reload_from_cfg(cfg, &mut layer, &self.pull_reader);
        // Reload the layers using the handle
        if let Err(e) = self.layer_handle.reload(layer) {
            tracing::error!("failed to reload OpenTelemetry layers: {}", e);
        } else {
            tracing::info!("reloaded OpenTelemetry layers successfully");
        }
    }
}

fn reload_from_cfg(
    cfg: &ovdc_settings::Cfg,
    layer: &mut OtelCompositeLayer,
    pull_reader: &MeterReader,
) {
    let _otel_cfg: Config = cfg.snap().subtable::<Config>("otel").apply_env();
    let snap = cfg.snap();

    let metrics_cfg: MetricConfig = snap.subtable::<MetricConfig>("otelMetrics").apply_env();
    let traces_cfg: TraceConfig = snap.subtable::<TraceConfig>("otelTraces").apply_env();
    let logs_cfg: LogConfig = snap.subtable::<LogConfig>("otelLogs").apply_env();

    let level = snap
        .get()
        .get::<String>("logLevel")
        .ok()
        .and_then(|s| parse_level_or_warn("logLevel", &s))
        .unwrap_or_else(parse_rust_log_level);
    let console_overrides = snap.get().get::<String>("logFilter").unwrap_or_default();
    let format = snap.get().get::<LogFormat>("logFormat").unwrap();

    if let Some((trace_layer, _p)) = build_otel_traces(&traces_cfg, svc_rsc()) {
        layer.traces = Some(trace_layer);
    }
    if let Some((logs_layer, _p)) = build_otel_logger(&logs_cfg, svc_rsc()) {
        layer.logs = Some(logs_layer);
    }

    let console_logs = build_stdout_logger(level, &console_overrides, format);
    layer.console_logs = Some(console_logs);

    let provider = build_otel_metrics(&metrics_cfg, pull_reader, svc_rsc());
    metrics::set(provider.meter("ovdc"), provider);
    metrics::bump_meter_generation();
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TelemetryCfg {
    prometheus_metrics_exposition: bool,
    prometheus_metrics_port: u16,
}

impl Default for TelemetryCfg {
    fn default() -> Self {
        Self {
            prometheus_metrics_exposition: true,
            prometheus_metrics_port: 3051,
        }
    }
}

/// Setup OTEL tracing and logging with hot-reload support.
pub fn setup(cfg: ovdc_settings::Cfg) -> ProcessorManager {
    //global::set_text_map_propagator(TraceContextPropagator::new());

    // Sections owned by this crate, checked on every reload before publish.
    cfg.register_validator::<Config>("otel");
    cfg.register_validator::<TraceConfig>("otelTraces");
    cfg.register_validator::<LogConfig>("otelLogs");
    cfg.register_validator::<MetricConfig>("otelMetrics");
    cfg.register_validator::<TelemetryCfg>("telemetry");

    let pull_reader = MeterReader::default();
    let mut layer = OtelCompositeLayer {
        console_logs: None,
        logs: None,
        traces: None,
    };
    reload_from_cfg(&cfg, &mut layer, &pull_reader);
    let (reload_layer, handle) = tracing_subscriber::reload::Layer::new(layer);

    let subscriber = tracing_subscriber::registry().with(reload_layer);

    // Ignore error if a subscriber was already set (e.g. in tests).
    let _ = tracing::subscriber::set_global_default(subscriber);

    let pm = ProcessorManager {
        pull_reader,
        layer_handle: Arc::new(handle),
    };

    let telemetry: TelemetryCfg = cfg.snap().subtable("telemetry");
    if telemetry.prometheus_metrics_exposition {
        http_metrics::serve(
            SocketAddr::new([0, 0, 0, 0].into(), telemetry.prometheus_metrics_port),
            pm.pull_reader.clone(),
        );
    }

    pm
}

type FilteredStdLogLayer = FilterGatedLayer<BoxedLayer, Registry>;

fn build_stdout_logger(
    level: LevelFilter,
    overrides: &str,
    format: LogFormat,
) -> FilterGatedLayer<BoxedLayer, Registry> {
    let filter = FilterSpec::from_level_and_overrides(level, overrides);
    FilterGatedLayer::<BoxedLayer, Registry> {
        inner: match format {
            LogFormat::Json => fmt::layer().json().boxed(),
            LogFormat::Human => fmt::layer().boxed(),
            LogFormat::HumanNoColor => fmt::layer().with_ansi(false).boxed(),
        },
        filter,
        _subscriber: std::marker::PhantomData,
    }
}

type LoggerLayerInner =
    OpenTelemetryTracingBridge<SdkLoggerProvider, opentelemetry_sdk::logs::SdkLogger>;
type FilteredLogLayer = FilterGatedLayer<LoggerLayerInner, Registry>;

fn build_otel_logger(
    logs_cfg: &LogConfig,
    rsc: Resource,
) -> Option<(FilteredLogLayer, SdkLoggerProvider)> {
    // Build logs layer, gated by otel_log_filter.
    let processor = processors::build_log_processor(logs_cfg)?;
    let provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_resource(rsc.clone())
        .with_log_processor(processors::Logs(processor))
        .build();

    let level = parse_level_or_warn("otelLogs.level", &logs_cfg.level).unwrap_or(LevelFilter::INFO);
    Some((
        FilterGatedLayer::<LoggerLayerInner, Registry> {
            inner: OpenTelemetryTracingBridge::new(&provider),
            filter: FilterSpec::from_level_and_overrides(level, &logs_cfg.filter),
            _subscriber: std::marker::PhantomData,
        },
        provider,
    ))
}

type TraceLayerInner =
    tracing_opentelemetry::OpenTelemetryLayer<Registry, opentelemetry_sdk::trace::Tracer>;
type FilteredTraceLayer = FilterGatedLayer<TraceLayerInner, Registry>;

fn build_otel_traces(
    traces_cfg: &TraceConfig,
    rsc: Resource,
) -> Option<(FilteredTraceLayer, SdkTracerProvider)> {
    // Build traces layer, gated by otel_span_filter.
    let processor = processors::build_span_processor(traces_cfg)?;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(rsc.clone())
        .with_max_attributes_per_span(traces_cfg.attribute_count_limit as _)
        .with_max_events_per_span(traces_cfg.event_count_limit as _)
        .with_max_links_per_span(traces_cfg.link_count_limit as _)
        .with_max_attributes_per_event(traces_cfg.event_attribute_count_limit as _)
        .with_max_attributes_per_link(traces_cfg.link_attribute_count_limit as _)
        .with_span_processor(processors::Spans(processor))
        .build();

    //global::set_tracer_provider(provider.clone());

    let inner: tracing_opentelemetry::OpenTelemetryLayer<
        Registry,
        opentelemetry_sdk::trace::Tracer,
    > = tracing_opentelemetry::layer().with_tracer(provider.tracer("ovdc"));
    let level =
        parse_level_or_warn("otelTraces.level", &traces_cfg.level).unwrap_or(LevelFilter::INFO);
    Some((
        FilterGatedLayer::<_, Registry> {
            inner,
            filter: FilterSpec::from_level_and_overrides(level, &traces_cfg.filter),
            _subscriber: std::marker::PhantomData,
        },
        provider,
    ))
}

#[inline]
fn svc_rsc() -> Resource {
    Resource::builder_empty()
        .with_attributes([KeyValue::new("service.name", "ovdc")])
        .build()
}

/// Always builds a provider with `pull_reader` attached -- the Prometheus
/// scrape endpoint must keep working regardless of `otelMetrics.exporter`,
/// which only controls whether an OTLP push reader is added alongside it.
fn build_otel_metrics(
    metrics_cfg: &MetricConfig,
    pull_reader: &MeterReader,
    svcrsc: Resource,
) -> SdkMeterProvider {
    let mut builder = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_resource(svcrsc)
        .with_reader(pull_reader.clone());
    if let Some(processor) = processors::build_metric_reader(metrics_cfg) {
        builder = builder.with_reader(processors::Metrics(processor));
    }
    //global::set_meter_provider(provider.clone());
    builder.build()
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::{metrics::SdkMeterProvider, Resource};

    use crate::metrics::{self, collect_openmetrics, Measure, MeterReader};

    use crate::tests_util::{EnvGuard, ENV_MUTEX};

    // Serializes tests that mutate global OTel state (subscriber + meter provider).
    static GLOBAL_STATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // OTEL_* env vars the opentelemetry-otlp HTTP/gRPC exporters consult at
    // `.build()` time. If any of these are set in the ambient environment
    // (CI runners often export OTEL_EXPORTER_OTLP_ENDPOINT etc. to forward
    // telemetry to a real collector), the exporter will silently redirect
    // our test traffic away from the mock collector and the test hangs on
    // the `body_rx.recv()` timeout. We scrub them with EnvGuard::unset for
    // the duration of the test and restore on drop.
    const OTLP_ENV_VARS: &[&str] = &[
        "OTEL_SDK_DISABLED",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        "OTEL_EXPORTER_OTLP_PROTOCOL",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TIMEOUT",
        "OTEL_EXPORTER_OTLP_COMPRESSION",
        "OTEL_EXPORTER_OTLP_CERTIFICATE",
        "OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE",
        "OTEL_EXPORTER_OTLP_CLIENT_KEY",
        "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_TIMEOUT",
        "OTEL_EXPORTER_OTLP_METRICS_COMPRESSION",
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
        "OTEL_METRIC_EXPORT_INTERVAL",
        "OTEL_METRICS_EXPORTER",
        "OTEL_TRACES_EXPORTER",
        "OTEL_LOGS_EXPORTER",
    ];

    crate::meter! {
        static;
        pub PREFIX_TEST_COUNTER: Count { };
    }

    crate::meter! {
        static;
        pub FILTERED_SPAN_TEST_COUNTER: Count { };
    }

    /// Verifies that after `ProcessorManager::reload()`, a metric instrument re-registers
    /// with the new provider.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_meter_after_reload() {
        let _guard = GLOBAL_STATE.lock().await;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _env_scrub = EnvGuard::unset(OTLP_ENV_VARS);

        // Build config that loads an inline TOML with the right shape, with metrics
        // initially disabled so nothing is pushed before reload.
        let make_cfg = |exporter: &str| {
            ovdc_settings::load_document_with(include_str!("../../ovdc/src/config.toml"), |c| {
                c["otelMetrics"]["exporter"] = exporter.into();
                c["otelMetrics"]["otlp"]["endpoint"] = "http://127.0.0.1:4317".into();
                c["otelMetrics"]["otlp"]["protocol"] = "grpc".into();
                c["otelMetrics"]["processor"]["interval"] = "25ms".into();
                c["otelTraces"]["exporter"] = "none".into();
                c["otelLogs"]["exporter"] = "none".into();
            })
        };

        // Start with metrics off so no push happens before reload.
        let pm = crate::setup(make_cfg("otlp"));

        // Create a counter and record an initial value (goes to the no-op provider).
        let counter = crate::meter! {
            EXAMPLE_RELOAD_COUNTER: Count { };
        };
        counter.measure(1, &[]);

        // Reload with OTLP enabled — this swaps the global provider and bumps the generation.
        pm.reload(&make_cfg("otlp"));

        // Second increment: generation mismatch triggers rebuild onto the new OTLP provider.
        counter.measure(2, &[]);
    }

    /// Regression test: `otelMetrics.exporter = "none"` must not disable the
    /// Prometheus pull reader. `build_otel_metrics` used to build the whole
    /// `SdkMeterProvider` -- pull reader included -- behind an early return
    /// that only should have gated the OTLP push reader, so setting the OTLP
    /// metrics exporter to "none" silently zeroed out `/metrics` too.
    #[tokio::test]
    async fn prometheus_metrics_work_when_otel_metrics_exporter_is_none() {
        let _guard = GLOBAL_STATE.lock().await;
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let _env_scrub = EnvGuard::unset(OTLP_ENV_VARS);

        let cfg =
            ovdc_settings::load_document_with(include_str!("../../ovdc/src/config.toml"), |c| {
                c["otelMetrics"]["exporter"] = "none".into();
                c["otelTraces"]["exporter"] = "none".into();
                c["otelLogs"]["exporter"] = "none".into();
            });

        let pm = crate::setup(cfg);

        let counter = crate::meter! {
            OTEL_NONE_PROMETHEUS_TEST_COUNTER: Count { };
        };
        counter.measure(1, &[]);

        let out = collect_openmetrics(pm.meter_reader());
        assert!(
            out.contains("ovdc_otel_none_prometheus_test_counter 1"),
            "Prometheus scrape output should still contain recorded metrics even when \
             otelMetrics.exporter is \"none\": {out}"
        );
    }

    /// Creates a counter with meter!, records to it, then asserts collect_openmetrics shows it went up.
    #[tokio::test]
    async fn test_meter_counter_collect_openmetrics() {
        let _guard = GLOBAL_STATE.lock().await;
        let reader = MeterReader::default();
        let resource = Resource::builder_empty()
            .with_attributes([opentelemetry::KeyValue::new("service.name", "ovdc")])
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader.clone())
            .build();
        metrics::set(provider.meter("ovdc"), provider);

        PREFIX_TEST_COUNTER.measure(1, &[]);
        PREFIX_TEST_COUNTER.measure(2, &[]);

        let out = collect_openmetrics(&reader);
        assert!(
            out.contains("ovdc_prefix_test_counter 3"),
            "openmetrics output should contain test counter name and count at 3: {out}"
        );
    }

    /// End-to-end regression test for the `FilterGatedLayer` fix: a TRACE-level span is
    /// filtered out of the console JSON sink (its level is below the sink's INFO threshold),
    /// but an `info!` log nested inside that span and a metric recorded alongside it must
    /// both still make it out correctly. Before the fix, the nested `info!` event would panic
    /// the JSON formatter instead of being emitted (see `filter::tests` for the narrower
    /// panic-reproduction cases); this test additionally asserts the *content* that reaches
    /// each sink, and that metrics — a separate pipeline entirely — are unaffected either way.
    #[tokio::test]
    async fn console_logs_and_metrics_survive_a_filtered_out_trace_span() {
        let _guard = GLOBAL_STATE.lock().await;
        use crate::filter::{FilterGatedLayer, FilterSpec};
        use std::sync::{Arc, Mutex};
        use tracing::metadata::LevelFilter;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::registry::Registry;

        #[derive(Clone)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = buf.clone();

        // Console sink: JSON format, filtered at INFO -- the same shape build_stdout_logger
        // produces, but writing to a buffer we can inspect instead of stdout.
        let console = FilterGatedLayer::<_, Registry> {
            inner: tracing_subscriber::fmt::layer()
                .json()
                .with_writer(move || SharedBuf(writer_buf.clone())),
            filter: FilterSpec::from_level_and_overrides(LevelFilter::INFO, ""),
            _subscriber: std::marker::PhantomData,
        };
        let subscriber = Registry::default().with(console);

        // Metrics: a separate pipeline from the tracing subscriber above (build_otel_metrics
        // never touches FilterGatedLayer), set up here to confirm it keeps working
        // independently while a trace-level span is being filtered from the log sinks.
        let reader = MeterReader::default();
        let resource = Resource::builder_empty()
            .with_attributes([opentelemetry::KeyValue::new("service.name", "ovdc")])
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader.clone())
            .build();
        metrics::set(provider.meter("ovdc"), provider);

        tracing::subscriber::with_default(subscriber, || {
            // Below the console sink's INFO threshold, so filtered out of the JSON layer.
            let span = tracing::trace_span!("filtered_out_span");
            let _span_guard = span.enter();

            // Passes the INFO threshold; nested inside the filtered-out span. Before the fix
            // this panicked the JSON formatter instead of reaching the writer.
            tracing::info!(
                marker = "console_survives",
                "this log line must still reach the console sink"
            );

            // Metrics recorded in the same scope must be unaffected by trace-level filtering.
            FILTERED_SPAN_TEST_COUNTER.measure(1, &[]);
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            output.contains("this log line must still reach the console sink"),
            "expected the info! event to reach the console JSON sink, got: {output}"
        );
        assert!(
            output.contains("\"level\":\"INFO\""),
            "expected the emitted line to carry level INFO, got: {output}"
        );
        assert!(
            !output.contains("filtered_out_span") || output.contains("\"spans\""),
            "if the filtered span appears at all it must be inside a well-formed spans array, not have crashed formatting: {output}"
        );

        let metrics_out = collect_openmetrics(&reader);
        assert!(
            metrics_out.contains("ovdc_filtered_span_test_counter 1"),
            "expected the metric recorded alongside the filtered trace span to still be counted: {metrics_out}"
        );
    }
}
