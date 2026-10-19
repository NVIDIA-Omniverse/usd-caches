Up: [shared derived-data cache](README.md)

# Server configuration reference (`config.toml`)

`rs/` means `shared-derived-cache/reference/rust/`. The reference file is `rs/ovdc/src/config.toml` (263 lines). It is embedded in the binary and written to `./config.toml` when no config path is given (`rs/ovdc/src/bin/main.rs`). Each section is deserialized by the struct that owns it:

| Section | Struct (file) | Unknown keys |
|---|---|---|
| top-level `logFormat`, `logLevel`, `logFilter` | read ad hoc in `rs/ovdc_trace/src/lib.rs` `reload_from_cfg` | not checked |
| `[store]` | `Store` (`rs/ovdc/src/context/config.rs`) | rejected (`deny_unknown_fields`) |
| `[engine]` | `conf::Config` (`rs/ovdc_map/src/environment/rocks_adapter/config.rs`) | rejected at the top level; `db.*`/`cf.*`/`block.*`/`blobsCf.*`/`blobsBlock.*` are open maps |
| `[garbageCollection]` | `ovdc_gc::Config` (`rs/ovdc_gc/src/lib.rs`) | rejected |
| `[telemetry]` | `TelemetryCfg` (`rs/ovdc_trace/src/lib.rs`) | rejected |
| `[otel]`, `[otelTraces]`, `[otelLogs]`, `[otelMetrics]` | `Config`, `TraceConfig`, `LogConfig`, `MetricConfig` (`rs/ovdc_trace/src/config.rs`) | rejected |
| `[grpc]`, `[grpcTls]`, `[grpcJwt]` | `Grpc`, `Tls`, `Jwt` (`rs/ovdc/src/context/config.rs`) | rejected |

"Required" below means the field has no `#[serde(default)]`: if it is missing, the section fails to deserialize and the process panics (`Snap::subtable` → `panic!("failed to deserialize subtable …")`, `rs/ovdc_settings/src/lib.rs`). The `Default` impls on `Grpc`, `Tls`, `Jwt`, and `Store` are **not** used to fill missing fields.

## Units

Parsed by `rs/ovdc_bytes/src/human_serde/` and coerced by `rs/ovdc_settings/src/value_de.rs`:

- Sizes: `B`; `K`/`k`/`KB` = 10³; `Ki` = 1024; `M`/`MB` = 10⁶; `Mi` = 2²⁰; `G`/`g`/`GB` = 10⁹; `Gi`; `T`/`TB` = 10¹²; `Ti`.
- Durations: `ns`, `us`/`µs`, `ms`, `s`, `m` (**minutes**), `h`, `d`. A bare integer is milliseconds.
- Case matters: `"64m"` is 64 minutes = 3,840,000, while `"64M"` is 64,000,000 (`rs/ovdc_settings/src/lib.rs` test `lowercase_m_is_minutes_not_megabytes`).
- RocksDB option strings that look like sizes or durations are converted to integers **before** reaching RocksDB. RocksDB's own parser would read `M` as MiB (`rocks_adapter/config.rs` `OptionValue`).

## Top-level keys

| Key | Type | config.toml | Required | Effect |
|---|---|---|---|---|
| `logFormat` | `json`\|`human`\|`human_no_color` | `"json"` | yes (`unwrap`) | stdout format |
| `logLevel` | `error`\|`warn`\|`info`\|`debug`\|`trace`\|`off` | `"info"` | no | stdout level. If absent or unparseable, falls back to `RUST_LOG`. |
| `logFilter` | string | `""` | no | stdout per-module overrides `module[*]=level[,…]` |

## `[store]`

| Key | Type | config.toml | Struct default | Effect |
|---|---|---|---|---|
| `store.maxSize` | size | `"32Mi"` | (32,000,000) | RocksDB target and GC denominator. Read only at startup; a reload ignores it, so restart to apply a new value. The config comment says "Once a DB is created this value cannot be changed", but the value is not stored in the DB, and an existing DB accepts a new one after a restart. **32 MiB is tiny**; raise it for real use. |
| `store.blobCutoff` | size | `8000` | (8000) | Declared size above which a PUT is chunked. Read per PUT. |
| `store.blobChunkSize` | size | `4000000` | (4,000,000) | Chunk size. Read per PUT. |
| `store.mode` | `legacy`\|`legacy_and_discrete`\|`log` | `"log"` | (`legacy`) | Storage layout and flags (`rs/ovdc/src/context.rs`). Fixed for the life of the DB. |

