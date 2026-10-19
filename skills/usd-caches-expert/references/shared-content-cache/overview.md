Up: [shared content cache](README.md)

# OVCC architecture and behavior

Path prefix used below: `deploy/` = `shared-content-cache/reference/deploy/`.

## Components and how they are wired

| Component | Role | Wiring | Source |
|---|---|---|---|
| OpenResty/NGINX (default image `nvcr.io/nvidia/omniverse/usd-content-cache:3.0.2`) | Data plane: TLS termination, `server_name` routing by `Host`, `proxy_cache` disk cache, cache lock, upstream proxying | `nginx.conf` and the backend `*.conf` files are Helm-templated into ConfigMap `<fullname>-nginx-conf` and mounted with `subPath` | `deploy/templates/configmap-nginx.yaml`, `deploy/templates/statefulset.yaml` |
| Lua (OpenResty) | Access checks (presigned URL / SAS validation plus a `Range: bytes=0-0` probe to origin), presigned-auth cache, request lock, DNS-resolving keepalive balancer, cache generation (reset), mutation detection, Azure response overrides and cache variant, Prometheus metrics, optional TLS cert hot reload | ConfigMap `<fullname>-lua`, mounted at `/etc/nginx/conf.d/lua-runtime/`. Only files listed in `nginx.lua.files` that exist in `deploy/files/lua/` are rendered. | `deploy/templates/configmap-lua.yaml`, `deploy/files/lua/*.lua` |

## Pod layout

The pod is a StatefulSet with `replicaCount` replicas, default `1`. It has one container named `ovcc` (or `usd-content-cache` with compat) and these ports (`deploy/templates/statefulset.yaml`):

| Port | Name | Purpose |
|---|---|---|
| `13129` | `healthz` | Plaintext health server; readiness and liveness probes `GET /healthz` |
| `service.cache.containerPort` (14128) | `https`/`http` | All cloud backends, the self-server, and passthrough |
| `service.nucleus.containerPort` (14129) | `nucleus` | Nucleus backend (catch-all `server_name _`) |
| `9145` | `cache-metrics` | `GET /cache_metrics` (Prometheus text); `/nginx_status` restricted to 127.0.0.1 |

Volumes: TLS secret at `/etc/nginx/ssl` (when `tls.enabled`), `emptyDir` at `/tmp`, NGINX/Lua ConfigMaps, and one cache volume per materialized `persistence.volumes[]` entry. That is a PVC (`volumeClaimTemplates`, RWO) or an `emptyDir` with `sizeLimit` (`persistence.mode=ephemeral`).

Services (`deploy/templates/service.yaml`):
- `<fullname>` serves the cache port.
- `<fullname>-nucleus` serves 14129 and exists only when Nucleus is included.
- `<fullname>-metrics` serves 9145 and exists when `metrics.prometheus.enabled`.

## Request routing per backend

