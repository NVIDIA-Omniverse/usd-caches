# usd-caches — local and shared caching for USD workflows

usd-caches is the caching layer of Omniverse Content Services in the Omniverse ecosystem, bringing a local node, shared content, and shared derived-data caching together in one monorepo. It reduces repeated content transfers and derived-data generation by reusing data close to the applications and compute that consume it.

> **[Choose a cache component](docs/getting_started/choosing_caches.md)** · Reuse fetched content and generated results near applications and compute instead of re-downloading assets or re-deriving data on every run. Brings together the existing Hub, Omniverse Content Cache, and Derived Data Cache Service technologies.
>
> *Each component keeps its own version, build workspace, and deployment requirements. There is no combined monorepo build or installer; validate each component against its own release documentation.*

---

## 1. What is usd-caches?

**usd-caches** is a product monorepo holding three complementary cache components, with their client interfaces, tests, and shared-cache Helm charts. The local cache reuses source content and derived data on a single node, the shared content cache reuses source assets across clients, and the shared derived-data cache reuses generated results across workers. Existing storage systems remain the source of truth for authored content.

The components serve local workstations as well as distributed rendering and simulation deployments. For how usd-caches relates to the other Omniverse Content Services repositories — ovstorage, usd-content-workspace, and ovpackage — see [where usd-caches fits](docs/overview/content_services.md).

---

## 2. What functionalities are available, and who are the target users?

**What you can do with it** — start with the node an application runs on, then share across clients and workers:

- **Keep data next to the application with the local cache (also called Hub).** A service on each workstation or compute node keeps the source content and derived data used by Kit-based applications and Client Library tools on local disk, so repeated loads are served locally instead of from upstream storage.
- **Share source content across clients with the shared content cache (also called ovcontentcache or OVCC, formerly UCC).** When many clients read the same assets, a cache placed in front of your storage retrieves each asset once and serves it to all of them.
- **Share derived data across workers with the shared derived-data cache (also called ovderivedcache or OVDC, formerly DDCS).** When workers generate the same results, one worker stores them and the others retrieve them instead of regenerating them.

**Who benefits:**

- **USD application developers** — choose the cache layer that addresses repeated asset loading in their applications. We recommend always using at least the local cache, so end users get optimal performance.
- **Rendering and simulation teams** — reuse source content and derived results across workers processing common scenes. We recommend deploying the caches where the compute runs, so workers share data without each managing independent storage devices.
- **Platform operators** — deploy and configure shared caches near compute, with storage capacity suited to the workload.
- **Cache contributors and integrators** — locate the local, shared-content, and shared-derived components from one entry point and understand their boundaries.

> **Note:** the shared derived-data cache can evict or lose entries. Applications must be able to regenerate missing data and handle the [documented consistency limits](shared-derived-cache/reference/README.md); keep authoritative content in backing storage.

---

## 3. Documentation and reference links

**Interface contracts** — each cache has its own contract; there is currently not a common API across them.

- **Local cache:** Protobuf RPC over TCP with length-framed messages, covering cache-aside file operations and block-cache operations for derived data. Hub also keeps its legacy MessagePack RPC and HTTP endpoints. See the [API index](local-cache/api/README.md) and [Protobuf RPC guide](local-cache/reference/docs/PROTOBUF_RPC.md).
- **Shared content cache:** HTTP. Clients send their usual S3, Azure Blob Storage, GCS, or Nucleus requests through the caching reverse proxy, and deployments configure its behavior through the Helm chart's `values.yaml`. See the [interface index](shared-content-cache/api/README.md).
- **Shared derived-data cache:** a gRPC `KeyValue` service with `Get`, `Put`, and `Touch` operations. See the [API index](shared-derived-cache/api/README.md) and [Protobuf definition](shared-derived-cache/reference/rust/ovdc_grpc/proto/rpc.proto).

