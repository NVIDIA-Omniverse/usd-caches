Up: [shared derived-data cache](README.md)

# Helm values reference (`ovderivedcache` 6.0.1)

Source: `shared-derived-cache/reference/docker/helm/values.yaml` (615 lines). Validation: `values.schema.json` (closed objects; an unknown key fails `helm install/template`). Rendering: `templates/configmap.yaml`, `templates/statefulset.yml`, `templates/service.yaml`, `templates/debug.yaml`, `templates/_helpers.tpl`. Below, `helm/` means `shared-derived-cache/reference/docker/helm/`.

The values have two halves:

- **`settings.*`** mirrors the server's `config.toml` schema and is rendered key by key into the ConfigMap with `toJson`. Every required key must be present, or the process panics at startup. The five RocksDB maps `settings.engine.db`, `cf`, `block`, `blobsCf`, and `blobsBlock` are rendered in full with `range`, so arbitrary RocksDB options added there **are** passed through (`helm/templates/configmap.yaml`). Everywhere else, only the keys the template names explicitly are rendered. Closed objects reject unknown keys at schema validation (`additionalProperties: false`). The chart README says that a key "added under `engine:` that the chart does not know about is silently dropped", but with the closed schema it fails the render instead.
- **Everything else** is Kubernetes-level.

`--set` cannot express floats (`--set x=0.5` gives the string `"0.5"`). Use `--set-json` or a values file (`helm/README.md` "Gotchas").

Server-side meaning of each `settings.*` key, and how it is parsed, is in [configuration-server.md](configuration-server.md). This file records chart defaults, which **differ from `config.toml` defaults** for several keys (marked †).

## Validation and fail calls in templates

| Condition | Result | Source |
|---|---|---|
| Unknown key in a closed object (everything except `global`, `nodeSelector`, `affinity`, `resources`, `podAnnotations`, `podSecurityContext`, `securityContext`, `service.annotations`, `settings.engine.{db,cf,block,blobsCf,blobsBlock}`, `tolerations`) | schema error naming the path | `values.schema.json`; `helm/README.md` "Validation" |
| `settings.store.maxSize: AUTO` with `storage.volume.enabled: false` | `fail "settings.store.maxSize: \"AUTO\" requires storage.volume.enabled=true; set an explicit size instead"` | `_helpers.tpl` `ovdc.autoMaxSizeBytes` |
| `storage.volume.size` unit not in `"" B Ki K Mi M Gi G Ti T` (when AUTO) | `fail "unrecognised size unit …"` | `_helpers.tpl` `ovdc.sizeToBytes` |
| `presets.cpu` not 1–8 | schema enum, then `fail "presets.cpu: unsupported value …"` | schema; `ovdc.cpuPreset` |
| `presets.ram` not `^([4-9]\|[1-8][0-9]\|9[0-6])GB$` | schema pattern, then `fail "presets.ram: unsupported value …"` | schema; `ovdc.ramPreset` |
| `settings.grpcTls.enabled: true` with empty `secretName` | `required "settings.grpcTls.secretName is required when settings.grpcTls.enabled is true"` | `templates/statefulset.yml` |
| Missing required `settings` sections or keys | schema `required` lists | `values.schema.json` |

