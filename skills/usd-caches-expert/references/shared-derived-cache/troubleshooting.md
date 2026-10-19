Up: [shared derived-data cache](README.md)

# Troubleshooting OVDC

`ref/` means `shared-derived-cache/reference/`. Each entry gives the symptom, the cause, and the fix. Only causes supported by source are listed. The published runbooks (cache misses, no active shards, disk exhaustion, network bottlenecks, RocksDB corruption) are mapped at the end, with what source does and does not say about each.

## Writes slow down or fail (write stalls)

| Symptom | Cause | Fix |
|---|---|---|
| PUT latency climbs, then PUTs fail `ABORTED "put failed"` / `INTERNAL "write failure"`. Logs show `"write failed because the database is busy"`. `ovdc_api_writes_waiting` and `ovdc_api_write_queue_time` rise. | RocksDB back-pressure: flush or compaction cannot retire writes. The server retries `Busy` 60 times with exponential sleeps, then fails (`ref/rust/ovdc/src/grpc/put.rs`). | Clients back off and retry, or regenerate (`ref/README.md` "Write Stalls"). Then work through the trigger-specific rows below. |
| `__rocksdb_stalls_{stops,delays}_memtable_limit` > 0 | Memtables reached `cf.max_write_buffer_number` (or `blobsCf.*`) faster than flushes complete | Faster storage, more flush threads (`engine.threadsHigh`, `db.max_background_jobs`). Larger or more memtables only buy burst headroom (`ref/README.md`). |
| `__rocksdb_stalls_{stops,delays}_l0_file_count_limit` > 0 | L0→L1 compaction is behind (`level0_stop_writes_trigger`, 24) | More compaction threads (`engine.threadsLow`, `presets.cpu`), `db.max_subcompactions`, faster storage |
| `__rocksdb_stalls_{stops,delays}_pending_compaction_bytes` > 0 | Lower levels are behind the soft or hard pending-compaction limits | Same as above. This is the signature of under-provisioned disk throughput. |
| Stalls mostly on `cf="blobs"` | Large values land in `blobs`, which "is generally the one that stalls first" | Tune `blobsCf.*`. `presets.ram` ≥5GB raises `blobsCf.max_write_buffer_number` to 32 |
| Fine for about an hour, then constant stalls | CSP volume burst credits exhausted | Provision for **sustained ≥800 MB/s** per PVC. On AKS (~150 MB/s burst per volume), consider several replicas per node (`ref/README.md` "Storage Throughput Requirements"; `values.yaml` `replicas` comment) |
| Stalls persist whatever the tuning | Sustained ingest is above what storage can absorb. Write amplification is about 60× at defaults. | Faster storage, more background threads, or less ingest. These are the only levers that change the sustainable rate (`ref/README.md`). |

## Cache misses (GET returns nothing)

