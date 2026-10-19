// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::CString;
use std::sync::Arc;

use parking_lot::RwLock;
use tracing::Level;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::filter::Filtered;
use tracing_subscriber::fmt::format::{DefaultFields, FmtSpan, Format, Full};
use tracing_subscriber::layer::Filter;
use tracing_subscriber::reload::Handle;
use tracing_subscriber::{fmt, prelude::*, reload, Registry};

use crate::types::{LogCallback, LogLevel};
use crate::Result;

pub(crate) struct LoggerBuilder {
    log_callback: Option<LogCallback>,
    log_level: Option<LogLevel>,
}

impl LoggerBuilder {
    fn new() -> Self {
        Self {
            log_callback: None,
            log_level: None,
        }
    }

    pub fn log_callback(mut self, log_callback: LogCallback) -> Self {
        self.log_callback = Some(log_callback);
        self
    }

    pub fn log_level(mut self, log_level: LogLevel) -> Self {
        self.log_level = Some(log_level);
        self
    }

    pub fn build(self) -> Result<Logger> {
        Logger::new(self.log_callback, self.log_level)
    }
}

type ReloadHandle = Handle<
    Filtered<fmt::Layer<Registry, DefaultFields, Format<Full, ()>, NonBlocking>, CallbackFilter, Registry>,
    Registry,
>;

pub(crate) struct Logger {
    _guards: Vec<WorkerGuard>,
    log_callback: Arc<RwLock<Option<LogCallback>>>,
    reload_handle: ReloadHandle,
}

impl Logger {
    pub(crate) fn builder() -> LoggerBuilder {
        LoggerBuilder::new()
    }

    fn new(log_callback: Option<LogCallback>, log_level: Option<LogLevel>) -> Result<Self> {
        let mut guards = Vec::new();

        // Create the layer for outputting to a provided callback
        let format = fmt::format()
            .without_time()
            .with_level(true)
            .with_target(false)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_ansi(false);

        let filter = CallbackFilter::new(log_callback.is_some(), log_level);
        let log_callback: Arc<RwLock<Option<LogCallback>>> = Arc::new(RwLock::new(log_callback));
        let writer = CallbackWriter::new(log_callback.clone());
        let (non_blocking, guard) = tracing_appender::non_blocking(writer);
        let callback_layer = fmt::layer()
            .with_writer(non_blocking)
            .with_span_events(FmtSpan::NONE)
            .event_format(format)
            .with_filter(filter);
        guards.push(guard);

        let (callback_layer, reload_handle) = reload::Layer::new(callback_layer);

        let subscriber = tracing_subscriber::registry().with(callback_layer);

        // Best effort to set tracing subscriber. If someone else has already set it, that's fine
        let _ = tracing::subscriber::set_global_default(subscriber);

        Ok(Self {
            _guards: guards,
            log_callback,
            reload_handle,
        })
    }

    pub(crate) fn set_log_level(&self, level: LogLevel) -> Result<()> {
        self.reload_handle
            .modify(|layer| layer.filter_mut().set_log_level(level))?;
        Ok(())
    }

    pub(crate) fn set_log_callback(&self, callback: LogCallback) -> Result<()> {
        *self.log_callback.write() = Some(callback);
        self.reload_handle.modify(|layer| layer.filter_mut().enable())?;
        Ok(())
    }

    pub(crate) fn disable_log_callback(&self) -> Result<()> {
        *self.log_callback.write() = None;
        self.reload_handle.modify(|layer| layer.filter_mut().disable())?;
        Ok(())
    }
}

struct CallbackFilter {
    enabled: bool,
    level: LogLevel,
}

impl CallbackFilter {
    fn new(enabled: bool, level: Option<LogLevel>) -> Self {
        Self {
            enabled,
            level: level.unwrap_or_default(),
        }
    }

    fn enable(&mut self) {
        self.enabled = true;
    }

    fn disable(&mut self) {
        self.enabled = false;
    }

    fn set_log_level(&mut self, level: LogLevel) {
        self.level = level;
    }
}

impl<S> Filter<S> for CallbackFilter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: &tracing_subscriber::layer::Context<'_, S>) -> bool {
        if !self.enabled {
            return false;
        }

        match *metadata.level() {
            Level::TRACE => self.level <= LogLevel::Trace,
            Level::DEBUG => self.level <= LogLevel::Debug,
            Level::INFO => self.level <= LogLevel::Info,
            Level::WARN => self.level <= LogLevel::Warn,
            Level::ERROR => self.level <= LogLevel::Error,
        }
    }
}

struct CallbackWriter {
    log_callback: Arc<RwLock<Option<LogCallback>>>,
}

impl CallbackWriter {
    fn new(log_callback: Arc<RwLock<Option<LogCallback>>>) -> Self {
        Self { log_callback }
    }
}

impl std::io::Write for CallbackWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        static LEVEL_STR_LEN: usize = 6;
        if buf.len() < LEVEL_STR_LEN {
            return Err(std::io::Error::other("Log message did not start with level string"));
        }
        // Don't let the log callback change while we're logging
        let read = self.log_callback.read();

        match read.as_ref() {
            Some(call) => {
                let level = match buf[0] {
                    // INFO and WARN start with a space
                    b' ' => match buf[1] {
                        b'I' => LogLevel::Info,
                        b'W' => LogLevel::Warn,
                        _ => return Err(std::io::Error::other("Log message did not start with level string")),
                    },
                    b'D' => LogLevel::Debug,
                    b'E' => LogLevel::Error,
                    b'T' => LogLevel::Trace,
                    _ => return Err(std::io::Error::other("Log message did not start with level string")),
                };
                // Log the message without the level string. The level string is always 5 bytes and then a space.
                // Also strip the newline at the end of the string
                match CString::new(&buf[LEVEL_STR_LEN..buf.len() - 1]) {
                    Ok(s) => {
                        call(level, s.as_ptr());
                        Ok(buf.len())
                    },
                    Err(_) => Err(std::io::Error::other("Failed to allocate CString for log callback")),
                }
            },
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
