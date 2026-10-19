// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .codec_path("crate::measuring_codec::MeasuringProstCodec")
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(&["proto/rpc.proto"], &["proto/"])?;
    Ok(())
}
