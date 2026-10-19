// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! OTel-native telemetry for OVCC, the one place to initialize structured logging
//! and tracing, plus the flat access-log event and the span/redaction/W3C
//! helpers the proxy uses.
//!
//! # Two output streams
//!
//! - **Diagnostics + spans** -> stderr, via a `tracing-subscriber` registry
//!   (`EnvFilter` + a JSON/pretty `fmt` layer + an optional `tracing-opentelemetry`
//!   layer that exports OTLP spans).
//! - **Access logs** -> stdout, as one flat JSON object per request using the
//!   stable `cloud_backend_log` schema (see [`access`]). Keeping
//!   the access line on its own stdout sink — not inside the `tracing` JSON
//!   envelope — is what lets it parse identically to the nginx log for the
//!   frozen `jq`/logging-test contract.
//!
//! # OTLP is env-gated
//!
//! The span exporter is built only when an endpoint is configured (env or
//! config), so a dev/CI run with no collector still gets full structured logs
//! and spans without a network dependency. See [`otlp_traces_enabled`].

pub mod access;
pub mod metrics;
mod otlp_client;
pub mod propagation;
pub mod redact;

use ovcc_core::{Error, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How diagnostic logs are rendered to stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per event/span (machine-readable; the deployment default).
    Json,
    /// Human-readable multi-line blocks.
    Pretty,
    /// Compact single line per event.
    Compact,
    /// JSON when stderr is not a TTY, Compact when it is.
    #[default]
    Auto,
}

/// OTLP exporter settings. An empty `endpoint` leaves the exporter off unless a
/// standard `OTEL_EXPORTER_OTLP*` env var supplies one.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct OtlpConfig {
    /// Collector endpoint (OTLP/HTTP). Overridden by `OTEL_EXPORTER_OTLP_ENDPOINT`
    /// / `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` when those are set.
    #[serde(default)]
    pub endpoint: String,
}

/// Telemetry configuration slice. Deserializes from the engine config with
/// camelCase aliases for chart compatibility; every field is optional so a
/// missing `telemetry:` block yields working defaults.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// OTel `service.name` resource attribute. Overridden by `OTEL_SERVICE_NAME`.
    #[serde(alias = "serviceName")]
    pub service_name: String,
    /// OTel `service.version` resource attribute. Overridden by `OVCC_SERVICE_VERSION`.
    #[serde(alias = "serviceVersion")]
    pub service_version: String,
    /// `EnvFilter` directives, used when neither `OVCC_LOG` nor `RUST_LOG` is set.
    pub level: String,
    /// Diagnostic-log rendering on stderr.
    pub format: LogFormat,
    /// OTLP span export.
    pub otlp: OtlpConfig,
    /// Prefix prepended to every exported metric name. Defaults to `cc_`; the
    /// chart sets `ucc_` under `compat.useUccDefaults` so the prefixed series
    /// match whichever dashboards the deployment uses.
    #[serde(alias = "metricPrefix")]
    pub metric_prefix: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "ovcc".to_owned(),
            service_version: env!("CARGO_PKG_VERSION").to_owned(),
            level: "warn,ovcc=info".to_owned(),
            format: LogFormat::Auto,
            otlp: OtlpConfig::default(),
            metric_prefix: "cc_".to_owned(),
        }
    }
}

