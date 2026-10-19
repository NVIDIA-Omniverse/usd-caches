// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use tracing::subscriber::DefaultGuard;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::{fmt, prelude::*};

use std::io;

use crate::error::Error;
use crate::Result;

use hub_util::settings::{LogLevel, Settings};

#[derive(Debug)]
pub struct Logging {
    _subscriber_guard: DefaultGuard,
    _worker_guards: Vec<WorkerGuard>,
}

impl Logging {
    pub fn new(settings: &Settings) -> Result<Self> {
        let format = fmt::format()
            .with_level(true)
            .with_source_location(true)
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true);

        let mut worker_guards = Vec::new();

        let file_layer = if settings.hub.logging.file.enabled {
            let log_path = settings.hub.logging.path.as_ref().ok_or(Error::Configuration(
                "Hub Logging Path not set or calculated".to_owned(),
            ))?;

            let path = log_path.join(&settings.hub.logging.file.file);
            let count = settings.hub.logging.file.count;
            let max_size = settings.hub.logging.file.max_size.as_u64() as usize;
            let compression = match settings.hub.logging.file.compression {
                true => Compression::OnRotate(1),
                false => Compression::None,
            };

            let file_appender = FileRotate::new(
                path,
                AppendCount::new(count),
                ContentLimit::BytesSurpassed(max_size),
                compression,
                #[cfg(unix)]
                Some(0o600),
            );

            let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
                .buffered_lines_limit(1_048_576)
                .lossy(false)
                .finish(file_appender);

            // drop of guard will ensure any pending logs are flushed
            worker_guards.push(guard);

            let filter = EnvFilter::builder()
                .with_default_directive(to_level(&settings.hub.logging.file.max_level).into())
                .from_env_lossy();

            Some(
                fmt::layer()
                    .with_writer(non_blocking)
                    .event_format(format.clone())
                    .with_ansi(false)
                    .json()
                    .with_current_span(true)
                    .with_filter(filter),
            )
        } else {
            None
        };

        let stdout_layer = if settings.hub.logging.stdout.enabled {
            let filter = EnvFilter::builder()
                .with_default_directive(to_level(&settings.hub.logging.stdout.max_level).into())
                .from_env_lossy();

            Some(
                fmt::layer()
                    .with_writer(io::stdout)
                    .event_format(format.clone())
                    .with_ansi(settings.hub.logging.stdout.color)
                    .with_filter(filter),
            )
        } else {
            None
        };

        let stderr_layer = if settings.hub.logging.stderr.enabled {
            let filter = EnvFilter::builder()
                .with_default_directive(to_level(&settings.hub.logging.stderr.max_level).into())
                .from_env_lossy();

            Some(
                fmt::layer()
                    .with_writer(io::stderr)
                    .event_format(format)
                    .with_ansi(settings.hub.logging.stderr.color)
                    .with_filter(filter),
            )
        } else {
            None
        };

        // set the default subscriber, which will exist as long as we hold the guard
        // if hub settings change and we have to reload tracing, we will drop the guard
        // and recreate it, since tracing subscriber cannot dynamically reload filtered layers
        // see issue: https://github.com/tokio-rs/tracing/issues/1629
        //
        // this is fine for hub, as if settings change, we have to reload the entire main loop
        // so reloading the tracing subscriber let's us just reload the entire tracing setup
        // and we do not have to worry about trying to reload filterd layers dynamically
        let subscriber_guard = tracing_subscriber::registry()
            .with(file_layer)
            .with(stdout_layer)
            .with(stderr_layer)
            .set_default();

        Ok(Self {
            _subscriber_guard: subscriber_guard,
            _worker_guards: worker_guards,
        })
    }
}

fn to_level(log_level: &LogLevel) -> LevelFilter {
    match log_level {
        LogLevel::Off => LevelFilter::OFF,
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // use assert_matches::assert_matches;
    use bytesize::ByteSize;
    use rstest::*;
    use tempfile::tempdir;
    use tracing::{debug, error, info, trace, warn};

    use std::path::PathBuf;

    // this test needs to be run standalone because of `tracing::subscriber::set_global_default`
    // unless there is a clever way to set this up per thread. using `#[ignore]` for now

    #[rstest]
    #[case(true)]
    #[case(false)]
    #[ignore]
    fn log_rotation(#[case] use_compression: bool) {
        let tmp_dir = tempdir().unwrap();
        let path = tmp_dir.path().to_path_buf();
        let file = PathBuf::from("test.log");
        let max_size = 50;

        let mut settings = Settings::default();
        settings.hub.logging.path = Some(path.clone());
        settings.hub.logging.file.enabled = true;
        settings.hub.logging.file.max_level = LogLevel::Trace;
        settings.hub.logging.file.file = file.clone();
        settings.hub.logging.file.count = 5;
        settings.hub.logging.file.max_size = ByteSize::b(max_size);
        settings.hub.logging.file.compression = use_compression;

        let logging = Logging::new(&settings).unwrap();
        let log_path = path.join(&file);
        let md = log_path.metadata().unwrap();
        assert!(md.is_file());
        assert_eq!(md.len(), 0);

        // test rotation
        for _ in 0..4 {
            error!(?settings, ?path, ?file, "error");
            warn!(?settings, ?path, ?file, "warn");
            info!(?settings, ?path, ?file, "info");
            debug!(?settings, ?path, ?file, "debug");
            trace!(?settings, ?path, ?file, "trace");
        }

        // need to drop logging so that the non-blocking guard
        // is dropped and any queues are flushed to ensure file rotations
        drop(logging);

        // test.log
        let log_path = path.join(&file);
        let md = log_path.metadata().unwrap();
        assert!(md.is_file());
        assert!(md.len() > 0);

        // test.log.1
        let log_path = path.join(format!("{}.1", file.display()));
        let md = log_path.metadata().unwrap();
        assert!(md.is_file());
        assert!(md.len() >= max_size);

        // test.log.{2..5}.gz
        for i in 2..=5 {
            let log_path = match use_compression {
                true => path.join(format!("{}.{}.gz", file.display(), i)),
                false => path.join(format!("{}.{}", file.display(), i)),
            };
            let md = log_path.metadata().unwrap();
            assert!(md.is_file());
            if use_compression {
                assert!(md.len() > 0);
            } else {
                assert!(md.len() >= max_size);
            }
        }

        // test.log.6.gz should not exist
        let log_path = path.join("test.log.6.gz");
        assert!(!log_path.exists());
    }
}
