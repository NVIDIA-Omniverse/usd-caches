// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Configuration types for OpenTelemetry tracing, logging, and metrics.

use ovdc_bytes::human_serde::{HumanDuration, HumanF64};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;

pub(crate) fn from_env_into<V, E>(key: &str, into: &mut V)
where
    V: Default,
    V: FromStr<Err = E>,
{
    let Ok(var) = std::env::var(key) else { return };
    if let Ok(value) = V::from_str(var.as_str()) {
        *into = value;
    }
}

pub(crate) fn with_env_u64(key: &str, f: impl FnOnce(u64)) {
    if let Some(var) = std::env::var(key).ok().and_then(|y| y.parse::<u64>().ok()) {
        f(var)
    }
}

pub(crate) fn from_env_u64(key: &str, into: &mut u64) {
    if let Some(var) = std::env::var(key).ok().and_then(|y| y.parse::<u64>().ok()) {
        *into = var;
    }
}

pub(crate) fn from_env_string(key: &str, into: &mut Option<String>) {
    if let Ok(var) = std::env::var(key) {
        *into = Some(var);
    }
}

/// Default filter string for OTEL layers: emit everything at INFO and above.
pub(crate) fn default_otel_filter() -> String {
    "info".to_string()
}

/// Default base level for a sink: INFO.
pub(crate) fn default_level() -> String {
    "info".to_string()
}

/// Available formats for log serialization.
#[derive(serde::Deserialize, Default, Debug, Clone, Copy)]
pub enum LogFormat {
    /// JSON formatted logs. Better for interpretation by systems.
    #[serde(rename = "json")]
    Json,

    /// Human-readable logs. Better for interpretation by humans.
    #[default]
    #[serde(rename = "human")]
    Human,

    /// Human-readable logs, but without colors.
    #[serde(rename = "human_no_color")]
    HumanNoColor,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum OtlpProtocol {
    Unknown,

    #[serde(rename = "grpc")]
    #[default]
    Grpc,

    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    #[serde(rename = "http/json")]
    HttpJson,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum ExporterKind {
    Unknown,

    #[serde(rename = "otlp")]
    #[default]
    Otlp,
    #[serde(rename = "console")]
    Console,
    #[serde(rename = "none")]
    None,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
pub enum Sampler {
    #[serde(rename = "always_on")]
    #[default]
    On,
    #[serde(rename = "always_off")]
    Off,
    #[serde(rename = "traceidratio")]
    TraceIdRatio,
    #[serde(rename = "parentbased_always_on")]
    ParentOn,
    #[serde(rename = "parentbased_always_off")]
    ParentOff,
    #[serde(rename = "parentbased_traceidratio")]
    ParentTraceIdRatio,
    Unknown,
}

pub trait StrConv {
    fn _to_str(self) -> &'static str;
    fn _from_str(val: &str) -> Self;
}

macro_rules! str_conv {
    ($for:ident: [$($iname:ident: $value:expr),*]) => {
        impl StrConv for $for {
            fn _to_str(self) -> &'static str {
                match self {
                    $(
                    Self::$iname => $value,
                    )*
                    Self::Unknown => "unknown",
                }
            }
            fn _from_str(value: &str) -> Self {
                match value {
                    $(
                    $value => Self::$iname,
                    )*
                    _ => Self::Unknown
                }
            }
        }

        impl FromStr for $for {
            type Err = Infallible;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok($for::_from_str(s))
            }
        }
    };
}

str_conv! {ExporterKind: [Otlp: "otlp", Console: "console", None: "none"]}
str_conv! {OtlpProtocol: [Grpc: "grpc", HttpProtobuf: "http/protobuf", HttpJson: "http/json"]}
str_conv! {Sampler: [
    On: "always_on", Off: "always_off", TraceIdRatio: "traceidratio",
    ParentOn: "parentbased_always_on", ParentOff: "parentbased_always_off",
    ParentTraceIdRatio: "parentbased_traceidratio"
]}

/// Configuration for OTLP (OpenTelemetry Protocol) export settings.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OtlpVars {
    /// The OTLP Export Protocol to use.
    ///
    /// Acceptable values are one of: [null, "grpc"].
    ///
    /// HTTP protocol values still deserialize for config compatibility, but this build only
    /// installs OTLP exporters for gRPC.
    #[serde(default)]
    pub protocol: OtlpProtocol,

