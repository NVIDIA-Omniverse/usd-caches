Up: [usd-caches-expert](../../SKILL.md)

# Shared derived-data cache (ovderivedcache / OVDC, formerly DDCS)

OVDC is a persistent key-value store on RocksDB. It exposes a gRPC `ovdc.KeyValue` service with `Get` (server-streaming), `Put` (bidirectional streaming), and `Touch`. It evicts by approximate LRU, supports per-PUT TTL, and **may lose data or return any stored version of a mutated key** (`shared-derived-cache/reference/README.md`). It ships as the Helm chart `ovderivedcache` 6.0.1, which deploys a StatefulSet with one PVC per replica (`shared-derived-cache/reference/docker/helm/Chart.yaml`, `templates/statefulset.yml`). Default ports: gRPC `3010`, Prometheus `3051`. Keys are at most 32 bytes. Each key is namespaced by a tenant of up to 48 bytes, taken from `tenant` metadata or from the JWT `assertion.ncaId` claim (`shared-derived-cache/reference/rust/ovdc_bytes/src/keys.rs`).

Primary sources: [reference README](../../../../shared-derived-cache/reference/README.md), [rpc.proto](../../../../shared-derived-cache/reference/rust/ovdc_grpc/proto/rpc.proto), [config.toml](../../../../shared-derived-cache/reference/rust/ovdc/src/config.toml), [values.yaml](../../../../shared-derived-cache/reference/docker/helm/values.yaml), [Helm README](../../../../shared-derived-cache/reference/docker/helm/README.md). Published: [docs](https://docs.omniverse.nvidia.com/ovcaches/ovderivedcache/latest/index.html), [NGC Omniverse Caching Services collection](https://catalog.ngc.nvidia.com/orgs/nvidia/omniverse/collections/omniverse-cache-services/-) (lists OVDC as "Derived Data Cache (DDCS)"), [NGC Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovderivedcache), [NGC container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/ovderivedcache).

## When OVDC is needed

- More than one worker or GPU generates the **same** derived data, for example UJITSO real-time rendering artifacts. One worker `Put`s the result; the others `Get` it instead of recomputing it. The repo describes this as "When workers generate the same results, one worker stores them and the others retrieve them" (`README.md` at repo root).
- The data set is larger than memory, needs no range scans, tolerates loss, and needs only PUT/GET with approximate-LRU eviction. These are the store's own fitness criteria (`shared-derived-cache/reference/README.md` "This store should be considered when").

## When Hub's local derived-data caching suffices

- One node regenerates or rereads its own derived data across sessions. Hub (the local cache) already offers "block-cache operations for derived data" on each node (`README.md` at repo root, "Local cache" interface bullet). OVDC adds value only when the data is shared across workers.
- Each worker produces unique derived data. Nothing is shared, so OVDC returns only misses.
- The application cannot regenerate missing data or cannot tolerate stale versions. OVDC is not a source of truth (`README.md` at repo root, Note).

## Files in this reference

| File | Read it when |
|---|---|
| [overview.md](overview.md) | You need the architecture: multi-node behavior (what is and is not in the code), consistency and data loss, key and tenant model, LRU, TTL, GC, the RocksDB memory model, ports, auth, TLS, telemetry, placement facts, or what OVDC does not do. |
| [build.md](build.md) | You are building or testing natively (Cargo/Make) or with Docker Buildx (`scripts/docker-check.sh`), or building the container image. |
| [deploy.md](deploy.md) | You are installing, upgrading, scaling, or uninstalling the Helm chart, or setting up TLS, an OTel collector, or Grafana. Also covers running locally, native or in Docker. |
| [configuration-helm.md](configuration-helm.md) | You need the type, default, effect, or constraint of any `values.yaml` key, the sizing presets, or a starter override file. |
| [configuration-server.md](configuration-server.md) | You need any `config.toml` key, the settings structs behind it, CLI arguments, environment variables, hot-reload behavior, JWT claim requirements, or how Helm values map onto the server config. |
| [api-grpc.md](api-grpc.md) | You are writing or testing a client: every RPC, message, and field; status codes; key, tenant, and JWT metadata; TLS; grpcurl and Python examples. |
| [validate.md](validate.md) | You are checking a build, a local server, or a Helm release: pods, probes, smoke round trip, metrics, logs. |
| [troubleshooting.md](troubleshooting.md) | Writes stall or slow down, GETs miss, pods crash-loop after an upgrade, TLS or JWT fails, metrics are empty, or disk fills. |