NGINX picks the `server` block by `Host`. All cloud servers listen on 14128. Server definitions: `deploy/files/backends/*.conf`. Routing table: [AGENTS.md Backend Comparison](../../../../shared-content-cache/reference/deploy/AGENTS.md#backend-comparison).

| Backend | `server_name` (default) | Port | Upstream (`proxyPass`) | Cache zone / volume |
|---|---|---|---|---|
| S3 | `~^[^.]+\.s3\.[^.]+\.amazonaws\.com$` (virtual-hosted only) | 14128 | `$scheme://$host`, or the resolved keepalive peer when `enableUpstreamKeepalive` | `s3` / `/proxy_cache_s3` |
| Azure Blob | `~^(?<container_name>[^\.]+)\.blob\.core\.windows\.net$` | 14128 | `$scheme://$host` | `az` / `/proxy_cache_az` |
| GCS (S3 XML API, HMAC) | `~^([^.]+\.)?storage\.googleapis\.com$` | 14128 | `https://$host` (always HTTPS) | `gcs` / `/proxy_cache_gcs` |
| Nucleus | `_` (catch-all on its own port) | 14129 | `$scheme://$host` | `nucleus` / `/proxy_cache_nucleus` |
| Self | `<fullname>`, `<fullname>.<ns>`, `<fullname>.<ns>.svc`, `<fullname>.<ns>.svc.cluster.local`, plus `nginx.selfServer.extraNames` | 14128 and 14129 | none; returns **421 Misdirected Request** | — |
| Passthrough | `_ default_server` on 14128 | 14128 | `$scheme://$http_host`; no `proxy_cache` directive, so nothing is cached | — |

`$scheme` is the client-side scheme. With `tls.enabled=false`, S3 and Azure requests are proxied to the origin over plain `http://`. GCS is hardcoded to `https`. Outbound TLS always verifies against the system CA bundle (`proxy_ssl_verify on`, `proxy_ssl_server_name on`, depth `nginx.sslVerifyDepth`). There is no values override for self-signed origins (`AGENTS.md` [Configuration](../../../../shared-content-cache/reference/deploy/AGENTS.md#configuration-valuesyaml)).

Within the S3, GCS, and Azure servers (`deploy/files/backends/s3-compat-server.conf`, `azure-server.conf`):
- `location /` runs `rewrite_by_lua`. If `featureFlags.enableBurstyCache` is on and the request matches a `burstyCache.rules[]` entry (method plus `request_uri` prefix), it goes to `@bursty_cache`; otherwise to `@proxy_cache`.
- `@proxy_cache` runs, in order: `cache-bypass.conf`, the cache generation, `mutation-detection.conf` (only when `requiresVersioning=false`), then the `lua-access.lua` access check, then `proxy_pass`.
- Lua access check, per request kind (`deploy/files/lua/lua-access.lua`):
  - S3 presigned (`X-Amz-Signature` present): validate the `X-Amz-Date` + `X-Amz-Expires` window; a malformed window returns **400**. Then consult `presigned_url_cache`. On a miss, take a lock and probe the origin with `Range: bytes=0-0`. Cache the success for (expiry − 2 s) if at least 10 s of validity remain.
  - Azure SAS (`sig` present): same flow using `st`/`se`.
  - `Authorization` header: probe the origin on every request, then serve from cache.
  - Anonymous GET with `requiresVersioning=false`: send a `HEAD` probe to capture validators.
  - HEAD: no check (always bypassed).
  - A probe answered **403** is relayed to the client. A probe that fails twice disables caching for that request (`disable_cache=1`).

Nucleus (`deploy/files/backends/nucleus-server.conf`):
- `/omni/lft` caches with key `$cache_generation|$http_nvcf_nca_id|$request_method|$http_omniverse_content_uid|$http_range`. It forwards `X-OV-URI`, `Omniverse-Proxy-Scheme`, `Omniverse-Content-Uid`, and `Omni-Tenant-Token`, and adds CORS `*`.
- `/` bypasses the cache when `Authorization` is present, strips `Omni-Tenant-Token`, and uses key `$cache_generation|$http_nvcf_nca_id|$request_method|$host|$request_uri|$http_omniverse_content_uid|$http_range`.
- Cache lock is hardcoded to `proxy_cache_lock_age 10s`, `proxy_cache_lock_timeout 3s`.
- No Lua access check and no `If-Match` requirement.

## Cache key

| Path | Key | Source |
|---|---|---|
| S3/GCS standard | `$cache_generation\|$host\|$request_method\|$uri\|$filtered_args\|$http_if_match\|$http_range` | `s3-compat-server.conf` |
| Azure standard | `…\|$http_if_match\|$azure_cache_variant`. The variant canonicalizes `x-ms-range` over `Range` and separates structured-body/checksum representations. | `azure-server.conf`, `lua/lua-cache-key.lua` |
| Bursty (S3/GCS/Azure) | `$cache_generation\|$adaptive_cache_key\|$host\|$request_method\|$request_uri` | same |
| Nucleus | see above | `nucleus-server.conf` |

`$filtered_args` is the sorted, URI-escaped query string with credentials removed:
- S3/GCS remove `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-Signature`, `X-Amz-SignedHeaders`, and `x-id`.
- Azure removes the SAS params (`sv ss srt sp sr se st sig spr sip si skoid sktid skt ske sks skv saoid suoid scid`) and response overrides (`rscc rscd rsce rscl rsct`).

As a result, different presigned URLs for the same object and ETag share one entry. `$adaptive_cache_key` picks the first available of: `nvcf-function-id|nvcf-function-version`, then `nvcf-nca-id`, then `md5(Authorization)`, then `-`. Those three NVCF headers are stripped before going upstream.

## Consistency model

Detail: [AGENTS.md Consistency Model](../../../../shared-content-cache/reference/deploy/AGENTS.md#consistency-model).

- Per-replica isolation: each pod has its own cache volume, presigned-auth cache, and generation counter (`lua_shared_dict cc_generation`). There is no cross-replica synchronization. The consistency model is eventual, with TTL expiry.
- Standard entries: `proxy_cache_valid` per `nginx.proxyCache.validity` (default `200`/`206` → `1d`). An entry idle for longer than `paths[].maxIdleTime` (default `1d`) is evicted (`inactive=`). `proxy_cache_revalidate on` refreshes expired entries with conditional requests (a 304 refreshes without re-download).
- Version gating (`requiresVersioning: true`, default for s3/azure/gcs): a GET without `If-Match` bypasses the cache. Because the key includes `If-Match`, a new object version means a new key.
- Mutable mode (`requiresVersioning: false`): caches by URI. On a HIT, the fresh upstream ETag (or Last-Modified) from the access probe is compared with the cached one. On mismatch the entry is purged and the response is **503**, which `aws-sdk-s3` retries (`deploy/files/mutation-detection.conf`, `lua/lua-mutation.lua`). Detail: [AGENTS.md Mutable Mode](../../../../shared-content-cache/reference/deploy/AGENTS.md#mutable-mode-requiresversioning-false).
- Logical invalidation: `POST /__/cache/reset?gen=N` bumps the generation that is embedded in every key. This happens per replica; clients must reach every replica. See [api-http.md](api-http.md#cache-reset-generation).

## Two-layer caching (presigned URLs)

1. Auth layer: `lua_shared_dict presigned_url_cache` (`nginx.sharedMemory.limits.presignedUrlCache`, default 1024m). Key = full request URI including signature. TTL = URL expiry − 2 s.
2. Content layer: the NGINX disk cache. The key excludes signatures. TTL = `validity`.

An expired presigned URL fails the date check. Origin responses to expired or invalid signatures (403) are relayed (`AGENTS.md` [S3 Presigned URL Caching](../../../../shared-content-cache/reference/deploy/AGENTS.md#s3-presigned-url-caching)).

## Bursty vs standard cache

| | Standard | Bursty |
|---|---|---|
| Ops (defaults) | GET objects | S3/GCS `GET /?list-type=`, Azure `GET /?comp=list`, `HEAD /` (all HEAD) |
| TTL | `validity` (1d) | `burstyCache.ttlSec` (300 s) for 200 and 404 |
| Key has ETag | yes | no |
| Key has identity | no | yes (`$adaptive_cache_key`) |
| Honors `Cache-Control: no-cache` | yes (bypass) | no (values comment: "ignores `cache-control: no-cache` for the sake of performance") |

The bursty path sets `$disable_cache "1"`, but that variable only drives `proxy_cache_bypass` in `cache-bypass.conf`, which `@bursty_cache` does not include. So bursty responses are cached under the bursty key. Its purpose is consistency within one simulation run, not reuse across runs.

## Session affinity and load balancing

- `featureFlags.enableSessionAffinity` sets `sessionAffinity: ClientIP` on the cache and Nucleus Services, with `sessionAffinityTimeoutSeconds`. Use it so in-memory, per-pod caches (bursty, presigned-auth) stay effective across replicas. Constraint: timeout ≥ `burstyCache.ttlSec` ([AGENTS.md Critical Constraints](../../../../shared-content-cache/reference/deploy/AGENTS.md#critical-constraints)). Benchmark before enabling.
- `featureFlags.useRoundRobinLoadBalancing` sets `externalTrafficPolicy: Local`, but only when the Service type is `LoadBalancer` or `NodePort`. It counters Azure LB 5-tuple hash imbalance and SNAT exhaustion.

## Scaling out

- Increasing `replicaCount` adds independent caches. Each replica warms separately, so the origin may see up to N× cold fetches per object. Hit rate per replica depends on how the Service spreads clients.
- Sizing guidance from source ([AGENTS.md Infrastructure per Cloud](../../../../shared-content-cache/reference/deploy/AGENTS.md#infrastructure-per-cloud)):
  - GKE: bottleneck is the NGINX connection queue depth (one worker per vCPU). The recommendation is `c4-highcpu-48` with `hyperdisk-balanced` and a 2048Gi PVC per backend. The OVCC:worker ratio is 1:34 (`ceil(workers/34)` replicas); 3 replicas for 100 workers measured 42.9 s vs 79.9 s direct. Multi-replica beat single large nodes.
  - Azure: `workerConnections ≈ (Max_SNAT / pods / vCPUs) × 1.2`.
  - AWS: disk ≥ 1024GiB per backend, gp3, and resolver `successTtlSecs` ≤ 52 s.
  - Metadata zone: `metadataMemorySize` 1024m default, 2048m for production (`AGENTS.md` [General Settings](../../../../shared-content-cache/reference/deploy/AGENTS.md#general-settings)).

## Placement guidance (what source supports)

- Deploy it in-cluster, in the same cluster and namespace as its clients. Clients reach it via cluster DNS or Service. External exposure is possible with `service.*.type: LoadBalancer` and `loadBalancerIP`.
- PVCs are RWO. The storage classes recommended in source use `volumeBindingMode: WaitForFirstConsumer` so the disk lands in the pod's zone.
- One release serves all enabled backends: S3, Azure, GCS, and Nucleus can run side by side, each with its own zone and volume. Multiple regions: the source makes no explicit per-region recommendation. Because replicas never share state and clients address OVCC by in-cluster Service, the practical unit is one release per cluster. Putting it near compute is implied by the in-cluster design; nothing in source pins it near storage. Cross-region or multi-cluster topologies are not determinable from source.

## What OVCC does not do

- No cross-replica cache sharing or invalidation.
- No caching of HEAD on the standard path, of requests without `If-Match` when versioning is required, of `Cache-Control: no-cache` requests, of authenticated Nucleus `/` requests, of Azure CPK/lease/tag-conditioned requests, or of passthrough traffic.
- No write caching. Uploads stream through (`client_max_body_size 0`, `proxy_request_buffering off`).
- No path-style S3 routing (`s3.amazonaws.com/bucket`). The default S3 `serverName` requires virtual-hosted style. GCS supports only `storage.googleapis.com` with HMAC, no OAuth2.
- No physical purge API. Reset is logical, by generation.
- No self-signed origin trust override.
- No authentication of OVCC clients itself. It relies on origin auth (presigned/SAS/Authorization probe).
