// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    borrow::Cow,
    io::{self, Write},
};

use opentelemetry::Value as OtelValue;
use opentelemetry_sdk::metrics::{
    data::{AggregatedMetrics, MetricData},
    reader::MetricReader,
};

use crate::metrics::MeterReader;

#[derive(Default)]
struct Descriptor<'a> {
    ty: Option<&'a str>,
    unit: Option<&'a str>,
    desc: Option<&'a str>,
}

fn render_def(
    name: &str,
    ty: Option<&str>,
    // OpenMetrics requires the `# UNIT` value to literally be a suffix of the
    // metric name -- our OTEL unit strings (raw UCUM codes like "By") never
    // satisfy that, and UNIT is optional, so it's intentionally never emitted
    // rather than emitting a spec-invalid line that breaks strict scrapers.
    _unit: Option<&str>,
    desc: Option<&str>,
    writer: &mut impl Write,
) -> io::Result<()> {
    if let Some(ty) = ty {
        writeln!(writer, "# TYPE {name} {ty}")?;
    }
    if let Some(help) = desc {
        writeln!(writer, "# HELP {name} {help}")?;
    }
    write!(writer, "{}", name)
}

fn render_metric_labels<'a>(
    mut iter: impl Iterator<Item = (&'a str, &'a OtelValue)>,
    writer: &mut impl Write,
) -> io::Result<()> {
    let first = iter.next();

    match first {
        Some((key, value)) => {
            let _ = writer.write(b"{")?;

            let label_name = sanitize_label_name(key);
            write!(writer, "{}=\"{}\"", label_name, fmt_otel(value))?;
        }
        None => {
            return Ok(());
        }
    }

    for (key, value) in iter {
        let label_name = sanitize_label_name(key);
        write!(writer, ",{}=\"{}\"", label_name, fmt_otel(value))?;
    }

    let _ = writer.write(b"}")?;
    Ok(())
}

#[inline]
fn render_metric<'a, T: Copy + Into<M64>>(
    name: &str,
    ty: Option<&str>,
    unit: Option<&str>,
    desc: Option<&str>,
    attr: impl Iterator<Item = (&'a str, &'a OtelValue)>,
    value: T,
    writer: &mut impl Write,
) -> io::Result<()> {
    render_def(name, ty, unit, desc, writer)?;
    render_metric_labels(attr, writer)?;
    writeln!(writer, " {}", value.into())
}

#[allow(clippy::too_many_arguments)]
fn render_histogram<'a, I, Ib, F>(
    name: &str,
    ty: Option<&str>,
    unit: Option<&str>,
    desc: Option<&str>,

    attr: F,

    buckets: Ib,
    sum: M64,
    count: M64,

    writer: &mut impl Write,
) -> io::Result<()>
where
    F: Fn() -> I,
    I: Iterator<Item = (&'a str, &'a OtelValue)>,
    Ib: Iterator<Item = (f64, M64)>,
{
    // Sum & Count
    render_metric(&format!("{name}_sum"), ty, unit, desc, attr(), sum, writer)?;
    render_metric(
        &format!("{name}_count"),
        None,
        None,
        None,
        attr(),
        count,
        writer,
    )?;

    for (le_key, value) in buckets {
        let le_key: OtelValue = match le_key.is_infinite() {
            true => OtelValue::String("+Inf".into()),
            false => le_key.into(),
        };
        let mut o_le_key = Some(&le_key);
        let mut sub_iter = attr();

        let iter = std::iter::from_fn(move || {
            if let Some(le_key) = o_le_key.take() {
                return Some(("le", le_key));
            }
            sub_iter.next()
        });

        render_metric(name, None, None, None, iter, value, writer)?;
    }

    Ok(())
}

