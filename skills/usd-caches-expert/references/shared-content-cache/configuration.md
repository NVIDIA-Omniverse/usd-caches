Up: [shared content cache](README.md)

# OVCC `values.yaml` reference

Source: [`deploy/values.yaml`](../../../../shared-content-cache/reference/deploy/values.yaml) (chart `4.0.4`). Every leaf key is listed below; `[]` marks list elements. Consumers named in the tables:
- **NGINX** = `deploy/files/nginx.conf` or `deploy/files/backends/*.conf`.
- **STS** = `deploy/templates/statefulset.yaml`.
- **Helpers** = `deploy/templates/_helpers.tpl`.

Validation runs from `deploy/templates/validation.yaml`. "Unused" means no template or file in this snapshot reads the key.

Helm merge rule: lists (`persistence.volumes`, `nginx.proxyCache.paths`, `burstyCache.rules`, `image.pullSecrets`, `nginx.lua.files`) are **replaced**, not merged, by overrides.

## Identity

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nameOverride` | string | `ovcc` | Name root. Fullname = `<release>-<nameOverride>`, or `<release>` if the release name contains it. Truncated to 63. Avoid names that make the Service name start with `ovcc-config`. |
| `replicaCount` | int | `1` | StatefulSet replicas. Each replica has independent caches. Also bounds the ordinals that the PVC-size and storage-mode `lookup` guards check. |

## `image`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `image.registry` | string | `nvcr.io` | Prefix of the image reference (omitted if empty). |
| `image.repository` | string | `nvidia/omniverse/usd-content-cache` | The repository intentionally keeps the UCC name. |
| `image.pullPolicy` | string | `IfNotPresent` | **Unused**: STS sets no `imagePullPolicy`, so Kubernetes defaults apply. |
| `image.tag` | string | `"3.0.2"` | Image tag; matches `Chart.yaml` `appVersion`. |
| `image.pullSecrets[].name` | string | `gitlab-registry-secret` | Rendered as `imagePullSecrets`. Override with your `nvcr.io` secret. |

## `featureFlags`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `featureFlags.ignoreClientAbort` | bool | `true` | `proxy_ignore_client_abort on`: finish the origin fetch and cache it after the client disconnects. |
| `featureFlags.enableBurstyCache` | bool | `true` | Enables `@bursty_cache` routing for backends with `burstyCache.rules`. The bursty path ignores `Cache-Control: no-cache`. |
| `featureFlags.enableSessionAffinity` | bool | `false` | `sessionAffinity: ClientIP` on the cache and Nucleus Services. |
| `featureFlags.sessionAffinityTimeoutSeconds` | int | `30` | `sessionAffinityConfig.clientIP.timeoutSeconds`. Must be ≥ every `burstyCache.ttlSec` (documented constraint; not enforced by the chart). |
| `featureFlags.useRoundRobinLoadBalancing` | bool | `true` | `externalTrafficPolicy: Local`, applied only when the Service `type` is `LoadBalancer`/`NodePort`. |

## `service`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `service.cache.type` | string | `ClusterIP` | Type of Service `<fullname>`. |
| `service.cache.port` | int | `14128` | Service port. Under compat, map from the old `service.ucc.port`. |
| `service.cache.containerPort` | int | `14128` | NGINX `listen` for S3/Azure/GCS/self/passthrough. |
| `service.cache.annotations` | map | `{}` | Service annotations (e.g. cloud LB). |
| `service.cache.loadBalancerIP` | string/null | `null` | `spec.loadBalancerIP` if set. |
| `service.nucleus.type` | string | `ClusterIP` | Type of Service `<fullname>-nucleus` (rendered only if `nginx.backends.nucleus.include`). |
| `service.nucleus.port` | int | `14129` | Nucleus Service port. |
| `service.nucleus.containerPort` | int | `14129` | Nucleus `listen`. |
| `service.nucleus.annotations` | map | `{}` | Annotations. |
| `service.nucleus.loadBalancerIP` | string/null | `null` | `spec.loadBalancerIP` if set. |

## `tls`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `tls.enabled` | bool | `true` | Adds `ssl` to listeners and mounts the secret at `/etc/nginx/ssl`; port name becomes `https`. When false, `$scheme=http`, so S3 and Azure are proxied to the origin over HTTP. |
| `tls.secretName` | string | `ovcc-tls` | Secret with `tls.crt` and `tls.key`. Under compat, if it is left at `ovcc-tls`, the chart uses `compat.uccDefaults.tlsSecretName` (`func.effectiveTlsSecretName`). |

## `compat` (UCC → OVCC migration)

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `compat.useUccDefaults` | bool | `false` | Uses UCC names: fullname `<release>-usd-content-cache`, selector name, TLS secret, `/data` → `/ucc_data` cache paths, `ucc_` metric prefix. Required to upgrade a release that still has a UCC StatefulSet (`func.validateUccUpgradeCompat`). |
| `compat.uccDefaults.chartName` | string | `usd-content-cache` | Old name root used under compat. |
| `compat.uccDefaults.tlsSecretName` | string | `usd-content-cache-tls` | TLS secret under compat (unless `tls.secretName` was changed). |
| `compat.uccDefaults.metricPrefix` | string | `ucc_` | Metric prefix under compat. |
| `compat.emitLegacyCacheReset` | bool | `true` | Also mounts `/__ucc/cache/reset` and emits the `UCC-Instance` header wherever cache reset is enabled. Planned removal in 5.0.0. |

The runtime names `/__/cache/reset` and `CC-Instance` are fixed (`func.cacheResetPath`, `func.instanceHeader`).

## `persistence`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `persistence.mode` | enum | `pvc` | `pvc` or `ephemeral`; anything else fails. Switching modes on an existing release fails (`func.validatePersistenceModeUpgrade`). |
| `persistence.storageClassName` | string | `managed-csi-premium-burst` | PVC-mode default class. If empty and the volume sets none, the cluster default is used. Azure-specific default: override elsewhere. |
| `persistence.volumes[].name` | string | `az`, `s3`, `nucleus`, `gcs` | Volume and claim-template name, which is also the cache zone link. It must equal the backend `proxyCacheName` and a `nginx.proxyCache.paths[].name` for every enabled backend. |
| `persistence.volumes[].path` | string | `/proxy_cache_<name>` | Mount path. The cache path must be equal to it or under it. |
| `persistence.volumes[].sizeGi` | int | `2048` | PVC request, or emptyDir `sizeLimit`. Upgrade fails if an existing PVC is smaller (PVC mode). |
| `persistence.volumes[].minFreeSizePercentage` | number | `7` | PVC mode: NGINX `min_free = max(int(sizeGi × pct / 100), 1)g`. Ignored in ephemeral mode, which uses flat `min(10, sizeGi)g`. |

Optional undeclared key: `persistence.volumes[].storageClassName` (per-volume class, PVC mode).

## `nginx` — global tuning

Template fallbacks in parentheses apply only if a key is deleted.

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.workerConnections` | int | `8192` (1024) | `worker_connections`; worker count is `auto`, i.e. one per vCPU. Azure formula: `(Max_SNAT/pods/vCPUs)×1.2`. |
| `nginx.keepaliveRequests` | int | `999999` | Client `keepalive_requests`. |
| `nginx.http2MaxConcurrentStreams` | int | `32768` (1024) | `http2_max_concurrent_streams`. |
| `nginx.sslVerifyDepth` | int | `2` | `lua_ssl_verify_depth` and `proxy_ssl_verify_depth`. |
| `nginx.proxyBuffers.count` | int | `64` (16) | `proxy_buffers` count. 64 × 512k = 32 MB per request. |
| `nginx.proxyBuffers.size` | size | `512k` (256k) | `proxy_buffers` size. |
| `nginx.proxyBufferSize` | size | `128k` | `proxy_buffer_size` (headers). |
| `nginx.proxyBusyBuffersSize` | size | `8m` (512k) | `proxy_busy_buffers_size`. |
| `nginx.proxyMaxTempFileSize` | size | `512m` (4096m) | `proxy_max_temp_file_size`. |
| `nginx.proxyTempFileWriteSize` | size | `4m` (1024k) | `proxy_temp_file_write_size`. |
| `nginx.socketPoolSize` | int | `512` | `lua_socket_pool_size`: cosocket pool per remote. |

