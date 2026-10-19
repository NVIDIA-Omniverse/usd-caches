Up: [shared derived-data cache](README.md)

# OVDC architecture overview

All paths are relative to the repo root. `ref/` below stands for `shared-derived-cache/reference/`.

## Process and components

One binary, `main` (crate `ovdc`), serves one node (`ref/rust/ovdc/src/bin/main.rs`). At startup it does the following:

1. It reads `argv[1]` as the environment directory. That argument is required, and the binary panics without it. `argv[2]` is an optional TOML config; the default is `./config.toml`, which is written from the embedded default if missing.
2. It parses env `IP`, if set; the value must be a valid IP address or the process panics. The parsed value is passed to `Prepared::new` as `_pod_ip`, which is **unused** (`ref/rust/ovdc/src/lib.rs`).
3. It loads settings with a 5 s file-poll watcher (`ref/rust/ovdc_settings/src/lib.rs`), then sets up tracing and metrics (`ovdc_trace::setup`).
4. It binds gRPC on `0.0.0.0:<grpc.port>` and opens RocksDB at `<envdir>/db` (`ref/rust/ovdc/src/context.rs`). Then it starts the GC task, a 15 s metrics/stat report loop, the LRU access-log writer, and optionally the JWT verifier.
5. It serves `ovdc.KeyValue` plus `grpc.health.v1.Health`. The health service reports SERVING for the KeyValue server and for the legacy name `ddcs.KeyValue`. Requests sent to `/ddcs.KeyValue/*` are rewritten to `/ovdc.KeyValue/*`, so pre-6.0 clients still work (`ref/rust/ovdc/src/lib.rs` `rewrite_legacy_package`).
6. On SIGINT or SIGTERM it closes the DB gracefully and exits 0.

| Crate | Role (source) |
|---|---|
| `ovdc` | Server, gRPC handlers (`src/grpc/{get,put,mod}.rs`), TLS, JWT interceptor, context/config structs |
| `ovdc_grpc` | `proto/rpc.proto`, generated stubs, client helpers (`put_request`, `get_request`, `set_tenant`, `set_jwt`) |
| `ovdc_map` | Storage adapter over RocksDB: modes, blobs, log/merge operator, row cache, RocksDB option loading (`environment/rocks_adapter/config.rs`) |
| `ovdc_gc` | Free-capacity-driven GC: expired-entry scan plus an access-time histogram, then LRU eviction |
| `ovdc_lru` | Coalesced key-access log (`KeyLog`), periodically written back as access-time updates |
| `ovdc_jwt` | JWK fetch and JWT verification; extracts the tenant from `assertion.ncaId` |
| `ovdc_settings` | TOML settings with 5 s polling reload and per-section validators |
| `ovdc_trace` | stdout logging, OTLP (gRPC only) traces/logs/metrics, Prometheus/OpenMetrics endpoint |
| `ovdc_sharded_map` | In-process sharded map (used, for example, for the JWT cache). It is **not** cross-node sharding. |
| `ovdc_bytes`, `ovdc_version`, `ovdc_attributions` | Key types and human-readable units; version string; license tooling |

## Multi-node behavior

- The README states: "Multi-node deployments partition the key space across all the nodes. The nodes in a deployment can be aware of each other and if configured correctly, will 'gossip'. Currently the system does nothing when a node is failed or failing" (`ref/README.md`).
- **The server source contains no gossip, peer discovery, partitioning, forwarding, or replication code.** There are no such modules, and the only use of `IP` is parsed and then discarded (`ref/rust/ovdc/src/lib.rs` `Prepared::new(..., _pod_ip)`). The chart injects `IP` (pod IP) and `HOST_IP`, but nothing reads `HOST_IP` (`ref/docker/helm/templates/statefulset.yml`). Each pod is therefore an independent store with its own PVC and its own GC.
- How keys reach the right pod is not in this repo. Any partitioning must be done by the client across pod addresses. The headless Service resolves to all pod IPs, and each pod is addressable as `<fullname>-<ordinal>.<fullname>.<ns>.svc.cluster.local` (`ref/docker/helm/templates/NOTES.txt`). The UJITSO/RTX client's hashing and failover behavior is **not determinable from this source**.
- No replication: losing a pod or its PVC loses that pod's keys. The README says TTL expiry across nodes is not simultaneous ("a subset of the keys may still be served when others become unavailable").

## Consistency and data loss

