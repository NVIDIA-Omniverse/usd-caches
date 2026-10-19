// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Writes `rust/ATTRIBUTIONS.md` from the resolved cargo dependency graph.
//!
//! Usage:
//!   cargo run -p ovdc_attributions --bin gen-attributions
//!   cargo run -p ovdc_attributions --bin gen-attributions -- --check
//!
//! `--check` writes nothing and exits non-zero if the committed file is stale,
//! which is what CI runs to catch a dependency change that did not bring the
//! attributions along with it.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The workspace root, derived from this crate's location rather than the
/// current directory, so the command works from anywhere in the tree.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate directory is nested in the workspace")
        .to_path_buf()
}

fn run() -> ovdc_attributions::Result<ExitCode> {
    let check_only = std::env::args().skip(1).any(|arg| arg == "--check");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let root = workspace_root();
    let target = root.join("ATTRIBUTIONS.md");

    let metadata = ovdc_attributions::cargo_metadata(&cargo, &root.join("Cargo.toml"))?;
    let rendered = ovdc_attributions::render(&metadata)?;

    if check_only {
        if ovdc_attributions::check(&target, &rendered) {
            println!("{} is up to date", target.display());
            return Ok(ExitCode::SUCCESS);
        }
        eprintln!(
            "{} is stale; regenerate with `cargo run -p ovdc_attributions --bin gen-attributions`",
            target.display()
        );
        return Ok(ExitCode::FAILURE);
    }

    match ovdc_attributions::write_if_changed(&target, &rendered)? {
        true => println!("wrote {}", target.display()),
        false => println!("{} is already up to date", target.display()),
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("gen-attributions: {err}");
            ExitCode::FAILURE
        }
    }
}