### `nginx.proxyCache`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.proxyCache.lockAge` | duration | `60s` | `proxy_cache_lock_age` (http level). Nucleus is hardcoded to `10s`. |
| `nginx.proxyCache.lockTimeout` | duration | `120s` | `proxy_cache_lock_timeout`; after it, waiters fetch without caching. Nucleus is hardcoded to `3s`. |
| `nginx.proxyCache.validity.200` | duration | `1d` | `proxy_cache_valid 200` in S3/GCS/Azure/Nucleus servers. Any status-code key is accepted. |
| `nginx.proxyCache.validity.206` | duration | `1d` | `proxy_cache_valid 206`. |
| `nginx.proxyCache.paths[].name` | string | `az`,`s3`,`gcs`,`nucleus` | `keys_zone` name. Rendered only for enabled backends. Must match the volume and `proxyCacheName`. |
| `nginx.proxyCache.paths[].path` | string | `/proxy_cache_<name>/data` | `proxy_cache_path`. Must be under the matching volume path. Under compat, a `/data` suffix becomes `/ucc_data`. |
| `nginx.proxyCache.paths[].maxIdleTime` | duration | `1d` | `inactive=` eviction horizon. |
| `nginx.proxyCache.paths[].metadataMemorySize` | size | `1024m` (256m) | `keys_zone` size. Too small causes premature eviction; production guidance is `2048m`. |

