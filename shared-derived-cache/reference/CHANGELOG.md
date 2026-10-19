<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Changelog

## [6.0.1]

### Fixed

- **Prometheus metrics stopped being collected when `otelMetrics.exporter` was set to anything other than `"otlp"`.** `build_otel_metrics` built the entire `SdkMeterProvider` -- including the Prometheus pull reader that's supposed to work independently of OTLP export -- behind the same early return that skipped building the OTLP push reader. Setting `otelMetrics.exporter = "none"` (or `"console"`, which isn't implemented) silently zeroed out `/metrics` too, not just OTLP export. The provider now always registers the pull reader; only the OTLP reader is conditional.
- **Fixed OpenMetrics compliance bugs in the `/metrics` endpoint.** The response had no `Content-Type` header, a freeform `# Scope: ...` comment line isn't a recognized OpenMetrics comment type, and `# UNIT` lines carried OTEL's raw unit strings (e.g. `"By"`) even though OpenMetrics requires the unit to literally be a suffix of the metric name. Added the `application/openmetrics-text` content type, dropped the non-compliant `# Scope` comment, and stopped emitting `# UNIT` (optional, and never valid for our unit strings).
- **`ovdc_trace::span_extractor` no longer logs an `ERROR` on every gRPC request when tracing export is disabled.** `apply_distributed_trace` unconditionally logged any `set_parent` failure at `error!`, including `SetParentError::LayerNotFound`, which `tracing_opentelemetry` returns on every call when no OpenTelemetry layer is registered (i.e. `otelTraces: none`). That's an expected, not exceptional, condition, so it's now ignored entirely; the other `SetParentError` variants (`AlreadyStarted`, `SpanDisabled`) are logged at `debug!` instead of `error!`.
- **The startup version log no longer reports `UNKNOWN`.**
  `ovdc_version::VERSION_STR` was `include_str!` of a checked-in file that
  nothing ever wrote, so every build -- 5.2.0 included -- logged
  `ovdc version version=UNKNOWN`, and a running pod's logs could not identify
  the build. The version is now baked in at compile time from an `OVDC_VERSION`
  build arg, which CI sets from the chart's `appVersion` plus the short commit
  SHA (e.g. `6.0.0+8560c55`). Local `cargo build`s, which have no such
  variable, report `dev`.

## [6.0.0]

### Breaking Changes

- **Project renamed from DDCS (Derived Data Cache Service) to OVDC (Omniverse Derived Cache).** The `ddcs` token was replaced with `ovdc` everywhere, including runtime- and wire-visible names. Operators must update deployments in lockstep:
  - Container image repository `nvidia/omniverse/ddcs-dist-kv` → `nvidia/omniverse/ovdc-dist-kv`; the standalone debug image ref `.../ov-ddcs` → `.../ovdc`.
  - Helm chart name `ddcs` → `ovdc`; generated Kubernetes resource, service, and label names follow the chart name.
  - Prometheus metric prefix `ddcs_*` → `ovdc_*` (the OpenTelemetry `service.name` resource attribute is now `ovdc`). Existing dashboards, recording rules, and alerts must be migrated; the bundled Grafana dashboards already are.
  - gRPC proto package `ddcs` → `ovdc`. Clients generated against the old package must be regenerated.
  - TLS secret mount path `/etc/ddcs/tls` → `/etc/ovdc/tls`.
  - Rust crates and their directories `ddcs*` → `ovdc*` (`ddcs_map` → `ovdc_map`, etc.).
  - CI variables `TAG_DDCS_OUT` / `BUILD_ARGS_DDCS` → `TAG_OVDC_OUT` / `BUILD_ARGS_OVDC`, and the test-gate variable `DDCS_REQUIRE_HELM` → `OVDC_REQUIRE_HELM`.

  The TOML configuration file format and key names are unchanged — the service takes its settings from `config.toml`, not from a `DDCS_`-prefixed environment namespace, so no config migration is needed.
- **Helm chart renamed `ovdc` → `ovderivedcache-helm`.** Generated resource,
  service and label names derive from the chart name, so a release that
  previously produced `<release>-ovdc` now produces
  `<release>-ovderivedcache-helm`. Helm will not upgrade in place across the
  rename — the new objects do not match the old selectors. Either reinstall, or
  set `nameOverride: ovdc` to keep the previous names. The container inside the
  pod is still named `ovdc`.
- **The Helm release image repository was corrected to match the published image.**
  `image.repository` previously pointed at `nvidia/omniverse/ovdc-dist-kv`, a
  path no pipeline published to. Review `image.registry` and `image.repository`
  overrides when upgrading so the chart pulls an image available to your
  deployment.

### Removed

- **`docker/o11y-stack` is gone.** It held a vendored fluent-bit chart and a
  `kube-prometheus-stack` values file for a development deployment installed
  by hand with manually created secrets. Nothing in the repo referenced the
  directory. Deploy log collection and Prometheus separately if needed. The
  OVDC chart's `prometheus.io/*` annotations, ServiceMonitor, and OTel
  collector sidecar are unaffected.

