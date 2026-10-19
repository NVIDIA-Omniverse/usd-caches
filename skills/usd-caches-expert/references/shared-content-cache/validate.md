Up: [shared content cache](README.md)

# Validating an OVCC deployment

Commands assume release `ovcc` (StatefulSet `ovcc`) in namespace `ovcc`. With another release name `R`, substitute `R-ovcc`. Set the namespace first:

```bash
NS=ovcc   # the namespace you installed into
```

 The chart ships **no `helm test` hooks**: there are no `helm.sh/hook: test` templates in `deploy/templates/`. It also has no conformance suite yet ("not published yet", `shared-content-cache/conformance/README.md`). The checks below are manual.

## 1. Render and install output

- [ ] `helm upgrade --install ovcc omniverse/ovcontentcache -n "$NS" -f values-s3.yaml --dry-run=server` completes, with your own override file. Server-side dry run is needed to exercise the `lookup` guards: PVC size, storage mode, UCC upgrade.
- [ ] NOTES output shows no `WARNING:` lines (`deploy/templates/NOTES.txt`).
- [ ] `helm get values ovcc -n $NS` matches the intended overrides. In particular, `image.pullSecrets` and `persistence.storageClassName` should not be left at the defaults (`gitlab-registry-secret`, Azure class) on another cloud.

## 2. Workload ready

```bash
kubectl -n $NS rollout status statefulset/ovcc --timeout=10m
kubectl -n $NS get pods -l app.kubernetes.io/instance=ovcc -o wide     # READY 1/1 for every ordinal
kubectl -n $NS get svc,pvc                                              # ovcc, ovcc-nucleus, ovcc-metrics; PVCs Bound
kubectl -n $NS describe pod ovcc-0 | grep -A3 -E 'Readiness|Liveness'   # httpGet /healthz :healthz(13129)
```

