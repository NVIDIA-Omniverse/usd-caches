<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# OVDC dashboards

Import `main.json`, `read.json`, and `write.json` into the same Grafana instance.
Keep their dashboard UIDs so the cluster view's read and write links resolve.
Select your Prometheus datasource and namespace in each dashboard.

## Metrics

Enable `telemetry.prometheusMetricsExposition` in your OVDC configuration and
configure Prometheus to scrape the OVDC metrics port (`3051` by default) at
`/metrics`. The dashboards expect a `namespace` label on scraped metrics; add
that label in your scrape configuration or through Kubernetes service discovery.
The Helm chart adds scrape annotations when `prometheusAnnotation` is enabled;
your Prometheus installation must be configured to discover annotated services.

## Logs and operational monitoring

Collect OVDC process or container logs with your existing logging agent. For
OpenTelemetry logs, enable `otel.enabled`, set `otelLogs.exporter` to `"otlp"`,
and set `otelLogs.otlp.endpoint` to your own collector's gRPC endpoint. Configure
that collector to forward logs to your logging backend. See the available
settings in [config.toml](../rust/ovdc/src/config.toml).

Edit the cluster view's text panel to add links to your own log search and
runbooks. Scope log searches to the OVDC namespace and pod, and document how to
investigate request errors, write stalls, and low storage capacity. Use your
infrastructure monitoring for filesystem usage and host health, and configure
alerts in your monitoring system for the conditions your deployment needs.