/// Holds the OTel tracer provider for the process lifetime; `Drop` flushes and
/// shuts it down so batched spans are not lost at exit. A no-op guard (no
/// provider) is returned when OTLP export is disabled or already initialized.
#[must_use = "hold the guard for the process lifetime; dropping it flushes and shuts down the exporter"]
pub struct TelemetryGuard {
    tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter_provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

impl TelemetryGuard {
    /// A guard that owns no provider (export disabled or the subscriber was
    /// already installed by another caller, e.g. in tests).
    pub fn noop() -> Self {
        Self {
            tracer_provider: None,
            meter_provider: None,
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Initializes telemetry from `TelemetryConfig::default()` merged with the
/// environment. Convenience wrapper around [`init`].
pub fn init_from_env() -> Result<TelemetryGuard> {
    init(&TelemetryConfig::default())
}

/// Installs the global subscriber (diagnostics+spans to stderr, access logs to
/// stdout) and, when an OTLP endpoint is configured, an OTLP span exporter.
///
/// Precedence for every setting is env var > config > built-in default, so an
/// operator can override a baked chart value without a redeploy. Returns an
/// `AlreadyExists` error if a global subscriber is already installed.
pub fn init(config: &TelemetryConfig) -> Result<TelemetryGuard> {
    // `log::log!` records bridge into tracing via the subscriber's `try_init`
    // below (tracing-subscriber's tracing-log feature). Do NOT init the
    // `LogTracer` separately first, because `try_init` installs it too and returns
    // an error if a `log` logger is already set, which would fail telemetry init
    // *after* the subscriber is already installed — leaving logs and traces up
    // but skipping the metrics init that runs after this point.

    // W3C trace-context propagation for inbound/outbound correlation.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer_provider = if otlp_traces_enabled(config) {
        Some(build_tracer_provider(config)?)
    } else {
        None
    };
    let otel_layer = tracer_provider.as_ref().map(|provider| {
        use opentelemetry::trace::TracerProvider as _;
        tracing_opentelemetry::layer().with_tracer(provider.tracer("ovcc"))
    });

    let filter = tracing_subscriber::EnvFilter::try_from_env("OVCC_LOG")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.level));
    let filter = noisy_silenced(filter);

    let registry = tracing_subscriber::registry().with(filter).with(otel_layer);
    let result = match resolve_format(config.format) {
        LogFormat::Json | LogFormat::Auto => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(std::io::stderr),
            )
            .try_init(),
        LogFormat::Pretty => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .pretty()
                    .with_writer(std::io::stderr),
            )
            .try_init(),
        LogFormat::Compact => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_writer(std::io::stderr),
            )
            .try_init(),
    };
    result.map_err(|_| Error::internal("telemetry already initialized"))?;

    // Metrics ride the same env-gating as spans; `init_metrics` is a no-op (and
    // returns `None`) when OTLP metrics are disabled, so a collector-less run
    // keeps full logs + spans with no network dependency. Metrics are optional
    // telemetry, so a failure to build the meter provider must not take down logs
    // or traces, but it must be logged (once) so it is never silent.
    let meter_provider = match metrics::init_metrics(config) {
        Ok(provider) => {
            if provider.is_some() {
                tracing::info!("OTLP metrics export initialized");
            }
            provider
        }
        Err(err) => {
            tracing::warn!(error = %err, "OTLP metrics export init failed; continuing without metrics");
            None
        }
    };

    Ok(TelemetryGuard {
        tracer_provider,
        meter_provider,
    })
}

