// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod cache;
pub use cache::Cache;

mod http;
pub use http::Http;

mod hub;
pub use hub::Hub;
pub use hub::SerializeToToml;

mod logging;
pub use logging::LogLevel;
pub use logging::Logging;

mod omniverse;
pub use omniverse::Omniverse;

mod runtime;
pub use runtime::Runtime;

mod transport;
pub use transport::Transport;

use anyhow::Result;
use config::{Config, ConfigError, Environment, File, Map, Source, Value};
use etcetera::home_dir;
use lazy_static::lazy_static;
use std::env;
use std::path::{Path, PathBuf};

lazy_static! {
    pub static ref CONFIG_DIR: PathBuf = home_dir().unwrap_or_default().join(".nvidia-omniverse/config");
    pub static ref HUB_TOML: PathBuf = PathBuf::from("hub.toml");
    pub static ref OMNIVERSE_TOML: PathBuf = PathBuf::from("omniverse.toml");
}

#[derive(Debug, Default)]
pub struct Settings {
    /// settings from omniverse.toml
    pub omniverse: Omniverse,
    /// settings from settings.hub.toml
    pub hub: Hub,
}

impl Settings {
    pub fn default_hub_config() -> PathBuf {
        CONFIG_DIR.join(HUB_TOML.as_path())
    }

    pub fn default_omni_config() -> PathBuf {
        CONFIG_DIR.join(OMNIVERSE_TOML.as_path())
    }

    pub fn load_hub_config(
        default_ports_to_any: bool,
        hub_config: Option<&Path>,
        cl_source: CommandLineSource,
    ) -> Result<Hub> {
        // if hub config is specified on command line, just use that, otherwise
        // try and load the config next to the executable
        // then try and load it from user's home directory and there is no home directory
        // try and load it from the current working directory
        let mut builder = Config::builder();

        // In exclusive mode, we want to let the OS assign us free ports to avoid any conflicts. However we still want the
        // user to be able to override these values, so we set the defaults to port 0 in this case and then override those
        // values with any user provided settings.
        if default_ports_to_any {
            builder = builder.set_default("transport.tcp.rpc_port", "0")?;
            builder = builder.set_default("transport.tcp.ext_port", "0")?;
            builder = builder.set_default("http.port", "0")?;
        }

        if let Some(hub_config) = hub_config {
            builder = builder.add_source(File::from(hub_config).required(false));
        } else {
            if let Some(current_path) = env::current_exe()?.parent() {
                builder = builder.add_source(File::from(current_path.join(HUB_TOML.as_path())).required(false));
            }
            // do not load user hub.toml when running test
            if !cfg!(test) {
                builder = builder.add_source(File::from(Self::default_hub_config()).required(false));
            }
        }

        // add in env var overrides
        builder = builder.add_source(
            Environment::with_prefix("HUB")
                .separator("__")
                .ignore_empty(true)
                .try_parsing(true),
        );

        // add in command line overrides
        builder = builder.add_source(cl_source);

        Ok(builder.build()?.try_deserialize()?)
    }

    /// Loads the Hub settings only from environment variables and hub.toml
    pub fn load_hub_config_from_user_settings() -> Result<Hub> {
        let mut builder = Config::builder();

        if let Some(current_path) = env::current_exe()?.parent() {
            builder = builder.add_source(File::from(current_path.join(HUB_TOML.as_path())).required(false));
        }
        // do not load user hub.toml when running test
        if !cfg!(test) {
            builder = builder.add_source(File::from(Self::default_hub_config()).required(false));
        }

        // add in env var overrides
        builder = builder.add_source(
            Environment::with_prefix("HUB")
                .separator("__")
                .ignore_empty(true)
                .try_parsing(true),
        );

        Ok(builder.build()?.try_deserialize()?)
    }