## `[engine]` (read once at DB open)

| Key | Type | config.toml | serde default | Effect |
|---|---|---|---|---|
| `engine.useWriteBufferManager` | bool | `true` | `false` | Shared memtable budget charged to the block cache LRU |
| `engine.writeBufferManagerSize` | size | — (not in file) | `0` (= no limit) | Memtable budget for the write buffer manager |
| `engine.writeBufferManagerAllowStall` | bool | — (not in file) | `false` | Test-only. A global stall once the budget is exceeded. |
| `engine.cacheSize` | size | `"32G"` | required | OVDC row cache; `0` disables it |
| `engine.blockCacheSize` | size | `"8G"` | required | `rocksdb::Cache::new_lru_cache`, currently only attached to the write buffer manager |
| `engine.increaseParallelism` | u8 | `8` | `2` | `Options::increase_parallelism` |
| `engine.threadsHigh` | u8 | `4` | `2` | Env high-priority background threads |
| `engine.threadsLow` | u8 | `4` | `2` | Env low-priority background threads |

Option maps. Each map is overlaid on the RocksDB defaults captured at build time (`rs/ovdc_map/build.rs` opens a scratch DB and embeds its `OPTIONS` file), joined with `;`, and applied with `get_options_from_string`. Keys are RocksDB option names; a bad option panics at open (`unwrap`).

| Key | config.toml | Effect |
|---|---|---|
| `engine.db.allow_concurrent_memtable_write` | `true` | Concurrent memtable writes |
| `engine.db.max_open_files` | `128` | Open file limit (`-1` = unlimited) |
| `engine.db.max_background_jobs` | `6` | Concurrent flush and compaction jobs |
| `engine.db.table_cache_numshardbits` | `8` | Table cache shards (2^n) |
| `engine.db.fail_if_options_file_error` | `true` | Fail on a bad OPTIONS file |
| `engine.db.keep_log_file_num` | `8` | Retained RocksDB LOG files |
| `engine.db.avoid_unnecessary_blocking_io` | `true` | Background file deletion |
| `engine.db.max_log_file_size` | `"64M"` | LOG rollover size (64,000,000) |
| `engine.db.stats_dump_period_sec` | `0` | Stats dump to LOG; `0` disables it |
| `engine.db.create_if_missing` | `true` | Create the DB if absent |
| `engine.cf.min_blob_size` | `8096` | `default` column family: blob threshold |
| `engine.cf.enable_blob_files` | `true` | Integrated BlobDB |
| `engine.cf.enable_blob_garbage_collection` | `true` | Blob GC in compaction |
| `engine.cf.blob_garbage_collection_age_cutoff` | `1.0` | All blob files eligible for relocation |
| `engine.cf.blob_garbage_collection_force_threshold` | `0.25` | Force compaction of a blob file with this much garbage |
| `engine.cf.write_buffer_size` | `"64M"` | Memtable size |
| `engine.cf.max_write_buffer_number` | `3` | Memtables before stall (the Helm chart sets `4`) |
| `engine.cf.memtable_whole_key_filtering` | `true` | Memtable whole-key bloom |
| `engine.cf.periodic_compaction_seconds` | `28800` | Periodic compaction (8 h) |
| `engine.cf.memtable_prefix_bloom_size_ratio` | `0.020000` | Memtable prefix-bloom ratio |
| `engine.blobsCf.enable_blob_files` | `true` | `blobs` column family: BlobDB |
| `engine.blobsCf.min_blob_size` | `8096` | Blob threshold (the Helm chart sets `0`) |
| `engine.blobsCf.blob_file_size` | `"32M"` | Blob file size (the Helm chart sets `"64M"`) |
| `engine.blobsCf.write_buffer_size` | `"64M"` | Memtable size |
| `engine.blobsCf.max_write_buffer_number` | `24` | Memtables before stall |
| `engine.blobsCf.memtable_whole_key_filtering` | `true` | |
| `engine.blobsCf.level0_stop_writes_trigger` | `24` | L0 files that stop writes |
| `engine.blobsCf.memtable_prefix_bloom_size_ratio` | `0.020000` | |
| `engine.blobsCf.enable_blob_garbage_collection` | `true` | |
| `engine.blobsCf.blob_garbage_collection_age_cutoff` | `1.0` | (the Helm chart sets `0.8`) |
| `engine.blobsCf.blob_garbage_collection_force_threshold` | `0.25` | |
| `engine.block.cache_index_and_filter_blocks` | `true` | `default` column family table options |
| `engine.block.block_size` | `16384` | |
| `engine.block.index_type` | `"kTwoLevelIndexSearch"` | |
| `engine.block.filter_policy` | `"bloomfilter:10"` | |
| `engine.block.partition_filters` | `true` | |
| `engine.block.optimize_filters_for_memory` | `true` | |
| `engine.block.data_block_index_type` | `"kDataBlockBinaryAndHash"` | |
| `engine.block.pin_l0_filter_and_index_blocks_in_cache` | `true` | |
| `engine.block.prepopulate_block_cache` | `"kFlushOnly"` | |