    /// The OTLP URI for exposition.
    pub endpoint: Option<String>,

    /// The timeout duration for OTLP export requests.
    pub timeout: HumanDuration,
}

impl Default for OtlpVars {
    fn default() -> Self {
        Self {
            protocol: Default::default(),
            endpoint: None,
            timeout: Duration::from_secs(15).into(),
        }
    }
}

/// Configuration for batch processing of telemetry data.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BatchProcessorConfig {
    /// The delay between batch export attempts.
    pub schedule_delay: HumanDuration,

    /// The maximum number of items to queue before dropping new items.
    pub max_queue_size: u64,

    /// The maximum number of items to include in a single batch export.
    pub max_batch_size: u64,
}

impl Default for BatchProcessorConfig {
    fn default() -> Self {
        Self {
            schedule_delay: Duration::from_secs(5).into(),
            max_queue_size: 2048,
            max_batch_size: 512,
        }
    }
}

impl BatchProcessorConfig {
    pub fn apply_bsp_env(&mut self) {
        with_env_u64("OTEL_BSP_SCHEDULE_DELAY", |v| {
            self.schedule_delay = Duration::from_millis(v).into()
        });
        from_env_u64("OTEL_BSP_MAX_QUEUE_SIZE", &mut self.max_queue_size);
        from_env_u64("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", &mut self.max_batch_size);
    }
    pub fn apply_blrp_env(&mut self) {
        with_env_u64("OTEL_BLRP_SCHEDULE_DELAY", |v| {
            self.schedule_delay = Duration::from_millis(v).into()
        });
        from_env_u64("OTEL_BLRP_MAX_QUEUE_SIZE", &mut self.max_queue_size);
        from_env_u64("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", &mut self.max_batch_size);
    }
}

/// Configuration for periodic metric export.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeriodicExporter {
    /// The delay in milliseconds between export attempts.
    pub interval: HumanDuration,
}

impl Default for PeriodicExporter {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30).into(),
        }
    }
}

impl PeriodicExporter {
    pub(crate) fn apply_env(&mut self) {
        with_env_u64("OTEL_METRIC_EXPORT_INTERVAL", |v| {
            self.interval = Duration::from_millis(v).into()
        });
    }
}

/// Configuration for distributed tracing.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TraceConfig {
    /// The Exporter to use.
    ///
    /// Acceptable values are one of: [null, "otlp", "console", "none"]
    #[serde(default)]
    pub exporter: ExporterKind,

    /// OTLP-specific configuration when using the OTLP exporter.
    #[serde(default)]
    pub otlp: OtlpVars,

    /// Batch processor configuration for trace spans.
    #[serde(default)]
    pub processor: BatchProcessorConfig,

    /// Maximum length in bytes for span attribute values. Longer values are truncated.
    pub attribute_value_length_limit: u64,

    /// Maximum number of attributes allowed per span.
    pub attribute_count_limit: u64,

    /// Maximum number of events allowed per span.
    pub event_count_limit: u64,

    /// Maximum number of links allowed per span.
    pub link_count_limit: u64,

    /// Maximum number of attributes allowed per span event.
    pub event_attribute_count_limit: u64,

    /// Maximum number of attributes allowed per span link.
    pub link_attribute_count_limit: u64,

    /// Argument for the sampler, used with ratio-based samplers (0.0 to 1.0).
    pub sampler_arg: HumanF64,

    /// The sampling strategy to use for traces.
    pub sampler: Sampler,

    /// Base level for events forwarded to the OTEL span exporter, independent of the other
    /// sinks (stdout, OTEL logs). Allowed values: "error", "warn", "info", "debug", "trace", "off".
    #[serde(default = "default_level")]
    pub level: String,

    /// Optional per-module level overrides layered on top of `level`.
    ///
    /// Format: `module[*]=level[,module2[*]=level2]...`  e.g. `"ovdc::grpc=debug"`. Empty means
    /// no overrides — every module uses `level`.
    #[serde(default = "default_otel_filter")]
    pub filter: String,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            exporter: Default::default(),
            otlp: Default::default(),
            processor: Default::default(),

            attribute_value_length_limit: 128_000,
            attribute_count_limit: 128,
            event_count_limit: 128,
            link_count_limit: 128,
            event_attribute_count_limit: 128,
            link_attribute_count_limit: 128,

            sampler: Sampler::On,
            sampler_arg: 0.0.into(),
            level: default_level(),
            filter: default_otel_filter(),
        }
    }
}