type SdkHistogram<T> = opentelemetry_sdk::metrics::data::Histogram<T>;
type SdkGauge<T> = opentelemetry_sdk::metrics::data::Gauge<T>;
type SdkCounter<T> = opentelemetry_sdk::metrics::data::Sum<T>;
enum OtelMetricGeneric<'a, T> {
    Hist(&'a SdkHistogram<T>),
    Gauge(&'a SdkGauge<T>),
    Counter(&'a SdkCounter<T>),
    Unsupported,
}

impl<'a, T> From<&'a MetricData<T>> for OtelMetricGeneric<'a, T> {
    fn from(value: &'a MetricData<T>) -> Self {
        match value {
            opentelemetry_sdk::metrics::data::MetricData::Gauge(gauge) => Self::Gauge(gauge),
            opentelemetry_sdk::metrics::data::MetricData::Sum(sum) => Self::Counter(sum),
            opentelemetry_sdk::metrics::data::MetricData::Histogram(histogram) => {
                Self::Hist(histogram)
            }
            opentelemetry_sdk::metrics::data::MetricData::ExponentialHistogram(..) => {
                tracing::trace!("exponential histogram cannot be exported to prometheus");
                Self::Unsupported
            }
        }
    }
}

fn render_from_otel<T: Into<M64> + Copy>(
    metric_name: &str,
    mut desc: Descriptor,
    x: OtelMetricGeneric<T>,
    writer: &mut impl Write,
) -> io::Result<()> {
    match x {
        OtelMetricGeneric::Gauge(gauge) => {
            desc.ty = Some("gauge");
            let mut desc = Some(desc);
            for data_point in gauge.data_points() {
                let Descriptor { ty, unit, desc } = desc.take().unwrap_or_default();
                render_metric(
                    metric_name,
                    ty,
                    unit,
                    desc,
                    data_point.attributes().map(|y| (y.key.as_str(), &y.value)),
                    data_point.value(),
                    writer,
                )?;
            }
        }
        OtelMetricGeneric::Counter(counter) => {
            desc.ty = Some("gauge");
            let mut desc = Some(desc);
            for data_point in counter.data_points() {
                let Descriptor { ty, unit, desc } = desc.take().unwrap_or_default();
                render_metric(
                    metric_name,
                    ty,
                    unit,
                    desc,
                    data_point.attributes().map(|y| (y.key.as_str(), &y.value)),
                    data_point.value(),
                    writer,
                )?;
            }
        }
        OtelMetricGeneric::Hist(histogram) => {
            desc.ty = Some("histogram");
            let mut desc = Some(desc);

            for data_point in histogram.data_points() {
                let Descriptor { ty, unit, desc } = desc.take().unwrap_or_default();

                let iter = data_point
                    .bounds()
                    .chain(std::iter::once(f64::INFINITY))
                    .zip(data_point.bucket_counts().map(M64::from));

                render_histogram(
                    metric_name,
                    ty,
                    unit,
                    desc,
                    || data_point.attributes().map(|y| (y.key.as_str(), &y.value)),
                    iter,
                    data_point.sum().into(),
                    data_point.count().into(),
                    writer,
                )?;
            }
        }
        OtelMetricGeneric::Unsupported => {}
    }
    Ok(())
}

pub fn collect_openmetrics(reader: &MeterReader) -> String {
    let mut rm = opentelemetry_sdk::metrics::data::ResourceMetrics::default();
    if crate::err!(reader.collect(&mut rm), "failed to collect OTEL metrics").is_err() {
        return Default::default();
    }
    let mut value = vec![];
    if crate::err!(
        write_openmetrics(&rm, &mut value),
        "failed to convert OTEL to openmetrics"
    )
    .is_err()
    {
        return Default::default();
    }
    String::from_utf8(value)
        .expect("generated openmetrics string contained invalid utf-8 characters")
}

pub fn write_openmetrics(
    resource_metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
    writer: &mut impl Write,
) -> Result<(), String> {
    // Process each scope metrics
    for scope_metrics in resource_metrics.scope_metrics() {
        // OpenMetrics only allows "# HELP"/"# TYPE"/"# UNIT"/"# EOF" comment
        // lines; a freeform "# Scope: ..." line here breaks strict OpenMetrics
        // parsers (e.g. Prometheus scraping with the openmetrics Content-Type).
        let service_name = resource_metrics
            .resource()
            .get(&opentelemetry::Key::from_static_str("service.name"))
            .map(|y| match y.as_str() {
                Cow::Owned(y) => y,
                Cow::Borrowed(y) => y.to_owned(),
            })
            .unwrap_or_else(|| "_unknown".to_owned());

        // Process each metric
        for metric in scope_metrics.metrics() {
            crate::drop!(
                write_metric(&service_name, metric, writer),
                "failed to write individual metric"
            );
        }
    }

    // Write EOF marker
    writeln!(writer, "# EOF").map_err(|e| e.to_string())?;

    Ok(())
}

/// Write a single metric in OpenMetrics format.
fn write_metric(
    service_name: &str,
    metric: &opentelemetry_sdk::metrics::data::Metric,
    writer: &mut impl Write,
) -> io::Result<()> {
    // Sanitize metric name for Prometheus
    let metric_name = sanitize_metric_name(service_name, metric.name());

    let desc = Descriptor {
        ty: None,
        desc: (!metric.description().is_empty()).then_some(metric.description()),
        unit: (!metric.unit().is_empty()).then_some(metric.unit()),
    };

    match metric.data() {
        AggregatedMetrics::F64(x) => {
            render_from_otel(&metric_name, desc, OtelMetricGeneric::from(x), writer)
        }
        AggregatedMetrics::U64(x) => {
            render_from_otel(&metric_name, desc, OtelMetricGeneric::from(x), writer)
        }
        AggregatedMetrics::I64(x) => {
            render_from_otel(&metric_name, desc, OtelMetricGeneric::from(x), writer)
        }
    }
}

/// Sanitize metric name for Prometheus.
fn sanitize_metric_name(service_name: &str, name: &str) -> String {
    let mut result = String::with_capacity(name.len() + service_name.len() + 1);
    result.push_str(service_name);
    result.push('_');

    let mut chars = name.chars();

    // First character must be alphabetic
    if let Some(ch) = chars.next() {
        if ch.is_alphabetic() {
            result.push(ch);
        } else {
            result.push('_');
        }
    }

    // Remaining characters can be alphanumeric or underscore
    for ch in chars {
        if ch.is_alphanumeric() || ch == '_' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }

    result
}

/// Sanitize label name for Prometheus.
fn sanitize_label_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut chars = name.chars();

    // First character must be alphabetic
    if let Some(ch) = chars.next() {
        if ch.is_alphabetic() {
            result.push(ch);
        } else {
            result.push('_');
        }
    }

    // Remaining characters can be alphanumeric or underscore
    for ch in chars {
        if ch.is_alphanumeric() || ch == '_' {
            result.push(ch);
        } else {
            result.push('_');
        }
    }

    result
}