**Build and deployment** — each component builds from its own `reference/` workspace with its own pinned Rust toolchain.

- **Local cache:** a Cargo workspace that builds the `hub` service, which runs natively or in Docker on a workstation. See the [native build guide](local-cache/reference/README.md#developing) and [Hub in Docker](local-cache/reference/docs/HUB_IN_DOCKER.md).
- **Shared content cache:** a [Rust engine workspace](shared-content-cache/reference/engine/) and the `ovcontentcache` Helm chart, which deploys the cache to Kubernetes. See the [Helm deployment and operations guide](shared-content-cache/reference/deploy/AGENTS.md).
- **Shared derived-data cache:** a Cargo workspace built natively or with Docker, and the `ovderivedcache` Helm chart. See [native development](shared-derived-cache/reference/README.md#native-development), [Docker build and check](shared-derived-cache/reference/README.md#build-and-check-with-docker), and the [Helm deployment guide](shared-derived-cache/reference/docker/helm/README.md).

**Browse the repository** — each component directory has `api/` for its interface, `conformance/` for its tests, and `reference/` for its source, build, and deployment material.

- [docs/](docs/index.rst) — the documentation site: choosing caches, getting started with each cache, the [component map](docs/overview/components.md), and [where usd-caches fits](docs/overview/content_services.md). [docs/README.md](docs/README.md) explains how to build it.
- [local-cache/](local-cache/README.md) · [shared-content-cache/](shared-content-cache/README.md) · [shared-derived-cache/](shared-derived-cache/README.md) — component entry points.
- [skills/](skills/README.md) — the `usd-caches-expert` skill for AI coding agents; [AGENTS.md](AGENTS.md) makes an agent in this repository act as that expert.
- **Conformance status:** [local cache](local-cache/conformance/README.md) · [shared content cache](shared-content-cache/conformance/README.md) · [shared derived-data cache](shared-derived-cache/conformance/README.md)

---

## 4. System requirements and distribution

The local cache needs Omniverse Client Library 2.49.2 or greater and local disk space for the cache. The shared caches need Kubernetes and Helm, with storage and networking suited to the workload. Source builds need each component's pinned Rust toolchain and build tools. See the component guides for full requirements: [local cache](local-cache/reference/README.md), [shared content cache](shared-content-cache/reference/deploy/AGENTS.md), and [shared derived-data cache](shared-derived-cache/reference/README.md#prerequisites).

Prebuilt executables, containers, and Helm charts are published on NGC:

| Component | Prebuilt on NGC | Published documentation |
| --- | --- | --- |
| **Local cache** | [Collection](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/collections/hub-workstation-cache) · [Windows and Linux installers](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/resources/hub_workstation_cache) · [Container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/hub_workstation_cache) | [Hub Workstation Cache](https://docs.omniverse.nvidia.com/utilities/latest/cache/hub-workstation.html) |
| **Shared content cache** | [Container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/usd-content-cache) · [Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovcontentcache) | [Omniverse Content Cache (OVCC)](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html) |
| **Shared derived-data cache** | [Container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/ovderivedcache) · [Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovderivedcache) | [Omniverse Derived Cache (OVDC)](https://docs.omniverse.nvidia.com/ovcaches/ovderivedcache/latest/index.html) |

Pulling containers and charts requires an NGC API key. Published documentation uses the short names OVCC and OVDC; earlier releases were named UCC and DDCS. Validate each component against its own release documentation; a common repository does not establish a common component version or compatibility matrix.

---

## 5. Licensing

- **Source:** [Apache License 2.0](LICENSE). Each component also carries its own LICENSE file.
- **Third-party components:** remain under their own licenses. See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution and component notice locations.
- **Contributions and security:** this project is not currently accepting contributions. To report a security vulnerability, follow [SECURITY.md](SECURITY.md).

---

*usd-caches · Omniverse Content Services · Copyright (c) 2026 NVIDIA Corporation.*
