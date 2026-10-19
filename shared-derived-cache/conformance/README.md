<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Shared derived cache tests

- [Server and gRPC integration tests](../reference/rust/ovdc/src/tests.rs)
- [Database tests](../reference/rust/ovdc_map/src/tests.rs)
- [RocksDB adapter tests](../reference/rust/ovdc_map/src/environment/rocks_adapter/tests.rs)
- [JWT verification tests and synthetic fixtures](../reference/rust/ovdc_jwt/src/)
- [Docker build, test, and Clippy runner](../reference/scripts/docker-check.sh)
- [Runner instructions](../reference/scripts/README.md)

These are the retained upstream test suites. From `shared-derived-cache/reference/`,
run `./scripts/docker-check.sh all` to build and run tests and Clippy using the
repository's Dockerfiles. A successful export dry run does not run these tests.