## Top-level and Kubernetes keys

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `global.additionalLabels` | map[string]string \| null | `null` | Extra labels on the StatefulSet, PVC template, pod template, ConfigMap, Services, and debug pod. `global` stays open for parent charts. |
| `image.registry` | string (minLength 1) | `nvcr.io` | Image = `registry/repository:tag`. Required. |
| `image.repository` | string (minLength 1) | `nvidia/omniverse/ovderivedcache` | Required. |
| `image.pullPolicy` | enum `Always`,`IfNotPresent`,`Never` | `IfNotPresent` | Required. |
| `image.pullSecrets` | list of `{name}` | `[{name: regcred}]` | Rendered as `imagePullSecrets`. Each must be a `kubernetes.io/dockerconfigjson` Secret in the namespace. |
| `image.tag` | string | `latest` | Used **only** if `overrideTag: true`. |
| `image.overrideTag` | bool | `false` | `false` uses Chart `appVersion` (`6.0.1`) as the tag. |
| `nameOverride` | string | `""` | Replaces the chart name in `<release>-<name>`. If the release name contains it, the name is just `<release>`. |
| `fullnameOverride` | string | `""` | Replaces the full resource name. |
| `replicas` | int ≥ 0 | `1` | StatefulSet replicas. Each gets its own PVC. Also the count of per-pod LoadBalancers. |
| `nodeSelector` | object | `{}` | Pod nodeSelector, e.g. `agentpool: ovdcpool`. |
| `tolerations` | array | `[]` | Pod tolerations. |
| `selfAntiAffinity` | bool | `true` | Adds `preferredDuringScheduling` podAntiAffinity (weight 100, label `ovdc In [kvnode]`, `topologyKey: kubernetes.io/hostname`). Merged under `affinity` (user wins). |
| `affinity` | object | `{}` | Deep-merged with the self-anti-affinity rule (`mergeOverwrite`). Your keys are added beside the chart's `podAntiAffinity.preferredDuringSchedulingIgnoredDuringExecution`; only a list under that same key replaces it. Set `selfAntiAffinity: false` to drop the chart's rule. |
| `podAnnotations` | object | `{}` | Pod template annotations. Changing them rolls the StatefulSet. |
| `podSecurityContext.runAsNonRoot` | bool | `true` | Pod securityContext (also used by the debug pod). |
| `podSecurityContext.runAsUser` | int | `1000` | Change together with `runAsGroup` and `fsGroup`. |
| `podSecurityContext.runAsGroup` | int | `1000` | |
| `podSecurityContext.fsGroup` | int | `1000` | Makes `/env` (PVC or emptyDir) writable by the non-root user. A mismatch leaves the DB unwritable. |
| `podSecurityContext.seccompProfile.type` | string | `RuntimeDefault` | Keeps compatibility with the restricted Pod Security Standard. |
| `rustLog` | string | `info` | Env `RUST_LOG`. Only a fallback for the stdout level when `settings.logLevel` fails to parse. Has no effect in a default install. |
| `rustBacktrace` | string | `full` | Env `RUST_BACKTRACE`. |
| `securityContext.runAsNonRoot` | bool | `true` | Container securityContext. |
| `securityContext.runAsUser` | int | `1000` | |
| `securityContext.runAsGroup` | int | `1000` | |
| `securityContext.allowPrivilegeEscalation` | bool | `false` | |
| `securityContext.capabilities.drop` | list | `["ALL"]` | `readOnlyRootFilesystem` is deliberately not set (untested with RocksDB temp files). |
| `resources.requests.memory` | quantity | `56G` | Passed verbatim as `resources`. See [Sizing guidance](#sizing-guidance). `limits.memory: 248G` is present but commented out. |
| `storage.volume.enabled` | bool (required) | `true` | `true` creates a PVC per replica at `/env`. `false` uses an emptyDir at `/env` and data is lost on restart (dev/test only). |
| `storage.volume.size` | string \| int | `300Gi` | PVC request. Drives `settings.store.maxSize: AUTO`. **Immutable** after install (volumeClaimTemplates). |
| `storage.volume.storageClassName` | string | `managed-csi` | Azure-specific default. Must support RWO. |
| `presets.cpu` | int 1–8 \| null | `null` | Overrides `engine.increaseParallelism/threadsHigh/threadsLow`; `>4` also sets `db.max_subcompactions=4`. See [Sizing presets](#sizing-presets). |
| `presets.ram` | `"<4..96>GB"` \| null | `null` | Overrides `engine.cacheSize`, `engine.blockCacheSize`, and `blobsCf.max_write_buffer_number`. |
| `service.annotations` | object (may contain templates) | `{}` | Applied to the headless Service and each per-pod LoadBalancer. Rendered via `tpl`. |
| `service.loadBalancer` | bool | `false` | Creates `<fullname>-<i>` `LoadBalancer` Services, one per replica (`externalTrafficPolicy: Local`, gRPC port only). |
| `service.loadBalancerSourceRanges` | list[string] | `[]` | Source CIDRs on the per-pod LoadBalancers. |
| `service.loadBalancerIP` | string | `""` | One IP shared by every per-pod Service, so only useful with `replicas: 1`. Deprecated in Kubernetes 1.24+. |
| `prometheusAnnotation` | bool | `true` | Adds `prometheus.io/scrape: "true"`, `path`, `port` (`= settings.telemetry.prometheusMetricsPort`), and `scheme` to the headless Service. |
| `monitoring.path` | string | `/metrics` | Annotation value only. The server answers on any path. |
| `monitoring.scheme` | enum `http`,`https` | `http` | Annotation value only. |
| `debug` | bool | `false` | Main container runs `sleep 999999999` instead of the server, plus a `<fullname>-debug` pod (`docker.io/library/ubuntu:24.04`) with the config at `/config`. Development only. |

Service ports are not separately configurable. They come from `settings.grpc.port` and `settings.telemetry.prometheusMetricsPort`, which also set the container ports and probes (`helm/README.md` "Ports").

## `settings` top-level logging

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.logFormat` | enum `json`,`human`,`human_no_color` | `json` | stdout log format. |
| `settings.logLevel` | enum `error`,`warn`,`info`,`debug`,`trace`,`off` | `info` | stdout level. Wins over `RUST_LOG`. |
| `settings.logFilter` | string | `""` | stdout per-module overrides: `module[*]=level[,…]`, e.g. `ovdc::grpc=debug`. |

## `settings.store`

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.store.maxSize` | `AUTO` \| size string \| int | `AUTO` † | DB target and GC denominator. `AUTO` = `storage.volume.size − min(20%, 100 GB)`, rendered as an integer byte count (300Gi → 257,698,037,760 B). Explicit values such as `"275G"` are used verbatim. Keep them below the PVC size; nothing validates that. Schema pattern `^([Aa][Uu][Tt][Oo]\|[0-9]+([KMGT]i?\|[KMGT]B\|B)?)$`. Startup-only. |
| `settings.store.blobCutoff` | int \| string | `2500` † (config.toml `8000`) | A PUT with declared size above this is chunked. Read per PUT. |
| `settings.store.blobChunkSize` | int \| string | `4000000` | Chunk size for values above `blobCutoff`; `ceil(size/chunk)` parts. Read per PUT. |
| `settings.store.mode` | enum `legacy`,`legacy_and_discrete`,`log` | `log` | Storage layout. Fixed for the life of the volume. Startup-only. |

## `settings.engine` (RocksDB; startup-only)

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.engine.useWriteBufferManager` | bool | `true` | Charges memtable memory to the `blockCacheSize` LRU cache. |
| `settings.engine.cacheSize` | size | `32G` | OVDC row/value cache; `0` disables it. Overridden by `presets.ram`. Required. |
| `settings.engine.blockCacheSize` | size | `8G` | RocksDB LRU cache. Today it is backing the write buffer manager only (see [overview.md](overview.md#memory-model)). Overridden by `presets.ram`. Required. |
| `settings.engine.increaseParallelism` | int 0–255 | `8` | RocksDB `increase_parallelism`. Overridden by `presets.cpu`. |
| `settings.engine.threadsHigh` | int 0–255 | `4` | High-priority (flush) threads. Overridden by `presets.cpu`. |
| `settings.engine.threadsLow` | int 0–255 | `4` | Low-priority (compaction) threads. Overridden by `presets.cpu`. |

### `settings.engine.db` (DBOptions; open map, passed verbatim; required)

| Path | Default | Effect (values.yaml comment) |
|---|---|---|
| `settings.engine.db.allow_concurrent_memtable_write` | `true` | Multiple threads write a memtable at once. |
| `settings.engine.db.max_open_files` | `128` | File handles kept open; `-1` = unlimited. |
| `settings.engine.db.max_background_jobs` | `6` | Max concurrent flush and compaction jobs. |
| `settings.engine.db.table_cache_numshardbits` | `8` | Table cache shards (power of two). |
| `settings.engine.db.fail_if_options_file_error` | `true` | Refuse to start on a malformed OPTIONS file. |
| `settings.engine.db.keep_log_file_num` | `8` | Old RocksDB LOG files kept. |
| `settings.engine.db.avoid_unnecessary_blocking_io` | `true` | Defers file deletion to a background thread. |
| `settings.engine.db.max_log_file_size` | `"64M"` | LOG rollover size. Converted to bytes by OVDC (SI `M` = 10⁶) before reaching RocksDB. |
| `settings.engine.db.stats_dump_period_sec` | `0` | Periodic stats dump to LOG; `0` disables it. |
| `settings.engine.db.create_if_missing` | `true` | Creates the DB if absent. |
| `settings.engine.db.max_subcompactions` | `2` † (not in config.toml) | Parallel sub-range compaction; measured about 7.7× on blobs GC compaction. Forced to `4` when `presets.cpu > 4`. |

### `settings.engine.cf` (CFOptions for `default`; open map; required)

| Path | Default | Effect |
|---|---|---|
| `settings.engine.cf.max_write_buffer_number` | `4` † (config.toml `3`) | Max memtables before writes stall. |
| `settings.engine.cf.min_write_buffer_number_to_merge` | `1` † (not in config.toml) | Min immutable memtables merged on flush. |
| `settings.engine.cf.write_buffer_size` | `"64M"` | One memtable's size. |
| `settings.engine.cf.level0_stop_writes_trigger` | `24` † (not in config.toml for `cf`) | L0 file count that stops writes. |
| `settings.engine.cf.blob_garbage_collection_force_threshold` | `0.25` | Garbage ratio that forces blob-file compaction. |
| `settings.engine.cf.min_blob_size` | `8096` | Values ≥ this go to blob files. |
| `settings.engine.cf.enable_blob_files` | `true` | Integrated BlobDB. |
| `settings.engine.cf.enable_blob_garbage_collection` | `true` | Reclaim blob space during compaction. |
| `settings.engine.cf.memtable_whole_key_filtering` | `true` | Memtable bloom for whole-key lookups. |
| `settings.engine.cf.periodic_compaction_seconds` | `28800` | Forces files through compaction after 8 h. Needed in `log` mode to walk rows to the bottom level. |
| `settings.engine.cf.memtable_prefix_bloom_size_ratio` | `0.02` | Fraction of `write_buffer_size` used for the prefix bloom. |

config.toml also sets `cf.blob_garbage_collection_age_cutoff = 1.0`, which the chart does **not** set (RocksDB default applies).

### `settings.engine.block` (BlockBasedTableOptions for `default`; open map)

| Path | Default | Effect |
|---|---|---|
| `settings.engine.block.cache_index_and_filter_blocks` | `true` | Index/filter blocks counted in the block cache. |
| `settings.engine.block.block_size` | `16384` | Data block size. |
| `settings.engine.block.index_type` | `"kTwoLevelIndexSearch"` | Two-level (partitioned) index. |
| `settings.engine.block.filter_policy` | `"bloomfilter:10"` | Bloom filter, about 10 bits/key. |
| `settings.engine.block.partition_filters` | `true` | Partitioned filters. |
| `settings.engine.block.optimize_filters_for_memory` | `true` | Packs filter blocks. |
| `settings.engine.block.data_block_index_type` | `"kDataBlockBinaryAndHash"` | Hash index in data blocks. |
| `settings.engine.block.pin_l0_filter_and_index_blocks_in_cache` | `true` | Pins L0 filter/index blocks. |
| `settings.engine.block.prepopulate_block_cache` | `"kFlushOnly"` | Warms the cache with flushed blocks. |

### `settings.engine.blobsCf` (CFOptions for `blobs`; exists in `log` and `legacy_and_discrete`)

| Path | Default | Effect |
|---|---|---|
| `settings.engine.blobsCf.max_write_buffer_number` | `24` | Max memtables before stall. `presets.ram` sets `24` (4 GB) or `32` (≥5 GB). |
| `settings.engine.blobsCf.min_write_buffer_number_to_merge` | `1` † | |
| `settings.engine.blobsCf.write_buffer_size` | `"64M"` | |
| `settings.engine.blobsCf.level0_stop_writes_trigger` | `24` | |
| `settings.engine.blobsCf.blob_garbage_collection_force_threshold` | `0.25` | |
| `settings.engine.blobsCf.enable_blob_files` | `true` | |
| `settings.engine.blobsCf.min_blob_size` | `0` † (config.toml `8096`) | `0` puts every value in blob files. |
| `settings.engine.blobsCf.blob_file_size` | `"64M"` † (config.toml `"32M"`) | Max blob file size. |
| `settings.engine.blobsCf.enable_blob_garbage_collection` | `true` | |
| `settings.engine.blobsCf.blob_garbage_collection_age_cutoff` | `0.8` † (config.toml `1.0`) | Fraction of oldest blob files eligible for relocation. |
| `settings.engine.blobsCf.blob_compaction_readahead_size` | `"4M"` † | Readahead for blob reads during compaction. |
| `settings.engine.blobsCf.memtable_whole_key_filtering` | `true` | |
| `settings.engine.blobsCf.periodic_compaction_seconds` | `28800` † | |
| `settings.engine.blobsCf.memtable_prefix_bloom_size_ratio` | `0.02` | |

### `settings.engine.blobsBlock` (BlockBasedTableOptions for `blobs`; not in config.toml †)

| Path | Default |
|---|---|
| `settings.engine.blobsBlock.cache_index_and_filter_blocks` | `true` |
| `settings.engine.blobsBlock.block_size` | `16384` |
| `settings.engine.blobsBlock.index_type` | `"kTwoLevelIndexSearch"` |
| `settings.engine.blobsBlock.filter_policy` | `"bloomfilter:10"` |
| `settings.engine.blobsBlock.partition_filters` | `true` |
| `settings.engine.blobsBlock.optimize_filters_for_memory` | `true` |
| `settings.engine.blobsBlock.data_block_index_type` | `"kDataBlockBinaryAndHash"` |
| `settings.engine.blobsBlock.pin_l0_filter_and_index_blocks_in_cache` | `true` |
| `settings.engine.blobsBlock.prepopulate_block_cache` | `"kFlushOnly"` |

Each key has the same meaning as the matching `settings.engine.block.*` key. An unknown or malformed RocksDB option fails at DB open (pod start) and is not validated by the chart. `engine.writeBufferManagerSize` and `engine.writeBufferManagerAllowStall` exist in the server struct but **cannot be set through the chart**, because the template does not render them and the schema rejects them.

## `settings.garbageCollection` (read per pass)

| Path | Type | Default | Effect |
|---|---|---|---|
| `settings.garbageCollection.minFreeCapacity` | int 0–100 | `40` | GC starts when free % of `store.maxSize` ≤ this (checked every 5 s). |
| `settings.garbageCollection.deleteKeyspaceQuantile` | int 0–100 | `60` | Fraction of keys a pass tries to evict, oldest access first. |
| `settings.garbageCollection.cooldownSecs` | int ≥ 0 | `10` | Sleep after a pass before rechecking. |

## `settings.telemetry` (startup-only)

| Path | Type | Default | Effect |
|---|---|---|---|
| `settings.telemetry.prometheusMetricsExposition` | bool | `true` | Serves `/metrics`. When `false`, the Service and annotations still exist, so scrapes fail. |
| `settings.telemetry.prometheusMetricsPort` | int 1–65535 | `3051` | Bind `0.0.0.0`. Also the container port, Service port, and annotation. |

## `settings.otel`, `settings.otelTraces`, `settings.otelLogs`, `settings.otelMetrics`

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.otel.enabled` | bool | `true` | Required, but **inert**: setting it `false` does not disable export. Use `exporter: "none"`. |
| `settings.otel.useEnvVars` | bool | `true` | Required, but **inert**: `OTEL_*` env vars always apply. The chart sets none. |
| `settings.otelTraces.exporter` | enum `otlp`,`console`,`none` | `otlp` | Only `otlp` exports; `console` is not compiled in. |
| `settings.otelTraces.otlp.protocol` | enum `grpc`,`http/protobuf`,`http/json` | `grpc` | Only `grpc` exports. |
| `settings.otelTraces.otlp.endpoint` | string (minLength 1) | `http://localhost:4317` | Collector URI, no path. Must be reachable from the pod; `localhost` is the pod itself. |
| `settings.otelTraces.otlp.timeout` | duration | `10s` | |
| `settings.otelTraces.processor.scheduleDelay` | int (ms) \| string | `5000` | Batch export delay. |
| `settings.otelTraces.processor.maxQueueSize` | int ≥ 1 | `2048` | |
| `settings.otelTraces.processor.maxBatchSize` | int ≥ 1 | `512` | |
| `settings.otelTraces.attributeValueLengthLimit` | int \| size | `"4K"` | Span attribute value bytes. |
| `settings.otelTraces.attributeCountLimit` | int ≥ 0 | `128` | |
| `settings.otelTraces.eventCountLimit` | int ≥ 0 | `128` | |
| `settings.otelTraces.linkCountLimit` | int ≥ 0 | `128` | |
| `settings.otelTraces.eventAttributeCountLimit` | int ≥ 0 | `128` | |
| `settings.otelTraces.linkAttributeCountLimit` | int ≥ 0 | `128` | |
| `settings.otelTraces.samplerArg` | number 0–1 | `0.25` | Required but **not read**; sampling is not applied. |
| `settings.otelTraces.sampler` | string | `always_on` | Required but **not read**. |
| `settings.otelTraces.level` | enum log level | `info` | Base level for the span exporter. |
| `settings.otelTraces.filter` | string | `""` | Per-module overrides for the span exporter. |
| `settings.otelLogs.exporter` | enum | `otlp` | As for traces. |
| `settings.otelLogs.otlp.protocol` | enum | `grpc` | |
| `settings.otelLogs.otlp.endpoint` | string | `http://localhost:4317` | |
| `settings.otelLogs.otlp.timeout` | duration | `10s` | |
| `settings.otelLogs.processor.scheduleDelay` | int (ms) | `5000` | |
| `settings.otelLogs.processor.maxQueueSize` | int ≥ 1 | `2048` | |
| `settings.otelLogs.processor.maxBatchSize` | int ≥ 1 | `512` | |
| `settings.otelLogs.attributeValueLengthLimit` | int \| size | `"4K"` | |
| `settings.otelLogs.attributeCountLimit` | int ≥ 0 | `128` | |
| `settings.otelLogs.level` | enum log level | `info` | Base level for the log exporter. |
| `settings.otelLogs.filter` | string | `""` | |
| `settings.otelMetrics.exporter` | enum | `otlp` | Independent of the Prometheus endpoint. |
| `settings.otelMetrics.otlp.protocol` | enum | `grpc` | |
| `settings.otelMetrics.otlp.endpoint` | string | `http://localhost:4317` | |
| `settings.otelMetrics.otlp.timeout` | duration | `15s` | |
| `settings.otelMetrics.processor.interval` | duration | `30s` | Push interval. |
| `settings.otelMetrics.filter` | string | `info` † (absent in config.toml; server default `info`) | `global_level[,module[*]=level]`. |

## `settings.grpc`

`putTimeout` is read per PUT stream. The rest is read once at startup.

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.grpc.port` | int 1–65535 | `3010` | gRPC bind. Also the container port, Service ports, and probes. Required. |
| `settings.grpc.initialStreamWindowSize` | size | `"512K"` | HTTP/2 stream window. Must not be 0. |
| `settings.grpc.initialConnectionWindowSize` | size | `"32M"` | HTTP/2 connection window. Must not be 0. |
| `settings.grpc.connectionConcurrencyLimit` | int ≥ 1 | `32` | Deserialized, but its use is **commented out** in `grpc_on_socket` (`rust/ovdc/src/lib.rs`), so it has no effect. |
| `settings.grpc.maxConcurrentStreams` | int ≥ 0 | `0` | HTTP/2 max streams per connection; `0` = unlimited. |
| `settings.grpc.maxFrameSize` | int ≥ 0 | `0` | `0` = HTTP/2 default. |
| `settings.grpc.timeout` | duration | `"30s"` | Required by the schema. Deserialized into `Grpc.timeout`, but no code in `rust/ovdc/src` reads it, so it has **no effect** in this source. |
| `settings.grpc.writeTimeout` | duration | `"15s"` | Timeout for queuing a response message to the client stream. On expiry the call fails `ABORTED "write timeout"`. |
| `settings.grpc.tcpKeepaliveAfterIdleTime` | duration | `"3s"` | TCP keepalive idle time. |
| `settings.grpc.tcpKeepaliveInterval` | duration | `"3s"` | |
| `settings.grpc.tcpKeepaliveRetries` | int ≥ 0 | `5` | |
| `settings.grpc.http2KeepaliveInterval` | duration | `"3s"` | HTTP/2 PING interval. |
| `settings.grpc.http2KeepaliveTimeout` | duration | `"20s"` | |
| `settings.grpc.tenantFromMetadata` | bool | `true` | Accept tenant from the `tenant` metadata. |
| `settings.grpc.maxDecodingMessageSize` | size | `"5M"` | Max inbound gRPC message. Keep it above `blobChunkSize` plus overhead (values.yaml). The proto comment says 4 MB. |
| `settings.grpc.putTimeout` | duration | `"1h"` | Whole-PUT-stream limit. Above 4 h is clamped to 4 h; 0 or negative falls back to 1 h, logged once. Expiry fails with `DEADLINE_EXCEEDED`. |

## `settings.grpcTls` (startup; cert files re-read every 300 s)

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.grpcTls.enabled` | bool (required) | `false` | Serve gRPC over TLS. Mounts Secret `secretName` at `/etc/ovdc/tls`. Server paths are fixed to `/etc/ovdc/tls/tls.crt`, `tls.key`, and `ca.crt`. |
| `settings.grpcTls.secretName` | string | `""` | Required when `enabled`; otherwise the render fails. Chart-only; not a server key. |
| `settings.grpcTls.includeCaRoot` | bool | `false` | Use `ca.crt` to verify client certificates when presented. Unauthenticated clients are still accepted. The Secret must carry `ca.crt`. |

## `settings.grpcJwt` (startup-only)

| Path | Type | Default | Effect / constraints |
|---|---|---|---|
| `settings.grpcJwt.enabled` | bool (required) | `false` | Verify JWTs and take the tenant from `assertion.ncaId`. |
| `settings.grpcJwt.require` | bool | `false` | Reject requests with no token (`PERMISSION_DENIED "no jwt"`). Only effective when `enabled`. |
| `settings.grpcJwt.jwkPublicKeysetUrl` | list[string] | `[]` † (config.toml `["debug"]`) | JWKS URLs, all fetched and merged. `["debug"]` selects a built-in test keyset; **never use it outside development**. With `enabled: true` and `[]`, no keys load and every token fails `"no key was available"` (inferred from `ovdc_jwt` code). |
| `settings.grpcJwt.jwkUpdateIntervalSecs` | int ≥ 0 | `60` | JWKS refresh interval. If 24 h pass without a successful refresh, the process panics. |
| `settings.grpcJwt.cacheSizeMb` | int ≥ 0 | `16` | Verified-token cache size, ×10⁶ bytes. |
| `settings.grpcJwt.verifyAud` | bool | `false` | Validate `aud` against `audClaim`. |
| `settings.grpcJwt.audClaim` | string | `""` | Required audience when `verifyAud`. |
| `settings.grpcJwt.verifyExp` | bool | `false` | Validate `exp`. When `false`, `exp` is not required. |

## Sizing presets

Source: `helm/templates/_helpers.tpl` (`ovdc.cpuPreset`, `ovdc.ramPreset`), applied in `templates/configmap.yaml`. A preset value wins over `settings.engine`.

`presets.cpu` → `increaseParallelism / threadsHigh / threadsLow`. For `cpu > 4`, `db.max_subcompactions = 4`.

| cpu | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| increaseParallelism | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
| threadsHigh | 1 | 1 | 2 | 2 | 3 | 3 | 4 | 4 |
| threadsLow | 1 | 1 | 1 | 2 | 2 | 3 | 3 | 4 |

`presets.ram` (N GB, 4–96). The block cache is interpolated from the anchors (4→3, 6→4, 8→4, 12→5, 16→6, 24→8, 32→10, 48→12, 96→24 GB), integer-rounded down. `cacheSize = N − block`. `blobsCf.max_write_buffer_number` = 24 if N < 5, else 32. Computed examples:

| ram | 4GB | 8GB | 16GB | 24GB | 32GB | 48GB | 64GB | 96GB |
|---|---|---|---|---|---|---|---|---|
| blockCacheSize | 3G | 4G | 6G | 8G | 10G | 12G | 16G | 24G |
| cacheSize | 1G | 4G | 10G | 16G | 22G | 36G | 48G | 72G |
| blobsCf.max_write_buffer_number | 24 | 32 | 32 | 32 | 32 | 32 | 32 | 32 |

Presets do not set Kubernetes `resources`. Set `resources.requests` yourself, for example CPU equal to `presets.cpu`, and memory ≥ preset RAM + 2–6 GB.

## Sizing guidance

- **Memory request** (`values.yaml` `resources` comment): ≥ `cacheSize + blockCacheSize + ~2–6 GB` network buffers. With `useWriteBufferManager: true`, memtables are already charged to the block cache, so do not add them again. With `false`, add `cf.write_buffer_size × max_write_buffer_number` (per column family). The chart README says "roughly 6GB of TCP buffers" and lists write buffers as additive. The values.yaml comment is the more precise rule. Default `56G` covers 32G + 8G + headroom.
- **Disk:** `storage.volume.size` per replica. `AUTO` gives 80% for volumes up to 500 GB (decimal, about 465.7 GiB; `templates/_helpers.tpl` `ovdc.autoMaxSizeBytes`), 90.9% at 1Ti, and 99.1% at 10Ti (`helm/README.md` "Storage sizing"). GC keeps usage near `(100 − minFreeCapacity)`% of `maxSize`.
- **Throughput:** ≥800 MB/s sustained per PVC. Burst-credit volumes stall once credits run out (`shared-derived-cache/reference/README.md`).
- **Write-stall headroom:** larger and more memtables absorb bursts but do not raise the sustainable rate. Only faster storage, more background threads (`threadsLow`, `db.max_background_jobs`), or less ingest do (`shared-derived-cache/reference/README.md` "What Causes a Stall").

## Example override files

### Single-node development (ephemeral, no OTLP)

```yaml
replicas: 1
image:
  pullSecrets: [{ name: regcred }]
storage:
  volume:
    enabled: false          # emptyDir; data lost on restart
presets:
  ram: "4GB"                # cacheSize 1G, blockCacheSize 3G
  cpu: 2
resources:
  requests: { cpu: "2", memory: 8G }
settings:
  store:
    maxSize: "20G"          # AUTO is not allowed without a volume
  otelTraces:  { exporter: "none" }
  otelLogs:    { exporter: "none" }
  otelMetrics: { exporter: "none" }
```

### Multi-replica production (persistent, TLS, JWT, collector)

```yaml
replicas: 3
nodeSelector: { agentpool: ovdcpool }
storage:
  volume:
    enabled: true
    size: 1Ti               # immutable after install
    storageClassName: managed-csi-premium   # choose a class sustaining >=800 MB/s
presets:
  cpu: 8                    # 8/4/4 threads, max_subcompactions 4
  ram: "64GB"               # cacheSize 48G, blockCacheSize 16G
resources:
  requests: { cpu: "8", memory: 70G }
settings:
  store: { maxSize: AUTO }  # 1Ti - 100GB
  grpcTls:
    enabled: true
    secretName: ovdc-tls
  grpcJwt:
    enabled: true
    require: true
    jwkPublicKeysetUrl: ["https://issuer.example.com/.well-known/jwks.json"]
    verifyExp: true
  otelTraces:  { otlp: { endpoint: http://ovdc-otel-collector:4317 } }
  otelLogs:    { otlp: { endpoint: http://ovdc-otel-collector:4317 } }
  otelMetrics: { otlp: { endpoint: http://ovdc-otel-collector:4317 } }
```

### External clients via per-pod LoadBalancers

```yaml
replicas: 2
service:
  loadBalancer: true
  loadBalancerSourceRanges: ["10.0.0.0/8"]
  annotations:
    service.beta.kubernetes.io/azure-load-balancer-internal: "true"
```

The annotation shown is an Azure example of a provider-specific annotation. Neither `jwkPublicKeysetUrl` URL nor the annotation comes from the repo; substitute your own.
