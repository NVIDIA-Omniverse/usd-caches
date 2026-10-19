// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Regenerates `rust/ATTRIBUTIONS.md` as part of building the service.
//!
//! This is keyed to `Cargo.lock` rather than to every build: the attribution
//! list can only change when the resolved dependency set changes, so
//! `rerun-if-changed` on the lock file regenerates exactly when it could differ
//! and costs nothing on an ordinary incremental build.
//!
//! Failure here is a warning, never an error. The file is a documentation
//! artifact, and an environment where `cargo metadata` cannot run -- a vendored
//! source tree, a read-only checkout, a sandbox without the registry -- should
//! still produce a working binary. `OVDC_SKIP_ATTRIBUTIONS=1` opts out entirely.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../Cargo.lock");
    println!("cargo:rerun-if-env-changed=OVDC_SKIP_ATTRIBUTIONS");

    if std::env::var_os("OVDC_SKIP_ATTRIBUTIONS").is_some_and(|value| value != "0") {
        return;
    }

    let manifest_dir =
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let Some(workspace_root) = Path::new(&manifest_dir).parent() else {
        println!("cargo:warning=attributions: crate directory has no parent; skipping");
        return;
    };

    // Cargo points `CARGO` at the exact binary driving this build, which matters
    // when the toolchain is pinned: a bare `cargo` could resolve to a different
    // one on PATH and report a different graph than the one being compiled.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    match ovdc_attributions::generate(&cargo, workspace_root) {
        Ok(true) => println!("cargo:warning=attributions: regenerated ATTRIBUTIONS.md"),
        Ok(false) => {}
        Err(err) => println!("cargo:warning=attributions: skipped ({err})"),
    }
}