#[inline]
fn fmt_otel<'a>(value: &'a OtelValue) -> DisplayOtelValue<'a> {
    DisplayOtelValue(value)
}

fn escape_label_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out
}

struct DisplayOtelValue<'a>(&'a OtelValue);
impl std::fmt::Display for DisplayOtelValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            OtelValue::Bool(true) => f.write_str("true"),
            OtelValue::Bool(false) => f.write_str("false"),
            OtelValue::Array(_) => f.write_str("[..]"),
            OtelValue::I64(x) => x.fmt(f),
            OtelValue::F64(x) => x.fmt(f),
            OtelValue::String(value) => f.write_str(&escape_label_value(value.as_str())),
            _ => f.write_str("???"),
        }
    }
}

macro_rules! variants_values {
    ($enum:ident: $(($name:ident, P::$otel_kind:ident, $kind:ident)),*) => {
        #[derive(Clone, Copy)]
        pub enum $enum {
            $(
            $name($kind),
            )*
        }

        impl std::fmt::Display for $enum {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(
                    Self::$name(x) => write!(f, "{x}"),
                    )*
                }
            }
        }

        impl From<$enum> for OtelValue {
            fn from(value: $enum) -> OtelValue {
                match value {
                $(
                    $enum::$name(value) => OtelValue::$otel_kind(value as _),
                )*
                }
            }
        }

        $(
        impl From<$kind> for $enum {
            fn from(value: $kind) -> Self {
                Self::$name(value)
            }
        }
        )*
    };
}

variants_values! {
    M64:
        (F64, P::F64, f64), (I64, P::I64, i64), (U64, P::I64, u64)
}