Optional undeclared keys: `paths[].maxSizeGi` (`max_size=<n>g`), `paths[].levels` (default `1:2`).

### `nginx.logging`, `nginx.sharedMemory`, `nginx.resolver`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.logging.buffer` | size | `4m` | `access_log … buffer=`. Log entry is ~1.2 KiB. |
| `nginx.logging.flushInterval` | duration | `15s` | `access_log … flush=`. Short intervals across replicas can interleave JSON lines. |
| `nginx.sharedMemory.limits.lockByRequestUriTable` | size | `1024m` | `lua_shared_dict lock_by_request_uri_table` (Lua access lock). |
| `nginx.sharedMemory.limits.presignedUrlCache` | size | `1024m` | `lua_shared_dict presigned_url_cache` (auth layer). |
| `nginx.sharedMemory.limits.upstreamBalancerCache` | size | `1024m` | `lua_shared_dict upstreamBalancerCache` (balancer config and DNS cache). |
| `nginx.resolver.config` | string | `local=on ipv6=off` | NGINX `resolver` (used when the Lua balancer is not in play). `local=on` reads `/etc/resolv.conf` (OpenResty). |
| `nginx.resolver.timeoutSecs` | int | `30` | `resolver_timeout`. |

### `nginx.upstreamBalancer`