impl TraceConfig {
    pub(crate) fn apply_env(mut self) -> Self {
        _otlp_traces_from_env(&mut self.exporter, &mut self.otlp);

        self.processor.apply_bsp_env();

        with_env_u64("OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT", |v| {
            self.attribute_value_length_limit = v
        });
        from_env_u64(
            "OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT",
            &mut self.attribute_count_limit,
        );
        from_env_u64("OTEL_SPAN_EVENT_COUNT_LIMIT", &mut self.event_count_limit);
        from_env_u64("OTEL_SPAN_LINK_COUNT_LIMIT", &mut self.link_count_limit);
        from_env_u64(
            "OTEL_EVENT_ATTRIBUTE_COUNT_LIMIT",
            &mut self.event_attribute_count_limit,
        );
        from_env_u64(
            "OTEL_LINK_ATTRIBUTE_COUNT_LIMIT",
            &mut self.link_attribute_count_limit,
        );
        if let Ok(v) = std::env::var("OTEL_TRACES_LEVEL") {
            self.level = v;
        }
        if let Ok(v) = std::env::var("OTEL_TRACES_FILTER") {
            self.filter = v;
        }
        self
    }
}

/// Configuration for log telemetry export.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogConfig {
    /// The Exporter to use.
    ///
    /// Acceptable values are one of: [null, "otlp", "console", "none"]
    #[serde(default)]
    pub exporter: ExporterKind,

    /// OTLP-specific configuration when using the OTLP exporter.
    #[serde(default)]
    pub otlp: OtlpVars,

    /// Batch processor configuration.
    #[serde(default)]
    pub processor: BatchProcessorConfig,

    /// Maximum length in bytes for log attribute values. Longer values are truncated.
    pub attribute_value_length_limit: u64,

    /// Maximum number of attributes allowed per log record.
    pub attribute_count_limit: u64,

    /// Base level for events forwarded to the OTEL log exporter, independent of the other
    /// sinks (stdout, OTEL traces). Allowed values: "error", "warn", "info", "debug", "trace", "off".
    #[serde(default = "default_level")]
    pub level: String,

    /// Optional per-module level overrides layered on top of `level`.
    ///
    /// Format: `module[*]=level[,module2[*]=level2]...`  e.g. `"ovdc::grpc=debug"`. Empty means
    /// no overrides — every module uses `level`.
    #[serde(default = "default_otel_filter")]
    pub filter: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            exporter: Default::default(),
            otlp: Default::default(),
            processor: Default::default(),
            attribute_value_length_limit: 256_000,
            attribute_count_limit: 128,
            level: default_level(),
            filter: default_otel_filter(),
        }
    }
}

impl LogConfig {
    pub(crate) fn apply_env(mut self) -> Self {
        _otlp_logs_from_env(&mut self.exporter, &mut self.otlp);
        self.processor.apply_blrp_env();

        from_env_u64(
            "OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT",
            &mut self.attribute_value_length_limit,
        );
        from_env_u64(
            "OTEL_LOGRECORD_ATTRIBUTE_COUNT_LIMIT",
            &mut self.attribute_count_limit,
        );
        if let Ok(v) = std::env::var("OTEL_LOGS_LEVEL") {
            self.level = v;
        }
        if let Ok(v) = std::env::var("OTEL_LOGS_FILTER") {
            self.filter = v;
        }
        self
    }
}

/// Configuration for metric telemetry export.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricConfig {
    /// The Exporter to use.
    ///
    /// Acceptable values are one of: [null, "otlp", "console", "none"]
    #[serde(default)]
    pub exporter: ExporterKind,

    /// OTLP-specific configuration when using the OTLP exporter.
    #[serde(default)]
    pub otlp: OtlpVars,

    /// Batch processor configuration.
    #[serde(default)]
    pub processor: PeriodicExporter,

    /// Filter string controlling which events are forwarded to the OTEL metrics exporter.
    ///
    /// Format: `global_level[,module[*]=level]...`  e.g. `"error,ovdc=info"`
    #[serde(default = "default_otel_filter")]
    pub filter: String,
}

