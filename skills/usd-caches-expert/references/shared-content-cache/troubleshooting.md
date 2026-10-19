Up: [shared content cache](README.md)

# OVCC troubleshooting

This file condenses [AGENTS.md Critical Constraints](../../../../shared-content-cache/reference/deploy/AGENTS.md#critical-constraints), the per-backend and infrastructure sections, and the chart guards (`deploy/templates/_helpers.tpl`).

The published Runbooks section ([docs](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html)) lists five runbooks: connection saturation, disk bandwidth, metadata cache undersizing, network bandwidth saturation, and upstream S3 connection spikes. Their text is **not** in this repo. The rows mapped to them below use only facts that are in source.

## Install and upgrade failures (render time)

| Symptom (Helm error contains) | Cause | Fix |
|---|---|---|
| `backend is enabled, but it does not declare which cache storage` / `cache path is not configured` / `no cache storage is configured` | An included backend lacks `proxyCacheName`, a `nginx.proxyCache.paths[]` entry, or a `persistence.volumes[]` entry. This is common after overriding the lists. | Add the matching entries, or set `nginx.backends.<n>.include=false`. |
| `cache path is outside its configured storage volume` | `paths[].path` is not under `volumes[].path`. | Move the path under the volume. |
| `PVC "…" … has NGi but … sizeGi requires M` | An existing PVC is smaller than `sizeGi`. | Delete the PVC (it is recreated) or set `sizeGi` to match ([AGENTS.md PVC Size Guard](../../../../shared-content-cache/reference/deploy/AGENTS.md#pvc-size-guard)). |
| `uses cache storage mode "pvc", but requested persistence.mode="ephemeral"` (or the reverse) | In-place mode switch. | Uninstall, delete the StatefulSet and PVCs, then reinstall. |
| `still backed by the UCC chart` | The release still has a `<release>-usd-content-cache` StatefulSet. | `--set compat.useUccDefaults=true` ([AGENTS.md Upgrade From UCC / usd-content-cache](../../../../shared-content-cache/reference/deploy/AGENTS.md#upgrade-from-ucc--usd-content-cache)). |
| `would make the kubelet inject OVCC_CONFIG_* service-link vars` | Service name starts with `ovcc-config`. | Rename the release, or change `nameOverride`/`fullnameOverride`. |

## Runtime symptoms

| Symptom | Likely cause (source) | Fix |
|---|---|---|
| Pod `CrashLoopBackOff`, log shows `nginx: [emerg]` | Invalid rendered config, e.g. `upstream_queue_time` added to log formats (NGINX Plus only), or a missing TLS secret or key when `tls.enabled`. | Revert template edits (values only). Create Secret `tls.secretName` with `tls.crt`/`tls.key` (`usd-content-cache-tls` under compat). |
| `ImagePullBackOff` | Default pull secret `gitlab-registry-secret` does not exist. | Create an `nvcr.io` docker-registry secret and set `image.pullSecrets[0].name`. |
| Every response `BYPASS` | Missing `If-Match` with `requiresVersioning: true`; `HEAD`; `Cache-Control: no-cache`; Nucleus with `Authorization`; Azure CPK, lease, or tags headers; or a failed access probe (sets `disable_cache`). | Send `If-Match: <ETag>`, or set `requiresVersioning: false` for that backend (mutable mode). Check logs for `auth request failed`. |
| No `X-Cache-Status` header at all | Host did not match any backend `serverName`, so it fell to passthrough. Examples: path-style S3, `s3.amazonaws.com` global endpoint, dotted bucket names, GCS regional endpoint. | Use virtual-hosted regional hostnames, or widen `serverName`. |
| `421 Misdirected Request` | Client sent the OVCC Service name or an `extraNames` host as `Host`. | Send the origin storage host as `Host` and connect to the Service IP/name. |
| `400 Bad Request` from OVCC | S3 presigned without a valid `X-Amz-Date`/`X-Amz-Expires`, or Azure SAS without valid `st`/`se`. | Fix URL generation (`lua-access.lua`). |
| `403` | The origin rejected the signature or permissions (expired URL, wrong key), relayed from the access probe. | Re-sign. OVCC does not cache 403s (`validity` has only 200/206). |
| `503` on a HIT | Mutable mode detected an upstream ETag or Last-Modified change, purged the entry, and expects a client retry. | Expected. Persistent high `cc_mutation_detection_purges_total` points to origin rewrites or Last-Modified clock skew ([AGENTS.md Monitoring mutable mode](../../../../shared-content-cache/reference/deploy/AGENTS.md#monitoring-mutable-mode)). |
| `503 SlowDown` | S3 throttling. | Disambiguate with CloudWatch `FirstByteLatency` ([AGENTS.md AWS Infrastructure](../../../../shared-content-cache/reference/deploy/AGENTS.md#aws-infrastructure)). |
| `504` pattern (504-504-200, ~60 s each) under load; *upstream S3 connection spikes* | No upstream keepalive, so each request opens a new TCP and TLS connection. | `nginx.backends.s3.enableUpstreamKeepalive: true` (default). For Azure, enable it too. Size `upstreamBalancer.keepalive.maxCachedConnectionsPerWorker` (× workers) and `nginx.socketPoolSize`. Keep `resolver.cache.successTtlSecs ≤ 52` to follow S3 IP rotation ([AGENTS.md S3 504 Timeout Fix](../../../../shared-content-cache/reference/deploy/AGENTS.md#s3-504-timeout-fix)). |
| `502` with log `failed to resolve upstream peer for keepalive` | Lua resolver could not resolve the origin. | Check cluster DNS, `upstreamBalancer.resolver.*`, `fallback` (default `8.8.8.8`, which may be blocked), and egress. |
| `sslv3 alert handshake failure` / upstream TLS verify errors | SNI missing or origin cert untrusted. `proxy_ssl_server_name on` is fixed. There is no override for self-signed origins. | Origin must present a publicly trusted cert, or a cert in the image CA bundle. Allow egress 443. |
| Clients fail TLS: "Could not create SSL/TLS secure channel" | TLS version mismatch. | Use a client that supports TLS 1.3. |
| *Connection saturation*: high latency on warm cache, CPU not maxed | NGINX connection queue depth per worker (one worker per vCPU). On Azure, SNAT exhaustion or LB 5-tuple imbalance. | More vCPU per replica, or more replicas (GKE: c4-highcpu-48, `ceil(workers/34)` replicas). Raise `nginx.workerConnections` (Azure: `(Max_SNAT/pods/vCPUs)×1.2`). `useRoundRobinLoadBalancing: true` with LB/NodePort ([AGENTS.md GCP Infrastructure](../../../../shared-content-cache/reference/deploy/AGENTS.md#gcp-infrastructure), [Azure Infrastructure](../../../../shared-content-cache/reference/deploy/AGENTS.md#azure-infrastructure)). |
| *Network bandwidth saturation*: some pods' NICs pegged, others idle | Uneven load balancing. A single large node has single-pod network contention. | Round-robin flag; spread replicas with `affinity`. Prefer several mid-size nodes over one huge node. Watch the GCM dashboard Network section. |
| *Disk bandwidth*: slow cache warmup, MISS latency high | Storage class throughput caps. Examples: `pd-standard`, small `pd-ssd` (0.48 MB/s/GB), `n2-standard-8` VM cap of 800 MB/s, Azure burst credits exhausted, gp2. | gp3 / pd-ssd ≥ 2048Gi / pd-extreme / hyperdisk-balanced; a bigger VM; monitor burst credits ([AGENTS.md Storage Class Recommendations](../../../../shared-content-cache/reference/deploy/AGENTS.md#storage-class-recommendations)). |
| *Metadata cache undersizing*: premature eviction, hit rate falls while disk has space | `keys_zone` (`paths[].metadataMemorySize`) is full. | Raise to `2048m` (production guidance) ([AGENTS.md General Settings](../../../../shared-content-cache/reference/deploy/AGENTS.md#general-settings)). Check `nginx_shared_dict_info` for the Lua dicts. |
| Cache never fills; constant eviction | `min_free` larger than real free space. Typical cause: ephemeral disk smaller than `sizeGi`, or a PVC smaller than `sizeGi` on older charts. | Ephemeral reserve is fixed at 10g; set `sizeGi` to the real disk. Fix the PVC size. |
| Replicas serve different content / stale after an origin change | Per-replica caches with TTL; generation is per pod. | Reset every replica (two-phase algorithm); shorten `validity` or `maxIdleTime`; use versioned keys (`If-Match`). |
| Bursty LIST/HEAD stale or inconsistent across pods | Client bounces between pods. | `enableSessionAffinity` with `sessionAffinityTimeoutSeconds ≥ burstyCache.ttlSec`. |
| Dashboards "No data" | ServiceMonitor disabled (default), GKE without PodMonitoring, or `ucc_` prefix under compat with `cc_` dashboards. | `serviceMonitor.enabled` / `podMonitoring.enabled`; use the matching dashboard variant. |
| Corrupted JSON log lines | Buffer flushes from several replicas interleave. | Longer `flushInterval` / larger `buffer`. |
| Azure HIT shows an old `x-ms-request-id` | Known limitation; correlation metadata only. | None needed. |