`engine.blobsBlock.*` is accepted (default `{}`) but not present in config.toml; the Helm chart sets it. Hard-coded in addition (`rocks_adapter/config.rs`): `create_missing_column_families`, statistics level `All`, `add_compact_on_deletion_collector_factory(100, 10, 0.6)`, a merge operator, and, in `log` mode, a bottommost-level compaction filter plus `num_levels=4` on `default` unless `engine.cf.num_levels` is set.

## `[garbageCollection]` (read per pass)

| Key | Type | config.toml | Required | Effect |
|---|---|---|---|---|
| `garbageCollection.minFreeCapacity` | u8 % | `40` | yes | Start GC when free % ≤ this (checked every 5 s) |
| `garbageCollection.deleteKeyspaceQuantile` | u8 % | `60` | yes | Target fraction of keys removed per pass, LRU first |
| `garbageCollection.cooldownSecs` | u64 s | `10` | no (default 10) | Sleep after a pass |

## `[telemetry]` (startup)

| Key | Type | config.toml | Effect |
|---|---|---|---|
| `telemetry.prometheusMetricsExposition` | bool | `true` | Start the OpenMetrics HTTP server |
| `telemetry.prometheusMetricsPort` | u16 | `3051` | Bound on `0.0.0.0` |

## `[otel]`

| Key | Type | config.toml | Effect |
|---|---|---|---|
| `otel.enabled` | bool | `true` | Required, but read and discarded. Does not gate export. `OTEL_SDK_DISABLED=true` sets it false, which is equally inert. |
| `otel.useEnvVars` | bool | `true` | Required, but inert. Each `otel*` section calls `apply_env()` unconditionally (`rs/ovdc_trace/src/lib.rs` `reload_from_cfg`). |

## `[otelTraces]`

| Key | Type | config.toml | serde default | Env override |
|---|---|---|---|---|
| `otelTraces.exporter` | `otlp`\|`console`\|`none` | `"otlp"` | `otlp` | `OTEL_TRACES_EXPORTER` |
| `otelTraces.otlp.protocol` | `grpc` (HTTP values export nothing) | `"grpc"` | `grpc` | `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` |
| `otelTraces.otlp.endpoint` | string | `"http://localhost:4317"` | none | `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` |
| `otelTraces.otlp.timeout` | duration | `"10s"` | required in `otlp` | `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` (ms) |
| `otelTraces.processor.scheduleDelay` | duration | `5000` | required in `processor` | `OTEL_BSP_SCHEDULE_DELAY` (ms) |
| `otelTraces.processor.maxQueueSize` | u64 | `2048` | required | `OTEL_BSP_MAX_QUEUE_SIZE` |
| `otelTraces.processor.maxBatchSize` | u64 | `512` | required | `OTEL_BSP_MAX_EXPORT_BATCH_SIZE` |
| `otelTraces.attributeValueLengthLimit` | u64/size | `"4k"` | required | `OTEL_SPAN_ATTRIBUTE_VALUE_LENGTH_LIMIT` |
| `otelTraces.attributeCountLimit` | u64 | `128` | required | `OTEL_SPAN_ATTRIBUTE_COUNT_LIMIT` |
| `otelTraces.eventCountLimit` | u64 | `128` | required | `OTEL_SPAN_EVENT_COUNT_LIMIT` |
| `otelTraces.linkCountLimit` | u64 | `128` | required | `OTEL_SPAN_LINK_COUNT_LIMIT` |
| `otelTraces.eventAttributeCountLimit` | u64 | `128` | required | `OTEL_EVENT_ATTRIBUTE_COUNT_LIMIT` |
| `otelTraces.linkAttributeCountLimit` | u64 | `128` | required | `OTEL_LINK_ATTRIBUTE_COUNT_LIMIT` |
| `otelTraces.samplerArg` | f64 | `0.25` | required; not applied | — |
| `otelTraces.sampler` | `always_on`\|`always_off`\|`traceidratio`\|`parentbased_*` | `"always_on"` | required; not applied | — |
| `otelTraces.level` | level | `"info"` | `info` | `OTEL_TRACES_LEVEL` |
| `otelTraces.filter` | string | `""` | `info` | `OTEL_TRACES_FILTER` |

