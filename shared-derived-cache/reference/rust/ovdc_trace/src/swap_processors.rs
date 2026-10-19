// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, Mutex, Weak};

use opentelemetry_sdk::{
    logs::LogProcessor,
    metrics::{reader::MetricReader, Pipeline},
    trace::SpanProcessor,
    Resource,
};

type Pipes = Vec<Weak<Pipeline>>;
#[derive(Default)]
struct Common {
    pipes: Pipes,
    resource: Option<Resource>,
}
type Shared = Arc<Mutex<Common>>;

impl std::fmt::Debug for Common {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Common")
    }
}

fn use_pipes(pipes: &mut Pipes, f: impl Fn(&Weak<Pipeline>)) {
    pipes.retain(|y| {
        if y.strong_count() == 0 {
            return false;
        }
        f(y);
        true
    });
}

#[allow(dead_code)]
pub fn processors() -> (Logs, Traces, Metrics) {
    let shared = Shared::default();
    (
        Logs {
            inner: Default::default(),
            shared: shared.clone(),
        },
        Traces {
            inner: Default::default(),
            shared: shared.clone(),
        },
        Metrics {
            inner: Default::default(),
            shared,
        },
    )
}

#[derive(Debug, Clone)]
pub struct Logs {
    inner: Arc<Mutex<Option<Box<dyn LogProcessor + Send + Sync>>>>,
    shared: Shared,
}

impl Logs {
    /// Replace the inner processor with a new one.
    /// Returns the previous processor, if any.
    pub fn swap(
        &self,
        mut value: Option<Box<dyn LogProcessor + Sync + Send + 'static>>,
    ) -> Option<Box<dyn LogProcessor + Sync + Send + 'static>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(new_inner) = value.as_mut() {
            let shared = self.shared.lock().unwrap();
            if let Some(rsx) = shared.resource.as_ref() {
                new_inner.set_resource(rsx);
            }
        }
        std::mem::replace(&mut *inner, value)
    }

    /// Clear the inner processor, making this a no-op.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = None;
    }
}

impl LogProcessor for Logs {
    fn emit(
        &self,
        data: &mut opentelemetry_sdk::logs::SdkLogRecord,
        instrumentation: &opentelemetry::InstrumentationScope,
    ) {
        if let Some(inner) = self.inner.lock().unwrap().as_mut() {
            inner.emit(data, instrumentation);
        }
        // No-op if inner is None
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            return inner.force_flush();
        }
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            return inner.shutdown_with_timeout(timeout);
        }
        Ok(())
    }

    fn shutdown(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            return inner.shutdown();
        }
        Ok(())
    }

    fn event_enabled(
        &self,
        level: opentelemetry::logs::Severity,
        target: &str,
        name: Option<&str>,
    ) -> bool {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            return inner.event_enabled(level, target, name);
        }
        false
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        if let Some(inner) = self.inner.lock().unwrap().as_mut() {
            inner.set_resource(resource);
        }
        let mut shared = self.shared.lock().unwrap();
        shared.resource = Some(resource.clone());
    }
}

#[derive(Debug, Clone)]
pub struct Traces {
    inner: Arc<Mutex<Option<Box<dyn SpanProcessor + Send + Sync>>>>,
    shared: Shared,
}

impl Traces {
    /// Replace the inner processor with a new one.
    /// Returns the previous processor, if any.
    pub fn swap(
        &self,
        mut value: Option<Box<dyn SpanProcessor + Sync + Send + 'static>>,
    ) -> Option<Box<dyn SpanProcessor + Sync + Send + 'static>> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(new_inner) = value.as_mut() {
            let shared = self.shared.lock().unwrap();
            if let Some(rsx) = shared.resource.as_ref() {
                new_inner.set_resource(rsx);
            }
        }
        std::mem::replace(&mut *inner, value)
    }

    /// Clear the inner processor, making this a no-op.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = None;
    }
}

impl SpanProcessor for Traces {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.on_start(span, cx);
        }
        // No-op if inner is None
    }

    fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.on_end(span);
        }
        // No-op if inner is None
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.force_flush()
        } else {
            Ok(())
        }
    }

    fn shutdown_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.shutdown_with_timeout(timeout)
        } else {
            Ok(())
        }
    }

    fn shutdown(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.shutdown()
        } else {
            Ok(())
        }
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        if let Some(inner) = self.inner.lock().unwrap().as_mut() {
            inner.set_resource(resource);
        }
        let mut shared = self.shared.lock().unwrap();
        shared.resource = Some(resource.clone());
    }
}

#[derive(Debug, Clone)]
pub struct Metrics {
    inner: Arc<Mutex<Option<Box<dyn MetricReader + Send + Sync>>>>,
    shared: Shared,
}

impl Metrics {
    /// Replace the inner reader with a new one.
    /// Returns the previous reader, if any.
    pub fn swap(&self, value: Option<Box<dyn MetricReader + Sync + Send + 'static>>) {
        let mut inner = self.inner.lock().unwrap();
        inner.take();
        if let Some(reader) = value.as_ref() {
            let mut shared = self.shared.lock().unwrap();
            use_pipes(&mut shared.pipes, |pipe| {
                reader.register_pipeline(pipe.clone());
            });
        }
        *inner = value;
    }

    /// Clear the inner reader, making this a no-op.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        *inner = None;
    }
}

impl MetricReader for Metrics {
    fn register_pipeline(&self, pipe: Weak<Pipeline>) {
        {
            let guard = self.inner.lock().unwrap();
            if let Some(inner) = guard.as_ref() {
                inner.register_pipeline(pipe.clone());
            }
        }

        let mut shared = self.shared.lock().unwrap();
        shared.pipes.push(pipe);

        shared.pipes.retain(|y| y.strong_count() > 0);
    }

    fn collect(
        &self,
        rm: &mut opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.collect(rm)
        } else {
            Ok(())
        }
    }

    fn force_flush(&self) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.force_flush()
        } else {
            Ok(())
        }
    }

    fn shutdown_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.shutdown_with_timeout(timeout)
        } else {
            Ok(())
        }
    }

    fn shutdown(&self) -> Result<(), opentelemetry_sdk::error::OTelSdkError> {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.shutdown()
        } else {
            Ok(())
        }
    }

    fn temporality(
        &self,
        kind: opentelemetry_sdk::metrics::InstrumentKind,
    ) -> opentelemetry_sdk::metrics::Temporality {
        if let Some(inner) = self.inner.lock().unwrap().as_ref() {
            inner.temporality(kind)
        } else {
            // Default to cumulative temporality when no reader is present
            opentelemetry_sdk::metrics::Temporality::Cumulative
        }
    }
}