/// Builds the OTLP/HTTP-protobuf span exporter and a batched tracer provider
/// with the `service.name` / `service.version` resource. The endpoint comes
/// from `OTEL_EXPORTER_OTLP*` env when set, else from `config.otlp.endpoint`.
fn build_tracer_provider(
    config: &TelemetryConfig,
) -> Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry_otlp::{WithExportConfig as _, WithHttpConfig as _};

    let client = otlp_client::OtlpHttpClient::new(otlp_client::export_timeout(
        opentelemetry_otlp::OTEL_EXPORTER_OTLP_TRACES_TIMEOUT,
    ))
    .map_err(|err| Error::internal(format!("OTLP span client init failed: {err}")))?;
    let mut builder = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary);
    // Honor the precedence rule where env wins, so only apply the config endpoint
    // when no standard OTLP endpoint env var is present.
    let env_endpoint = std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
        .or_else(|| std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT"))
        .is_some();
    if !env_endpoint && !config.otlp.endpoint.is_empty() {
        builder = builder.with_endpoint(config.otlp.endpoint.clone());
    }
    let exporter = builder
        .build()
        .map_err(|err| Error::internal(format!("OTLP span exporter init failed: {err}")))?;

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

    Ok(opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

/// Whether OTLP span export should be enabled, by precedence.
/// `OTEL_SDK_DISABLED`/`OVCC_OTLP=false` force off; `OVCC_OTLP=true` forces on;
/// otherwise on when any OTLP endpoint is configured (env or config).
pub fn otlp_traces_enabled(config: &TelemetryConfig) -> bool {
    if env_is_true("OTEL_SDK_DISABLED") || env_is_false("OVCC_OTLP") {
        return false;
    }
    if env_is_true("OVCC_OTLP") {
        return true;
    }
    std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || !config.otlp.endpoint.is_empty()
}

pub(crate) fn env_is_true(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

pub(crate) fn env_is_false(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false)
}

/// Resolves `Auto` to `Json` (non-TTY, e.g. a pod) or `Compact` (interactive).
/// `OVCC_LOG_FORMAT` overrides the configured value.
fn resolve_format(configured: LogFormat) -> LogFormat {
    let chosen = match std::env::var("OVCC_LOG_FORMAT").ok().as_deref() {
        Some("json") => LogFormat::Json,
        Some("pretty") => LogFormat::Pretty,
        Some("compact") | Some("text") => LogFormat::Compact,
        _ => configured,
    };
    if chosen == LogFormat::Auto {
        use std::io::IsTerminal;
        if std::io::stderr().is_terminal() {
            LogFormat::Compact
        } else {
            LogFormat::Json
        }
    } else {
        chosen
    }
}

/// Appends `<crate>=warn` directives that silence per-frame/per-poll plumbing
/// chatter (HTTP/2, TLS, connection pools). Appended last so they override a
/// broad user filter unless the user re-enables a crate explicitly.
fn noisy_silenced(filter: tracing_subscriber::EnvFilter) -> tracing_subscriber::EnvFilter {
    const NOISY: &[&str] = &[
        "h2=warn",
        "hyper=warn",
        "hyper_util=warn",
        "reqwest=warn",
        "rustls=warn",
        "mio=warn",
    ];
    NOISY
        .iter()
        .fold(filter, |acc, directive| match directive.parse() {
            Ok(parsed) => acc.add_directive(parsed),
            Err(_) => acc,
        })
}

/// Serializes tests that read or mutate the process environment: a test that
/// sets `OVCC_OTLP` would otherwise flip the outcome of a concurrently running
/// test (in either module) that expects it unset.
#[cfg(test)]
pub(crate) fn lock_test_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var(self.key) };
        }
    }
    fn set_env(key: &'static str, value: &str) -> EnvGuard {
        let lock = lock_test_env();
        unsafe { std::env::set_var(key, value) };
        EnvGuard { key, _lock: lock }
    }

    #[test]
    fn otlp_disabled_by_default() {
        let _env = lock_test_env();
        let config = TelemetryConfig::default();
        // No env, empty config endpoint -> off.
        assert!(!otlp_traces_enabled(&config));
    }

    #[test]
    fn otlp_enabled_when_config_endpoint_set() {
        let _env = lock_test_env();
        let mut config = TelemetryConfig::default();
        config.otlp.endpoint = "http://localhost:4318".to_owned();
        assert!(otlp_traces_enabled(&config));
    }

    #[test]
    fn ovcc_otlp_false_forces_off_even_with_endpoint() {
        let _g = set_env("OVCC_OTLP", "false");
        let mut config = TelemetryConfig::default();
        config.otlp.endpoint = "http://localhost:4318".to_owned();
        assert!(!otlp_traces_enabled(&config));
    }

    #[test]
    fn default_config_values() {
        let config = TelemetryConfig::default();
        assert_eq!(config.service_name, "ovcc");
        assert_eq!(config.level, "warn,ovcc=info");
        assert_eq!(config.format, LogFormat::Auto);
    }

    #[test]
    fn telemetry_config_parses_camelcase_aliases() {
        // JSON exercises the same serde alias path the chart's YAML uses.
        let json = r#"{"serviceName":"ovcc-edge","serviceVersion":"9.9.9","format":"json","otlp":{"endpoint":"http://collector:4318"}}"#;
        let config: TelemetryConfig = serde_json::from_str(json).expect("parses camelCase aliases");
        assert_eq!(config.service_name, "ovcc-edge");
        assert_eq!(config.service_version, "9.9.9");
        assert_eq!(config.format, LogFormat::Json);
        assert_eq!(config.otlp.endpoint, "http://collector:4318");
    }

    #[test]
    fn metric_prefix_defaults_to_cc_and_parses_camelcase_alias() {
        assert_eq!(TelemetryConfig::default().metric_prefix, "cc_");
        let config: TelemetryConfig =
            serde_json::from_str(r#"{"metricPrefix":"ucc_"}"#).expect("parses metricPrefix alias");
        assert_eq!(config.metric_prefix, "ucc_");
    }

    #[test]
    fn telemetry_config_empty_object_is_defaults() {
        let config: TelemetryConfig =
            serde_json::from_str("{}").expect("empty object uses defaults");
        assert_eq!(config, TelemetryConfig::default());
    }
}
