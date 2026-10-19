// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bakes the product version into the crate at compile time.
//!
//! The product version is not the Cargo package version -- every crate in the
//! workspace is 0.1.x and independent of what is released. The release version
//! lives in `docker/helm/Chart.yaml` (`appVersion`), which is outside the build
//! context the Docker `build` stage binds, so CI passes it in through
//! `OVDC_VERSION` instead. A build without that variable set -- any local
//! `cargo build` -- reports `dev`, which is the honest answer.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=OVDC_VERSION");

    let version = env::var("OVDC_VERSION")
        .unwrap_or_default()
        .trim()
        .to_string();
    let version = if version.is_empty() {
        "dev".to_string()
    } else {
        version
    };

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo")).join("version.rs");
    fs::write(
        &out,
        format!("pub static VERSION_STR: &str = {version:?};\n"),
    )
    .unwrap_or_else(|e| panic!("failed to write {}: {e}", out.display()));
}
