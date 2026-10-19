<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Shared derived cache

[OVDC source workspace](reference/README.md). OVDC is a persistent, distributed
key-value store backed by RocksDB, with GET, PUT, and TOUCH operations, LRU
eviction, and approximate TTL support.

- [API and configuration index](api/README.md)
- [Conformance and test index](conformance/README.md)
- [Build and test instructions](reference/scripts/README.md)
- [Helm chart and deployment guide](reference/docker/helm/README.md)
- [Apache-2.0 license](reference/LICENSE)

The source and build workspace is retained under `reference/`. Run build and
test commands from that directory. Copybara records the source revision in the
export commit's `GitOrigin-RevId` metadata.
