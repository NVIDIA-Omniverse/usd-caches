// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{net::IpAddr, path::PathBuf};

const DEFAULT_CONFIG: &str = include_str!("../config.toml");

/// Resolves the config file path from the optional CLI argument.
///
/// - If no argument is given, defaults to `./config.toml` and writes the
///   embedded default config to that path if the file does not yet exist.
/// - If an argument is given, warns if the file does not have a `.toml`
///   extension, and exits with an error if the file does not exist.
fn resolve_config_path(arg: Option<String>) -> PathBuf {
    match arg {
        None => {
            let path = PathBuf::from("./config.toml");
            if !path.exists() {
                std::fs::write(&path, DEFAULT_CONFIG).expect("failed to write default config.toml");
            }
            path
        }
        Some(s) => {
            let path = PathBuf::from(&s);
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                eprintln!("WARN: config path '{s}' does not have a .toml extension");
            }
            if !path.exists() {
                eprintln!("ERROR: config file '{s}' does not exist");
                std::process::exit(1);
            }
            path
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // k8s has a mount that provides this, but we need to provide an env
    // directory
    let mut args = std::env::args();
    let arg_0 = args
        .nth(1)
        .expect("must provide a path as the first argument for the location of the database");

    let config_path = resolve_config_path(args.next());

    let this_ip: Option<IpAddr> = std::env::var("IP")
        .ok()
        .map(|y| y.parse().expect("IP env is not an ip address"));

    let settings: ovdc_settings::Cfg = ovdc_settings::Settings::new(config_path).await;

    {
        // Load trace configuration from settings and set up with hot-reload
        let _pm = ovdc_trace::setup(settings.clone());
    }

    // Serve the KV node
    ovdc::Prepared::new(arg_0, settings, this_ip)
        .await
        .serve()
        .await;
}
