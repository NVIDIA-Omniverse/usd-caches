// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure_protobuf();

    let mut config = prost_build::Config::new();

    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config.compile_protos(
        &["proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto"],
        &["proto"],
    )?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn configure_protobuf() {
    // Expect "PROTOC" to be set in the environment
}

#[cfg(not(target_os = "windows"))]
fn configure_protobuf() {
    // Use vendored protoc from protobuf-src
    std::env::set_var("PROTOC", protobuf_src::protoc());
}