| Symptom | Cause | Fix |
|---|---|---|
| Key written, GET empty from another client | Different **tenant**: `tenant` metadata or JWT `ncaId` differs, or one side sends none (default tenant) (`ref/rust/ovdc/src/tests.rs` `test_tenant_set`) | Send the same `tenant` or the same token identity on every call |
| GET empty after scaling or with multiple replicas | Each pod is an independent store, and the client hit a different pod than the writer. The server does no forwarding (`ref/rust/ovdc/src/lib.rs`; see [overview.md](overview.md#multi-node-behavior)). | Make the client map keys to pods deterministically. Scaling changes the pod set, and the effect on the mapping is client-specific and not in this repo. |
| Keys disappear well before TTL | GC eviction: free space fell to `minFreeCapacity` and a pass removed about `deleteKeyspaceQuantile`% of keys, oldest access first. Logs show `"db max capacity reached…"`. | Raise `store.maxSize`, keeping it below the PVC size, and restart the pods (`kubectl rollout restart`): the server reads it only at startup, a settings-only upgrade doesn't roll pods, and an existing DB accepts the new value. With `AUTO` it follows `storage.volume.size`, which can't grow on existing PVCs (`volumeClaimTemplates` are immutable). Also lower `deleteKeyspaceQuantile`, and `Touch` keys still in use. Local configs: the reference `config.toml` has `maxSize = "32Mi"`. |
| Keys gone after a pod restart | `storage.volume.enabled: false` (emptyDir), or `Low` durability writes not yet flushed at crash | Enable the PVC. Use `durability: Normal` for important single-value writes (the chunked path ignores it) |
| Expired keys still take space | Expired keys are only removed by GC (`ref/README.md`) | Expected behavior |
| GET ends with `ABORTED "value changed"` | Concurrent in-progress write of that key | Retry, or treat as a miss |
| GET returns wrong or shifted `tag`s | A key longer than 32 bytes, or an empty `Key`, was silently dropped and indices shifted (`ref/rust/ovdc/src/grpc/mod.rs` `convert_proto_keys`) | Validate key length client-side |
| Two different short keys collide | Zero-padding: `b"x"` == `b"x\0"` (`ref/rust/ovdc_bytes/src/keys.rs`) | Use fixed-length keys such as hashes |
| Hit rate low and falling as data grows | Index and filter working set no longer fits in the block cache. Today `blockCacheSize` sizes only the write-buffer-manager cache, not the table cache (`ref/README.md` "Block Cache") | Watch `__rocksdb_optional_block_cache_{hit,miss}`. Raising `blockCacheSize` will not help reads as currently wired; this is a source limitation. |

## Disk exhaustion

| Symptom | Cause | Fix |
|---|---|---|
| PVC full, pod failing writes | `store.maxSize` set explicitly too close to the volume size, since nothing validates it, or the DB "may temporarily grow larger" under heavy writes (`values.yaml`) | Use `maxSize: AUTO` (volume − min(20%, 100 GB)), or leave larger headroom |
| GC runs constantly | `maxSize` too small for the working set | A bigger volume. `storage.volume.size` is immutable on an existing release, so reinstall or migrate (`ref/docker/helm/README.md`) |
| `helm upgrade` rejected after changing `storage.volume.size` | `volumeClaimTemplates` are immutable | Reinstall with the new size. Expanding PVCs out of band is standard Kubernetes and not covered by this chart. |
| Space not reclaimed after deletes | LSM: deletes are appends, reclaimed only when compaction reaches the bottom level. Emptied rows in `log` mode are dropped only at the bottommost level; `periodic_compaction_seconds` walks them down (`ref/README.md` "What OVDC configures") | Keep `cf.periodic_compaction_seconds` (28800). Do not raise `cf.num_levels` without reason |

## Network bottlenecks

| Symptom | Cause | Fix |
|---|---|---|
| Large GET or PUT slow over high-latency links | HTTP/2 flow-control windows (`grpc.initialStreamWindowSize` 512K, `initialConnectionWindowSize` 32M) | Raise the windows (startup-only; needs a restart) |
| PUT rejected with a message-size decode error from tonic (exact code not pinned in this repo) | A single message is larger than `grpc.maxDecodingMessageSize` (5M) | Split values into ≤1–4 MB `value` messages (`rpc.proto` comment) |
| Streams aborted with `"write timeout"` | The client is not reading GET responses fast enough (`grpc.writeTimeout`, 15 s) | Read promptly, or raise `writeTimeout` |
| Idle connections dropped | Keepalive settings (`tcpKeepalive*`, `http2Keepalive*`, 3 s/20 s) | Tune to your network |
| External clients cannot reach pods | No per-pod exposure | Set `service.loadBalancer: true` (one LB per pod, `externalTrafficPolicy: Local`). `loadBalancerIP` only works with 1 replica. |

The source has no NIC-saturation metric beyond `ovdc_api_tcp_{rx,tx}_total` and the gRPC size histograms. The bundled dashboard uses cAdvisor `container_network_*`.

## Startup and configuration failures

| Symptom | Cause | Fix |
|---|---|---|
| CrashLoopBackOff right after `helm upgrade`, with a `"failed to deserialize subtable …"` or `"invalid settings file"` panic | The reloaded or initial `config.toml` failed validation. Release builds abort on panic (`ref/rust/ovdc_settings/src/lib.rs`; `rust/Cargo.toml` `panic='abort'`). | Fix the values. Render with `helm template` and inspect the ConfigMap, or use `debug: true` |
| Panic at DB open | Unknown or malformed RocksDB option in `engine.db/cf/block/blobsCf/blobsBlock`, which are passed verbatim (`values.yaml`) | Remove or fix the option |
| `helm template` error `additional properties 'X' not allowed` | Closed schema, for example removed keys `service.grpcPort`, `otelCollector.*`, `settings.grpcTls.cert` | See [deploy.md](deploy.md#4-upgrade) |
| Render error `settings.store.maxSize: "AUTO" requires storage.volume.enabled=true` | AUTO without a volume | Set an explicit `maxSize` |
| Container cannot write `/env` | `runAsUser`/`runAsGroup` changed without `fsGroup` (`ref/docker/helm/README.md` "Running as non-root") | Change all three together |
| Pods 0/1 with readiness and liveness `connection refused` on 3010, and a `<fullname>-debug` pod running | `debug: true` | Set `debug: false`, then delete the StatefulSet pods |
| `IP env is not an ip address` panic | A non-IP `IP` env var (`main.rs`) | Unset it or fix it. It is unused anyway. |
| Settings change has no effect | It is a startup-only setting, or a log/OTel setting (not hot-reloaded in this source), or the ConfigMap changed without rolling pods | `kubectl rollout restart statefulset/<fullname>`. See [configuration-server.md](configuration-server.md#hot-reload) |

## TLS and JWT

| Symptom | Cause | Fix |
|---|---|---|
| Render error `settings.grpcTls.secretName is required…` | TLS enabled without a secret | Create the secret and set `secretName` |
| Pod panics `"unable to load certificates"` / `"failed to open cert"` | Missing or invalid `tls.crt`/`tls.key`, or a missing `ca.crt` with `includeCaRoot` | Recreate the secret with the right keys ([deploy.md](deploy.md#7-tls)) |
| Rotated cert not picked up immediately | Re-read every 300 s | Wait, or restart |
| `INVALID_ARGUMENT "authorization: no key was available to validate the jwt"` | Empty JWKS (Helm default `jwkPublicKeysetUrl: []`), unreachable URL, or a `kid` not in the set | Set the JWKS URLs and make them reachable from the pod (5 s timeout per URL) |
| `INVALID_ARGUMENT "authorization: jwt was not valid: …"` (for example `"failed to deserialize JSON, maybe missing nid claim"` in logs) | Missing `assertion.ncaId/functionId/functionVersionId`, bad `aud` (with `verifyAud`), or an expired token (with `verifyExp`) | See [configuration-server.md](configuration-server.md#jwt-claim-requirements) |
| `PERMISSION_DENIED "no jwt"` | `require: true` and no `authorization`/`jwt` metadata | Send `authorization: Bearer <token>` |
| Pod restarts after about 24 h | JWKS not refreshed successfully for 24 h, so the process panics (`ref/rust/ovdc_jwt/src/lib.rs`) | Fix JWKS reachability |

## Telemetry

| Symptom | Cause | Fix |
|---|---|---|
| Grafana panels empty, but `/metrics` has data | Dashboards query `ovdc_m_*`/`_bucket` names that the in-process exporter does not emit ([validate.md](validate.md#metrics-to-watch)) | Edit the panel queries to the `ovdc_*` names, or rename series in your pipeline |
| Scrapes fail | `telemetry.prometheusMetricsExposition: false`. The Service and annotations still exist. | Enable it, or set `prometheusAnnotation: false` |
| `/metrics` empty on ≤6.0.0 with `otelMetrics.exporter: none` | Bug fixed in 6.0.1 (`ref/CHANGELOG.md`) | Upgrade |
| No OTLP data | The endpoint is `localhost` (the pod itself), HTTP protocol, or `console` (not compiled in). `otel.enabled: false` does nothing. | Point at a gRPC collector Service; use `exporter: "none"` to disable |
| `ERROR` per request from `span_extractor` on ≤6.0.0 | Fixed in 6.0.1 | Upgrade |

## Published runbook topics: what source covers

| Runbook (published docs) | Source coverage |
|---|---|
| Cache misses | Tenant, pod routing, GC eviction, concurrent-write aborts, key padding, block cache (above) |
| No active shards | **Not in source.** No shard registry, gossip, or shard metric exists in this code. Check that every pod is Ready and answers ([validate.md](validate.md#4-active-shards-every-replica-answers)). The client's notion of a "shard" is not in this repo. |
| Disk exhaustion | `maxSize`/AUTO, GC, immutable PVC size, compaction reclaim (above) |
| Network bottlenecks | HTTP/2 windows, message size, write timeout, keepalive, LB exposure (above) |
| RocksDB corruption | **Not covered** beyond `db.fail_if_options_file_error = true` (refuse a bad OPTIONS file) and `rocksdb_background_errors`. No repair tooling is in the repo. The cache is disposable: deleting the pod's PVC and restarting rebuilds an empty store. That remedy is inferred from the data-loss semantics and is not a documented procedure. |