Serialized to snake_case JSON into `upstreamBalancerCache` for `lua-balancer.lua`. Effective only for backends with `enableUpstreamKeepalive: true`.

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.upstreamBalancer.keepalive.idleTimeoutSecs` | int | `60` | `balancer.enable_keepalive` idle timeout and `lua_socket_keepalive_timeout`. |
| `nginx.upstreamBalancer.keepalive.maxRequestsPerConnection` | int | `0` | `0` = unlimited. |
| `nginx.upstreamBalancer.keepalive.maxCachedConnectionsPerWorker` | int | `200` | `keepalive` and `balancer_keepalive` in `upstream dyn_upstream`. Total = value × workers. |
| `nginx.upstreamBalancer.resolver.cache.successTtlSecs` | int | `30` | DNS success cache. Keep ≤ 52 s for S3 scale-out. |
| `nginx.upstreamBalancer.resolver.cache.failureTtlSecs` | int | `10` | DNS failure cache. |
| `nginx.upstreamBalancer.resolver.query.timeoutMs` | int | `20000` | Per-attempt DNS timeout. |
| `nginx.upstreamBalancer.resolver.query.retries` | int | `3` | Max time = timeout × (retries + 1). |
| `nginx.upstreamBalancer.resolver.query.random` | bool | `true` | Random nameserver order. |
| `nginx.upstreamBalancer.resolver.fallback` | list | `["8.8.8.8"]` | Fallback nameservers. Air-gapped clusters should override this (not verified how it is used beyond the values comment). |

### `nginx.selfServer`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.selfServer.enabled` | bool | `true` | Renders a server returning **421** for `Host` equal to `<fullname>[.<ns>[.svc[.cluster.local]]]` on the cache and Nucleus ports. |
| `nginx.selfServer.extraNames` | list | `[]` | Extra rejected hostnames (e.g. Ingress FQDN). |

## `nginx.backends`

Common semantics:
- `include` gates the server block, the zone, and Services (Nucleus). An included backend needs `proxyCacheName` plus matching `paths[]` and `volumes[]` (`func.validateBackendCacheCompatibility`).
- `allowCacheReset` mounts `POST /__/cache/reset` (and the legacy path) in that server, and switches `$cache_generation` to the Lua lookup for all backends if any backend has it.
- `requiresVersioning` (default `true`) bypasses S3, Azure, and GCS GETs without `If-Match`; `false` enables mutable mode and mutation detection. Find where your chart declares it with `helm show values omniverse/ovcontentcache | grep -n requiresVersioning`.
- `proxyPass` is `tpl`-rendered into `proxy_pass` (or the keepalive target).
- `proxyAuthPass` is the URL prefix for the Lua access probe (`$proxy_auth_pass`; if empty, `$scheme://$host`).
- `allowHttp2Clients` adds `http2 on` (needs TLS).
- `enableUpstreamKeepalive` routes through `dyn_upstream` with the Lua resolver; Host and SNI come from the resolved peer.
- `burstyCache.ttlSec`/`rules[]` set the bursty TTL for 200/404. Rules match `method` and a `request_uri` prefix; `name` is a comment only.


### `nginx.backends.azure`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.backends.azure.include` | bool | `true` | Azure server on 14128. |
| `nginx.backends.azure.allowCacheReset` | bool | `false` | Reset endpoint on Azure hosts. |
| `nginx.backends.azure.serverName` | regex | `~^(?<container_name>[^\.]+)\.blob\.core\.windows\.net$` | `server_name`. |
| `nginx.backends.azure.proxyCacheName` | string | `az` | Zone/volume link. |
| `nginx.backends.azure.proxyPass` | string | `$scheme://$host` | Origin. |
| `nginx.backends.azure.proxyAuthPass` | string | `$scheme://$host` | Access-probe prefix. |
| `nginx.backends.azure.burstyCache.ttlSec` | int | `300` | Bursty TTL. |
| `nginx.backends.azure.burstyCache.rules[].name` | string | `azure-list-operations`, `azure-head-operations` | Label only. |
| `nginx.backends.azure.burstyCache.rules[].method` | string | `GET`, `HEAD` | Exact method match. |
| `nginx.backends.azure.burstyCache.rules[].uriStartsWith` | string | `/?comp=list`, `/` | Prefix of `request_uri`. |
| `nginx.backends.azure.allowHttp2Clients` | bool | `true` | `http2 on`. |
| `nginx.backends.azure.enableUpstreamKeepalive` | bool | `false` | Source: "Consider enabling — resolved 504 timeout patterns in production". |

