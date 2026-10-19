<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Contributing

This project is currently not accepting contributions.

Use the compiler pinned in `rust/rust-toolchain.toml`. Follow the
[native setup](README.md#native-development) and run `make build`, `make test`,
and `make lint`, or use the [Docker helpers](scripts/README.md) and run
`./scripts/docker-check.sh all` with Docker and Buildx. The Docker helpers build
from the existing Dockerfiles with public bases and a local toolchain image.
Both paths check the locked workspace and deny Clippy warnings.