| Behavior | Source |
|---|---|
| The cache may lose data. If a key is mutated, any stored version may be returned; "the application is responsible". | `ref/README.md` |
| Concurrent PUT of a key already being written as a chunked blob: the first writer wins. Later writers' bytes are drained and they still receive `PUT_OK` ("null routed"). | `ref/rust/ovdc/src/grpc/put.rs` `sink_value` |
| GET during an in-progress write returns gRPC `ABORTED` `"value changed"`. Clients must treat that as retryable or as a miss. | `ref/rust/ovdc/src/grpc/get.rs`; `ref/rust/ovdc/src/tests.rs` `test_threaded_concurrent_put` |
| Default write durability is `Low` (no WAL). `PutOpen.durability = Normal` writes to the WAL. Unflushed `Low` writes can be lost on crash. | `ref/rust/ovdc_grpc/proto/rpc.proto`, `put.rs` |
| Access-time (LRU) updates are batched. If the DB stays busy after 3 attempts they are dropped with the log line `"some access time events were lost because the db is busy"`. | `ref/rust/ovdc/src/context.rs` `LruWriter` |
| `PutStatus.ERROR_CONCURRENT_WRITE` exists in the proto, but no server code path emits it. | `grep` of `ref/rust/ovdc/src` |

## Keys and tenants

- User key: at most 32 bytes, as `bytes key` or as the fixed `S32Key` (four big-endian `uint64`). Shorter keys are **zero-padded to 32 bytes**, and equality compares the padded array, so `"demo"` and `"demo\0"` are the same key (`ref/rust/ovdc_bytes/src/keys.rs` `Key::new`, `PartialEq`).
- Stored key = 48-byte tenant + 32-byte user key + 4 reserved bytes (84 bytes, `COMPOUND_KEY_SIZE`). Tenants are isolated: a key written under tenant A is invisible to tenant B and to the default (all-zero) tenant (`ref/rust/ovdc/src/tests.rs` `test_tenant_set`, `test_tenant_from_jwt`).
- Tenant source: the gRPC metadata `tenant` (UTF-8, ≤48 bytes) when `grpc.tenantFromMetadata = true`. A verified JWT's `assertion.ncaId` overrides it (`ref/rust/ovdc/src/lib.rs` `ServiceInterceptor`). With JWT disabled, the tenant is client-asserted and is **not a security boundary**.

## Values and storage modes

- `store.mode`: `legacy` (blobs and values in one column family), `legacy_and_discrete` (blobs in a separate `blobs` column family), or `log` (discrete blobs, values stored as merge-operator logs; default in both config.toml and Helm). The comment in values.yaml says: "treat it as fixed for the life of the volume" (`ref/docker/helm/values.yaml`; `ref/rust/ovdc/src/context.rs`).
- A PUT whose declared `size > store.blobCutoff` takes the chunked-blob path and is stored as `ceil(size / blobChunkSize)` parts. Smaller values are stored as one value (`ref/rust/ovdc/src/grpc/put.rs`).
- Maximum parts: `254` in the legacy modes (`MAX_PARTS`, `ref/rust/ovdc_map/src/environment/rocks_adapter/blobs/blob_table/mod.rs`), which caps a value at ≈254 × 4,000,000 B ≈ 1.016 GB with the default chunk size. In `log` mode the cap is `u32::MAX - 1` parts (`log/blobs.rs`). An oversized PUT is refused with `INVALID_ARGUMENT` before the body is read.
- Column families: `default` (keys, small values, merge logs) and `blobs` (present in `log` and `legacy_and_discrete`). Each is tuned separately (`engine.cf`/`block` and `engine.blobsCf`/`blobsBlock`). In `log` mode `default` gets `num_levels=4` unless you set `engine.cf.num_levels` (`ref/rust/ovdc_map/src/environment/rocks_adapter/config.rs`).

## LRU, TTL, and GC

- TTL: `PutOpen.ttl` (milliseconds) applies to every key in that PUT stream. GET does not return expired keys; they are deleted later by GC (`ref/README.md`; `rpc.proto`).
- LRU: a GET or TOUCH logs the key's access. Logs are coalesced (`KeyLog::new(2, 256, 10s, …)`) and written back as access-time updates (`ref/rust/ovdc/src/context.rs`).
- GC (per node, independent): every 5 s it compares RocksDB total size against `store.maxSize`. When `p_free <= garbageCollection.minFreeCapacity`, it runs one pass: scan and delete expired entries while building an access-time histogram, then pick a time floor that removes about `deleteKeyspaceQuantile`% of keys and evict everything older. It then sleeps `cooldownSecs` (`ref/rust/ovdc_gc/src/lib.rs`; `ref/README.md` "Garbage Collection"). Expired entries are dropped by the storage cursor's `retain` during the scan (`ref/rust/ovdc_map/src/environment/rocks_adapter/log/cursor.rs` in `log` mode), not by `ovdc_gc/src/first_pass.rs` itself.
- `store.maxSize` is the GC denominator and the RocksDB target. Under heavy writes the DB "may temporarily grow larger" (`ref/docker/helm/values.yaml`).

## Memory model

Three separate budgets must fit in the container memory (`ref/README.md` "Block Cache"; `ref/docker/helm/values.yaml` `resources` comment):