### Fixed

- **Release images are now tagged with the release version.** Every release
  image was previously tagged with the commit SHA alone (`TAG: github.sha`,
  never overridden), so no `6.0.0` image ever existed in any registry while the
  chart asked clusters to pull exactly that. A released chart could not pull
  its own image. Release images now carry the git tag name and the chart's
  `appVersion` alongside the SHA.
- **Release images are promoted to NGC.** The pipeline builds and pushes to
  GHCR, but the chart directs clusters to `nvcr.io`, and nothing copied between
  them. A `promote-image` job now mirrors the finished multi-arch index to NGC
  on tag builds, and verifies each promoted tag resolves before the job
  succeeds.
- The chart is also published as an OCI artifact to GHCR alongside the image.
- **SBOM image filtering now matches the release image repository.** The
  filter previously tracked the old `ovdc-dist-kv` path. Its substring match
  would have silently excluded the chart's image after the repository rename.
- **The SBOM generator image no longer runs as root.** `docker/sbom-generator`
  ran its entrypoint as uid 0, which image scanners flag. It now runs as
  65532:65532; BuildKit mounts the scanned rootfs read-only and an output
  directory the invoking user can write, so the scan produces the same package
  set with no privileges. Rebuild and republish the pinned generator tag for
  the change to reach release attestations.

## [5.2.0]

### Added

- Release artifacts now include per-architecture binaries, binary SBOMs, vendored source SBOMs, `sources.tar.gz`, and `third_party_licenses.md`.

## [5.1.1]

### Added

- **gRPC server call metrics (OpenTelemetry).** The HTTP/gRPC stack now records per-RPC metrics using `tower_http::trace::TraceLayer` hooks and application-level instrumentation:
  - `grpc.server.call.started` — counter; one increment per RPC start, labeled with `grpc.method` (`proto.KeyValue/Get`, `proto.KeyValue/Put`, `proto.KeyValue/Touch`, or `other`).
  - `grpc.server.call.duration` — histogram (seconds); end-to-end RPC duration on successful stream completion or HTTP-layer failure, labeled with `grpc.method` and `grpc.status` (canonical `google.rpc.Code` name, e.g. `OK`, `UNAVAILABLE`).
  - `grpc.server.call.sent_total_compressed_message_size` — histogram (bytes); outbound message sizes for streaming responses (e.g. Get chunks, Put acknowledgements), labeled with `grpc.method` and `grpc.status`.
  - `grpc.server.call.rcvd_total_compressed_message_size` — histogram (bytes); inbound sizes from `Content-Length` where present, plus aggregate received bytes for Put streams, labeled with `grpc.method` and `grpc.status`.

## [5.1]

### Breaking Changes

- **Configuration format changed from YAML to TOML.** The server configuration file is now TOML. Existing YAML configs must be converted before upgrading.

- **Engine options restructured.** The `engine` field in config was previously a flat string list. It is now a structured TOML table with namespaced keys grouped by prefix (`db.*`, `cf.*`, `block.*`, `sys.*`). Example:
  ```toml
  [engine]
  db.max_open_files = 128
  cf.enable_blob_files = true
  block.block_size = 16384
  ```

- **gRPC config fields renamed.** Duration and size fields now use human-readable types and have been renamed to drop the `_seconds` suffix:
  - `timeout_seconds` → `timeout` (e.g., `"30s"`)
  - `write_timeout_seconds` → `write_timeout`
  - `tcp_keepalive_after_idle_seconds` → `tcp_keepalive_after_idle_time`
  - `tcp_keepalive_interval_seconds` → `tcp_keepalive_interval`
  - `http2_keepalive_interval_seconds` → `http2_keepalive_interval`
  - `http2_keepalive_timeout_seconds` → `http2_keepalive_timeout`
  - `initial_stream_window_size` and `initial_connection_window_size` now accept integer byte values

- **Helm values flattened.** All values previously nested under `cluster:` are now top-level. Update your `values.yaml` overrides:
  - `cluster.replicas` → `replicas`
  - `cluster.nodeSelector` → `nodeSelector`
  - `cluster.tolerations` → `tolerations`
  - `cluster.affinity` → `affinity`
  - `cluster.selfAntiAffinity` → `selfAntiAffinity`
  - `cluster.podAnnotations` → `podAnnotations`
  - `cluster.podSecurityContext` → `podSecurityContext`
  - `cluster.otelCollector` → `otelCollector`

### Changes

- **Metrics now exported via OpenTelemetry.** All server metrics are now emitted using the OpenTelemetry SDK. Configure an OTLP-compatible collector endpoint via the `otelCollector` Helm values or `otelHost`/`otelPort` settings.

- **Helm ServiceMonitor disabled when OTel collector sidecar is enabled.** The two monitoring modes are now mutually exclusive; enabling `otelCollector.enabled: true` automatically disables the Prometheus `ServiceMonitor`.

- **Dependency updates.** Project dependencies were updated.