### `nginx.backends.s3`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.backends.s3.include` | bool | `true` | S3 server (shared template `s3-compat-server.conf`). |
| `nginx.backends.s3.allowCacheReset` | bool | `false` | Reset endpoint on S3 hosts. |
| `nginx.backends.s3.serverName` | regex | `~^[^.]+\.s3\.[^.]+\.amazonaws\.com$` | Virtual-hosted, regional endpoints only. Widen it for other S3-compatible hosts (e.g. MinIO); behavior with such hosts is not documented in source. |
| `nginx.backends.s3.proxyCacheName` | string | `s3` | Zone/volume link. |
| `nginx.backends.s3.proxyPass` | string | `$scheme://$host` | Origin; also the keepalive resolve target. |
| `nginx.backends.s3.proxyAuthPass` | string | `$scheme://$host` | Access-probe prefix. |
| `nginx.backends.s3.burstyCache.ttlSec` | int | `300` | Bursty TTL (`3` in CI per `AGENTS.md`). |
| `nginx.backends.s3.burstyCache.rules[].name` | string | `s3-list-operations`, `s3-head-operations` | Label. |
| `nginx.backends.s3.burstyCache.rules[].method` | string | `GET`, `HEAD` | Method. |
| `nginx.backends.s3.burstyCache.rules[].uriStartsWith` | string | `/?list-type=`, `/` | URI prefix. |
| `nginx.backends.s3.allowHttp2Clients` | bool | `true` | `http2 on`. |
| `nginx.backends.s3.enableUpstreamKeepalive` | bool | `true` | Keep `true`: without it, 504s occur under load (504-504-200, ~60 s each). |

### `nginx.backends.gcs`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.backends.gcs.include` | bool | `true` | GCS server (same template as S3). |
| `nginx.backends.gcs.allowCacheReset` | bool | `false` | Reset endpoint. |
| `nginx.backends.gcs.serverName` | regex | `~^([^.]+\.)?storage\.googleapis\.com$` | Path-style and virtual-hosted. |
| `nginx.backends.gcs.proxyCacheName` | string | `gcs` | Zone/volume link. |
| `nginx.backends.gcs.proxyPass` | string | `https://$host` | HTTPS only. |
| `nginx.backends.gcs.proxyAuthPass` | string | `https://$host` | Access-probe prefix. |
| `nginx.backends.gcs.burstyCache.ttlSec` | int | `300` | Bursty TTL. |
| `nginx.backends.gcs.burstyCache.rules[].name` | string | `gcs-list-operations`, `gcs-head-operations` | Label. |
| `nginx.backends.gcs.burstyCache.rules[].method` | string | `GET`, `HEAD` | Method. |
| `nginx.backends.gcs.burstyCache.rules[].uriStartsWith` | string | `/?list-type=`, `/` | URI prefix. |
| `nginx.backends.gcs.allowHttp2Clients` | bool | `true` | `http2 on`. |
| `nginx.backends.gcs.enableUpstreamKeepalive` | bool | `true` | Keepalive. |

### `nginx.backends.nucleus`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.backends.nucleus.include` | bool | `true` | Nucleus server on 14129, Service `<fullname>-nucleus`, self-server second listen. Set `false` if Nucleus is unused. |
| `nginx.backends.nucleus.allowCacheReset` | bool | `true` | Reset endpoint on the Nucleus port. |
| `nginx.backends.nucleus.serverName` | string | `_` | Catch-all on the Nucleus port. |
| `nginx.backends.nucleus.proxyCacheName` | string | `nucleus` | Zone/volume link. |
| `nginx.backends.nucleus.proxyPass` | string | `$scheme://$host` | Origin. |
| `nginx.backends.nucleus.proxyAuthPass` | null | `null` | Not used by the Nucleus server (no Lua access check). |
| `nginx.backends.nucleus.allowHttp2Clients` | bool | `false` | HTTP/2 disabled. |
| `nginx.backends.nucleus.enableUpstreamKeepalive` | bool | `false` | Documented as unsupported for Nucleus. |
| `nginx.backends.nucleus.bridgeResolver` | string/null | `null` | If set (e.g. `10.0.0.10:53`), adds `resolver <x> valid=60s` in `/omni/lft`. `null` uses cluster DNS, where a `coredns-overrides` addon (not in this chart) installs the Nucleus rewrites. |

