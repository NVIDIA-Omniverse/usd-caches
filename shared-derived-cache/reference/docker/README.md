<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# OpenTelemetry

OVDC can send traces, logs, and metrics to any reachable OTLP/gRPC collector.
The examples below use the public OpenTelemetry Collector and print received
data to its logs. No access token is required. These are development examples;
configure a storage backend for persistent telemetry.

Run commands from the repository root. For application build prerequisites, see
[the project README](../README.md); for deployment prerequisites and OVDC image
and storage overrides, see [the Helm chart README](helm/README.md).

## Local collector

With Docker running, start the collector in one terminal:

```sh
docker run --rm --name ovdc-otel-collector \
  -p 127.0.0.1:4317:4317 \
  -v "$PWD/docker/otel-collector-config.yaml:/etc/otelcol/config.yaml:ro" \
  otel/opentelemetry-collector:0.161.0 \
  --config=/etc/otelcol/config.yaml
```

In another terminal, run OVDC on the host using the supplied application config:

```sh
cd rust
cargo run --bin main -- ./target/env ./ovdc/src/config.toml
```

That config already selects `exporter = "otlp"`, `otlp.protocol = "grpc"`, and
`otlp.endpoint = "http://localhost:4317"` in each of `[otelTraces]`, `[otelLogs]`,
and `[otelMetrics]`. Startup logs should appear in the collector output; metrics
are exported every 30 seconds. Send requests to OVDC to produce request traces.
The collector's `debug` exporter prints payloads and does not retain them.
Stop OVDC and press Ctrl-C in the collector terminal to clean up.

When OVDC also runs in a container, attach both containers to the same Docker
network and use `http://ovdc-otel-collector:4317` as the endpoint in all three
application config sections. `localhost` inside a container refers to that
container.

## Collector in Kubernetes

This example requires Helm, kubectl, and a Kubernetes cluster. Install the
collector in the same namespace as OVDC (here, `ovdc`):

```sh
helm repo add open-telemetry https://open-telemetry.github.io/opentelemetry-helm-charts
helm repo update open-telemetry
helm upgrade --install ovdc-opentelemetry-collector \
  open-telemetry/opentelemetry-collector --version 0.175.1 \
  --namespace ovdc --create-namespace \
  --values docker/otel-collector-values.yaml --wait
```

The supplied values fix the Service name to `ovdc-otel-collector` and expose
OTLP/gRPC on port 4317. Save the following OVDC overrides to
`/tmp/ovdc-telemetry-values.yaml`:

```yaml
settings:
  otelTraces:
    exporter: otlp
    otlp:
      protocol: grpc
      endpoint: http://ovdc-otel-collector:4317
  otelLogs:
    exporter: otlp
    otlp:
      protocol: grpc
      endpoint: http://ovdc-otel-collector:4317
  otelMetrics:
    exporter: otlp
    otlp:
      protocol: grpc
      endpoint: http://ovdc-otel-collector:4317
```

Add that file after your existing deployment values so the telemetry settings
win. For example, with your image, storage, and sizing overrides in
`/tmp/ovdc-deployment-values.yaml`:

```sh
helm upgrade --install ovdc ./docker/helm --namespace ovdc \
  --values /tmp/ovdc-deployment-values.yaml \
  --values /tmp/ovdc-telemetry-values.yaml --wait
kubectl logs --namespace ovdc deployment/ovdc-otel-collector --follow
```

Generate requests and wait at least 30 seconds to see traces, logs, and metrics.
If the collector runs in another namespace, use its Service DNS name, such as
`http://ovdc-otel-collector.observability.svc.cluster.local:4317`. Allow traffic
from OVDC to the collector on TCP port 4317 in any applicable NetworkPolicies.
To remove the example collector:

```sh
helm uninstall ovdc-opentelemetry-collector --namespace ovdc
```

## Your own collector or backend

Set the endpoint in all three signal sections above to your collector's
OTLP/gRPC address. Use a full URI with no `/v1/traces`, `/v1/logs`, or
`/v1/metrics` path; those paths belong to OTLP/HTTP, which this build does not
export. Kubernetes values use `settings.otelTraces`, `settings.otelLogs`, and
`settings.otelMetrics`; standalone TOML uses the corresponding top-level
sections.

For a backend that requires authentication or TLS, configure the collector to
forward to it using its documented exporter, certificates, and credentials.
The OVDC chart does not provide an OTLP headers setting. Keep backend tokens in
your deployment's secret storage and inject them into the collector; do not
commit them to these examples. Replace the `debug` exporter and each pipeline's
exporter list when forwarding to your backend. See the upstream
[Collector configuration guide](https://opentelemetry.io/docs/collector/configuration/)
for pipeline, authentication, and TLS configuration.

Disable a signal by setting its `exporter` to `none`. To disable all OTLP
export, set that value on all three signals. The Prometheus scrape endpoint is
controlled separately by `settings.telemetry.prometheusMetricsExposition` and
`settings.telemetry.prometheusMetricsPort` (or `[telemetry]` in TOML).

Validate the standalone collector config before running it:

```sh
docker run --rm \
  -v "$PWD/docker/otel-collector-config.yaml:/etc/otelcol/config.yaml:ro" \
  otel/opentelemetry-collector:0.161.0 \
  validate --config=/etc/otelcol/config.yaml
```
