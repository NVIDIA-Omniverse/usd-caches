Up: [shared derived-data cache](README.md)

# Deploy OVDC

`ref/` stands for `shared-derived-cache/reference/`. The chart is `ref/docker/helm` (`name: ovderivedcache`, `version`/`appVersion: 6.0.1`, `ref/docker/helm/Chart.yaml`). It creates no CRDs, no cluster-scoped objects, and has no operator dependencies (`ref/docker/helm/README.md`).

## 1. Prerequisites

- Kubernetes and Helm 3. The repo's tests use helm `3.16.3` (`ref/docker/dockerfile/Dockerfile` `HELM_VERSION`).
- A StorageClass that supports **ReadWriteOnce**. The default `storage.volume.storageClassName: managed-csi` is Azure-specific, so change it for your cluster. Size the class for **≥800 MB/s sustained** per PVC (`ref/README.md` "Storage Throughput Requirements").
- Nodes with memory for `resources.requests.memory`, which defaults to `56G` (see [configuration-helm.md](configuration-helm.md#sizing-guidance)).
- An image you can pull. The chart default is `nvcr.io/nvidia/omniverse/ovderivedcache:<appVersion>` with pull secret `regcred` (`ref/docker/helm/values.yaml`). Pulling it needs an NGC API key: confirm the key can pull from `nvcr.io/nvidia/omniverse`, then create `regcred` from it ([section 2](#2-ngc-repo-and-pull-secret)).

## 2. NGC repo and pull secret

These steps aren't in the component source; they match [`docs/getting_started/shared_derived_cache.md`](../../../../docs/getting_started/shared_derived_cache.md) and follow standard NGC conventions: Helm repo `https://helm.ngc.nvidia.com/nvidia/omniverse`, an NGC API key, and a `kubernetes.io/dockerconfigjson` pull secret.

```sh
export NGC_API_KEY=...            # from your NGC account; never commit it
kubectl create namespace ovdc
kubectl create secret docker-registry regcred -n ovdc \
  --docker-server=nvcr.io --docker-username='$oauthtoken' --docker-password="$NGC_API_KEY"
helm repo add omniverse https://helm.ngc.nvidia.com/nvidia/omniverse \
  --username='$oauthtoken' --password="$NGC_API_KEY"
helm repo update omniverse
```

The secret name must match `image.pullSecrets[].name` (default `regcred`). The secret must be in the release namespace (`values.yaml` comment).

## 3. Install

From NGC:

```sh
helm install ovdc omniverse/ovderivedcache -n ovdc -f my-values.yaml
```

From the repo checkout (`ref/docker/helm/README.md` "Install"):

```sh
cd shared-derived-cache/reference
helm install ovdc ./docker/helm -n ovdc -f my-values.yaml
kubectl rollout status statefulset/ovdc-ovderivedcache -n ovdc
```

Starter values files are in [configuration-helm.md](configuration-helm.md#example-override-files).

### Naming

Names come from `ovdc.fullname` (`ref/docker/helm/templates/_helpers.tpl`):

| Inputs | Resource name |
|---|---|
| release `ovdc`, defaults | `ovdc-ovderivedcache` |
| release `ovdc`, `nameOverride: ovdc` | `ovdc` (release already contains the name) |
| release `foo`, `nameOverride: bar` | `foo-bar` |
| `fullnameOverride: x` | `x` (release ignored) |

Names are truncated to 63 chars. The container is always named `ovdc`. Pods are labeled `ovdc: kvnode`, plus `app.kubernetes.io/name` and `app.kubernetes.io/instance`.

### What gets created

| Object | Details (source) |
|---|---|
| StatefulSet `<fullname>` | `serviceName: <fullname>`, `replicas`, `minReadySeconds: 10`. TCP readiness probe on `ovdc-grpc` (delay 5 s, period 10 s); liveness probe the same (delay 15 s). `automountServiceAccountToken: false`. Command `/main /env/env /config/config.toml`. Env `HOST_IP`, `IP` (pod IP), `RUST_LOG`, `RUST_BACKTRACE` (`templates/statefulset.yml`). |
| PVC per replica | From `volumeClaimTemplates` name `environment`: RWO, `storage.volume.size`, mounted at `/env`. If `storage.volume.enabled=false`, `/env` is an `emptyDir` and data is lost on restart. |
| ConfigMap `<fullname>` | `config.toml` rendered from `settings.*`, mounted read-only at `/config` (`templates/configmap.yaml`) |
| Headless Service `<fullname>` | `clusterIP: None`. Ports `ovdc` = `settings.grpc.port` and `http-metrics` = `settings.telemetry.prometheusMetricsPort`. `prometheus.io/*` annotations when `prometheusAnnotation` is true (`templates/service.yaml`). |
| Service `<fullname>-<i>` × replicas (optional) | `type: LoadBalancer`, `externalTrafficPolicy: Local`, selects one pod via `statefulset.kubernetes.io/pod-name`. Only gRPC is exposed. Created when `service.loadBalancer: true`. |
| Pod `<fullname>-debug` (optional) | `docker.io/library/ubuntu:24.04` sleeping, with the config mounted at `/config`. Only when `debug: true`, which also replaces the main command with `sleep`. Never use it in production. |

Client endpoints (`templates/NOTES.txt`): `<fullname>.<ns>.svc.cluster.local:3010` resolves to **all** pod IPs. Each pod is `<fullname>-<ordinal>.<fullname>.<ns>.svc.cluster.local:3010`. The server does no forwarding (see [overview.md](overview.md#multi-node-behavior)), so a client must choose the pod per key itself. The client-side key-to-pod mapping is not in this repo.

## 4. Upgrade

```sh
helm upgrade ovdc omniverse/ovderivedcache -n ovdc -f my-values.yaml
```

- `volumeClaimTemplates` are immutable, so changing `storage.volume.size` on an existing release is **rejected** (`ref/docker/helm/README.md` "Storage sizing").
- The schema is closed, so a values file with a removed key fails the render with the offending path. Removed keys: `service.grpcPort`, `service.metricsPort`, `otelCollector.*`, `settings.grpcTls.cert/key/caRoot`. The `prom_rules.yaml` and `alerts/` manifests are also gone (`ref/docker/helm/README.md` "Upgrading from an earlier chart").
- Pre-6.0 (DDCS) releases: the image, chart, metric prefix (`ddcs_*` → `ovdc_*`), proto package, and TLS mount path all changed. Helm will not upgrade across the chart rename. Reinstall, or use `nameOverride` to keep old names (`ref/CHANGELOG.md` 6.0.0). The server still accepts `/ddcs.KeyValue/*` gRPC paths.
- **Config changes and restarts.** The StatefulSet template has no ConfigMap checksum annotation, so `helm upgrade` that only changes `settings.*` updates the ConfigMap but does not roll pods. The binary polls `/config/config.toml` every 5 s. Settings read per use take effect without a restart: `garbageCollection.*`, `store.blobCutoff`/`blobChunkSize`, and `grpc.putTimeout`. Everything read at startup needs a restart: `store.maxSize`/`mode`, all of `engine`, the rest of `grpc`, `grpcTls`, `grpcJwt`, and `telemetry`. Log and OTel settings also need a restart in practice; see [configuration-server.md](configuration-server.md#hot-reload). Force a roll with `kubectl rollout restart statefulset/<fullname> -n ovdc`, or by changing `podAnnotations` (`values.yaml` comment).
- A reloaded config that fails validation **panics the pod**, so a bad upgrade shows up as CrashLoopBackOff (`values.yaml` header; `ref/rust/ovdc_settings/src/lib.rs`).

## 5. Scale

```sh
helm upgrade ovdc omniverse/ovderivedcache -n ovdc --reuse-values --set replicas=3
```

- Each new ordinal gets its own empty PVC and an independent keyspace. Soft anti-affinity (`selfAntiAffinity: true`, weight 100, `topologyKey: kubernetes.io/hostname`) prefers separate nodes but still allows co-location (`templates/statefulset.yml`).
- values.yaml suggests that with networked volumes (AKS `managed-csi-premium` bursts to about 150 MB/s per volume, or NVMesh), several replicas on the same node can help (`values.yaml` `replicas` comment).
- What scaling does to client-side partitioning (remapping, miss storms) is not determinable from source.
- `service.loadBalancerIP` is shared by every per-pod Service, so it is only useful with `replicas: 1`.

## 6. Uninstall

```sh
helm uninstall ovdc -n ovdc
```

Kubernetes keeps StatefulSet `volumeClaimTemplates` PVCs after uninstall. That is standard StatefulSet behavior, not chart-specific. Delete them explicitly to discard data:

```sh
kubectl delete pvc -n ovdc -l app.kubernetes.io/instance=ovdc
```

(The PVC template carries the chart labels, per `templates/statefulset.yml`.)

## 7. TLS

```sh
kubectl create secret tls ovdc-tls -n ovdc --cert=cert.pem --key=key.pem
# with includeCaRoot: true the secret also needs ca.crt:
kubectl create secret generic ovdc-tls -n ovdc \
  --from-file=tls.crt=cert.pem --from-file=tls.key=key.pem --from-file=ca.crt=ca.pem
```

```yaml
settings:
  grpcTls:
    enabled: true
    secretName: ovdc-tls
    includeCaRoot: false
```

The secret is mounted read-only at `/etc/ovdc/tls`, which is fixed by the chart. The server reads `tls.crt` and `tls.key` (and `ca.crt`), and reloads changed files every 300 s. The render fails if `secretName` is empty while `enabled` is true (`ref/docker/helm/README.md` "TLS"; `templates/statefulset.yml` `required`). TCP probes keep working under TLS.

## 8. OpenTelemetry collector

The chart deploys no collector. Point all three `settings.otel*.otlp.endpoint` at an OTLP/**gRPC** collector, with no `/v1/...` path, or set `exporter: "none"` (`ref/docker/README.md`). Repo example:

```sh
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm upgrade --install ovdc-opentelemetry-collector open-telemetry/opentelemetry-collector \
  --version 0.175.1 -n ovdc --create-namespace \
  --values shared-derived-cache/reference/docker/otel-collector-values.yaml --wait
```

`otel-collector-values.yaml` names the Service `ovdc-otel-collector` (OTLP/gRPC 4317, `debug` exporter). Then layer:

```yaml
settings:
  otelTraces:  { exporter: otlp, otlp: { protocol: grpc, endpoint: http://ovdc-otel-collector:4317 } }
  otelLogs:    { exporter: otlp, otlp: { protocol: grpc, endpoint: http://ovdc-otel-collector:4317 } }
  otelMetrics: { exporter: otlp, otlp: { protocol: grpc, endpoint: http://ovdc-otel-collector:4317 } }
```

Each `otlp` block needs all of `protocol`, `endpoint`, and `timeout` (schema `required`). With `--values`, Helm deep-merges these onto the chart defaults, so `timeout` is inherited. If the collector is in another namespace, use its FQDN and allow TCP 4317 in NetworkPolicies. The chart provides no OTLP headers setting, so handle backend auth in the collector (`ref/docker/README.md`).

## 9. Prometheus and Grafana

- Scrape port `3051`. The server serves metrics on any path; `monitoring.path` only feeds the annotation. Annotations: `prometheus.io/scrape|path|port|scheme`. Your Prometheus must discover annotated Services (`ref/grafana-dashboards/README.md`).
- Import `ref/grafana-dashboards/main.json`, `read.json`, and `write.json` into one Grafana instance. Keep their UIDs (`ady785tl6zl6oa`, `fe2ypyturbhfke`, `ee2xlhixfghkwf`) so cross-links work. Select the `datasource` and `namespace` variables. The dashboards need a `namespace` label on series (`ref/grafana-dashboards/README.md`, `DATASOURCES.md`).
- **Caveat:** the dashboards query `ovdc_m_*` and `ovdc_rocksdb_intrinsic_gauge`, but the in-process exporter names series `ovdc_<instrument>`, for example `ovdc_api_request_total` and `ovdc_rocks_intrinsic_gauge`. See [validate.md](validate.md#metrics-to-watch).

## Run locally (native)

From `shared-derived-cache/reference/` (`ref/README.md` "Run locally"):

```sh
sed -e 's/cacheSize = "32G"/cacheSize = "64M"/' \
    -e 's/blockCacheSize = "8G"/blockCacheSize = "64M"/' \
    -e 's/exporter = "otlp"/exporter = "none"/g' \
    rust/ovdc/src/config.toml > /tmp/ovdc-local.toml
make run CONFIG=/tmp/ovdc-local.toml ENV_DIR=/tmp/ovdc-local-data
# or after make build:
RUST_BACKTRACE=1 ./rust/target/debug/main /tmp/ovdc-local-data /tmp/ovdc-local.toml
```

- gRPC listens on `:3010` and metrics on `:3051` (`curl http://localhost:3051/metrics`). Stop with Ctrl-C. `/tmp/ovdc-local-data` holds the DB and can be reused.
- The reference config has `store.maxSize = "32Mi"` (32 MiB). The sed above leaves it alone, so GC starts once the DB passes about 60% of 32 MiB. Raise it (for example `-e 's/maxSize = "32Mi"/maxSize = "10G"/'`) for anything beyond a smoke test. The server reads it only at startup, so restart to apply a new value; an existing DB accepts it, despite the `config.toml` comment that it "cannot be changed".
- JWT and TLS are disabled in the reference config, which accepts unauthenticated requests. Use it only on a trusted machine. Set `IP` only for gossip; the current code ignores it.

## Run locally (Docker)

The repo documents no `docker run` for the server. The command below is derived from the image layout in `ref/docker/dockerfile/Dockerfile` (`/main`, writable `/env` owned by 1000:1000, no `CMD`) and the chart's command line:

```sh
# pull access: docker login nvcr.io -u '$oauthtoken' (password: your NGC API key)
docker run --rm -p 3010:3010 -p 3051:3051 \
  -v /tmp/ovdc-local.toml:/config/config.toml:ro \
  nvcr.io/nvidia/omniverse/ovderivedcache:6.0.1 /main /env/env /config/config.toml
```

To run your own build instead, use the tag from [build.md](build.md#container-image-dockerdockerfiledockerfile).

Data in `/env` is lost when the container is removed unless you mount a volume writable by uid 1000 at `/env`. To export OTLP from a container, use a collector reachable on a shared Docker network, not `localhost` (`ref/docker/README.md`).

## Port-forward a cluster pod for testing

```sh
kubectl port-forward -n ovdc pod/ovdc-ovderivedcache-0 3010:3010 3051:3051
```

Then use the examples in [api-grpc.md](api-grpc.md) against `localhost:3010`.