`requiresVersioning` has no effect on Nucleus: the Nucleus server does not include `cache-bypass.conf`.

### `nginx.backends.passthrough` (always rendered; no `include`)

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.backends.passthrough.allowCacheReset` | bool | `false` | Reset endpoint on the default server. |
| `nginx.backends.passthrough.proxyPass` | string | `$scheme://$http_host` | Forward proxy for unmatched hosts on 14128. Not cached. |
| `nginx.backends.passthrough.allowHttp2Clients` | bool | `true` | `http2 on` on the default server. |

### `nginx.lua`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `nginx.lua.files` | list | 14 names | Lua files placed into the `-lua` ConfigMap. Names missing from `deploy/files/lua/` are skipped silently. `lua-generation.lua` and `lua-metrics.lua` are `tpl`-rendered. |
| `nginx.lua.debug` | bool | `false` | `lua_code_cache off` (dev only). |

## Scheduling and lifecycle

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `resources` | map | `{}` | Container resources (verbatim). No defaults. |
| `nodeSelector` | map | `{}` | Values are quoted. |
| `tolerations` | list | `[]` | Verbatim. |
| `affinity` | map | `{}` | Verbatim. Use it to spread replicas or pin them to NIC/disk-rich nodes. |

## `metrics`

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `metrics.dashboard.include` | bool | `false` | ConfigMapList `<fullname>-<dashboard>` for each `deploy/dashboards/*.json`. The API server rejects these ConfigMaps, because the names contain `_` and the default label value is a boolean; create the ConfigMap yourself ([deploy.md](deploy.md#8-monitoring)). |
| `metrics.dashboard.namespace.override` | bool | `false` | Put the dashboard ConfigMaps in another namespace. |
| `metrics.dashboard.namespace.value` | string | `default` | That namespace. |
| `metrics.dashboard.labels.label` | any | `false` | The whole `labels` map is added to ConfigMap labels (plus `app: <fullname>-grafana`). Replace it with your sidecar label (e.g. `grafana_dashboard: "1"`). The API server rejects a boolean label value. |
| `metrics.cacheMetricsStorageSize` | size | `128m` (32m) | `lua_shared_dict cache_metrics`. |
| `metrics.buckets.byte` | csv string | `1000,…,100000000000` | Byte histogram buckets. Positive integers only. |
| `metrics.buckets.time` | csv string | `0.001,…,10` | Time histogram buckets (seconds). |
| `metrics.prometheus.enabled` | bool | `true` | Renders Service `<fullname>-metrics` (label `metrics: 'true'`). Gates ServiceMonitor and PodMonitoring. The `:9145` listener itself is always present. |
| `metrics.prometheus.port` | int | `9145` | Metrics Service port (container port fixed at 9145). |
| `metrics.prometheus.serviceMonitor.enabled` | bool | `false` | Renders a `monitoring.coreos.com/v1` ServiceMonitor. |
| `metrics.prometheus.serviceMonitor.interval` | duration | `5s` | Scrape interval; also used by PodMonitoring. |
| `metrics.prometheus.serviceMonitor.path` | string | `/cache_metrics` | Scrape path; also used by PodMonitoring. |
| `metrics.prometheus.serviceMonitor.port` | string | `cache-metrics` | Service port name. |
| `metrics.prometheus.serviceMonitor.scheme` | string | `http` | Scrape scheme. |
| `metrics.prometheus.serviceMonitor.scrapeTimeout` | duration | `5s` | Scrape timeout. |
| `metrics.prometheus.podMonitoring.enabled` | bool | `false` | GKE `monitoring.googleapis.com/v1` PodMonitoring on port `cache-metrics`. |

Undeclared keys the ServiceMonitor reads: `metrics.prometheus.serviceMonitor.labels` (map) and `metrics.prometheus.serviceMonitor.spec.jobLabel`. `global.additionalLabels` keys become `targetLabels`.

## `otel`

The wiring does not match the comments in `values.yaml`.

| Key | Type | Default | Effect / constraints |
|---|---|---|---|
| `otel.enabled` | bool | `false` | Adds `otel_trace_id`/`otel_span_id`/`otel_parent_id` to `default_log` and `cloud_backend_log` (`deploy/files/logging-formats.conf`). The OTel module only loads when the undeclared `traces.enabled` is true (`nginx.conf`), so `otel.enabled=true` alone may reference undefined variables. Not verified. |
| `otel.headers` | list/null | `null` | **Unused**. |
| `otel.interval` | int | `5` | **Unused**. |
| `otel.batchSize` | int | `512` | **Unused**. |
| `otel.batchCount` | int | `4` | **Unused**. |
| `otel.endpoint` | string | `localhost:4317` | **Unused**. The exporter endpoint is hardcoded to `localhost:4317` under `traces.enabled`. |
| `otel.context` | string | `propagate` | **Unused** (`otel_trace_context propagate` is hardcoded on the health server). |
| `otel.ratio.enabled` | bool | `false` | **Unused**. |
| `otel.ratio.source` | string | `otel_trace_id` | **Unused**. |
| `otel.ratio.percent` | int | `10` | **Unused**. |
| `otel.collector.enabled` | bool | `false` | **Unused**. The sidecar `OpenTelemetryCollector` renders on `otel.enabled` AND the undeclared `otel.sidecar`. It does not disable the ServiceMonitor despite the comment. |
| `otel.collector.batch` | map/null | `null` | Collector `processors.batch`. |
| `otel.collector.export.otlp.endpoint` | string | `otel-collector.svc.svc.cluster.local:4317` | Collector `exporters.otlp/ls.endpoint`. |
| `otel.collector.export.otlp.tls.insecure` | bool | `true` | Collector exporter TLS. |


## Non-configurable (fixed in files)

These are fixed and cannot be changed from values:
- Health port `13129` and path `/healthz`; metrics port `9145`.
- `proxy_ssl_verify on` with the system CA bundle; `proxy_ssl_server_name on`.
- `proxy_cache_revalidate on`, `proxy_cache_lock on`, `proxy_cache_convert_head off`.
- `proxy_ignore_headers Cache-Control Expires Set-Cookie` on S3/GCS/Azure.
- Cache keys and filtered query params; bypass rules (`cache-bypass.conf`, Azure CPK/lease/tags).
- Nucleus lock `10s`/`3s` and its headers and CORS.
- `X-Cache-Status` header; the 421 self-server response; the 503 mutation status.
- `client_max_body_size 0`, `proxy_request_buffering off`, `sendfile`, `aio threads`.
- Reset path `/__/cache/reset` and `CC-Instance`; legacy strings.
- Log formats (`logging-formats.conf`). Do not add `upstream_queue_time`.

## Example override files

Lists replace defaults, so each example redefines `volumes` and `paths` with only the needed backend. Every default `persistence.volumes` entry becomes a PVC on every replica even when its backend is disabled (`_helpers.tpl` `func.shouldMaterializeCacheVolume`), so keeping the default list creates four 2048Gi PVCs per replica. On an existing release, removing entries changes the StatefulSet's volume claim templates, which Kubernetes rejects, so plan a reinstall. Disabled backends may omit both (`AGENTS.md` [PVC Size Guard](../../../../shared-content-cache/reference/deploy/AGENTS.md#pvc-size-guard)). These are derived from `values.yaml` and the `AGENTS.md` backend sections and have not been rendered.

### Single-backend S3 (`values-s3.yaml`)

```yaml
image:
  pullSecrets: [{name: ngc-registry}]
persistence:
  mode: pvc
  storageClassName: gp3                  # AWS recommendation; WaitForFirstConsumer
  volumes:
    - {name: s3, path: /proxy_cache_s3, sizeGi: 2048, minFreeSizePercentage: 7}
nginx:
  proxyCache:
    paths:
      - {name: s3, path: /proxy_cache_s3/data, maxIdleTime: 1d, metadataMemorySize: 2048m}
  upstreamBalancer:
    resolver:
      cache: {successTtlSecs: 30, failureTtlSecs: 10}   # keep <= 52s (S3 DNS TTL)
  backends:
    azure: {include: false}
    gcs: {include: false}
    nucleus: {include: false}
    s3:
      include: true
      enableUpstreamKeepalive: true      # avoids 504 pattern
      allowHttp2Clients: true
      burstyCache:
        ttlSec: 300
        rules:
          - {name: s3-list-operations, method: GET, uriStartsWith: "/?list-type="}
          - {name: s3-head-operations, method: HEAD, uriStartsWith: "/"}
tls: {enabled: true, secretName: ovcc-tls}
metrics:
  prometheus:
    serviceMonitor: {enabled: true}
```

### Azure Blob (`values-azure.yaml`)

```yaml
image:
  pullSecrets: [{name: ngc-registry}]
featureFlags:
  useRoundRobinLoadBalancing: true       # matters only for LoadBalancer/NodePort Services
persistence:
  storageClassName: managed-csi-premium-burst   # monitor burst credits
  volumes:
    - {name: az, path: /proxy_cache_az, sizeGi: 2048, minFreeSizePercentage: 7}
nginx:
  proxyCache:
    paths:
      - {name: az, path: /proxy_cache_az/data, maxIdleTime: 1d, metadataMemorySize: 2048m}
  backends:
    s3: {include: false}
    gcs: {include: false}
    nucleus: {include: false}
    azure:
      include: true
      enableUpstreamKeepalive: true      # AGENTS.md: resolved production 504s
      allowHttp2Clients: true
      burstyCache:
        ttlSec: 300
        rules:
          - {name: azure-list-operations, method: GET, uriStartsWith: "/?comp=list"}
          - {name: azure-head-operations, method: HEAD, uriStartsWith: "/"}
```

### Nucleus (`values-nucleus.yaml`)

NGINX needs at least one S3, Azure, or GCS server: the shared access-log format uses a variable that only those servers set, so a release with Nucleus alone fails with `unknown "bursty_cache" variable`. This example keeps a small S3 backend for that reason.

```yaml
image:
  pullSecrets: [{name: ngc-registry}]
persistence:
  storageClassName: your-storage-class   # the default is Azure managed-csi-premium-burst
  volumes:
    - {name: nucleus, path: /proxy_cache_nucleus, sizeGi: 2048, minFreeSizePercentage: 7}
    - {name: s3, path: /proxy_cache_s3, sizeGi: 16, minFreeSizePercentage: 7}
nginx:
  proxyCache:
    paths:
      - {name: nucleus, path: /proxy_cache_nucleus/data, maxIdleTime: 1d, metadataMemorySize: 1024m}
      - {name: s3, path: /proxy_cache_s3/data, maxIdleTime: 1d, metadataMemorySize: 256m}
  backends:
    s3: {include: true}
    azure: {include: false}
    gcs: {include: false}
    nucleus:
      include: true
      allowCacheReset: true
      allowHttp2Clients: false
      enableUpstreamKeepalive: false
      bridgeResolver: null               # or "<ip>:53" to force a resolver for /omni/lft
service:
  nucleus: {type: ClusterIP, port: 14129, containerPort: 14129}
```
