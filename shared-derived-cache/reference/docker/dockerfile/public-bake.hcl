// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Overlay the existing build/test/lint targets with public, locally built bases.
// Invoke through scripts/docker-check.sh, which disables registry caches.
target "public-rust-base" {
  context = "."
  dockerfile = "docker/dockerfile/rust-base.Dockerfile"
  args = {
    BASE = "ubuntu:24.04"
    RUST_VERSION = RUST_VERSION
  }
  platforms = split(",", PLATFORMS)
}

target "_common" {
  args = {
    BASE = "ubuntu:24.04"
    RUST_BASE = "public-rust-base"
  }
  contexts = {
    public-rust-base = "target:public-rust-base"
  }
}
