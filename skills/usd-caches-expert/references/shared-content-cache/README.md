Up: [usd-caches-expert](../../SKILL.md)

# Shared content cache (ovcontentcache / OVCC, formerly UCC / usd-content-cache)

OVCC is an in-cluster HTTP caching reverse proxy for immutable content in S3, Azure Blob, GCS (S3-compatible XML API) and Nucleus. It is shipped as the Helm chart `ovcontentcache` (rendered resource root `ovcc`) that deploys a Kubernetes StatefulSet of OpenResty/NGINX + Lua pods. Each replica has its own disk cache. Clients reach it by sending the real storage hostname (`Host`/SNI) to the OVCC Service: port `14128` for cloud storage, `14129` for Nucleus. Source: `shared-content-cache/reference/deploy/AGENTS.md`.

Primary source documents: [deploy/AGENTS.md](../../../../shared-content-cache/reference/deploy/AGENTS.md) (customer deployment and behavior guide), [values.yaml](../../../../shared-content-cache/reference/deploy/values.yaml) (the only supported configuration interface), [published docs](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html), [NGC Omniverse Caching Services collection](https://catalog.ngc.nvidia.com/orgs/nvidia/omniverse/collections/omniverse-cache-services/-) (lists OVCC as "USD Content Cache (UCC)"), [NGC Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovcontentcache), and [NGC container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/usd-content-cache).

## When OVCC is needed

- Many clients (simulation workers, render nodes, NVCF functions) in one Kubernetes cluster read the same objects from S3/Azure/GCS/Nucleus. Repeat reads become disk or page-cache hits rather than origin egress. The cache-lock collapses concurrent first reads into one origin fetch (`files/nginx.conf` `proxy_cache_lock on`).
- Content is versioned or immutable, and clients can send `If-Match: <ETag>`, or an operator accepts mutable mode (`requiresVersioning: false`, which returns a 503 on detected mutation).
- Cloud egress cost, origin throttling (S3 `503 SlowDown`), or cold-start latency of bursty LIST/HEAD storms at simulation start are problems (bursty cache).

## When OVCC is not the right tool

- A single node re-reads its own data: use the local (per-node) cache instead.
- Derived or computed artifacts: use the shared derived-data cache (OVDC).
- Writes and uploads: OVCC streams them through without caching (see [overview.md](overview.md#what-ovcc-does-not-do)).
- Clients outside the cluster that cannot steer `Host`/SNI to the Service, or that need strong cross-replica consistency (replicas are independent; see [overview.md](overview.md#consistency-model)).
- Authenticated Nucleus traffic on `/`: requests that carry `Authorization` bypass the cache by design (`files/backends/nucleus-server.conf`).

## Files in this reference

| File | Read it when |
|---|---|
| [overview.md](overview.md) | You need the architecture, request routing, cache key, consistency, scaling, or placement facts, or the list of what OVCC does not do. |
| [build.md](build.md) | You are linting or rendering the chart locally, or need to know where the container image comes from. |
| [deploy.md](deploy.md) | You are installing, upgrading (including from UCC), or uninstalling with Helm, or setting up TLS, storage, network policy, or monitoring. |
| [configuration.md](configuration.md) | You need the meaning, default, or constraint of any `values.yaml` key, or a starter override file for S3, Azure, or Nucleus. |
| [api-http.md](api-http.md) | You are writing a client, or testing with curl: addressing per backend, auth, bypass rules, headers, and the health, metrics, and cache-reset endpoints. |
| [validate.md](validate.md) | You are verifying a fresh or upgraded deployment, including the MISS-to-HIT check, metrics, logs, and PVCs. |
| [troubleshooting.md](troubleshooting.md) | Something is slow or failing: 504s, 400s, 421s, 503s, no cache hits, disk/NIC saturation, or blocked upgrades. |
