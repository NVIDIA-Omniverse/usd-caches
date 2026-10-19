// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing::warn;

use crate::{args::Args, error::Error, logging::Logging};
use hub_cache::Cache;
use hub_util::settings::Settings;

/// Context for the running Hub instance. This contains various structures representing the state of the running
/// Hub service. New pieces of state should be added to this object because it is already plumbed through the
/// subcomponents.
#[derive(Debug)]
pub struct Context {
    /// The Hub cache
    pub cache: Arc<Cache>,
    /// Settings: held in an RwLock so they can be modified at runtime
    pub settings: Arc<RwLock<Settings>>,
    /// Notified when the shutdown API or RPC method is called
    pub shutdown_notify: Arc<Notify>,
    /// Notified when the config changes and Hub should reset
    pub reset_notify: Arc<Notify>,
    /// Notified when the msgpack server is ready to serve. We need to wait on this to know the port the server bound to
    pub msgpack_ready: Arc<Notify>,
    /// Notified when the protobuf server is ready to serve
    pub protobuf_ready: Arc<Notify>,
    /// Hold the logging struct until the service exits
    _logging: Option<Logging>,
    /// The arguments that the service was launched with
    pub args: Arc<Args>,
}

impl Context {
    pub fn new(settings: Settings, args: Arc<Args>, logging: Logging) -> Result<Self> {
        // Issue warning that launch_id is deprecated. Need to wait until after logging is initialized to do this
        if args.launch_id.is_some() {
            warn!("Command line option 'launch_id' is deprecated!")
        }

        let cache = Cache::builder(
            settings
                .hub
                .cache
                .path
                .as_ref()
                .ok_or(Error::Configuration("Hub Cache Path not set or calculated".to_owned()))?,
        )
        .max_size(settings.hub.cache.max_size)?
        .garbage_collection_threshold(settings.hub.cache.cleanup_threshold)?
        .open()?;

        Ok(Self {
            cache: Arc::new(cache),
            settings: Arc::new(RwLock::new(settings)),
            shutdown_notify: Arc::new(Notify::new()),
            reset_notify: Arc::new(Notify::new()),
            msgpack_ready: Arc::new(Notify::new()),
            protobuf_ready: Arc::new(Notify::new()),
            _logging: Some(logging),
            args,
        })
    }
}

/// Helper struct for making Context objects for unit tests
#[cfg(test)]
pub struct TestContext {
    pub tmp_dir: tempfile::TempDir,
    pub context: Arc<Context>,
}

#[cfg(test)]
impl TestContext {
    pub fn new() -> Self {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cache = Cache::builder(tmp_dir.path()).open().unwrap();

        Self {
            tmp_dir,
            context: Arc::new(Context {
                cache: Arc::new(cache),
                settings: Arc::new(RwLock::new(Settings::default())),
                shutdown_notify: Arc::new(Notify::new()),
                reset_notify: Arc::new(Notify::new()),
                msgpack_ready: Arc::new(Notify::new()),
                protobuf_ready: Arc::new(Notify::new()),
                _logging: None,
                args: Arc::new(Args::default()),
            }),
        }
    }

    pub fn new_with_cache(tmp_dir: tempfile::TempDir, cache: Arc<Cache>) -> Self {
        Self {
            tmp_dir,
            context: Arc::new(Context {
                cache,
                settings: Arc::new(RwLock::new(Settings::default())),
                shutdown_notify: Arc::new(Notify::new()),
                reset_notify: Arc::new(Notify::new()),
                msgpack_ready: Arc::new(Notify::new()),
                protobuf_ready: Arc::new(Notify::new()),
                _logging: None,
                args: Arc::new(Args::default()),
            }),
        }
    }

    pub fn new_with_hub_toml() -> Self {
        let tmp_dir = tempfile::tempdir().unwrap();
        let cache = Cache::builder(tmp_dir.path()).open().unwrap();
        let hub_toml_path = tmp_dir.path().join("hub.toml");
        let args = Args {
            hub_config: Some(hub_toml_path),
            ..Default::default()
        };

        Self {
            tmp_dir,
            context: Arc::new(Context {
                cache: Arc::new(cache),
                settings: Arc::new(RwLock::new(Settings::default())),
                shutdown_notify: Arc::new(Notify::new()),
                reset_notify: Arc::new(Notify::new()),
                msgpack_ready: Arc::new(Notify::new()),
                protobuf_ready: Arc::new(Notify::new()),
                _logging: None,
                args: Arc::new(args),
            }),
        }
    }
}
