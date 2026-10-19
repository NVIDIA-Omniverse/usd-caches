// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Thin `ovcc` binary that loads a config file (path from argv, default
//! `values.yaml`) and serves the reverse-proxy cache until Ctrl-C or SIGTERM.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

// glibc keeps freed pages resident in per-thread arenas, so RSS ratchets
// toward the allocation peak. jemalloc's background threads return unused
// pages to the OS after a short decay, even while the process is idle.
#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() -> ExitCode {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "values.yaml".to_owned());

    // Config load runs before telemetry init (it carries the telemetry
    // settings), so a load failure is still reported with `eprintln!`.
    let config = match ovcc::load(Path::new(&config_path)) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ovcc: failed to load config {config_path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Hold the guard for the process lifetime so batched spans flush on exit.
    // A failure to install (e.g. a subscriber already set in a test harness)
    // degrades to a no-op rather than aborting startup, but is reported so a
    // telemetry misconfiguration is never silent.
    let _telemetry = match ovcc_telemetry::init(&config.telemetry) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("ovcc: telemetry init failed, continuing without it: {err}");
            ovcc_telemetry::TelemetryGuard::noop()
        }
    };

    let storage_root = PathBuf::from(config.cache.root.clone());
    if let Err(err) = ovcc::run(config, storage_root) {
        tracing::error!("ovcc: serve failed: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