## `[otelLogs]`

| Key | Type | config.toml | serde default | Env override |
|---|---|---|---|---|
| `otelLogs.exporter` | enum | `"otlp"` | `otlp` | `OTEL_LOGS_EXPORTER` |
| `otelLogs.otlp.protocol` | enum | `"grpc"` | `grpc` | `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` |
| `otelLogs.otlp.endpoint` | string | `"http://localhost:4317"` | none | `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` |
| `otelLogs.otlp.timeout` | duration | `"10s"` | required | `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` (ms) |
| `otelLogs.processor.scheduleDelay` | duration | `5000` | required | `OTEL_BLRP_SCHEDULE_DELAY` (ms) |
| `otelLogs.processor.maxQueueSize` | u64 | `2048` | required | `OTEL_BLRP_MAX_QUEUE_SIZE` |
| `otelLogs.processor.maxBatchSize` | u64 | `512` | required | `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` |
| `otelLogs.attributeValueLengthLimit` | u64/size | `"4K"` | required | `OTEL_LOGRECORD_ATTRIBUTE_VALUE_LENGTH_LIMIT` |
| `otelLogs.attributeCountLimit` | u64 | `128` | required | `OTEL_LOGRECORD_ATTRIBUTE_COUNT_LIMIT` |
| `otelLogs.level` | level | `"info"` | `info` | `OTEL_LOGS_LEVEL` |
| `otelLogs.filter` | string | `""` | `info` | `OTEL_LOGS_FILTER` |

## `[otelMetrics]`

| Key | Type | config.toml | serde default | Env override |
|---|---|---|---|---|
| `otelMetrics.exporter` | enum | `"otlp"` | `otlp` | `OTEL_METRICS_EXPORTER` |
| `otelMetrics.otlp.protocol` | enum | `"grpc"` | `grpc` | `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` |
| `otelMetrics.otlp.endpoint` | string | `"http://localhost:4317"` | none | `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` |
| `otelMetrics.otlp.timeout` | duration | `"15s"` | required | `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` (ms) |
| `otelMetrics.processor.interval` | duration | `"30s"` | required in `processor` | `OTEL_METRIC_EXPORT_INTERVAL` (ms) |
| `otelMetrics.filter` | string | — (not in file) | `info` | — |

The Prometheus pull reader is always registered, whatever `otelMetrics.exporter` says (`rs/ovdc_trace/src/lib.rs`; `CHANGELOG.md` 6.0.1).

## `[grpc]`

All keys are required. Everything is read at startup except `putTimeout`, which is read per PUT.