impl Default for MetricConfig {
    fn default() -> Self {
        Self {
            exporter: Default::default(),
            otlp: Default::default(),
            processor: Default::default(),
            filter: default_otel_filter(),
        }
    }
}

impl MetricConfig {
    pub(crate) fn apply_env(mut self) -> Self {
        _otlp_metrics_from_env(&mut self.exporter, &mut self.otlp);
        self.processor.apply_env();
        self
    }
}

/// Main configuration for OpenTelemetry integration.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// When true, OpenTelemetry telemetry export is enabled.
    pub enabled: bool,

    /// When true, configuration values can be overridden by environment variables.
    pub use_env_vars: bool,
}

#[derive(Default, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct EnvConfig {
    #[serde(flatten)]
    pub inner: Config,
}

impl<'de> serde::Deserialize<'de> for EnvConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut cfg = Config::deserialize(deserializer)?;
        if cfg.use_env_vars {
            cfg = cfg.apply_env();
        }
        Ok(Self { inner: cfg })
    }
}

impl Deref for EnvConfig {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            use_env_vars: true,
        }
    }
}

impl Config {
    pub(crate) fn apply_env(mut self) -> Self {
        let disabled = std::env::var("OTEL_SDK_DISABLED")
            .ok()
            .map(|y| y.to_lowercase() == "true")
            .unwrap_or_default();

        if disabled {
            self.enabled = false;
        }
        self
    }
}

macro_rules! otlp_config_help {
    (
        $($name:ident),*
    ) =>{
        pastey::paste! {
        $(
        pub(crate) fn [< _otlp_ $name:lower _from_env >] (kind: &mut ExporterKind, otlp: &mut OtlpVars) {
            const OTLP_TIMEOUT: &str = stringify!([< OTEL_EXPORTER_OTLP_ $name _TIMEOUT >]);
            const PROTOCOL: &str = stringify!([< OTEL_EXPORTER_OTLP_ $name _PROTOCOL >]);
            const ENDPOINT: &str = stringify!([< OTEL_EXPORTER_OTLP_ $name _ENDPOINT >]);
            const EXPORTER: &str = stringify!([< OTEL_ $name _EXPORTER >]);
            from_env_into(PROTOCOL, &mut otlp.protocol);
            from_env_string(ENDPOINT, &mut otlp.endpoint);
            with_env_u64(OTLP_TIMEOUT, |v| otlp.timeout = Duration::from_millis(v).into());
            from_env_into(EXPORTER, kind);
        }
        )*
        }
    };
}

