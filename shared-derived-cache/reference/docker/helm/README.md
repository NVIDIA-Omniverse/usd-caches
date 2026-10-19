<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# ovderivedcache

Helm chart for the Omniverse Derived Cache — a distributed key-value store.

## Install

```bash
helm install ovdc ./docker/helm -n <namespace>
```

The release name becomes the resource name prefix, chart-qualified unless the
release name already contains the chart name: release `ovdc` produces objects
named `ovdc-ovderivedcache`. Set `nameOverride: ovdc` to keep the shorter
names, or `fullnameOverride` to control them outright.

Published as `ovderivedcache` in NGC and as an OCI artifact on GHCR. The
chart is named for the artifact; the container inside it is still `ovdc`.

## What the chart creates

| Object | Notes |
|---|---|
| StatefulSet | one PVC per replica when `storage.volume.enabled` |
| Service (headless) | DNS for all pods, plus the metrics port |
| Service × N (optional) | one LoadBalancer per pod when `service.loadBalancer` |
| ConfigMap | the application's `config.toml` |
| Pod (optional) | a shell pod for inspection when `debug` |

No CRDs and no cluster-scoped objects. The chart has no dependencies on
operators.

## Configuration model

`values.yaml` has two distinct halves, and the distinction matters:

- **`settings.*`** mirrors the application's own config schema
  (`rust/ovdc/src/config.toml`) and is rendered into `config.toml` key by key.
  Every key here must be one the binary deserializes. Adding a key the
  application does not know is silently ignored; removing one it requires
  **panics the process at startup**.
- **Everything else** is Kubernetes-level chart configuration.

The ConfigMap templates each setting explicitly rather than piping
`.Values.settings` through `toToml`. This is deliberate: Helm parses YAML
numbers as `float64`, so `toToml` emitted `4e+06` where the application
expected an integer, and the config failed to deserialize. Values are rendered
with `toJson`, whose scalar syntax is a subset of TOML. **Do not "simplify"
this back to plain `{{ .Values.x }}` interpolation** — that reintroduces the
exponent bug.

### Ports

Ports are configured in exactly one place — the `settings.*` values the
application actually binds:

| Port | Value |
|---|---|
| gRPC | `settings.grpc.port` |
| Prometheus metrics | `settings.telemetry.prometheusMetricsPort` |

Container ports, probes, and Service `targetPort`s all derive from these. There
are no separate `service.grpcPort` / `service.metricsPort` knobs; they were
removed because they could silently disagree with what the process bound.

### Storage sizing

`settings.store.maxSize` is the RocksDB on-disk limit and the garbage-collection
ceiling. Setting it too low throttles writes and forces constant GC.

The default `AUTO` derives it from the volume:

```
maxSize = storage.volume.size − min(20%, 100GB)
```

| Volume | Resulting target |
|---|---|
| 10Gi | 80.0% |
| 300Gi | 80.0% |
| 1Ti | 90.9% |
| 10Ti | 99.1% |

`AUTO` requires `storage.volume.enabled: true`. Otherwise set an explicit size
such as `"275G"`, which is used verbatim.

Note that `volumeClaimTemplates` are immutable — changing `storage.volume.size`
on an existing release is rejected on upgrade.

### TLS

Set the Secret name; the paths are fixed by the chart.

```bash
kubectl create secret tls ovdc-tls -n <namespace> --cert=cert.pem --key=key.pem
```

```yaml
settings:
  grpcTls:
    enabled: true
    secretName: ovdc-tls
```

The Secret is mounted read-only at `/etc/ovdc/tls`, and the application reads
`tls.crt` / `tls.key` from there. `secretName` is required when `enabled` is
true; the render fails otherwise.

For `includeCaRoot: true` the Secret must also carry `ca.crt`, which
`kubectl create secret tls` does not add:

```bash
kubectl create secret generic ovdc-tls -n <namespace> \
  --from-file=tls.crt=cert.pem \
  --from-file=tls.key=key.pem \
  --from-file=ca.crt=ca.pem
```

### Telemetry

Prometheus metrics are served on `settings.telemetry.prometheusMetricsPort` and
scraped via `prometheus.io/*` annotations on the headless Service. The metrics
server ignores the request path, so `monitoring.path` only needs to match your
scrape configuration.

OTLP export is separate and **requires a collector you provide**. This build
ships only the gRPC exporter — `http/protobuf` and `http/json` deserialize but
export nothing. Point `settings.otel*.otlp.endpoint` at a reachable collector,
or set `exporter: "none"` to disable it. Prometheus metrics are unaffected
either way.

The chart does not deploy a collector and does not package alert rules.

### Scheduling

`selfAntiAffinity` (default true) adds a *soft* preference to spread pods across
nodes. It is merged into `affinity`; anything you set there wins on conflict.
Set `selfAntiAffinity: false` to drop the chart's rule entirely.

## Common overrides

