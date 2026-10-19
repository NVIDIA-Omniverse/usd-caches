Up: [shared content cache](README.md)

# OVCC HTTP client contract

OVCC is a transparent HTTP(S) reverse proxy. It has no OVCC-specific request API and no OpenAPI or Protobuf spec (`shared-content-cache/api/README.md`). A client sends the **same request it would send to the origin**, including its signature, but connects to the OVCC Service. The `Host` header (and SNI) selects the backend. Sources: `deploy/files/backends/*.conf`, `deploy/files/lua/lua-access.lua`, `deploy/files/cache-bypass.conf`, [AGENTS.md Request Routing](../../../../shared-content-cache/reference/deploy/AGENTS.md#request-routing).

## Addressing per backend

| Backend | Connect to | `Host` must match | Auth OVCC understands |
|---|---|---|---|
| S3 | `<fullname>.<ns>.svc:14128` | `<bucket>.s3.<region>.amazonaws.com` (virtual-hosted, regional, no dots in bucket) | SigV4 presigned query (`X-Amz-Date`, `X-Amz-Expires`, `X-Amz-Signature`), or `Authorization` header (probed per request) |
| Azure | `:14128` | `<account>.blob.core.windows.net` | SAS query (`st`, `se`, `sig`), or `Authorization` header |
| GCS | `:14128` | `storage.googleapis.com` or `<bucket>.storage.googleapis.com` | HMAC SigV4-style presigned (`X-Amz-*`); no OAuth2 |
| Nucleus | `<fullname>-nucleus.<ns>.svc:14129` | anything (catch-all) | `/omni/lft`: `Omni-Tenant-Token` forwarded (JWT for Nucleus Bridge). `/`: `Authorization` → **cache BYPASS** |
| other host on 14128 | `:14128` | anything else | Passthrough proxy to `$scheme://$http_host`, never cached |
| OVCC's own name | `:14128`/`:14129` | `<fullname>[.<ns>[.svc[.cluster.local]]]` or `selfServer.extraNames` | **421 Misdirected Request**. Do not put the Service name in `Host`. |

Two ways to route clients ([AGENTS.md S3 Client Access Patterns](../../../../shared-content-cache/reference/deploy/AGENTS.md#s3-client-access-patterns)):
1. Transparent: cluster DNS resolves the storage hostname to OVCC. The client is unchanged. The client must reach OVCC on 14128, so either the client uses that port or something maps 443 → 14128. The source does not include such a DNS or port mapping.
2. Opt-in: the SDK connects to the OVCC Service URL but keeps the signed origin `Host`. Example: a boto3 `before-send.s3` hook that rewrites only the connection URL. TLS then verifies the OVCC name, so put it in the cert SANs and trust its CA, or use `verify=False` in tests only.

`Host` is stripped of its port before `server_name` matching. Upstream requests use `Host: $host`, without the port. SigV4 and SAS signatures therefore stay valid when the signature was computed for the origin host on its default port.

## Cacheability rules (standard path)

| Condition | Result |
|---|---|
| GET with `If-Match: <ETag>` and valid auth (S3/Azure/GCS, `requiresVersioning: true`) | Cached. The key includes `If-Match` and `Range`. |
| GET without `If-Match` (`requiresVersioning: true`) | BYPASS |
| GET without `If-Match` (`requiresVersioning: false`) | Cached by URI; access probe on every request; **503** + purge if the origin ETag or Last-Modified changed. Retry. |
| `HEAD` | BYPASS on the standard path; served from the bursty cache if bursty is enabled (default rule `HEAD /`) |
| `Cache-Control: no-cache` | BYPASS (standard path only) |
| Azure `x-ms-encryption-key`/`-sha256`/`-algorithm`, `x-ms-if-tags`, `x-ms-lease-id` | BYPASS (CPK responses also `proxy_no_cache`) |
| Nucleus `/` with `Authorization` | BYPASS |
| S3 presigned with a bad or missing date/expiry window | **400** from OVCC Lua |
| Access probe returns 403 | 403 relayed (expired or invalid signature) |
| LIST (`/?list-type=` S3/GCS, `/?comp=list` Azure) with bursty enabled | Cached for `burstyCache.ttlSec`, keyed per identity (`nvcf-function-id`/`-version`, `nvcf-nca-id`, or md5 of `Authorization`) |

Range: `Range` (and Azure `x-ms-range`) is forwarded and is part of the key. Each distinct range is its own entry, and a 206 is cached for `validity.206`. Conditional headers `If-Match`, `If-Modified-Since`, `If-None-Match`, `If-Unmodified-Since`, `x-ms-if-tags`, and `x-ms-lease-id` are forwarded upstream (`func.upstreamCacheRequestHeaders`).

## Headers

| Header | Direction | Meaning | Source |
|---|---|---|---|
| `X-Cache-Status` | response | NGINX `$upstream_cache_status`: `MISS`, `HIT`, `BYPASS`, `EXPIRED`, `STALE`, `UPDATING`, `REVALIDATED`. Absent when nothing was cached or proxied (e.g. passthrough, local `return`). | `nginx.conf` `add_header … always`; Nucleus locations |
| `CC-Instance` (+ `UCC-Instance` when `compat.emitLegacyCacheReset`) | response (reset endpoint only) | Pod hostname that served the reset call | `lua-generation.lua` |
| `If-Match` | request | Required for caching on version-gated backends | `cache-bypass.conf` |
| `nvcf-function-id`, `nvcf-function-version`, `nvcf-nca-id` | request | Bursty-cache identity; stripped before upstream. `nvcf-nca-id` is also in Nucleus keys. | backend confs |
| `Omniverse-Content-Uid`, `X-OV-URI`, `Omniverse-Proxy-Scheme`, `Omni-Tenant-Token` | request (Nucleus) | Forwarded on `/omni/lft`. `Content-Uid` is part of the key. Tenant token is stripped on `/`. | `nucleus-server.conf` |
| `Access-Control-Allow-{Origin,Headers,Methods}: *` | response (`/omni/lft`) | CORS | `nucleus-server.conf` |

Azure limitation: a HIT may replay the MISS's `x-ms-request-id`/`x-ms-client-request-id` ([AGENTS.md Azure Known Limitations](../../../../shared-content-cache/reference/deploy/AGENTS.md#azure-known-limitations)).

## Operational endpoints

| Endpoint | Port | Notes |
|---|---|---|
| `GET /healthz` | `13129` (plaintext, probes); also on every backend server and passthrough | `200 healthy` |
| `GET /cache_metrics` | `9145` (plaintext) | Prometheus text. `/metrics` returns 404. `/nginx_status` allowed only from 127.0.0.1. |
| `/__/cache/reset` (+ `/__ucc/cache/reset`) | backend ports, only in servers with `allowCacheReset: true` (default: Nucleus only, port 14129) | See below |

### Cache reset (generation)

`lua-generation.lua` (any HTTP method is accepted; `POST` is documented). The response is always HTTP 200 JSON: `{"instance","generation","old_generation"?,"error"?}`.

- No `gen`: returns the current generation (default `"0"`).
- `gen=N` with N > current: sets it. All later keys include the new generation, so the next request is a MISS.
- `gen=N` with N ≤ current: `error: "generation_not_updated"`.
- Non-numeric: `error: "invalid_generation"`.
- Lock failure: `error: "execution_failed"` or `"lock_error"`.

The generation is **per pod** (`lua_shared_dict cc_generation`) and is logical only, with no disk purge. To reset a multi-replica deployment, read every replica, then set `max+1` on each, using `Connection: close` so the Service spreads calls. You can also port-forward each pod ([AGENTS.md Cache Reset Algorithm](../../../../shared-content-cache/reference/deploy/AGENTS.md#cache-reset-algorithm)). A pod restart does not persist the generation: the shared dict resets to `0`. This is not stated in `AGENTS.md`; it is inferred from `lua_shared_dict` semantics.

## Runnable curl examples

These assume `release=ovcc`, `namespace=ovcc`, TLS on, and a self-signed cert (hence `-k`). `--connect-to` keeps the origin `Host` and SNI while dialing the port-forward. `kubectl port-forward svc/...` pins **one pod**, so all calls hit the same replica.

```bash
NS=ovcc
kubectl -n $NS port-forward svc/ovcc 14128:14128 &           # cloud backends
kubectl -n $NS port-forward svc/ovcc-nucleus 14129:14129 &   # nucleus (if included)
kubectl -n $NS port-forward pod/ovcc-0 13129:13129 9145:9145 &   # health + metrics

curl -s http://127.0.0.1:13129/healthz                       # healthy
curl -s http://127.0.0.1:9145/cache_metrics | grep -E '^cc_(instance_info|http_requests_total)'
```

### S3: MISS then HIT, range, bypass

```bash
BUCKET=my-bucket KEY=assets/scene.usd REGION=us-west-2
URL=$(aws s3 presign "s3://$BUCKET/$KEY" --region "$REGION" --expires-in 3600)
HOST=$(printf '%s' "$URL" | sed -E 's#^https://([^/]+)/.*#\1#')     # must be $BUCKET.s3.$REGION.amazonaws.com
ETAG=$(aws s3api head-object --bucket "$BUCKET" --key "$KEY" --region "$REGION" --query ETag --output text)

ovcc() { curl -sk --connect-to "$HOST:443:127.0.0.1:14128" -o /dev/null -D - "$@" "$URL" \
          | grep -iE '^(HTTP/|x-cache-status|etag|content-range|content-length)'; }

ovcc -H "If-Match: $ETAG"                       # X-Cache-Status: MISS  (origin fetch, written to cache)
ovcc -H "If-Match: $ETAG"                       # X-Cache-Status: HIT
ovcc -H "If-Match: $ETAG" -r 0-1048575          # 206, MISS (Range is part of the key)
ovcc -H "If-Match: $ETAG" -r 0-1048575          # 206, HIT
ovcc                                            # X-Cache-Status: BYPASS (no If-Match, requiresVersioning=true)
ovcc -H "If-Match: $ETAG" -H 'Cache-Control: no-cache'   # BYPASS
```

If `aws s3 presign` returns `https://<bucket>.s3.amazonaws.com/...` (us-east-1 global endpoint), it does not match the default S3 `serverName` and falls to passthrough (no `X-Cache-Status`). Presign against the regional endpoint, or widen `nginx.backends.s3.serverName`. A new presigned URL for the same object and ETag is still a HIT, because signature params are excluded from the key, but its first request sends one authorization probe to S3 and increments `cc_access_requests_total` (`files/lua/lua-access.lua`).

### Azure: SAS

```bash
ACCOUNT=myacct CONTAINER=assets BLOB=scene.usd
# GNU date (Linux) first, BSD date (macOS) as the fallback.
EXPIRY=$(date -u -d '+1 hour' +%Y-%m-%dT%H:%MZ 2>/dev/null || date -u -v+1H +%Y-%m-%dT%H:%MZ)
SAS=$(az storage blob generate-sas --account-name $ACCOUNT -c $CONTAINER -n $BLOB \
        --permissions r --expiry "$EXPIRY" -o tsv)
URL="https://$ACCOUNT.blob.core.windows.net/$CONTAINER/$BLOB?$SAS"
ETAG=$(az storage blob show --account-name $ACCOUNT -c $CONTAINER -n $BLOB --query properties.etag -o tsv)
curl -sk --connect-to "$ACCOUNT.blob.core.windows.net:443:127.0.0.1:14128" \
  -H "If-Match: $ETAG" -o /dev/null -D - "$URL" | grep -i x-cache-status    # MISS, then HIT on repeat
```

The SAS must include `st` and `se`. A missing or invalid window returns 400.

### Nucleus

```bash
NHOST=nucleus.example.com   # real Nucleus host; any Host lands on the catch-all server
NPATH=path/to/asset.usd     # a path the Nucleus server serves
curl -sk --connect-to "$NHOST:443:127.0.0.1:14129" -o /dev/null -D - \
  "https://$NHOST/$NPATH" | grep -i x-cache-status       # MISS, then HIT (no Authorization header)
curl -sk --connect-to "$NHOST:443:127.0.0.1:14129" -o /dev/null -D - \
  -H "Authorization: Bearer x" "https://$NHOST/$NPATH" | grep -i x-cache-status   # BYPASS (multi-tenant safety)
```

`$scheme` is proxied as-is, so with TLS on, OVCC calls `https://$NHOST`. The exact `/omni/lft` request format (`Omniverse-Content-Uid`, `X-OV-URI` values) is defined by the Omniverse client library, not by this source.

### Cache reset (Nucleus port, single pod)

```bash
curl -sk -X POST https://127.0.0.1:14129/__/cache/reset -D - | grep -iE 'cc-instance|^\{'
# {"instance":"ovcc-0","generation":"0"}
curl -sk -X POST "https://127.0.0.1:14129/__/cache/reset?gen=1"
# {"instance":"ovcc-0","generation":"1","old_generation":"0"}
```

Host `127.0.0.1` lands on the Nucleus catch-all, which has reset enabled by default. For S3/Azure/GCS hosts, set `nginx.backends.<name>.allowCacheReset: true`. The generation dict is shared by all servers in the pod, so one reset invalidates every backend on that pod.