| Key | Type | config.toml | Effect (source) |
|---|---|---|---|
| `grpc.port` | u16 | `3010` | Bind `0.0.0.0:port` (`rs/ovdc/src/lib.rs`) |
| `grpc.initialStreamWindowSize` | u32 size | `"512K"` | HTTP/2 stream window; must not be 0 |
| `grpc.initialConnectionWindowSize` | u32 size | `"32M"` | HTTP/2 connection window; must not be 0 |
| `grpc.connectionConcurrencyLimit` | u64 | `32` | **No effect**: its use is commented out in `grpc_on_socket` |
| `grpc.maxConcurrentStreams` | u32 | `0` | HTTP/2 max streams; `0` = unlimited |
| `grpc.maxFrameSize` | u32 | `0` | `0` = HTTP/2 default |
| `grpc.timeout` | duration | `"30s"` | **No effect**: never read in `rs/ovdc/src` |
| `grpc.writeTimeout` | duration | `"15s"` | `send_timeout` for each response message. On expiry: `ABORTED "write timeout"`. If unset at runtime, the code falls back to 60 s. |
| `grpc.tcpKeepaliveAfterIdleTime` | duration | `"3s"` | `socket2` keepalive time |
| `grpc.tcpKeepaliveInterval` | duration | `"3s"` | Keepalive interval |
| `grpc.tcpKeepaliveRetries` | u32 | `5` | Keepalive retries |
| `grpc.http2KeepaliveInterval` | duration | `"3s"` | HTTP/2 PING interval |
| `grpc.http2KeepaliveTimeout` | duration | `"20s"` | HTTP/2 PING timeout |
| `grpc.tenantFromMetadata` | bool | `true` | Read the `tenant` metadata (≤48 UTF-8 bytes) |
| `grpc.maxDecodingMessageSize` | u64 size | `"5M"` | tonic `max_decoding_message_size` |
| `grpc.putTimeout` | duration | `"1h"` | Whole-PUT limit. `>4h` is clamped to 4 h; `≤0` uses 1 h, logged once per bad value (`Grpc::put_timeout`). |

## `[grpcTls]` (startup; certs re-read every 300 s)

| Key | Type | config.toml | Effect |
|---|---|---|---|
| `grpcTls.enabled` | bool | `false` | Serve TLS (rustls, aws-lc-rs) |
| `grpcTls.cert` | path | `"cert.pem"` | PEM certificate chain |
| `grpcTls.key` | path | `"cert.key"` | PEM private key |
| `grpcTls.includeCaRoot` | bool | `false` | Load `caRoot` as the client-cert verifier (`allow_unauthenticated`) |
| `grpcTls.caRoot` | path | `"ca.pem"` | CA bundle |

## `[grpcJwt]` (startup)

| Key | Type | config.toml | Effect |
|---|---|---|---|
| `grpcJwt.enabled` | bool | `false` | Build a `Verify` and verify tokens |
| `grpcJwt.require` | bool | `false` | No token gives `PERMISSION_DENIED "no jwt"` |
| `grpcJwt.jwkPublicKeysetUrl` | list[string] | `["debug"]` | Exactly `["debug"]` uses the built-in debug JWK (test only). Otherwise every URL is GET'd (5 s timeout each) and the keys are merged. One failing URL fails that refresh. |
| `grpcJwt.jwkUpdateIntervalSecs` | u64 | `60` | Refresh period. On failure, exponential backoff, capped. **Panics after 24 h** without a successful update. |
| `grpcJwt.cacheSizeMb` | u64 | `16` | Token cache budget (×10⁶ B). Tokens re-verify every 15 min. |
| `grpcJwt.verifyAud` | bool | `false` | Validate `aud` |
| `grpcJwt.audClaim` | string | `""` | Expected audience |
| `grpcJwt.verifyExp` | bool | `false` | Validate `exp` (otherwise `exp` is not required) |

## JWT claim requirements

From `rs/ovdc_jwt/src/lib.rs` and `rs/ovdc/src/jwt.rs`:

- Transport: gRPC metadata `authorization`, or the fallback key `jwt`. The value may carry the prefix `Bearer `.
- Header: `kid` must match a key in the JWK set. Supported `alg`: HS256/384/512, ES256/384, RS256/384/512, PS256/384/512, EdDSA. Others give `UnsupportedAlgorithm`.
- Payload must contain an `assertion` object with **string** fields `ncaId`, `functionId`, and `functionVersionId`. All three are required by serde, and a missing one fails with `InvalidJwt`. The `exp` claim is optional unless `verifyExp`, and `aud` is checked only if `verifyAud`.
- The tenant is `assertion.ncaId`, which must be ≤48 bytes, otherwise `INVALID_ARGUMENT "authorization: invalid length of tenant"`. A verified tenant overrides the `tenant` metadata.
- Errors: a bad token gives `INVALID_ARGUMENT "authorization: <reason>"`. A missing token with `require` gives `PERMISSION_DENIED "no jwt"`. A missing token without `require` falls through to the metadata or default tenant.
- Debug fixtures: `_DEBUG_JWT_0` (tenant `example-tenant-a`, aud `ovdc-test`) and `_DEBUG_JWT_1` (`example-tenant-b`) in `rs/ovdc_jwt/src/lib.rs`. They validate only against the built-in debug JWK set (`jwkPublicKeysetUrl = ["debug"]`), which is for testing only; never use it in production.