    pub fn load_omni_config(omniverse_config: Option<&Path>) -> Result<Omniverse> {
        let mut builder = Config::builder();

        if let Some(omniverse_config) = omniverse_config {
            builder = builder.add_source(File::from(omniverse_config).required(false));
        } else if !cfg!(test) {
            builder = builder.add_source(File::from(Settings::default_omni_config()).required(false));
        }

        // add in env var overrides
        builder = builder.add_source(
            Environment::with_prefix("OMNI")
                .separator("__")
                .ignore_empty(true)
                .try_parsing(true),
        );

        Ok(builder.build()?.try_deserialize()?)
    }
}

/// A config source for the command line options provided to the program.
#[derive(Debug, Clone)]
pub struct CommandLineSource {
    config: Vec<(String, Value)>,
}

impl CommandLineSource {
    pub fn new(config_values: &[String]) -> Result<Self> {
        let source = "Command Line".to_owned();

        // Attempt to parse each provide value. If any option is not in the form key=value, return an error
        let mut config: Vec<(String, Value)> = Vec::new();
        for value in config_values {
            let k_v = value.split('=').collect::<Vec<&str>>();
            if k_v.len() != 2 {
                return Err(ConfigError::Message(format!(
                    "expected configuration value in the form key=value, found {value}"
                ))
                .into());
            }
            config.push((k_v[0].to_string(), Value::new(Some(&source), k_v[1].to_string())))
        }

        Ok(Self { config })
    }
}

impl Source for CommandLineSource {
    fn clone_into_box(&self) -> Box<dyn Source + Send + Sync> {
        Box::new((*self).clone())
    }

    /// Collect all configuration properties available from this source and return a map
    fn collect(&self) -> std::result::Result<Map<String, Value>, ConfigError> {
        let mut map = Map::new();
        self.config.iter().for_each(|c| {
            map.insert(c.0.clone(), c.1.clone());
        });
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;

    #[test]
    fn omniverse_defaults() {
        // because ~/.nvidia-omniverse/config/omniverse.toml is different on each system
        // load it here through serde json
        let omniverse: Omniverse = serde_json::from_str("{}").unwrap();
        assert_eq!(omniverse.paths.cache_root, omniverse::default_cache_root());
        assert_eq!(omniverse.paths.data_root, omniverse::default_data_root());
        assert_eq!(omniverse.paths.logs_root, omniverse::default_logs_root());
    }

    #[test]
    fn hub_defaults() {
        // let settings = Settings::with_args(Rc::new(Args::default())).unwrap();
        let settings = Settings::default();
        println!("{settings:?}");

        // cache
        assert_eq!(settings.hub.cache.max_size, cache::default_max_size());
        assert_eq!(settings.hub.cache.cleanup_threshold, cache::default_cleanup_threshold());
        assert_eq!(
            settings.hub.cache.garbage_collection_interval,
            cache::default_garbage_collection_interval()
        );

        // http
        assert_eq!(settings.hub.http.host, http::default_host());
        assert_eq!(settings.hub.http.port, http::default_port());

        // logging
        assert_eq!(LogLevel::default(), LogLevel::Debug);
        assert!(settings.hub.logging.file.enabled);
        assert_eq!(settings.hub.logging.file.max_level, logging::default_file_max_level());
        assert_eq!(settings.hub.logging.file.file, logging::default_file_file());
        assert_eq!(settings.hub.logging.stdout.enabled, logging::default_stdout_enabled());
        assert_eq!(settings.hub.logging.stdout.color, logging::default_stdio_color());
        assert_eq!(
            settings.hub.logging.stdout.max_level,
            logging::default_stdio_max_level()
        );
        assert_eq!(settings.hub.logging.stderr.enabled, logging::default_stderr_enabled());
        assert_eq!(settings.hub.logging.stderr.color, logging::default_stdio_color());
        assert_eq!(
            settings.hub.logging.stderr.max_level,
            logging::default_stdio_max_level()
        );

        // runtime
        assert_eq!(
            settings.hub.runtime.num_worker_threads,
            runtime::default_num_worker_threads()
        );

        // transport
        assert_matches!(settings.hub.transport, Transport::Tcp { host, rpc_port, ext_port } => {
            assert_eq!(host, transport::default_host());
            assert_eq!(rpc_port, transport::default_rpc_port());
            assert_eq!(ext_port, transport::default_ext_port());
        });
    }
}
