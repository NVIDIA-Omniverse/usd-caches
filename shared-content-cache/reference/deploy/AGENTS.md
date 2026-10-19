<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Omniverse Content Cache (ovcc) – Helm Chart Documentation

This documentation follows the [AGENTS.md](https://agents.md/) standard for AI coding agents. It provides context for deploying and operating Omniverse Content Cache via Helm.

**Supported customization:** `values.yaml` is the **only supported interface** for configuration. Do not modify templates, Lua scripts, or backend config files directly.

---

## Critical Constraints

[!] **DO NOT** edit templates for config changes — use `values.yaml` only

[!] **DO NOT** add `upstream_queue_time` to `logging-formats.conf` — NGINX Plus only, causes `nginx: [emerg]` crash

[!] **NEVER** log auth headers — only log presence via `has_auth_header`

[!] **NEVER** set `sessionAffinityTimeoutSeconds` < `burstyCache.ttlSec` — clients bounce between pods, miss cache

[!] **NEVER** hardcode storage classes, image tags, or environment-specific values in templates

[!] **Nucleus-only:** Auth header presence **bypasses cache** (multi-tenant safety) — cloud backends do NOT bypass

[!] **Enabled backends require matching cache wiring:** If `nginx.backends.<name>.include=true`, the backend's `proxyCacheName` must exist in both `nginx.proxyCache.paths[].name` and `persistence.volumes[].name`; otherwise Helm rendering fails before rollout.

[!] **PVC size mismatch blocks upgrade:** If existing PVCs are smaller than `persistence.volumes[].sizeGi`, `helm upgrade` fails. Delete the PVC or adjust `sizeGi` — see "PVC Size Guard" under Persistence

[!] **Storage-mode transitions block upgrade:** `persistence.mode` can be `pvc` or `ephemeral`. Switching between PVC-backed cache storage and `emptyDir` cache storage is not a safe in-place `helm upgrade`; the chart fails before rollout.

[!] **Name migration:** Chart `v3.0.3 (hc)` was the last UCC release (`usd-content-cache`). Upgrading a stock/default install to OVCC (`3.1.x (hc)`) without `--set compat.useUccDefaults=true` fails at render time. See "Upgrade From UCC / usd-content-cache".

---

## What OVCC is

OVCC (`ovcc` chart) is an OpenResty/NGINX-based caching reverse proxy for blob/object storage. It accelerates access to Omniverse content (USD, MDL, textures) stored in S3, Azure Blob Storage, GCS, and Nucleus. Uses Lua for access control and presigned URL validation; caching is keyed by ETag with per-backend isolation. Deployed as a StatefulSet (default 1 replica), with independent cache volumes when scaled out. Published to NGC; customers install via Helm.

---

## Consistency Model

### Architecture

- **Deployment:** StatefulSet with independent replicas (default 1)
- **Cache isolation:** Each pod has its own cache volume — no shared cache, no cross-replica synchronization
- **Consistency type:** Eventual consistency with TTL-based expiration

### Two-Layer Caching (Presigned URLs)

OVCC separates **auth verification** from **content caching**:

| Layer | Storage | TTL | Key |
|-------|---------|-----|-----|
| Auth verification | `presigned_url_cache` (Lua shared dict) | URL expiration - 2s | Full request URI (with signature) |
| Content | NGINX disk cache | 1 day | `$host|$method|$uri|$filtered_args|$http_if_match|$http_range` (no signature) |

Content cached independently of presigned URL. Different presigned URLs for same object (same ETag) share cache entry.

### Cache Expiration

| Cache Type | TTL |
|------------|-----|
| Standard (200, 206) | 1 day |
| Bursty (200, 404) | 300s |

`proxy_cache_revalidate on` — expired entries use conditional requests (`If-Modified-Since`/`If-None-Match`); upstream `304` refreshes cache without re-download.

### Cache Bypass

| Condition | Behavior |
|-----------|----------|
| HEAD request | BYPASS |
| Missing `If-Match` header | BYPASS |
| `Cache-Control: no-cache` | BYPASS |
| Nucleus + auth header | BYPASS (multi-tenant safety) |

Cloud backends (S3/Azure/GCS) do **not** bypass on auth headers — access check performed, then serve from cache.

### Bursty Cache vs Standard Cache

| Aspect | Standard | Bursty |
|--------|----------|--------|
| Operations | GET | LIST, HEAD |
| TTL | 1 day | 300s |
| Key includes ETag | Yes | No |
| Key includes identity | No | Yes (`$adaptive_cache_key`) |
| Purpose | Long-term content | Consistency within simulation run |

`$adaptive_cache_key` priority: `fn_id|fn_version` > `nca_id` > `md5(auth_header)` > `"-"`

### Session Affinity

- **Config:** `enableSessionAffinity` (default `false`)
- **Behavior:** Routes same client IP to same pod
- **Constraint:** `sessionAffinityTimeoutSeconds` ≥ `burstyCache.ttlSec`
- **Note:** Performance impact varies by workload — benchmark before enabling

### Trade-offs

| Design Choice | Benefit | Cost |
|---------------|---------|------|
| Per-replica isolation | No coordination overhead | Replicas may serve different content until TTL expires |
| TTL + revalidation | Efficient refresh | Stale content possible within TTL window |
| ETag-based keys | Version accuracy | Requests without `If-Match` bypass cache |
| Separate auth/content caches | Long-term content caching with short-lived URLs | Two caches to reason about |

---

## Configuration (values.yaml)

`values.yaml` is the **only external interface**. Key sections:

- `image` — registry, repository, tag, pullSecrets
- `featureFlags` — `ignoreClientAbort`, `enableBurstyCache`, `enableSessionAffinity`, `useRoundRobinLoadBalancing`
- `service` — ports (OVCC: 14128, Nucleus: 14129)
- `tls` — `enabled`, `secretName`, `sslProtocols` (TLS versions offered to clients, default `TLSv1.3`)
- `compat` — UCC-to-OVCC migration: `useUccDefaults` (in-place upgrade), `uccDefaults` (UCC-side names used by that flag), `emitLegacyCacheReset` (dual-emit legacy reset URL + header)
- `terminationGracePeriodSeconds` — seconds a stopping pod has to finish in-flight requests before Kubernetes kills it (default `30`)
- `persistence` — `mode` (`pvc` or `ephemeral`), `storageClassName` (global default for PVC mode), `volumes[]` (per-backend: name, path, sizeGi, minFreeSizePercentage, optional per-volume storageClassName)
- `nginx.proxyCache` — `lockAge`, `lockTimeout`, `validity`, `paths[]` (per-backend cache zones)
- `nginx.backends` — per-backend config: `include`, `serverName`, `proxyCacheName`, `proxyPass`, `requiresVersioning` (default `true`), `allowHttp2Clients`, `enableUpstreamKeepalive`, `burstyCache` (per-backend, not global)
- `proxy_ssl_verify` is hardcoded to `on`. No Helm override exists yet for self-signed backend certificates.

Override via `helm install -f values-override.yaml` or `--set`. Do not edit templates for config changes.

**Client TLS versions.** `tls.sslProtocols` defaults to `TLSv1.3` and that
hardened default should stay in place wherever clients can negotiate it. Set it
to `"TLSv1.2 TLSv1.3"` for clients whose TLS stack cannot reach 1.3 — notably
.NET Framework's `System.Net.WebClient`, which fails against a 1.3-only endpoint
with "Could not create SSL/TLS secure channel". TLS 1.2 remains secure; only 1.0
and 1.1 are deprecated.

**Config changes roll the pods.** The pod template carries checksums of the
rendered config, so editing values and running `helm upgrade` restarts the
StatefulSet on its own. A manual `kubectl rollout restart` is no longer needed
for a config-only change, and older runbooks that prescribe one are stale.

---

## Chart identity and rendered resource names

The chart has two distinct identities. Treat them as fixed and pinned by tests; do not change either without coordinating with NGC ops, dashboards, and on-call runbooks.

- **NGC / marketing identity:** `Chart.yaml` `name: ovcontentcache`. Drives the published tarball (`ovcontentcache-<version>.tgz`) and the NGC catalog path. Came from marketing.
- **K8s / engineering identity:** `values.yaml` `nameOverride: ovcc`. Drives the StatefulSet, Service, ConfigMap, and PVC names rendered into the cluster. Existing dashboards (PVC regex `.*ovcc.*`), operator scripts, and runbooks depend on this.

### Rendered resource names by scenario

All values below are real `helm template` output.

| Resource | `helm install ovcc` (default) | `helm install myproxy` (default) | `--set compat.useUccDefaults=true` (release `ovcc`) | `--set compat.useUccDefaults=true` (release `myproxy`) | `--set nameOverride=cache` (release `myproxy`) | `--set fullnameOverride=ovcc-prod` (release `myproxy`) |
|---|---|---|---|---|---|---|
| StatefulSet | `ovcc` | `myproxy-ovcc` | `ovcc-usd-content-cache` | `myproxy-usd-content-cache` | `myproxy-cache` | `ovcc-prod` |
| Service (cache) | `ovcc` | `myproxy-ovcc` | `ovcc-usd-content-cache` | `myproxy-usd-content-cache` | `myproxy-cache` | `ovcc-prod` |
| Service (metrics) | `ovcc-metrics` | `myproxy-ovcc-metrics` | `ovcc-usd-content-cache-metrics` | `myproxy-usd-content-cache-metrics` | `myproxy-cache-metrics` | `ovcc-prod-metrics` |
| Service (nucleus) | `ovcc-nucleus` | `myproxy-ovcc-nucleus` | `ovcc-usd-content-cache-nucleus` | `myproxy-usd-content-cache-nucleus` | `myproxy-cache-nucleus` | `ovcc-prod-nucleus` |
| Service (headless) | `ovcc-headless` | `myproxy-ovcc-headless` | `ovcc-usd-content-cache-headless` | `myproxy-usd-content-cache-headless` | `myproxy-cache-headless` | `ovcc-prod-headless` |
| ConfigMap (nginx) | `ovcc-nginx-conf` | `myproxy-ovcc-nginx-conf` | `ovcc-usd-content-cache-nginx-conf` | `myproxy-usd-content-cache-nginx-conf` | `myproxy-cache-nginx-conf` | `ovcc-prod-nginx-conf` |
| ConfigMap (lua) | `ovcc-lua` | `myproxy-ovcc-lua` | `ovcc-usd-content-cache-lua` | `myproxy-usd-content-cache-lua` | `myproxy-cache-lua` | `ovcc-prod-lua` |
| ConfigMap (runtime) | `ovcc-runtime` | `myproxy-ovcc-runtime` | `ovcc-usd-content-cache-runtime` | `myproxy-usd-content-cache-runtime` | `myproxy-cache-runtime` | `ovcc-prod-runtime` |
| Pod 0 | `ovcc-0` | `myproxy-ovcc-0` | `ovcc-usd-content-cache-0` | `myproxy-usd-content-cache-0` | `myproxy-cache-0` | `ovcc-prod-0` |
| StatefulSet `serviceName` | `ovcc-headless` | `myproxy-ovcc-headless` | `ovcc-usd-content-cache-headless` | `myproxy-usd-content-cache-headless` | `myproxy-cache-headless` | `ovcc-prod-headless` |
| PVC template names | `az`, `s3`, `gcs`, `nucleus` | (same) | (same) | (same) | (same) | (same) |
| Bound PVCs at runtime\* | `az-ovcc-0`, … | `az-myproxy-ovcc-0`, … | `az-ovcc-usd-content-cache-0`, … | `az-myproxy-usd-content-cache-0`, … | `az-myproxy-cache-0`, … | `az-ovcc-prod-0`, … |

\* Materialized by the StatefulSet controller at apply time as `<volumeClaimTemplate>-<sts-name>-<ordinal>`.

### Rules to remember

- When the release name **already contains** `ovcc` (e.g. release named `ovcc`), Helm collapses `<release>-<name>` to just `<release>`. That is why a default install named `ovcc` renders short names. Standard Helm behavior; do not try to suppress it.
- When the release name **does not contain** `ovcc`, resources render as `<release>-ovcc-*`. This is the canonical pattern most production installs use, and what the `.*ovcc.*` PVC dashboard regex expects.
- `compat.useUccDefaults=true` switches the K8s identity to `usd-content-cache` (via `chart.compatFullname`) so existing UCC-era PVCs rebind on upgrade with no data migration. See "Upgrade From UCC / usd-content-cache" below.
- `nameOverride` is the supported escape hatch for renaming K8s resources. `fullnameOverride` bypasses the `<release>-<name>` template entirely — use sparingly; operators lose visibility into which release owns which resource.
- PVC template names (`az`, `s3`, `gcs`, `nucleus`) identify the cache backend, not the release. They are stable across all scenarios.

### Where the rules live in this chart

- `Chart.yaml` `name:` — the NGC / marketing identity.
- `values.yaml` `nameOverride:` — the K8s / engineering identity (default value).
- `templates/_helpers.tpl` — `chart.name`, `chart.fullname`, `chart.compatName`, and `chart.compatFullname` define how the two identities are projected into rendered resource names. The `<release>-<name>` collapsing and the `compat.useUccDefaults=true` branch live here.

If you change either identity, update those three places together and re-render the scenarios above to confirm the rendered names you expect.

---

## Upgrade From UCC / usd-content-cache

### Facts

- Last UCC chart: `v3.0.3 (hc)` (`usd-content-cache`), container `v3.0.1 (c)`.
- New chart identity: `ovcontentcache` (OVCC prose name).
- Chart default image: `nvcr.io/nvidia/omniverse/usd-content-cache:3.0.2`.

[!] **`nginx.proxyCache.requiresVersioning` is removed.** Version-gated caching
is now set per backend with `nginx.backends.<name>.requiresVersioning`, which
defaults to `true`. An upgrade that still supplies the old global key fails at
render time rather than silently ignoring it.

### What changes on upgrade

| Surface | UCC `v3.0.3` default | OVCC default |
|---------|----------------------|--------------|
| StatefulSet name | `<release>-usd-content-cache` | `<release>-ovcc` (with `compat.useUccDefaults=true`, stays `<release>-usd-content-cache`). See "Chart identity and rendered resource names" for the full rendered-name table. |
| TLS Secret | `usd-content-cache-tls` | `ovcc-tls` |
| Service values key | `service.ucc` | `service.cache` |
| Cache reset endpoint | `/__ucc/cache/reset` | `/__/cache/reset` |
| Instance header | `UCC-Instance` | `CC-Instance` |
| Prometheus metric prefix | `ucc_` | `cc_` |
| Cache data paths | `/proxy_cache_*/ucc_data` | `/proxy_cache_*/data` |

### Upgrade without compatibility (will fail)

If the cluster still has a UCC-era StatefulSet (`<release>-usd-content-cache`), `helm upgrade` to the OVCC chart **fails at render time** unless you enable legacy compatibility. Helm does not roll out a partial OVCC StatefulSet in that case. The error message tells you to add `--set compat.useUccDefaults=true`.

### Standard upgrade (stock UCC install)

`compat.useUccDefaults=true` targets **stock/default `v3.0.3 (hc)` installs only**. If the UCC release customized TLS, ports, storage paths, or resource names, pass explicit upgrade values; the automatic near no-op path does not apply.

For a default `v3.0.3 (hc)` install, enable legacy compatibility. OVCC preserves the UCC-era StatefulSet/Service names (so existing PVCs bind in place) and applies the same runtime defaults that chart used for TLS, cache paths, runtime names, and metrics:

```bash
helm upgrade <release> <ovcc-chart> \
  --set compat.useUccDefaults=true
```

With `compat.useUccDefaults=true` and no other overrides, OVCC applies UCC `v3.0.3` defaults including:

- `tls.secretName`: `usd-content-cache-tls` (instead of `ovcc-tls`)
- cache path suffix: `/proxy_cache_*/ucc_data` (instead of `/proxy_cache_*/data`)
- `ucc_` Prometheus metric prefix (instead of `cc_`)

The legacy cache-reset endpoint (`/__ucc/cache/reset`) and its `UCC-Instance` response header are dual-emitted by default via `compat.emitLegacyCacheReset` (see below); `useUccDefaults` does not control them.

The new runtime values (`/__/cache/reset`, `CC-Instance`) are fixed by the chart and not tunable via values.yaml.

Example (release `ucc`, same namespace):

```bash
helm upgrade ucc ./deploy \
  --namespace production \
  --set compat.useUccDefaults=true
```

### Upgrade with customized TLS or service ports

If the UCC install changed TLS or ports away from `v3.0.3` defaults, keep `compat.useUccDefaults=true` and override the fields you customized. Map former `service.ucc.*` to `service.cache.*`:

```bash
helm upgrade <release> <ovcc-chart> \
  --set compat.useUccDefaults=true \
  --set tls.secretName=<your-tls-secret> \
  --set service.cache.port=<former service.ucc.port> \
  --set service.cache.containerPort=<former service.ucc.containerPort>
```

Any explicit `tls.secretName` or `service.cache.*` value takes precedence over the UCC defaults that compat would otherwise apply. If you customized cache directories in the UCC chart, set `nginx.proxyCache.paths[]` explicitly to the desired upgraded paths.

### PVCs and cache data

Installing `v3.0.3 (hc)` as release `ucc` creates `ucc-usd-content-cache` and PVCs like `az-ucc-usd-content-cache-0`.

- **Without** `compat.useUccDefaults=true`, upgrade is blocked (no partial OVCC StatefulSet).
- **With** `compat.useUccDefaults=true` on a stock install, the StatefulSet stays `ucc-usd-content-cache`, existing PVCs are reused, and cache paths keep the UCC suffix (`/proxy_cache_*/ucc_data`). Helm does not delete unused PVCs if you previously upgraded without compat.

If the upgrade-block helper is bypassed (for example by deleting the UCC StatefulSet first, or by passing `--set fullnameOverride=...` to land under a different resource name), the OVCC install creates a fresh StatefulSet rooted at `<release>-ovcc` (default `nameOverride`) and new PVCs like `az-ucc-ovcc-0`; the old `*-usd-content-cache-*` PVCs remain but unused unless you migrate data manually.

### Optional image pin

The OVCC `4.0.4` chart defaults to `nvcr.io/nvidia/omniverse/usd-content-cache:3.0.2`. Pin explicitly if you need a different tag or registry:

```bash
--set image.registry=<registry> \
--set image.repository=nvidia/omniverse/usd-content-cache \
--set image.tag=<tag>
```

### Legacy cache-reset endpoint (default on)

OVCC 4.x keeps the UCC cache-reset endpoint live so in-flight clients do not break during the rename. When `compat.emitLegacyCacheReset` is `true` (default), the chart additionally:

- Mounts `/__ucc/cache/reset` alongside `/__/cache/reset` on every backend that has cache reset enabled.
- Sets the `UCC-Instance` response header alongside `CC-Instance` on the reset response.

This is unrelated to `compat.useUccDefaults` (K8s names, metric prefix, cache PVC paths). Set `compat.emitLegacyCacheReset=false` once telemetry confirms zero traffic on the legacy URL. The legacy strings are hardcoded in the chart and cannot be changed via values. Planned removal in OVCC 5.0.0.

---

## Backend Comparison

| Attribute | S3 | Azure | GCS | Nucleus |
|-----------|:---|:------|:----|:--------|
| **Port** | `14128` | `14128` | `14128` | `14129` |
| **Pattern** | `~^[^.]+\.s3\.[^.]+\.amazonaws\.com$` | `~^(?<container_name>[^.]+)\.blob\.core\.windows\.net$` | `~^([^.]+\.)?storage\.googleapis\.com$` | `_` (catch-all) |
| **Auth** | AWS SigV4 presigned URLs | SAS tokens | HMAC keys | JWT (optional) |
| **Keepalive** | `true` | `false` [!] | `true` | `false` (unsupported) |
| **HTTP/2 Clients** | `true` | `true` | `true` | `false` |
| **Auth bypasses cache** | No | No | No | **Yes** |
| **Bursty cache ops** | LIST, HEAD | LIST, HEAD | LIST, HEAD | N/A |
| **Log format** | `cloud_backend_log` | `cloud_backend_log` | `cloud_backend_log` | `nucleus_log` |
| **Cache lock (age/timeout)** | `60s`/`120s` | `60s`/`120s` | `60s`/`120s` | `10s`/`3s` |
| **Cache path** | `/proxy_cache_s3` | `/proxy_cache_az` | `/proxy_cache_gcs` | `/proxy_cache_nucleus` |

**Cache key formula (S3, GCS, Nucleus):**
```
$cache_generation|$host|$request_method|$uri|$filtered_args|$http_if_match|$http_range
```

Azure replaces the trailing range component with `$azure_cache_variant`, which
canonicalizes `x-ms-range` and `Range` into one value and separates Get Blob
structured-body and checksum representations. See Azure Cache Key.

---

## Request Routing

```
Host header matching:
  *.s3.*.amazonaws.com       → S3 Backend (Port 14128)
  *.blob.core.windows.net    → Azure Backend (Port 14128)
  *.storage.googleapis.com   → GCS Backend (Port 14128)
  * (catch-all)              → Nucleus Backend (Port 14129)

Cache decision (cloud backends - S3/Azure/GCS):
  Has presigned URL / SAS token?
    ├─ Missing required params (X-Amz-*) → 400 Bad Request
    ├─ Valid → Cache lookup
    │   ├─ HIT  → Return cached
    │   ├─ MISS → Proxy to backend, cache response
    │   └─ BYPASS → HEAD without If-Match, or Cache-Control: no-cache
    └─ Auth header present → Access check, then proxy (NO cache bypass)

Cache decision (Nucleus only):
  Auth header present?
    ├─ Yes → BYPASS cache [why: multi-tenant safety]
    └─ No  → Cache lookup (same as cloud)
```

---

## Backend - GCS

### GCS Core Settings

- **API:** S3-compatible XML API (HMAC Interoperability Mode)
- **Pattern:** `~^([^.]+\.)?storage\.googleapis\.com$`
- **Auth:** HMAC keys only (no OAuth2)
- **Endpoint:** `https://storage.googleapis.com` only (no regional endpoints)
- **Port:** `14128`

### GCS Cache Key

```
$cache_generation|$host|$request_method|$uri|$filtered_args|$http_if_match|$http_range
```

**Excluded params:** `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-Signature`, `X-Amz-SignedHeaders`, `x-id`

`x-id` is an AWS SDK operation hint (`GetObject`, `PutObject`) rather than part
of object identity, so two presigned URLs for the same object share a key
regardless of which operation produced them.

### GCS Critical Config

- **proxy_ignore_headers:** `Cache-Control Expires Set-Cookie`, from the shared `cacheability-overrides.conf` included by S3, GCS, and Azure [why: GCS returns `Cache-Control: private, max-age=0` on all responses — without this, NGINX never caches]
- **enableUpstreamKeepalive:** `true`
- **allowHttp2Clients:** `true`
- **TLS:** 1.3 only (1.2 disabled)

### GCS Presigned URL Validation

`lua-access.lua` validates presence of:
- `X-Amz-Date`
- `X-Amz-Expires`
- `X-Amz-Signature`

**Error responses:**
| Condition | Response | Source |
|-----------|----------|--------|
| Missing `X-Amz-*` params | `400` | OVCC Lua |
| Expired presigned URL | `403` | GCS (not OVCC) |
| Invalid signature | `403` | GCS |
| Auth header present | Access check → proxy | No cache bypass |

### GCS Cache Settings

- **Validity:** `200` → `1d`, `206` → `1d`
- **Bursty cache:** LIST and HEAD when `enableBurstyCache=true`
- **Cache volume:** `/proxy_cache_gcs`, default `2048Gi`

### GCS Limitations

- Only `storage.googleapis.com` (no regional endpoints)
- HMAC only (no OAuth2)
- Bucket names must comply with both GCP and S3 naming conventions

### GCS values.yaml

```yaml
nginx.backends.gcs.include: true
nginx.backends.gcs.serverName: ~^([^.]+\.)?storage\.googleapis\.com$
nginx.backends.gcs.proxyCacheName: gcs
nginx.backends.gcs.enableUpstreamKeepalive: true
nginx.backends.gcs.allowHttp2Clients: true
persistence.volumes: [{name: gcs, path: /proxy_cache_gcs, sizeGi: 2048}]
```

---

## Backend - S3

### S3 Core Settings

- **Pattern:** `~^[^.]+\.s3\.[^.]+\.amazonaws\.com$` (virtual-hosted style)
- **Auth:** AWS SigV4 presigned URLs with `X-Amz-*` query parameters
- **Port:** `14128`

### S3 Cache Key

```
$cache_generation|$host|$request_method|$uri|$filtered_args|$http_if_match|$http_range
```

**Excluded params:** `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-Signature`, `X-Amz-SignedHeaders`, `x-id`

`x-id` is an AWS SDK operation hint (`GetObject`, `PutObject`) rather than part
of object identity, so two presigned URLs for the same object share a key
regardless of which operation produced them.

### S3 Critical Config

- **enableUpstreamKeepalive:** `true` [!] Without this: 504 timeout pattern under load (504-504-200, ~60s per timeout)
- **allowHttp2Clients:** `true`
- **proxy_cache_convert_head:** `off` [why: Preserves HEAD method for AWS SigV4 signature validation]
- **proxy_ssl_server_name:** `on` [why: Required when `proxy_pass` uses a variable]

### S3 Client Access Patterns

- **Transparent:** cluster DNS maps the S3 virtual-hosted bucket name to OVCC, so
  clients stay unmodified and sign the real bucket host.
- **Opt-in:** clients connect to OVCC's well-known Service endpoint while keeping
  the signed S3 bucket `Host` header. A boto3 client can do this with a
  `before-send.s3` hook that rewrites only the connection URL; the signed `Host`
  still drives OVCC routing and SigV4 validation.

TLS verification on the opt-in path validates the OVCC Service name, not the S3
bucket host. Either include that Service name in the OVCC certificate SANs and
trust its CA from the client, or document a deliberate `verify=False` fallback
for test-only runs.

### S3 504 Timeout Fix

```diff
- enableUpstreamKeepalive: false
+ enableUpstreamKeepalive: true
```
[root cause: Without keepalive, each request creates new TCP+TLS connection]

### Upstream Keepalive Routing Semantics

When `enableUpstreamKeepalive=true` on S3 or GCS, OVCC resolves the configured
upstream target and forwards `Host` and scheme from that resolved upstream rather
than from the client-facing request, so a pooled connection always carries the
identity of the host it was opened to. Host-rewrite backends still resolve to
the incoming `$host`.

### S3 Bursty Cache

- **Enabled:** when `enableBurstyCache=true`
- **Operations:** LIST (`/?list-type=`), HEAD
- **TTL:** `burstyCache.ttlSec` (`300` default, `3` in CI)
- **Key:** `$adaptive_cache_key` (priority: `fn_id|fn_version` > `nca_id` > `md5(auth_header)` > `"-"`)
- **Routing:** LIST → `@bursty_cache` via `rewrite_by_lua_block`
### S3 DNS

- **S3 DNS TTL:** ~52s for A records
- **Recommendation:** `upstreamBalancer.resolver.cache.successTtlSecs` ≤ 52s to discover S3 scale-out IPs promptly
### S3 Presigned URL Caching

OVCC separates **auth verification** from **content caching**:

| Cache | Storage | TTL | Key |
|-------|---------|-----|-----|
| Auth verification | `presigned_url_cache` (Lua shared dict) | URL expiration - 2s | Full request URI (with signature) |
| Content | NGINX disk cache | 1 day | `$host|$method|$uri|$filtered_args|$http_if_match|$http_range` (no signature) |

Content is cached independently of the presigned URL that fetched it. Different presigned URLs for the same object (same ETag) share the same cache entry. Typical presigned URL expirations (15 min – 1 hour) are much shorter than content TTL (1 day), so content is almost always served from cache without revalidation.
### S3 values.yaml

```yaml
nginx.backends.s3.include: true
nginx.backends.s3.serverName: ~^[^.]+\.s3\.[^.]+\.amazonaws\.com$
nginx.backends.s3.proxyCacheName: s3
nginx.backends.s3.enableUpstreamKeepalive: true
nginx.backends.s3.allowHttp2Clients: true
nginx.backends.s3.burstyCache.ttlSec: 300
nginx.backends.s3.burstyCache.rules: [{name: s3-list-operations, method: GET, uriStartsWith: "/?list-type="}, {name: s3-head-operations, method: HEAD, uriStartsWith: "/"}]
persistence.volumes: [{name: s3, path: /proxy_cache_s3, sizeGi: 2048}]
```

---

## Backend - Azure Blob Storage

### Azure Core Settings

- **Pattern:** `~^(?<container_name>[^.]+)\.blob\.core\.windows\.net$` (extracts container name)
- **Auth:** SAS tokens in query params
- **Port:** `14128`

### Azure Cache Key

```
$cache_generation|$host|$request_method|$uri|$filtered_args|$http_if_match|$azure_cache_variant
```

`$azure_cache_variant` replaces the plain range component used by the other
backends. Azure treats `x-ms-range` as the effective byte range when present and
otherwise falls back to `Range`, so both are canonicalized into one value; the
variant also separates Get Blob structured-body and request-derived checksum
representations, which return different wire bytes for the same object.

**Excluded params:** SAS credentials and operation noise (`sv`, `ss`, `srt`,
`sp`, `sr`, `se`, `st`, `sig`, `spr`, `sip`, `si`, `skoid`, `sktid`, `skt`,
`ske`, `sks`, `skv`, `saoid`, `suoid`, `scid`) plus response overrides (`rscc`,
`rscd`, `rsce`, `rscl`, `rsct`). Cache hits apply response overrides from the
current request.

### Azure Known Limitations

- OpenResty's proxy cache stores Azure's per-request response identifiers with
  the response. A later HIT may replay the MISS `x-ms-client-request-id` and
  `x-ms-request-id` instead of returning current request metadata. These are
  request-correlation metadata; the limitation does not affect cached content,
  cache identity, or authorization.

### Azure Critical Config

- **enableUpstreamKeepalive:** `false` (default) [!] Consider enabling — resolved 504 timeout patterns in production
- **allowHttp2Clients:** `true` (requires `tls.enabled: true`)
- **proxy_http_version:** `1.1` (NGINX→Azure always HTTP/1.1)
- **proxy_ssl_server_name:** `on` (for SNI)

### Azure 504 Timeout Fix

```diff
- enableUpstreamKeepalive: false
+ enableUpstreamKeepalive: true
```
[root cause: No TCP connection reuse]

### Azure Cache Bypass Rules

| Condition | Result | Source |
|-----------|--------|--------|
| HEAD request | BYPASS | |
| No `If-Match` header | BYPASS | |
| `Cache-Control: no-cache` | BYPASS | |
| `x-ms-encryption-key`, `x-ms-encryption-key-sha256`, or `x-ms-encryption-algorithm` | BYPASS | Customer-provided keys — the decrypted response is never stored |
| `x-ms-if-tags` | BYPASS | Blob tags are independent of content freshness, so Azure evaluates the condition |
| `x-ms-lease-id` | BYPASS | Lease authorization is separate from content freshness and is verified upstream |

### Azure Bursty Cache

- **Enabled:** when `enableBurstyCache=true`
- **Operations:** LIST (`/?comp=list`), HEAD
- **TTL:** `300s` default
### Azure Logging Gotchas

[!] `upstream_queue_time` is NGINX Plus only — causes `nginx: [emerg]` crash
[!] Buffer race: multiple replicas with short flush intervals can interleave flushes → corrupted JSON log lines
### Azure Debugging

[!] Distroless container prevents `exec` — use:
- `kubectl logs`
- ConfigMap inspection
- External curl pods

### Azure values.yaml

```yaml
nginx.backends.azure.include: true
nginx.backends.azure.serverName: ~^(?<container_name>[^.]+)\.blob\.core\.windows\.net$
nginx.backends.azure.proxyCacheName: az
nginx.backends.azure.enableUpstreamKeepalive: false
nginx.backends.azure.allowHttp2Clients: true
nginx.backends.azure.burstyCache.ttlSec: 300
nginx.backends.azure.burstyCache.rules: [{name: azure-list-operations, method: GET, uriStartsWith: "/?comp=list"}, {name: azure-head-operations, method: HEAD, uriStartsWith: "/"}]
persistence.volumes: [{name: az, path: /proxy_cache_az, sizeGi: 2048}]
```

---

## Backend - Nucleus

[!] **Nucleus is architecturally different from cloud backends** — auth header presence **bypasses cache** (multi-tenant safety)

### Nucleus Core Settings

- **Pattern:** `_` (catch-all — any Host not matching cloud backends)
- **Port:** `14129` (not `14128`)
- **HTTP/2:** `false` (disabled)
- **Keepalive:** `false` (unsupported)

### Nucleus Location Blocks

| Location | Caching | JWT Behavior | CORS | Cache Key |
|----------|---------|--------------|------|-----------|
| `/omni/lft` | Yes | Forwarded via `Omni-Tenant-Token` | `Access-Control-Allow-Origin: *` | `$cache_generation\|$http_nvcf_nca_id\|$request_method\|$http_omniverse_content_uid\|$http_range` |
| `/` | Bypassed if auth present | Stripped (not forwarded) | No | `$cache_generation\|$http_nvcf_nca_id\|$request_method\|$host\|$request_uri\|$http_omniverse_content_uid\|$http_range` |

### Nucleus Headers Forwarded

- `X-OV-URI`
- `Omniverse-Proxy-Scheme`
- `Omniverse-Content-Uid`
- `Omni-Tenant-Token` (LFT only)
- `Range`
- `If-Match`, `If-Modified-Since`, `If-None-Match`, `If-Unmodified-Since`
- `x-ms-if-tags`, `x-ms-lease-id`

Conditional and range headers reach the origin on every backend, so a ranged GET
returns the partial `206` the client asked for instead of a full `200`.

### Nucleus Bridge Resolver

- **Config:** `bridgeResolver` in values.yaml (default `null`)
- **When to set:** Optional override only. Left `null`, the proxy resolves bridge/nucleus DNS via the cluster default resolver (CoreDNS / `/etc/resolv.conf`), where the `coredns-overrides` addon installs the nucleus rewrites.
- **Example:** rarely needed; e.g. `10.0.0.10:53` to force a specific resolver

### Nucleus Cache Settings

- **Cache reset:** `true` (default) — unlike cloud backends which default to `false`
- **Cache lock:** `lock_age: 10s`, `lock_timeout: 3s` (shorter than cloud backends: `30s`/`60s`)

### Nucleus Logging

- **Format:** `nucleus_log` (not `cloud_backend_log`)
- **Extra fields:** `http_omniverse_content_uid`, `http_x_ov_uri`, `http_omniverse_proxy_scheme`
- **No OpenTelemetry fields**

### Nucleus Optional

[!] **OVCC does not require Nucleus to function.** When using S3/Azure/GCS, Nucleus is not needed. The catch-all server block only activates if clients send requests to it.

To disable the Nucleus backend entirely, set `nginx.backends.nucleus.include: false`. This removes the Nucleus server block from the NGINX config. The same `include` flag is available for all backends (S3, Azure, GCS, Nucleus).

### Nucleus values.yaml

```yaml
nginx.backends.nucleus.include: true  # set to false to disable Nucleus backend
nginx.backends.nucleus.serverName: _
nginx.backends.nucleus.proxyCacheName: nucleus
nginx.backends.nucleus.allowCacheReset: true
nginx.backends.nucleus.allowHttp2Clients: false
nginx.backends.nucleus.enableUpstreamKeepalive: false
nginx.backends.nucleus.bridgeResolver: null  # optional resolver override; null = cluster CoreDNS
service.nucleus.port: 14129
persistence.volumes: [{name: nucleus, path: /proxy_cache_nucleus, sizeGi: 2048}]
```

---

## Mutable Mode (requiresVersioning: false)

When a backend's `nginx.backends.<name>.requiresVersioning` is `false`, OVCC caches GET responses by URI without requiring `If-Match` headers. This supports clients that don't send ETags (e.g., S3Provider with SigV4 auth) and package registries.

### Configuration

```yaml
nginx:
  backends:
    s3:
      requiresVersioning: false
```

Set per backend. Defaults to `true`. The removed global `nginx.proxyCache.requiresVersioning` fails helm if still supplied.

Orthogonal with bursty cache (`featureFlags.enableBurstyCache`).

### How it works

1. **Access check** (`lua-access.lua`, access phase): authenticated requests go through `access_upstream_resource()` which forwards credentials to upstream via `GET Range: bytes=0-0`. Public requests (no auth) use `head_upstream_resource()` which sends a lightweight HEAD. Both capture ETag and Last-Modified validators.
2. **Cache serve** (NGINX `proxy_cache`): serves cached content if available.
3. **Mutation detection** (`mutation-detection.conf`, header_filter phase): on a cache HIT, compares the fresh upstream ETag with the cached upstream ETag in `$upstream_http_etag`, falling back to Last-Modified from `http_cache.get_metadata()` when ETag is unavailable. If the validator changed, the entry is purged and the response is **503**.
4. **Client retry**: the client's SDK retries the 503, gets a cache MISS, fetches fresh content from upstream.

### Mutation detection status code: 503

[!] **Must be 503, not 429.** Benchmarked with `aws-sdk-s3`:

| Status Code | aws-sdk-s3 |
|-------------|-----------|
| 429 | **No retry** — error returned to caller (~5ms) |
| 429 + Retry-After: 1 | **No retry** — header ignored (~5ms) |
| 503 | **Retries** with backoff, succeeds (~1.35s) |

429 causes a hard failure visible to the application. 503 is transparently retried by the SDK. The retry penalty only occurs when content has actually changed upstream (cache invalidation event), which is rare.

### 503 and keep-alive

`ngx.exit(503)` in `header_filter_by_lua_block` can leave the HTTP connection in a bad state because the proxy_cache response body was already being streamed. Clients retry on a new connection. The SDK handles this transparently.

### Behavior matrix

| Request type | requiresVersioning=true (default) | requiresVersioning=false |
|---|---|---|
| Presigned S3 with ETag (If-Match) | Cached | Cached (unchanged) |
| Presigned S3 without ETag | Cache BYPASS | Cached, 503 if stale |
| SigV4 Auth header (S3Provider) | Cache BYPASS | Cached, access check every req, 503 if stale |
| Public (no auth) | Cache BYPASS | Cached, HEAD check for Last-Modified, 503 if stale |
| HEAD | Cache BYPASS | Cache BYPASS (unchanged) |

### Monitoring mutable mode

**Prometheus metrics (no additional setup required):**

| Metric | What it tells you |
|--------|-------------------|
| `cc_mutation_detection_purges_total{http_host}` | Cache entries purged due to a stale upstream validator. The primary signal for content mutations. |
| `cc_http_requests_total{cache_status="HIT", http_status="503"}` | Requests where mutation detection fired. Same event as above, visible in existing "Cache Status" panels. |
| `cc_lua_ngx_exit_total{phase="header_filter", http_status="503"}` | All 503 exits from header_filter phase. Equivalent to purge count without the host label. |
| `cc_access_requests_total{http_method="HEAD"}` | HEAD checks to upstream for public requests. Only appears when `requiresVersioning=false`. |
| `cc_access_upstream_request_seconds` | Latency of access checks (both auth GET+Range and public HEAD). |

**JSON access log field:**

`"mutation_detected":"1"` appears in the `cloud_backend_log` format when a stale cache entry is purged. Empty string for normal requests. Use this for log-based alerting or filtering.

**Grafana dashboard:**

The "Mutation Detection" row at the bottom of the OVCC dashboard has two panels: purge rate by host, and access check method distribution (HEAD = public, GET = authenticated).

**Alert recommendations:**

Sustained `cc_mutation_detection_purges_total` rate above baseline indicates frequent content changes upstream. This is expected during deployments but may indicate a problem if it persists (e.g., an origin that rewrites objects on every read, or clock skew causing `Last-Modified` oscillation where no ETag is available).

**ServiceMonitor prerequisite:**

`metrics.prometheus.serviceMonitor.enabled` must be `true` for Prometheus to discover and scrape OVCC's metrics endpoint. The default is `false`. Without it, all OVCC dashboard panels show "No data" even though the metrics endpoint (`/cache_metrics` on port 9145) is healthy and serving data.

---

## Persistence and Storage Class

Each backend has dedicated cache storage defined in `persistence.volumes[]`.
By default `persistence.mode: pvc` creates one PVC per backend per replica.
`persistence.mode: ephemeral` uses `emptyDir` volumes with the same names and
mount paths; cache data is lost when pods or nodes are recreated. Global
default `storageClassName` applies only to PVC mode and can be overridden per
volume.

### Storage Mode

| Mode | Backing | Data survives pod recreate? | Notes |
|------|---------|-----------------------------|-------|
| `pvc` | StatefulSet `volumeClaimTemplates` | Yes, while PVCs remain | Default and upgrade-compatible with older chart versions |
| `ephemeral` | Pod `emptyDir` volumes | No | Useful for local SSD / disposable cache deployments; `sizeGi` becomes `emptyDir.sizeLimit` |

[!] **Ephemeral `min_free` is a flat `10g` reserve, not a percentage of `sizeGi`.** In `pvc` mode NGINX `min_free` is `sizeGi × minFreeSizePercentage` (default 7%). In `ephemeral` mode `sizeGi` is only the `emptyDir.sizeLimit` cap, while the real backing disk (node ephemeral storage) is far smaller (e.g. AWS `c5d.2xlarge` 225GB, Azure `D8ds_v5` 300GiB, GCP 100GB GKE boot disk). A percentage of the oversized default `sizeGi` (2048) would reserve `143g` — more free space than a typical ephemeral disk holds — so NGINX would evict forever and the cache never fills. Ephemeral mode therefore uses a flat `10g` (clamped down to `sizeGi` when `sizeGi < 10`); `minFreeSizePercentage` does not apply.

[!] `helm upgrade` only supports same-mode upgrades. Existing releases with
PVCs or `volumeClaimTemplates` are treated as `pvc`, including releases from
older chart versions that did not have `persistence.mode`. To change modes,
intentionally remove the old release/StatefulSet state and delete the old
cache PVCs. This is required for `pvc` → `ephemeral` cost-saving migrations;
otherwise the old PVCs remain and continue to incur storage cost.

### Storage Class Recommendations

| Cloud | Recommended | Avoid | Notes |
|-------|-------------|-------|-------|
| AWS | `gp3` | `gp2` | gp3: 3000 IOPS / 125 MB/s baseline regardless of size  |
| Azure | `managed-csi-premium-burst` | — | Uses burst credits — monitor consumption |
| GCP | `pd-ssd` | `pd-standard` | Read throughput = `0.48 MB/s × GB`; need ≥2048 GiB for ~1000 MB/s. VM caps apply (see GCP Infrastructure). |

### AWS Storage Config

```yaml
# Create gp3 StorageClass for EKS (default is gp2)
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: managed-csi
provisioner: kubernetes.io/aws-ebs
parameters:
  type: gp3
volumeBindingMode: WaitForFirstConsumer  # [!] Required: ensures EBS in same AZ as pod
```

### GKE Storage Config

```yaml
# Create pd-ssd StorageClass for GKE
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: pd-ssd
provisioner: pd.csi.storage.gke.io
parameters:
  type: pd-ssd
volumeBindingMode: WaitForFirstConsumer  # [!] Required: ensures PD in same zone as pod
allowVolumeExpansion: true
```

Override in values:

```yaml
persistence:
  storageClassName: pd-ssd
```

### PVC Size Guard

[!] **Backend cache compatibility guard:** Helm rendering fails if an enabled backend does not have both a matching `nginx.proxyCache.paths[]` entry and a matching `persistence.volumes[]` entry. Disabled backends may omit either entry, so `nginx.backends.gcs.include=false` with no GCS PVC/path is valid.

[!] **Helm upgrade fails if any existing PVC is smaller than the configured `sizeGi`.** This prevents a dangerous misconfiguration where `minFreeSizePercentage` is computed against the configured size (e.g. 2048Gi) rather than the actual PVC capacity (e.g. 50Gi), causing NGINX to require more free space than the volume can hold.

| Scenario | Behavior |
|----------|----------|
| Upgrade with matching PVC size | Proceeds normally |
| Upgrade with smaller existing PVC | `fail` — upgrade aborted before any resources change |
| Fresh install (no existing PVCs) | Guard skipped — PVCs created at configured size |
| `persistence.mode=ephemeral` | PVC size guard skipped — `emptyDir.sizeLimit` is rendered from `sizeGi`; NGINX `min_free` is a flat `10g` (not `sizeGi × minFreeSizePercentage`) so the reserve fits the smaller ephemeral disk |
| `helm template` / `--dry-run` | Guard skipped (`lookup` returns empty without cluster) |

**Fix when blocked:** Delete the undersized PVC (`kubectl delete pvc <name>`) and re-run `helm upgrade`. The StatefulSet recreates it at the correct size. Alternatively, set `sizeGi` to match the existing PVC if downsizing is intentional.

### General Settings

| Setting | Default | Production | Notes |
|---------|---------|------------|-------|
| `minFreeSizePercentage` | `7` | `7` | NGINX stops caching below threshold |
| `metadataMemorySize` | `1024m` | `2048m` | Too low → premature eviction |
| PVC size | `2048Gi` | `2TiB` | Per backend |

### Single PVC Option

```yaml
# values-single-volume.yaml — consolidate storage
persistence:
  volumes:
    - name: unified-cache
      path: /proxy_cache/data
      sizeGi: 150
nginx:
  proxyCache:
    paths:
      - name: az
        path: /proxy_cache/data/az_cache
      - name: s3
        path: /proxy_cache/data/s3_cache
      - name: nucleus
        path: /proxy_cache/data/nucleus_cache
```

---

## Infrastructure per Cloud

### AWS Infrastructure

| Setting | Recommended | Notes |
|---------|-------------|-------|
| Disk size | ≥ `1024GiB` | Per backend  |
| Metadata memory | ≥ `256m` |  |
| Worker connections | `8192` |  |
| DNS resolver TTL | ≤ `52s` | Match S3 DNS TTL  |

**S3 throttling:**
- **Symptom:** `503 SlowDown` for seconds
- **Diagnosis:** Use `FirstByteLatency` CloudWatch metric to disambiguate from network issues
### Azure Infrastructure

**Worker connections formula:**
```
(Max_SNAT / Num_Pods / Num_vCPUs) * 1.2
```

**SNAT port exhaustion:**
```diff
- useRoundRobinLoadBalancing: false
+ useRoundRobinLoadBalancing: true
```
[why: Avoids Azure LB 5-tuple hash imbalance]
### GCP Infrastructure

**Required NGINX settings:**

| Setting | Value | Reason |
|---------|-------|--------|
| `proxy_ssl_server_name` | `on` | NGINX doesn't send SNI when `proxy_pass` uses variable  |
| `ca-certificates` | installed | Required for outbound HTTPS  |
| Firewall/NetworkPolicy | egress TCP `443` |  |

[!] Without `proxy_ssl_server_name on`: `sslv3 alert handshake failure`

**GCS URL format:**
```
https://storage.googleapis.com/<bucket>
```
(different from AWS `<bucket>.s3.<region>.amazonaws.com`)

**GKE node sizing:**

OVCC performance on GCP is bottlenecked by **NGINX connection queue depth** for hot/warm cache (page cache hits), not CPU, disk, or network bandwidth. Each NGINX worker process (one per vCPU) handles connections sequentially. More workers per replica = lower queue depth = higher per-worker throughput.

**Recommended (empirical, 2026-03-02):**

| Setting | Recommended | Notes |
|---------|-------------|-------|
| Machine type | `c4-highcpu-48` | 48 vCPU (Emerald/Granite Rapids), 96 GiB RAM, 34 Gbps network. 48 NGINX workers/replica = 3× n2-standard-16. |
| Disk type | `hyperdisk-balanced` | Only Hyperdisk supported on c4. Sufficient for warm-cache serving via page cache. |
| Storage class | `hyperdisk-balanced` | Must be created manually — not pre-installed on new GKE clusters. See runbook step 7. |
| PVC size | `2048Gi` | Per backend |
| Metadata memory | ≥ `1024m` | |
| OVCC:worker ratio | **1:34** | `ceil(workers / 34)` replicas. Optimal: 3 replicas for 100 workers → 42.9s vs 79.9s direct (−46%). |
| Zone | `us-west4-a` | C4 CPU quota confirmed. us-west1 had zero C4 quota. |

**Fallback (if c4 unavailable):**

| Setting | Value | Notes |
|---------|-------|-------|
| Machine type | `n2-standard-16` | 16 NGINX workers/replica. Beats direct GCS at 50/150/200w but loses at 100w with 1:50. |
| Disk type | `pd-extreme` | pd-extreme gives better disk throughput than pd-ssd; not available on c4. |
| Storage class | `pd-extreme-rwo` | |
| OVCC:worker ratio | 1:25 for 100w, 1:50 otherwise | |

**Performance comparison (100 workers, persistent model):**

| Machine | Disk | Replicas (ratio) | total\_time (s) | vs Direct GCS (79.9s) |
|---------|------|-----------------|-----------------|----------------------|
| n2-standard-16 | pd-extreme | 2 (1:50) | 83.4 | −7% (slower) |
| n2-standard-16 | pd-extreme | 4 (1:25) | 66.2 | +17% faster |
| **c4-highcpu-48** | hyperdisk-balanced | **3 (1:34)** | **42.9** | **+46% faster** |
| c4-highcpu-48 | hyperdisk-balanced | 4 (1:25) | 51.4 | +36% faster |
| c4-highmem-144 | hyperdisk-balanced | 1 (single node) | 52.5 | +34% faster |
| c4-highcpu-288 | hyperdisk-balanced | 1 (single node) | 54.6 | +8% faster |

**Large-node option (not recommended):** Single large nodes (c4-highmem-144, c4-highcpu-288) handle 100 workers on one node but suffer from single-pod networking contention at higher concurrency. A c4-highcpu-288 with 288 NGINX workers **loses to direct GCS at 200 workers** (72.9s vs 62.7s) despite a queue depth of only 14 — the same queue depth where 3× c4-highcpu-48 achieves 84 Mbps per worker. Multi-replica c4-highcpu-48 at 1:34 outperforms all single-node configurations at every tested worker count and costs less.

[!] `n2-standard-8` caps disk throughput at 800 MB/s regardless of disk size — insufficient for production cache warmup. Use `n2-standard-16` or larger.

[!] `pd-standard` silently caps throughput — no error, just slow cache warmup.

**GCP `pd-ssd` throughput scaling (for n2 fallback):**

Read throughput = `0.48 MB/s × disk_size_GB`, capped at 1,200 MB/s per disk.

| PVC size | pd-ssd read throughput | Sufficient for OVCC |
|----------|------------------------|--------------------|
| 100 GiB | 48 MB/s | No |
| 1024 GiB | 491 MB/s | Marginal |
| 2048 GiB | 983 MB/s | Yes |

**GKE Monitoring:**

GKE uses **PodMonitoring** (`monitoring.googleapis.com/v1`) instead of ServiceMonitor (`monitoring.coreos.com/v1`) for metric scraping. GKE's managed Prometheus collector runs as a DaemonSet in `gmp-system` and is enabled by default on new clusters.

| Setting | Value |
|---------|-------|
| Helm value | `metrics.prometheus.podMonitoring.enabled: true` |
| Template | `deploy/templates/podmonitoring.yaml` |
| Scrape port | `cache-metrics` (9145) |
| Scrape path | `/cache_metrics` |
| Data destination | Google Cloud Monarch (queryable via PromQL in Cloud Monitoring) |

**GCM Dashboard:**

OVCC ships a Google Cloud Monitoring dashboard at `deploy/dashboards/gcloud_dashboard.json`. Apply via:

```bash
gcloud monitoring dashboards create --config-from-file=deploy/dashboards/gcloud_dashboard.json
```

Sections follow the data-path troubleshooting model (not CPU-first):

| Section | Diagnoses |
|---------|-----------|
| **Glance** | Alive/healthy scorecards; CPU is a compact guardrail only |
| **Triage** | Scrape health, client 5xx, read/write/origin quick signals, storage-mode note |
| **Read Path** | Cache, client latency/throughput, burst req/s and egress |
| **Network** | Node RX/TX and per-node top-k saturation (primary read-path evidence) |
| **Storage** | Ephemeral/local cache usage and pod I/O first; PVC capacity and device saturation afterward |
| **Upstream** | Origin connect/header/response latency and upstream request mix |
| **Access** | Upstream authorization probes and mutation checks |
| **System Context** | Node CPU, OVCC container CPU/memory, throttling, unschedulable pods |

Panel scope notes:

- **Universal:** request/cache/upstream families from OVCC pull metrics.
- **Ephemeral/local:** GKE pod-volume usage and cAdvisor throughput/IOPS panels apply to `emptyDir`; verify that the node pool actually backs ephemeral storage with Local SSD.
- **PVC-only:** kubelet/compute disk panels stay empty on ephemeral cache volumes.
- **Burst companions:** 1-minute rate panels sit beside existing 5-minute production rates.
- **Run totals:** cumulative counter panels (`sum(...)`) capture one-shot stress events shorter than the 5s scrape interval.
- **Blocked by design:** TCP_INFO, access mutex, upstream queue time.

The dashboard hardcodes the `cc_` prefix. A UCC-compat deploy (`compat.useUccDefaults=true`) emits `ucc_*` and needs the matching dashboard variant; do not silently broaden queries across both prefixes.

| Attribute | Grafana (`grafana_dashboard.json`) | GCM (`gcloud_dashboard.json`) |
|-----------|-----------------------------------|-------------------------------|
| Widget types | timeseries, heatmap, stat, table | xyChart (LINE), Scorecard, text |
| Variables | `$namespace`, `$cluster`, `$pod` | Not yet implemented |
| Transforms | Client-side (calculateField, groupBy) | PromQL only |
| Deployment | Helm ConfigMap (`dashboard.include: true`) | `gcloud monitoring dashboards create` |

[!] `ServiceMonitor` CRD does not exist on GKE — use `podMonitoring.enabled` instead of `serviceMonitor.enabled`

---

## Operations

### Health Check

- **Endpoint:** `/healthz`
- **Response:** HTTP `200` "healthy"
- **Scope:** All backend server blocks
### Metrics

| Setting | Value |
|---------|-------|
| Port | `cache-metrics` |
| Endpoint | `/cache_metrics` |
| Scrape interval | `5s` |
| Dashboard | `deploy/dashboards/grafana_dashboard.json` |


### Cache Reset (Generation)

**Endpoint:** `POST /__/cache/reset`

| Param | Behavior |
|-------|----------|
| No `gen` | Returns current generation |
| `gen=<N>` where N > current | Bumps generation (monotonic) |
| `gen=<N>` where N ≤ current | Rejected |

**Response fields:** `instance`, `generation`, `old_generation` (optional), `error` (optional)

**Header:** `CC-Instance` (plus `UCC-Instance` when `compat.emitLegacyCacheReset` is true; default `true`)

**Key behaviors:**
- **Logical invalidation only** — no physical purge [why: Generation embedded in cache key]
- **Per-instance storage** — each replica stores generation in `lua_shared_dict` (eventually consistent)
- **Gated by:** `nginx.backends.<name>.allowCacheReset`

### Cache Lock (Thundering Herd Prevention)

| Backend | `lock_age` | `lock_timeout` |
|---------|------------|----------------|
| Cloud (S3/Azure/GCS) | `60s` | `120s` |
| Nucleus | `10s` | `3s` |

`lua-lock.lua` ensures only one request populates a cache key while others wait
### Cache Reset Algorithm

```text
# Phase 1: Discover current generation across instances
deadline = now() + 10s
instances = {}
max_gen = 0
while now() < deadline and len(instances) < expected_replicas:
    r = POST /__/cache/reset (Header: Connection=close)
    j = parse_json(r.body)
    inst = j["instance"] or r.headers["CC-Instance"]
    g = int(j["generation"])
    instances[inst] = g
    if g > max_gen: max_gen = g

# Phase 2: Bump to max + 1
g1 = max_gen + 1
seen_instances = {}
while now() < deadline and len(seen_instances) < expected_replicas:
    r = POST /__/cache/reset?gen=g1 (Header: Connection=close)
    j = parse_json(r.body)
    if j["generation"] == str(g1):
        seen_instances.add(j["instance"])
    # else: retry on "generation_not_updated" or lock error

# Phase 3 (optional): Verify MISS on cacheable GET
```

### Logging

**Format by backend:**

| Backend | Log Format |
|---------|------------|
| Default/fallback | `default_log` |
| S3, Azure, GCS | `cloud_backend_log` |
| Nucleus | `nucleus_log` |

**Field matrix:**

| Field Group | `default_log` | `cloud_backend_log` | `nucleus_log` |
|-------------|:-------------:|:-------------------:|:-------------:|
| Basic fields | Yes | Yes | Yes |
| Extended fields | No | Yes | Yes |
| OpenTelemetry | Yes | Yes | No |
| Nucleus-specific | No | No | Yes |

**Basic fields:** `access_time`, `remote_addr`, `request`, `status`, `bytes_sent`, `host`, `upstream_cache_status`, `connection`, `connection_requests`, `request_uri`, `etag`, `request_range`, `has_auth_header`, `request_time`, `http_trace_id`, `http_request_id`

**Extended fields:** `http_user_agent`, `proxy_host`, `upstream`, `upstream_status`, `ssl_protocol`, `upstream_http_location`, `upstream_response_time`, `disable_cache`

**OpenTelemetry fields:** `otel_trace_id`, `otel_span_id`, `otel_parent_id`

**Nucleus-specific fields:** `http_omniverse_content_uid`, `http_x_ov_uri`, `http_omniverse_proxy_scheme`

### Logging Config

| Setting | Default | Notes |
|---------|---------|-------|
| `nginx.logging.buffer` | `4m` | Trade-off: delay vs I/O |
| `nginx.logging.flushInterval` | `15s` | |
| Log entry size | ~1.2KiB | Per request  |

### Logging Security

[!] **NEVER** log auth headers — only log presence via `has_auth_header`
### Logging Gotchas

[!] `upstream_queue_time` is **NGINX Plus only** — causes `nginx: [emerg]` crash in open-source NGINX
[!] Multiple replicas with short flush intervals can interleave buffer flushes → corrupted JSON log lines

### Performance Characteristics

Measured with synthetic workloads (5,000 files, 48% small/52% large, sizes from 1KB-64MB based on production S3 bucket distribution).

| Scenario | Throughput | P50 Latency | Notes |
|----------|-----------|-------------|-------|
| Cache HIT (5k working set) | ~11,600 req/s | 9ms | HEAD + small GET mix |
| Cache HIT (10k working set) | ~7,000 req/s | 10ms | Larger working set reduces throughput |
| Cache MISS (first access) | ~100 req/s | 46-73ms | Includes backend fetch + cache write |
| Direct backend (no OVCC) | ~155 req/s | 240ms | Curl HTTPS baseline for large files |

Key behaviors:
- Cache hit performance scales inversely with working set size (5k files = 66% higher throughput than 10k files)
- Cache miss adds ~35% overhead over direct backend access due to proxy processing and cache writes
- TLS adds 31-40% overhead vs plain HTTP for backend connections (expected; uses system CAs)
- Cache hit rate reaches 100% in steady state when working set fits in configured cache volume

---

## Conventions

### Configuration

- **values.yaml is the only external interface** — override via `-f` or `--set`
- [!] **DO NOT** edit templates for config changes
- [!] **DO NOT** hardcode storage classes, image tags, or environment-specific values in templates

### Logging Rules

- [!] **DO NOT** add auth headers to log formats — only log presence (`has_auth_header`)
- [!] **DO NOT** add `upstream_queue_time` — NGINX Plus only, causes crash

### Versioning (Chart vs Container)

OVCC uses two version numbers in `Chart.yaml`:

| Field | What it versions | When it changes |
|-------|------------------|-----------------|
| `version` | Helm chart (configs, templates, Lua scripts) | Any change to `deploy/` |
| `appVersion` | Container image (OpenResty binary, libs) | Dockerfile or dependency changes |

**Why this matters:**
- NGINX configs and Lua scripts are **not baked into the container** — they're mounted via ConfigMaps from the Helm chart
- Most releases only bump `version` (chart) because they're config/Lua changes
- `appVersion` changes are rare (3-6 months) and trigger container rebuilds

**Checking versions:**
```bash
helm show chart <chart-path> | grep -E '^(app)?[Vv]ersion'
```
