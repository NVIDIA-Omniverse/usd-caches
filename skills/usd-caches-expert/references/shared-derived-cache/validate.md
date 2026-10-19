Up: [shared derived-data cache](README.md)

# Validate an OVDC build or deployment

`ref/` means `shared-derived-cache/reference/`. Work through the checklist in order; each step names the evidence that proves it.

## 1. Build and tests

| Check | Command (from `ref/`) | Pass |
|---|---|---|
| Native build | `make build` (or `make release`) | `rust/target/{debug,release}/main` exists |
| Tests | `OVDC_REQUIRE_HELM=1 make test` (needs `helm` on `PATH`) | All pass, **including** `test_helm_chart_config`. Without `OVDC_REQUIRE_HELM` that test silently skips when helm is absent (`ref/rust/ovdc/src/tests.rs`). |
| Lint | `make lint` | Clippy clean with `-D warnings` |
| Docker equivalent | `./scripts/docker-check.sh all` | Exit 0. Each step prints its output dir. `test` exports `junit.xml` and `exit_code=0`; `lint` exports `clippy.log` and `exit_code=0`; `build` exports a non-empty `binary` (`ref/scripts/docker-check.sh`). |
| Chart renders | `helm lint --strict ./docker/helm` and `helm template ovdc ./docker/helm -f my-values.yaml \| kubectl apply --dry-run=client -f -` | No schema or `fail` errors (`ref/docker/helm/README.md` "Validation") |

`test_helm_chart_config` renders **default** values only. Render your own values file and inspect the ConfigMap's `config.toml` yourself, or boot it once with `debug: true` and read `/config/config.toml` in the `<fullname>-debug` pod.

## 2. Pods ready

