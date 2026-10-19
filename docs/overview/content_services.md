# Where usd-caches fits in Omniverse Content Services

Omniverse Content Services connects content across systems to the teams and compute that need it. The related repositories cover access, service deployment, and packaging; **usd-caches** adds reuse near the consumers of that content.

| Product repository | What it provides | How it fits with caching |
| --- | --- | --- |
| **usd-caches** (this repository) | Local cache, shared content cache, and shared derived-data cache. | Reuses fetched content and generated data near applications and compute. |
| [**ovstorage**](https://github.com/NVIDIA-Omniverse/ovstorage) | A backend-neutral data-access library with storage plugins, language bindings, CLI, and MCP tools. | Provides the application-facing access layer. Cache use depends on the selected client, backend, and configuration. |
| [**usd-content-workspace**](https://github.com/NVIDIA-Omniverse/usd-content-workspace) | Service APIs, reference implementations, conformance tests, and deployment material for storage, permissions, identity, metadata, notifications, discovery, and thumbnails. | Supplies services over existing storage and infrastructure; caches reduce repeated retrieval and processing near consumers. |
| [**ovpackage**](https://github.com/NVIDIA-Omniverse/ovpackage) | Versioned asset packaging, publishing, installation, and dependency metadata through a CLI and Python API. | Identifies and distributes reproducible content versions; caching addresses reuse of the underlying data during supported delivery and consumption workflows. |

For example, an application can access its files with **ovstorage** on services provided by **usd-content-workspace** over existing infrastructure, and select and publish versions with **ovpackage**. **usd-caches** reduces repeated work where compatible clients and cache services are configured. This describes the products' complementary roles; each integration has its own setup and compatibility requirements.
