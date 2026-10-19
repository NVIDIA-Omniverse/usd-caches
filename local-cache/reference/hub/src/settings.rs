// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{args::Mode, Args, Result};

use hub_util::settings::{CommandLineSource, Settings};
use std::{env, sync::Arc};

pub fn from_args(args: Arc<Args>) -> Result<Settings> {
    let cl_source = CommandLineSource::new(&args.overrides)?;
    let omniverse = Settings::load_omni_config(args.omniverse_config.as_deref())?;
    let mut hub = Settings::load_hub_config(args.mode == Mode::Exclusive, args.hub_config.as_deref(), cl_source)?;

    // calculate the cache path
    // if the cache path is set if the hub configuration (hub.toml) always use that as the truth
    // if its not set, fallback to the omniverse paths
    if hub.cache.path.is_none() {
        hub.cache.path = Some(omniverse.paths.cache_root.join(env!("CARGO_PKG_NAME")));
    }

    // calculate the log path
    if hub.logging.path.is_none() {
        hub.logging.path = Some(omniverse.paths.logs_root.join(env!("CARGO_PKG_NAME")));
    }

    Ok(Settings { omniverse, hub })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_log::test]
    fn exclusive_default_ports() {
        // Exclusive defaults to 0
        let args = Arc::new(Args::new(vec!["hub", "--mode=exclusive"]));
        let settings = from_args(args).unwrap();
        assert_eq!(settings.hub.http.port, 0);
        match settings.hub.transport {
            hub_util::settings::Transport::Tcp { rpc_port, .. } => {
                assert_eq!(rpc_port, 0);
            },
            _ => panic!("Should be tcp"),
        }

        // Overrides are still possible in exclusive mode
        let args = Arc::new(Args::new(vec![
            "hub",
            "--mode=exclusive",
            "--",
            "http.port=1111",
            "transport.tcp.rpc_port=3333",
            "transport.tcp.ext_port=4444",
        ]));
        let settings = from_args(args).unwrap();
        assert_eq!(settings.hub.http.port, 1111);
        match settings.hub.transport {
            hub_util::settings::Transport::Tcp { rpc_port, ext_port, .. } => {
                assert_eq!(rpc_port, 3333);
                assert_eq!(ext_port, 4444);
            },
            _ => panic!("Should be tcp"),
        }
    }

    #[test_log::test]
    fn shared_default_ports() {
        let args = Arc::new(Args::new(vec!["hub", "--mode=shared"]));
        let settings = from_args(args).unwrap();
        assert_eq!(settings.hub.http.port, hub_util::settings::Http::default().port);

        match settings.hub.transport {
            hub_util::settings::Transport::Tcp { rpc_port, ext_port, .. } => {
                match hub_util::settings::Transport::default() {
                    hub_util::settings::Transport::Tcp {
                        rpc_port: default_rpc_port,
                        ext_port: default_ext_port,
                        ..
                    } => {
                        assert_eq!(rpc_port, default_rpc_port);
                        assert_eq!(ext_port, default_ext_port);
                    },
                    _ => panic!("Should be tcp"),
                };
            },
            _ => panic!("Should be tcp"),
        }

        // Overrides are still possible in shared mode
        let args = Arc::new(Args::new(vec![
            "hub",
            "--mode=shared",
            "--",
            "http.port=1111",
            "transport.tcp.rpc_port=3333",
            "transport.tcp.ext_port=4444",
        ]));
        let settings = from_args(args).unwrap();
        assert_eq!(settings.hub.http.port, 1111);
        match settings.hub.transport {
            hub_util::settings::Transport::Tcp { rpc_port, ext_port, .. } => {
                assert_eq!(rpc_port, 3333);
                assert_eq!(ext_port, 4444);
            },
            _ => panic!("Should be tcp"),
        }
    }
}