otlp_config_help! { METRICS, LOGS, TRACES }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_util::{EnvGuard, ENV_MUTEX};
    use std::time::Duration;

    // ── Config ─────────────────────────────────────────────────────────────

    #[test]
    fn otel_sdk_disabled_true_sets_enabled_false() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_SDK_DISABLED", "true")]);
        let cfg = Config::default().apply_env();
        assert!(!cfg.enabled);
    }

    #[test]
    fn otel_sdk_disabled_false_leaves_enabled_true() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_SDK_DISABLED", "false")]);
        let cfg = Config::default().apply_env();
        assert!(cfg.enabled);
    }

    #[test]
    fn otel_sdk_disabled_absent_leaves_enabled_true() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::unset(&["OTEL_SDK_DISABLED"]);
        let cfg = Config::default().apply_env();
        assert!(cfg.enabled);
    }

    // ── TraceConfig ─────────────────────────────────────────────────────────

    #[test]
    fn trace_exporter_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_TRACES_EXPORTER", "console")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.exporter, ExporterKind::Console);
    }

    #[test]
    fn trace_otlp_endpoint_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://collector:4318",
        )]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.otlp.endpoint, Some("http://collector:4318".to_string()));
    }

    #[test]
    fn trace_otlp_protocol_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "grpc")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.otlp.protocol, OtlpProtocol::Grpc);
    }

    #[test]
    fn trace_otlp_timeout_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "5000")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(*cfg.otlp.timeout, Duration::from_millis(5000));
    }

    #[test]
    fn trace_bsp_schedule_delay_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BSP_SCHEDULE_DELAY", "1000")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(*cfg.processor.schedule_delay, Duration::from_millis(1000));
    }

    #[test]
    fn trace_bsp_max_queue_size_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BSP_MAX_QUEUE_SIZE", "4096")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.processor.max_queue_size, 4096);
    }

    #[test]
    fn trace_bsp_max_batch_size_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", "256")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.processor.max_batch_size, 256);
    }

    #[test]
    fn trace_span_attribute_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT", "64")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.attribute_count_limit, 64);
    }

    #[test]
    fn trace_span_event_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_SPAN_EVENT_COUNT_LIMIT", "32")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.event_count_limit, 32);
    }

    #[test]
    fn trace_span_link_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_SPAN_LINK_COUNT_LIMIT", "16")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.link_count_limit, 16);
    }

    #[test]
    fn trace_event_attribute_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EVENT_ATTRIBUTE_COUNT_LIMIT", "10")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.event_attribute_count_limit, 10);
    }

    #[test]
    fn trace_link_attribute_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LINK_ATTRIBUTE_COUNT_LIMIT", "8")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.link_attribute_count_limit, 8);
    }

    #[test]
    fn trace_level_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_TRACES_LEVEL", "trace")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.level, "trace");
    }

    #[test]
    fn trace_filter_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_TRACES_FILTER", "ovdc::grpc=debug")]);
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.filter, "ovdc::grpc=debug");
    }

    #[test]
    fn trace_defaults_unchanged_when_env_absent() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::unset(&[
            "OTEL_TRACES_EXPORTER",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        ]);
        let defaults = TraceConfig::default();
        let cfg = TraceConfig::default().apply_env();
        assert_eq!(cfg.exporter, defaults.exporter);
        assert_eq!(cfg.otlp.endpoint, None);
        assert_eq!(cfg.otlp.protocol, defaults.otlp.protocol);
    }

    // ── LogConfig ───────────────────────────────────────────────────────────

    #[test]
    fn log_exporter_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LOGS_EXPORTER", "none")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.exporter, ExporterKind::None);
    }

    #[test]
    fn log_otlp_endpoint_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://logs:4318")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.otlp.endpoint, Some("http://logs:4318".to_string()));
    }

    #[test]
    fn log_otlp_protocol_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.otlp.protocol, OtlpProtocol::HttpJson);
    }

    #[test]
    fn log_blrp_schedule_delay_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BLRP_SCHEDULE_DELAY", "2000")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(*cfg.processor.schedule_delay, Duration::from_millis(2000));
    }

    #[test]
    fn log_blrp_max_queue_size_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BLRP_MAX_QUEUE_SIZE", "512")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.processor.max_queue_size, 512);
    }

    #[test]
    fn log_blrp_max_batch_size_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_BLRP_MAX_EXPORT_BATCH_SIZE", "128")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.processor.max_batch_size, 128);
    }

    #[test]
    fn log_attribute_value_length_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT", "1024")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.attribute_value_length_limit, 1024);
    }

    #[test]
    fn log_attribute_count_limit_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LOGRECORD_ATTRIBUTE_COUNT_LIMIT", "64")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.attribute_count_limit, 64);
    }

    #[test]
    fn log_level_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LOGS_LEVEL", "debug")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.level, "debug");
    }

    #[test]
    fn log_filter_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_LOGS_FILTER", "ovdc::lru=trace")]);
        let cfg = LogConfig::default().apply_env();
        assert_eq!(cfg.filter, "ovdc::lru=trace");
    }

    // ── MetricConfig ────────────────────────────────────────────────────────

    #[test]
    fn metric_exporter_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_METRICS_EXPORTER", "console")]);
        let cfg = MetricConfig::default().apply_env();
        assert_eq!(cfg.exporter, ExporterKind::Console);
    }

    #[test]
    fn metric_otlp_endpoint_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://metrics:4318")]);
        let cfg = MetricConfig::default().apply_env();
        assert_eq!(cfg.otlp.endpoint, Some("http://metrics:4318".to_string()));
    }

    #[test]
    fn metric_otlp_protocol_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_EXPORTER_OTLP_METRICS_PROTOCOL", "grpc")]);
        let cfg = MetricConfig::default().apply_env();
        assert_eq!(cfg.otlp.protocol, OtlpProtocol::Grpc);
    }

    #[test]
    fn metric_export_interval_overridden_by_env() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let _env = EnvGuard::set(&[("OTEL_METRIC_EXPORT_INTERVAL", "10000")]);
        let cfg = MetricConfig::default().apply_env();
        assert_eq!(*cfg.processor.interval, Duration::from_millis(10000));
    }
}