## CLI and environment

| Item | Meaning | Source |
|---|---|---|
| `main <envdir> [config.toml]` | `envdir` is required (DB at `<envdir>/db`). The config defaults to `./config.toml`, created from the embedded default if absent. A given path that does not exist exits 1; a non-`.toml` extension warns. | `rs/ovdc/src/bin/main.rs` |
| `IP` | Must parse as an IP address or the process panics. **Unused** afterwards (gossip not implemented). | `main.rs`, `lib.rs` |
| `RUST_LOG` | stdout level fallback when `logLevel` is absent or invalid | `rs/ovdc_trace/src/lib.rs` |
| `RUST_BACKTRACE` | `1`/`full` for panic detail | `shared-derived-cache/reference/README.md` |
| `OTEL_*` | See the env columns above, always applied | `rs/ovdc_trace/src/config.rs` |
| `OVDC_VERSION` (build time) | Version string logged at startup; `dev` when unset | `rs/ovdc_version/build.rs` |
| `OVDC_REQUIRE_HELM` (tests) | Fail rather than skip `test_helm_chart_config` | `rs/ovdc/src/tests.rs` |

## Hot reload

`Settings::new` polls the file's mtime and ctime every **5 s**. On change it re-parses the TOML and runs every registered section validator (`store`, `grpc`, `grpcTls`, `grpcJwt`, `garbageCollection`, `engine`, `otel`, `otelTraces`, `otelLogs`, `otelMetrics`, `telemetry`), then publishes. A parse or validation failure **panics**. The release profile uses `panic = 'abort'` (`rs/Cargo.toml`), so the process exits and Kubernetes restarts it.

What actually changes at runtime:

- **Read per use:** `garbageCollection.*` (each GC loop), `store.blobCutoff`/`store.blobChunkSize` (each PUT), `grpc.putTimeout` (each PUT).
- **Startup only:** `store.maxSize`, `store.mode`, `engine.*`, other `grpc.*`, `grpcTls.*`, `grpcJwt.*`, `telemetry.*`.
- **Logging and OTel (`logFormat`, `logLevel`, `logFilter`, `otel*`):** values.yaml says these apply without a restart. `ProcessorManager::reload` exists, but `main.rs` drops the manager and nothing outside tests calls `reload`, so **changes do not take effect until restart** in this source.

## How Helm values map onto this file

From `shared-derived-cache/reference/docker/helm/templates/configmap.yaml` and `statefulset.yml`:

| Helm | Server |
|---|---|
| `settings.<section>.<key>` | `[<section>] <key>`, rendered with `toJson` |
| `settings.store.maxSize: AUTO` | `store.maxSize = <bytes>` = volume − min(20%, 100 GB) |
| `presets.cpu` / `presets.ram` | Override `engine.increaseParallelism/threadsHigh/threadsLow` (+ `db.max_subcompactions=4` when cpu > 4) / `engine.cacheSize`, `engine.blockCacheSize`, `engine.blobsCf.max_write_buffer_number` |
| `settings.engine.{db,cf,block,blobsCf,blobsBlock}.*` | `engine.<map>.<key>`, all keys |
| `settings.grpcTls.secretName` | (chart only) mounts the Secret. `grpcTls.cert = "/etc/ovdc/tls/tls.crt"`, `key = ".../tls.key"`, `caRoot = ".../ca.crt"` are fixed. |
| `settings.otelMetrics.filter` | `otelMetrics.filter` (not in the reference config.toml) |
| `rustLog`, `rustBacktrace` | env `RUST_LOG`, `RUST_BACKTRACE` |
| (always) | env `IP` = pod IP, `HOST_IP` = host IP; args `/main /env/env /config/config.toml` |

The Helm chart's defaults differ from config.toml in `store.maxSize` (AUTO vs `32Mi`), `store.blobCutoff` (2500 vs 8000), `grpcJwt.jwkPublicKeysetUrl` (`[]` vs `["debug"]`), and several RocksDB options. See [configuration-helm.md](configuration-helm.md).
