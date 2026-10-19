Up: [usd-caches-expert](../SKILL.md)

# Choosing caches for a workflow

Recommend the smallest set that removes the repeated work the workflow actually has. Each cache addresses a different repetition:

| Repeated work | Cache that removes it | Scope |
| --- | --- | --- |
| One node re-reads the same source content or regenerates derived data across sessions | Local cache (Hub) | One node |
| Many clients or nodes read the same source content from object storage or Nucleus | Shared content cache (OVCC) | One deployment serves the clients that can reach it |
| Many workers or GPUs generate the same derived data | Shared derived-data cache (OVDC) | One deployment serves the workers that can reach it |

## Inputs that change the answer

Ask for these, or state what you assumed:

1. **Where compute runs** — a workstation, on-prem nodes, a cloud cluster, or several clusters.
2. **How many nodes, and in how many regions or sites.**
3. **Where source content lives** — S3, Azure Blob Storage, GCS, Nucleus, or local disk. OVCC fronts only remote S3, Azure, GCS, and Nucleus storage.
4. **Whether workers generate derived data** — for example RTX rendering through the UJITSO pipeline — and whether many workers load the same scenes.
5. **Whether Kubernetes and Helm are available** — in a cluster, both shared caches deploy through their Helm charts. OVDC can also run natively or in Docker for development ([deploy](shared-derived-cache/deploy.md)).

## Decision rules

1. **Always include the local cache** on every node that runs a Kit-based application or Client Library tool. It is the only cache that helps a single machine, and it keeps derived data local as well as source content. Hub serves only processes on its own machine: it binds `127.0.0.1` by default and returns local file paths, so one Hub can't serve other nodes. Hub never fetches from storage itself; the client fetches and writes into Hub.
2. **Add OVCC** when more than one node reads the same source content from remote storage. One node gets no benefit beyond what Hub already provides.
3. **Add OVDC** when more than one worker generates derived data for the same scenes. Hub already reuses derived data on a single node.
4. **Place shared caches with the compute.** Deploy OVCC and OVDC in the same region and cluster as the workers, so workers share data without each managing independent storage devices. Use one deployment of each per region or site the compute spans; don't serve workers across regions from a single deployment.
5. **Scale within a region before adding deployments.** Add replicas (and size storage) inside a deployment as load grows. See each component's overview for how replicas behave:
   - **OVCC:** one release per cluster serves every enabled backend (S3, Azure, GCS, Nucleus) at once, each with its own cache volume. Replicas share nothing, so each warms independently. On GKE with `c4-highcpu-48` nodes, the measured sizing is `ceil(workers / 34)` replicas; other clusters need their own measurement — see [overview](shared-content-cache/overview.md).
   - **OVDC:** one release is a StatefulSet of independent pods, each with its own keyspace, PVC, and garbage collection. The server does no partitioning, forwarding, or replication; the client spreads keys across pod addresses, and that client logic is not in this repo. Size memory to at least row cache + block cache + 2–6 GB of network buffers; memtables count against the block cache when `engine.useWriteBufferManager` is true (the default) and come on top otherwise. Give each network-attached PVC ≥ 800 MB/s sustained — see [overview](shared-derived-cache/overview.md#multi-node-behavior) and [deploy](shared-derived-cache/deploy.md).
6. **Clients must route through the shared caches.** OVCC gives no benefit until clients send storage requests to its Service while keeping the storage hostname, through cluster DNS or an SDK endpoint override. Uncached by default: S3, Azure, and GCS GETs without `If-Match`, and S3 path-style URLs. Nucleus requests are cached only on `/omni/lft`. See [api-http.md](shared-content-cache/api-http.md).

## Scenarios

| Workflow | Recommendation |
| --- | --- |
| Local simulation or rendering on one workstation | Local cache only. |
| Kit application shipped to end users | Local cache on each user's machine. Shared caches only if users share a site and storage. |
| One machine running batch jobs against cloud storage | Local cache only. |
| 15 machines on AWS in one region, reading assets from S3 and rendering the same scenes | Local cache on each machine, one OVCC deployment and one OVDC deployment in that region's cluster. |
| Same 15 machines split across two regions | Local cache on each machine; one OVCC and one OVDC deployment in each region. |
| Many nodes reading shared content, each generating unique derived data | Local cache on each node and OVCC. Skip OVDC — no derived data is shared. |
| Many render workers loading the same scenes from local or already-colocated storage | Local cache on each node and OVDC. Skip OVCC unless content comes from remote S3, Azure, GCS, or Nucleus. |
| No Kubernetes available | Local cache only; both shared caches are deployed to clusters through Helm. |

## Gaps to state in recommendations

This repository doesn't settle these, so list them under assumptions and gaps when they affect the answer:

- Whether Kit or the Client Library sends `If-Match` on storage reads. Without it, OVCC bypasses S3, Azure, and GCS reads under the default settings.
- How Kit, the Client Library, or RTX are configured to reach OVCC and OVDC, and how the RTX/UJITSO client spreads keys across OVDC pods.
- Sizing ratios outside GKE for OVCC, and any worker-to-pod ratio for OVDC.
- Running Hub inside Kubernetes pods. Source documents workstations and Docker only. Hub must share the node's network namespace and cache path with its clients (Docker host networking and `/var/cache/hub`), or run per pod in exclusive mode; see [install and run](local-cache/install-and-run.md).

## What to hand off next

- Local cache: [install and run](local-cache/install-and-run.md), then [validate](local-cache/validate.md).
- OVCC: [deploy](shared-content-cache/deploy.md), [configuration](shared-content-cache/configuration.md), then [validate](shared-content-cache/validate.md).
- OVDC: [deploy](shared-derived-cache/deploy.md), [Helm configuration](shared-derived-cache/configuration-helm.md), then [validate](shared-derived-cache/validate.md).
