# Components in this repository

Each component has `api/` and `conformance/` entry points, with its implementation and build workspace under `reference/`. Each component keeps its own version, build workspace, and deployment requirements; there is no combined build or installer.

| Component | Interfaces and source | Build, deployment, and tests |
| --- | --- | --- |
| **Local cache (Hub)** | {src}`Source and native build guide <local-cache/reference/README.md>` · {src}`Protobuf RPC guide <local-cache/reference/docs/PROTOBUF_RPC.md>` · {src}`Protobuf definitions <local-cache/reference/hub-rpc/proto/>` · {src}`Python client examples <local-cache/reference/hub-rpc/examples/python/>` | {src}`Docker and client configuration <local-cache/reference/docs/HUB_IN_DOCKER.md>` · {src}`Integration tests <local-cache/reference/hub/tests/>` |
| **Shared content cache (OVCC)** | {src}`HTTP and configuration interface <shared-content-cache/api/README.md>` · {src}`Rust engine workspace <shared-content-cache/reference/engine/>` | {src}`Helm deployment and operations guide <shared-content-cache/reference/deploy/AGENTS.md>` · {src}`Helm values <shared-content-cache/reference/deploy/values.yaml>` · {src}`Conformance status <shared-content-cache/conformance/README.md>` |
| **Shared derived-data cache (OVDC)** | {src}`Source and native setup <shared-derived-cache/reference/README.md>` · {src}`gRPC Protobuf definition <shared-derived-cache/reference/rust/ovdc_grpc/proto/rpc.proto>` | {src}`Docker build and test guide <shared-derived-cache/reference/README.md#build-and-check-with-docker>` · {src}`Helm deployment guide <shared-derived-cache/reference/docker/helm/README.md>` · {src}`Test index <shared-derived-cache/conformance/README.md>` |

## Developer workflows

- **Build and run Hub:** follow the {src}`native build guide <local-cache/reference/README.md#developing>` from `local-cache/reference/`. For client integration, use the {src}`Protobuf RPC guide <local-cache/reference/docs/PROTOBUF_RPC.md>`, which documents framed Protobuf messages over TCP.
- **Deploy shared content caching:** follow the {src}`Helm deployment and operations guide <shared-content-cache/reference/deploy/AGENTS.md>` from the chart in `shared-content-cache/reference/deploy/`. Configure deployments through `values.yaml`, the supported configuration interface.
- **Build and exercise shared derived-data caching:** follow {src}`native development <shared-derived-cache/reference/README.md#native-development>` or {src}`build and check with Docker <shared-derived-cache/reference/README.md#build-and-check-with-docker>` from `shared-derived-cache/reference/`. The {src}`PUT, GET, and TOUCH smoke check <shared-derived-cache/reference/README.md#put-get-and-touch-smoke-check>` verifies that a client can store, retrieve, and touch a value.

The shared content cache's {src}`conformance suite is not published yet <shared-content-cache/conformance/README.md>`. The Hub and shared derived-data cache test sources are linked above; their presence does not establish a test result for your environment.