| Budget | Key | Notes |
|---|---|---|
| OVDC row/value cache | `engine.cacheSize` | OVDC's own whole-entry cache above RocksDB; `0` disables it |
| RocksDB LRU cache | `engine.blockCacheSize` | As wired today it is handed **only to the write buffer manager**, not to the table factory. Tables fall back to RocksDB's per-column-family default block cache, so raising it widens the memtable charge, not SST read caching (`ref/README.md`). |
| Memtables | `cf.write_buffer_size × cf.max_write_buffer_number`, per column family | Charged against the block cache when `engine.useWriteBufferManager = true` |
| Network buffers | — | Budget about 2–6 GB (`values.yaml`) |

## Storage and write stalls

- RocksDB LSM: background flush and compaction are disk-bound. Write amplification is roughly the multiplier × levels (≈60× at defaults). A "stop" stall is surfaced to the client as a retryable failure (`ref/README.md` "Write Stalls"). In code, a busy DB is retried up to 60 times with exponential sleeps, then the PUT fails with `ABORTED "put failed"` or `INTERNAL "write failure"` (`ref/rust/ovdc/src/grpc/put.rs` `db_write`, `db_write_blob`).
- Provision network-attached PVCs for **≥800 MB/s sustained** throughput, and size for the sustained figure rather than the burst figure (`ref/README.md` "Storage Throughput Requirements"). values.yaml notes that on AKS each volume bursts to about 150 MB/s, so several replicas per node may help (`ref/docker/helm/values.yaml` `replicas` comment).

## Network, auth, TLS

| Item | Value | Source |
|---|---|---|
| gRPC (plaintext h2c, or TLS) | `grpc.port` default `3010`; tonic service on a hyper-util `auto` connection builder whose HTTP/2 settings come from `[grpc]` | `ref/rust/ovdc/src/lib.rs` `grpc_on_socket` |
| Prometheus/OpenMetrics | `telemetry.prometheusMetricsPort` default `3051`; any path; `application/openmetrics-text` | `ref/rust/ovdc_trace/src/http_metrics.rs` |
| gRPC reflection | Not registered: clients need `rpc.proto` | `ref/rust/ovdc/src/lib.rs` (only health and KeyValue routes) |
| TLS | rustls; cert and key reloaded every 300 s if their hash changes; ALPN `h2`, `http/1.1`, `http/1.0`. With `includeCaRoot=true`, client certificates are verified against `caRoot` **if presented**; unauthenticated clients are still allowed (`allow_unauthenticated`). | `ref/rust/ovdc/src/grpc/tls.rs`, `tls/certificates.rs` |
| JWT | Optional, via metadata `authorization` (or `jwt`), `Bearer` prefix optional; claim requirements in [configuration-server.md](configuration-server.md#jwt-claim-requirements) | `ref/rust/ovdc/src/jwt.rs`, `ref/rust/ovdc_jwt/src/lib.rs` |

## Telemetry

- stdout logs (`logFormat` json/human/human_no_color). OTLP export uses **gRPC only**; `console` and HTTP protocols deserialize but export nothing. Prometheus pull works even when `otelMetrics.exporter = "none"` (fixed in 6.0.1, `ref/CHANGELOG.md`).
- Prometheus names are `ovdc_` + the instrument name, with non-alphanumerics replaced by `_` (`ref/rust/ovdc_trace/src/metrics/prometheus.rs` `sanitize_metric_name`; test `ovdc_prefix_test_counter`). See [validate.md](validate.md#metrics-to-watch) for the list, and for the mismatch with the bundled Grafana dashboards.

## Placement (only what source says)

- Each replica needs its own RWO PVC (`storageClassName` must support ReadWriteOnce). Soft pod anti-affinity spreads pods across nodes; co-locating replicas is allowed and can help with per-volume throughput caps (`ref/docker/helm/values.yaml`).
- In-cluster clients use the headless Service DNS. Clients outside the cluster use optional per-pod `LoadBalancer` Services (`service.loadBalancer`, `externalTrafficPolicy: Local`) (`ref/docker/helm/templates/service.yaml`).
- The source says nothing about GPU proximity, per-region deployment, or cross-region behavior. The latency and placement advice in the skill's decision guide comes from published docs and product context, not from this code.

## What OVDC does not do

- No range scans, list, or delete RPCs: only `Get`, `Put`, `Touch` (`rpc.proto`).
- No replication, failover, rebalancing, or server-side partitioning in this source (see [Multi-node behavior](#multi-node-behavior)).
- No strong consistency or versioning (see [Consistency and data loss](#consistency-and-data-loss)).
- No authoritative storage: data can be evicted or lost at any time.
- No OTLP/HTTP export, no OTLP headers setting, no bundled collector, alert rules, or ServiceMonitor in the current chart (`ref/docker/helm/README.md` "Telemetry", "Upgrading from an earlier chart").
- Keys longer than 32 bytes are not accepted: they are rejected on PUT and silently skipped on GET/TOUCH (see [api-grpc.md](api-grpc.md#key-constraints)).