The commands in this section and the next ones use these variables. The values are for release `ovdc` with chart defaults; see [naming](deploy.md#naming) for other installs.

```sh
NS=ovdc RELEASE=ovdc FULLNAME=ovdc-ovderivedcache
kubectl rollout status "statefulset/$FULLNAME" -n "$NS"
kubectl get pods -n "$NS" -l "app.kubernetes.io/instance=$RELEASE" -o wide
kubectl get pvc -n "$NS" -l "app.kubernetes.io/instance=$RELEASE"
```

- Readiness and liveness are **TCP** probes on the gRPC port, so "Ready" only proves the listener is bound. The listener is bound before the DB opens (`ref/rust/ovdc/src/lib.rs` `Prepared::new`), which means a pod can be Ready a moment before it serves. `minReadySeconds: 10` (`templates/statefulset.yml`).
- Expect one PVC `environment-<fullname>-<i>` per replica, Bound, of `storage.volume.size`. The name follows the standard StatefulSet claim naming.
- `debug: true` replaces the server with `sleep` but keeps the TCP probes, so StatefulSet pods never become Ready and the liveness probe restarts them; only `<fullname>-debug` is Ready. Check it is `false`. After switching it back, delete the StatefulSet pods, because a StatefulSet doesn't replace a pod that never became Ready.

## 3. Startup log lines

```sh
kubectl logs -n "$NS" "$FULLNAME-0" | head -50
```

Expect (`ref/rust/ovdc/src/lib.rs`, `context.rs`):

- `ovdc version` with `version=<appVersion>+<sha>`, or `dev` for local builds (`ref/rust/ovdc_version`).
- `Engine: 'LOG' (…)`, matching `store.mode`.
- Every 15 s: `background` with `p_free=<n>`, and `rocks info` with `is_write_stopped`, `is_compaction_pending`, `running_flushes`, `running_compactions`, `memtable_pending_flush`.
- If JWT is on: `jwk set updated`, or `failed to get jwk set, will retry in background`.
- If TLS is on: a TLS info line after the certificates load (`grpc/tls/certificates.rs`).

## 4. "Active shards": every replica answers

The source has no shard registry or "active shards" metric. "No active shards" is a published-runbook topic that this source does not cover. The closest source-supported check is that **each pod individually** answers gRPC, since clients address pods individually. Run it in bash or zsh; it uses arrays, which POSIX `sh` lacks.

```bash
G=(-plaintext)   # TLS: G=(-cacert ca.crt -servername NAME_IN_CERT); required JWT: add -H "authorization: Bearer $JWT"
REPLICAS=$(kubectl get statefulset "$FULLNAME" -n "$NS" -o jsonpath='{.spec.replicas}')
FAILED=0
for i in $(seq 0 $((REPLICAS - 1))); do
  kubectl port-forward -n "$NS" "pod/$FULLNAME-$i" 13010:3010 >/dev/null & PF=$!
  OK=0
  for t in 1 2 3 4 5 6 7 8 9 10; do   # wait up to about 10 s for the forward
    kill -0 "$PF" 2>/dev/null || break
    if grpcurl "${G[@]}" -import-path rust/ovdc_grpc/proto -proto rpc.proto \
        -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:13010 ovdc.KeyValue/Touch >/dev/null 2>&1; then
      OK=1; break
    fi
    sleep 1
  done
  kill "$PF" 2>/dev/null
  if [ "$OK" = 1 ]; then echo "pod $i OK"; else echo "pod $i FAILED"; FAILED=1; fi
done
[ "$FAILED" = 0 ]
```

Also confirm from inside the cluster that headless DNS returns every pod IP:

```sh
kubectl run dns-check -n "$NS" --rm -i --restart=Never --image=docker.io/library/busybox:1.36.1 \
  -- nslookup "$FULLNAME.$NS.svc.cluster.local"
```

## 5. Smoke round trip

Run the PUT, GET, and TOUCH commands from [api-grpc.md](api-grpc.md#grpcurl-examples), or `python ovdc_client.py --target localhost:3010`. A pass is GET returning `value: "aGVsbG8="`, `size: "5"`, and TOUCH returning `touched: 1` (`ref/README.md` "PUT, GET, and TOUCH smoke check"). Repeat with a value larger than `store.blobCutoff` (for example `--size 9000000`) to exercise the chunked path. With TLS or JWT on, repeat with `-cacert` or `-H "authorization: Bearer $JWT"`, with `JWT` set to a token.

## Metrics to watch

`curl -s http://localhost:3051/metrics` (any path works; `Content-Type: application/openmetrics-text`). Metrics are per pod. A round trip increments counters only on the pod that served it, so read the same pod you sent the request to. To check fleet-wide traffic, sum across pods in Prometheus. **Naming rule:** `ovdc_` + the instrument name, with `.` replaced by `_` (`ref/rust/ovdc_trace/src/metrics/prometheus.rs` `sanitize_metric_name`; the instrument name is the lowercased static name, per `metrics/macros.rs`). Counters get no `_total` suffix beyond what the name already has. Histograms emit `<name>_sum`, `<name>_count`, and bucket samples under **`<name>{le="…"}`**, with no `_bucket` suffix (`render_histogram`).

| Series | Kind / labels | Watch for (source) |
|---|---|---|
| `ovdc_api_request_total` | counter, `operation` = `grpc_get`/`grpc_put`/`grpc_touch` | Traffic is arriving (`ref/rust/ovdc/src/grpc/mod.rs`) |
| `ovdc_api_keys_total` | counter, `operation` | Keys per op |
| `ovdc_api_errors_total` | counter, `operation`, optional `kind="put_timeout"` | Any sustained rise |
| `ovdc_api_request_time` | histogram (s), `operation` | GET/PUT latency |
| `ovdc_api_write_queue_time`, `ovdc_api_writes_waiting` | histogram (s), up-down | Writes waiting on a busy DB (stalls) (`grpc/put.rs` `db_write`) |
| `ovdc_api_connections`, `ovdc_api_tcp_rx_total`, `ovdc_api_tcp_tx_total`, `ovdc_api_uptime_secs` | up-down, counters, gauge | Connections, bytes, uptime (`count_connections.rs`, `context.rs`) |
| `ovdc_grpc_server_call_started`, `ovdc_grpc_server_call_duration`, `ovdc_grpc_server_call_sent_total_compressed_message_size`, `ovdc_grpc_server_call_rcvd_total_compressed_message_size` | counter and histograms, `grpc.method`/`grpc.status` (label sanitized to `grpc_method`/`grpc_status`) | Per-RPC status and size (`CHANGELOG.md` 5.1.1) |
| `ovdc_miss`, `ovdc_bytes_returned`, `ovdc_get_bytes`, `ovdc_query`, `ovdc_answer` | counters, `level` | Hit/miss at the adapter. Levels `0` and `1` count row-cache and block-cache lookups, several per key; only `level="2"` is a data miss. Watch `ovdc_miss{level="2"}` against `ovdc_api_keys_total` for `grpc_get` plus `grpc_touch` (`ref/rust/ovdc_map/src/environment/rocks_adapter/metrics.rs`) |
| `ovdc_put`, `ovdc_put_bytes`, `ovdc_put_delay`, `ovdc_put_byte_hist` | counters, histogram | Not ingest. In the default `log` mode, `ovdc_put` and `ovdc_put_byte_hist` count only the parts of chunked values, `ovdc_put_bytes` is not emitted, and `ovdc_put_delay` counts RocksDB busy results, including normal GET and TOUCH lookups. For ingest, use `ovdc_api_keys_total{operation="grpc_put"}` and the gRPC received-size histogram. |
| `ovdc_error`, `ovdc_blob_begin`, `ovdc_blob_abandon`, `ovdc_blob_deny` | counters | Storage errors; abandoned or refused blob writes |
| `ovdc_io_time` | histogram (s), `io` = `rocksdb_read`/`_l1`/`_l2`/`rocksdb_write`/`_delete`/`_merge` | Storage latency |
| `ovdc_rocks_capacity` | gauge, `name` = `bytes_free` \| `p_free` | Free space versus `store.maxSize`. GC starts at `p_free ≤ minFreeCapacity` (`context.rs`) |
| `ovdc_rocks_interval_bytes_per_sec` | gauge, `name` = `write` \| `read` | RocksDB disk throughput. Compare with the ≥800 MB/s PVC guidance |
| `ovdc_rocks_intrinsic_gauge` / `ovdc_rocks_intrinsic_f64_gauge` | gauge, `name` = property identifier, `cf` = `default`/`blobs`, or `-` for DB-wide | Stalls: `__rocksdb_stalls_total_delays`, `__rocksdb_stalls_total_stop`, `__rocksdb_stalls_{stops,delays}_{memtable_limit,l0_file_count_limit,pending_compaction_bytes}` (per column family; `blobs` usually stalls first). State: `rocksdb_is_write_stopped`, `rocksdb_compaction_pending`, `rocksdb_mem_table_flush_pending`, `rocksdb_num_running_{flushes,compactions}`, `rocksdb_background_errors`, `rocksdb_size_all_mem_tables`. Cache: `rocksdb_block_cache_{usage,capacity,pinned_usage}`, `__rocksdb_optional_block_cache_{hit,miss}` (`ref/README.md` "Write Stalls"/"Block Cache"; identifiers from `rocks_adapter/stats.rs`). The README writes the RocksDB property strings (`rocksdb.block-cache-usage`); the exported `name` label uses the identifier form shown here. |
| `ovdc_gc_time_hist` | histogram (s), `operation="gc"` | GC pass duration (`ref/rust/ovdc_gc/src/lib.rs`) |

**Grafana dashboard mismatch.** The bundled dashboards (`ref/grafana-dashboards/*.json`) query `ovdc_m_api_request_total`, `ovdc_m_adapter_capacity`, `ovdc_m_adapter_bytes_returned_total`, `ovdc_m_adapter_miss_total`, `ovdc_m_api_request_time_bucket`, `ovdc_rocksdb_intrinsic_gauge`, and similar names. The in-process exporter produces `ovdc_api_request_total`, `ovdc_rocks_capacity`, `ovdc_bytes_returned`, `ovdc_miss`, `ovdc_api_request_time{le=…}`, and `ovdc_rocks_intrinsic_gauge`. Expect empty panels unless your pipeline renames series, for example an OTLP→collector→Prometheus path that adds `_m_`/`_total`/`_bucket`. The source does not say which pipeline the dashboards target. Before relying on a panel, check `curl -s http://localhost:3051/metrics | grep '^ovdc_' | cut -d'{' -f1 | sort -u`.

## Logs to check

- `"db max capacity reached, garbage collection will start"`, then `"will evict entries"` and `"GC stop"`: a normal GC cycle (`ovdc_gc`). Back-to-back cycles every `cooldownSecs` mean `maxSize` is too small for the working set.
- `"write failed because the database is busy"` or `"timeout while trying to write to DB"`: write stalls (`grpc/put.rs`).
- `"some access time events were lost because the db is busy"`: LRU updates dropped under load (`context.rs`).
- `"concurrent write and read"` (warn): GET hit an in-progress write.
- `"grpc.putTimeout is outside the accepted range"`: config clamp.
- `"rejected new settings"` / `"failed to load new settings"` followed by a panic: a bad ConfigMap reload (`ovdc_settings`).
- `"failed to update JWK set"`, or a panic `"24h has passed since jwk has been updated"` (`ovdc_jwt`).
- `"invalid TLS certificates; using existing certificates; will retry"` (`grpc/tls.rs`).

## Telemetry pipeline (optional)

With the repo's collector example (`ref/docker/README.md`), run `kubectl logs -n "$NS" deployment/ovdc-otel-collector --follow`. Startup logs should appear. Metrics arrive every 30 s (`otelMetrics.processor.interval`), and request traces appear after traffic.