- [ ] Each PVC `<volume>-ovcc-<n>` is `Bound` with the requested `sizeGi`, in the same zone as the pod.
- [ ] Image pulled. `ImagePullBackOff` means the pull secret is wrong or missing.
- [ ] The container is distroless, so `kubectl exec` is unavailable. Use logs, ConfigMaps, and an external curl pod for debugging ([AGENTS.md Azure Debugging](../../../../shared-content-cache/reference/deploy/AGENTS.md#azure-debugging)).

## 3. Rendered config sanity

```bash
kubectl -n $NS get cm ovcc-nginx-conf -o jsonpath='{.data.nginx\.conf}' | grep -E 'proxy_cache_path|keys_zone|min_free|worker_connections'
kubectl -n $NS get cm ovcc-runtime -o yaml        # cacheResetPath, instanceHeader, metricPrefix, tlsSecretName
kubectl -n $NS get cm ovcc-lua -o jsonpath='{.data}' | head -c 200
```

- [ ] There is one `proxy_cache_path` per enabled backend. `min_free` = `max(int(sizeGi × minFreeSizePercentage / 100), 1)`g with a 7% default (PVC mode), or `min(10, sizeGi)`g (ephemeral) (`_helpers.tpl` `func.cacheMinFreeGi`).
- [ ] `metricPrefix` is `cc_`, or `ucc_` under compat. Dashboards must match.

## 4. Health and metrics

```bash
kubectl -n $NS port-forward pod/ovcc-0 13129:13129 9145:9145 &
curl -s http://127.0.0.1:13129/healthz                                # healthy
curl -s http://127.0.0.1:9145/cache_metrics | grep -E '^cc_instance_info'   # cc_instance_info{chart_version="4.0.4"} 1
```

- [ ] `cc_instance_info{chart_version=…}` matches the expected chart version. It is set in `nginx.conf` `init_worker_by_lua_block`.
- [ ] Prometheus or GMP actually scrapes. With `serviceMonitor.enabled=false` (the default), dashboards show "No data" even though the endpoint is healthy.

## 5. MISS → HIT end to end

Use the curl recipes in [api-http.md](api-http.md#runnable-curl-examples). For each enabled backend:

- [ ] First request with `If-Match` shows `X-Cache-Status: MISS`; the same request again shows `HIT`.
- [ ] A different presigned URL (new signature) for the same object and ETag shows `HIT`.
- [ ] A ranged request is `MISS` then `HIT`, with status `206` and the correct `Content-Range`.
- [ ] A request without `If-Match` shows `BYPASS` (when `requiresVersioning: true`).
- [ ] A `Host` equal to the Service name returns `421`. A typo'd storage host has no `X-Cache-Status` (passthrough).
- [ ] Metrics move: `cc_http_requests_total{cache_status="HIT"}` increases, and `cc_http_upstream_requests_total` does not increase on HITs.
- [ ] To prove no S3 traffic at all, also check `cc_access_requests_total`. A HIT can still send a 1-byte authorization probe (`Range: bytes=0-0`) to S3: on the first use of each presigned URL, on every request authorized by an `Authorization` header, and as a `HEAD` on every request when `requiresVersioning: false` (`files/lua/lua-access.lua`). The access log's empty `upstream` and `upstream_connect_time` fields confirm a HIT made no body fetch.

```bash
curl -s http://127.0.0.1:9145/cache_metrics | grep -E '^cc_http_requests_total' | sort
```

Metric names (prefix `cc_`, from `deploy/files/lua/lua-metrics.lua`):
- `http_requests_total{http_host,request_method,cache_status,http_status}` and `http_upstream_requests_total`
- `access_requests_total`, `access_responses_total`, `access_upstream_request_seconds`, `access_handler_seconds`, `access_mutex_*`
- `mutation_detection_purges_total{http_host}`
- `request_duration_seconds`, `response_size_bytes`, `upstream_{connect,header,response,queue}_time_seconds`, `upstream_{rx,tx}_bytes`, `rx_bytes_total`, `tx_bytes_total`
- `tcpinfo_*`, `nginx_http_connections{state}`, `nginx_shared_dict_info{name,stat}`, `nginx_workers`, `metric_poll_total`, `lua_exception_total`, `lua_ngx_exit_total{phase,http_status}`

## 6. Logs

```bash
kubectl -n $NS logs ovcc-0 --tail=200 | grep '"upstream_cache_status"' | head
```

- [ ] JSON lines show `upstream_cache_status`, `has_auth_header` (0/1; tokens are never logged), `etag` (= the request `If-Match`), `request_range`, `bursty_cache`, `mutation_detected`, and `pod_name` ([AGENTS.md Logging](../../../../shared-content-cache/reference/deploy/AGENTS.md#logging)).
- [ ] No `nginx: [emerg]` lines and no Lua `❌` errors, such as "failed to resolve upstream peer for keepalive" (`_helpers.tpl` `func.upstreamKeepaliveResolve`).
- [ ] Log lines appear in batches, up to `nginx.logging.flushInterval` (15 s) late. This is expected.

## 7. Storage

- [ ] Cache usage grows on the volume. Use kubelet volume stats (`kubelet_volume_stats_used_bytes{persistentvolumeclaim=~".*ovcc.*"}`), which the shipped dashboard regex expects, or the GCM dashboard Storage section.
- [ ] Usage plateaus below `sizeGi − min_free`. It must not hover at the reserve with constant eviction. In ephemeral mode, check the real node disk size.

## 8. Multi-replica and reset

- [ ] Each replica warms independently. Expect per-pod MISS on first access.
- [ ] Cache reset: run the two-phase algorithm against every pod and confirm every `instance` reports the new `generation`. A following GET is a `MISS` ([AGENTS.md Cache Reset Algorithm](../../../../shared-content-cache/reference/deploy/AGENTS.md#cache-reset-algorithm)).
- [ ] With `enableSessionAffinity`, `sessionAffinityTimeoutSeconds ≥ burstyCache.ttlSec`.