```yaml
replicas: 3

storage:
  volume:
    size: 1Ti
    storageClassName: managed-csi-premium

settings:
  store:
    maxSize: AUTO
  engine:
    cacheSize: "64G"
    blockCacheSize: "16G"

resources:
  requests:
    memory: 96G
```

Sizing note: `resources.requests.memory` should cover the row cache
(`engine.cacheSize`) plus the block cache (`engine.blockCacheSize`) plus write
buffers (`engine.cf.write_buffer_size` × `max_write_buffer_number`) plus roughly
6GB of TCP buffers.

## Validation

`values.schema.json` validates types, enums, and port ranges at
`helm install`/`template` time.

Objects whose key set this chart owns are **closed**: an unrecognised key fails
the render rather than being silently ignored. This mirrors the application's
own `deny_unknown_fields`, so a typo or a key from an older chart surfaces at
`helm upgrade` instead of quietly having no effect.

```
$ helm template ovdc ./docker/helm --set settings.store.maxSizee=1G
Error: values don't meet the specifications of the schema(s) in the following chart(s):
ovdc:
- at '/settings/store': additional properties 'maxSizee' not allowed
```

Three places stay deliberately open, because their contents are not this
chart's to enumerate:

- `settings.engine.db` / `cf` / `block` / `blobsCf` / `blobsBlock` — passed
  through to RocksDB verbatim.
- `global` — parent charts inject their own keys here.
- Kubernetes passthrough objects: `nodeSelector`, `affinity`, `resources`,
  `podAnnotations`, `podSecurityContext`, `securityContext`,
  `service.annotations`.

```bash
helm lint --strict ./docker/helm
helm template ovdc ./docker/helm | kubectl apply --dry-run=client -f -
```

The end-to-end test `test_helm_chart_config` (`rust/ovdc/src/tests.rs`) renders
this chart and boots an embedded database against the result, which is what
catches chart/application drift. It skips silently when `helm` is not on `PATH`,
so a local `cargo test` without helm installed still passes. CI installs helm
and sets `OVDC_REQUIRE_HELM=1`, which turns that skip into a failure. Note that
it renders **default values only** — a values file that overrides settings is
not covered by it.

## Upgrading from an earlier chart

The keys below were removed. Because the schema is now closed, a values file
that still sets one fails the render with the path it objected to — it will not
silently do nothing.

| Removed key | Replacement |
|---|---|
| `service.grpcPort` | Derived from `settings.grpc.port` |
| `service.metricsPort` | Derived from `settings.telemetry.prometheusMetricsPort` |
| `otelCollector.*` | The sidecar template was removed. Run a collector separately and point `settings.otel*.otlp.endpoint` at it. |
| `settings.grpcTls.cert` / `key` / `caRoot` | Paths are fixed at `/etc/ovdc/tls`; supply `settings.grpcTls.secretName` instead. See [TLS](#tls). |

Also removed, with no values-file equivalent: the `prom_rules.yaml` recording
rules and the `alerts/` manifests. If you relied on them, deploy them as
standalone `PrometheusRule` objects outside this chart.

## Running as non-root

The image and chart both run the container as uid/gid 1000 by default:
`podSecurityContext` and `securityContext` pin `runAsUser` / `runAsGroup`, set
`runAsNonRoot: true`, use `seccompProfile.type: RuntimeDefault`, drop all
capabilities, and disallow privilege escalation.

The application writes only under `/env`, which is always a mounted volume —
the PVC when `storage.volume.enabled` is true, an `emptyDir` when it is false.
`podSecurityContext.fsGroup` makes that volume group-writable, which is what
lets a non-root process create its environment and DB directories there.

If you override the uid, change all three of `runAsUser`, `runAsGroup` and
`fsGroup` together. Leaving `fsGroup` behind gives you a running container that
cannot write its database.

`readOnlyRootFilesystem` is not set. It is untested against RocksDB's temp-file
behaviour; verify a full compaction cycle before enabling it.

To deliberately return to running as root:

```bash
helm install ovdc ./docker/helm \
  --set podSecurityContext.runAsNonRoot=false --set podSecurityContext.runAsUser=0 \
  --set podSecurityContext.runAsGroup=0 --set podSecurityContext.fsGroup=0 \
  --set securityContext.runAsNonRoot=false --set securityContext.runAsUser=0 \
  --set securityContext.runAsGroup=0
```

## Gotchas

- **`debug: true`** replaces the main container's command with `sleep`. The
  service does not run. Never enable in production.
- **`storage.volume.enabled: false`** makes the database ephemeral — all data is
  lost on restart or reschedule. Dev/test only.
- **`--set` cannot express floats.** `--set settings.otelTraces.samplerArg=0.5`
  yields the string `"0.5"`. Use `--set-json` or a values file.
- **`settings.otelTraces.sampler` / `samplerArg`** are required by the config
  schema but nothing currently reads them; sampling is not applied. Do not
  remove them — the application panics if they are absent.
