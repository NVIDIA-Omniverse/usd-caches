{{/*
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
*/}}
{{/*
Expand the name of the chart.
*/}}
{{- define "ovdc.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "ovdc.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "ovdc.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "ovdc.labels" -}}
helm.sh/chart: {{ include "ovdc.chart" . }}
{{ include "ovdc.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "ovdc.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ovdc.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Convert a Kubernetes quantity such as "300Gi", "275G" or a bare byte count to
an integer number of bytes.

The multipliers deliberately match the application's own parser in
rust/ovdc_bytes/src/human_serde/size.rs, so the chart and the binary agree on
what each unit means. An unrecognised unit fails the render rather than
silently producing a wrong size.
*/}}
{{- define "ovdc.sizeToBytes" -}}
{{- $q := . | toString -}}
{{- $unit := regexFind "[A-Za-z]+$" $q -}}
{{- $mult := dict "" 1.0 "B" 1.0 "Ki" 1024.0 "K" 1000.0 "Mi" 1048576.0 "M" 1000000.0 "Gi" 1073741824.0 "G" 1000000000.0 "Ti" 1099511627776.0 "T" 1000000000000.0 -}}
{{- if not (hasKey $mult $unit) -}}
{{- fail (printf "unrecognised size unit %q in %q" $unit $q) -}}
{{- end -}}
{{- mulf ($q | trimSuffix $unit | float64) (index $mult $unit) | int64 -}}
{{- end }}

{{/*
Byte count for `settings.store.maxSize: AUTO`.

Derives the DB target from the provisioned volume: the full volume size less
min(20%, 100GB) of headroom. The clamp stops the reserve growing without bound
on large volumes — 20% of a 10Ti disk would waste 2TB.

Requires a volume to derive from; AUTO with `storage.volume.enabled: false`
fails the render rather than inventing a cap.
*/}}
{{- define "ovdc.autoMaxSizeBytes" -}}
{{- if not .Values.storage.volume.enabled -}}
{{- fail "settings.store.maxSize: \"AUTO\" requires storage.volume.enabled=true; set an explicit size instead" -}}
{{- end -}}
{{- $volume := include "ovdc.sizeToBytes" .Values.storage.volume.size | float64 -}}
{{- subf $volume (minf (mulf $volume 0.2) 100000000000.0) | int64 -}}
{{- end }}

{{/*
The ports the application actually binds.

`settings.*` is the single source of truth: settings.grpc.port is what the gRPC
listener binds (rust/ovdc/src/lib.rs:82) and settings.telemetry.prometheusMetricsPort
is what the metrics HTTP server binds (rust/ovdc_trace/src/lib.rs:334-340).
Container ports, probes and Service targetPorts must all follow these, or the
pod fails its probes and metrics scraping silently returns nothing.
*/}}
{{- define "ovdc.grpcPort" -}}{{ .Values.settings.grpc.port | int64 }}{{- end }}
{{- define "ovdc.metricsPort" -}}{{ .Values.settings.telemetry.prometheusMetricsPort | int64 }}{{- end }}

{{/*
Directory the gRPC TLS secret is mounted into.

Fixed by the chart rather than configurable: the certificate paths written into
config.toml must agree with the volume mount, and deriving one from the other
is what previously mounted the secret over "/".

Filenames follow the kubernetes.io/tls convention (tls.crt, tls.key), plus
ca.crt when settings.grpcTls.includeCaRoot is set.
*/}}
{{- define "ovdc.tlsMountPath" -}}/etc/ovdc/tls{{- end }}

{{/*
increaseParallelism/threadsHigh/threadsLow for presets.cpu.

presets.cpu is closed by values.schema.json (1-8), so the `fail` below is
defense in depth for direct template invocations that skip schema validation,
not the primary error path.

Built from a YAML literal (rather than chained `dict` calls) so each row can
carry a plain YAML comment -- Go template actions can't be commented mid-call.
*/}}
{{- define "ovdc.cpuPreset" -}}
{{- $presets := fromYaml `
# vCPUs: increaseParallelism, threadsHigh, threadsLow
"1": {increaseParallelism: 1, threadsHigh: 1, threadsLow: 1}
"2": {increaseParallelism: 2, threadsHigh: 1, threadsLow: 1}
"3": {increaseParallelism: 3, threadsHigh: 2, threadsLow: 1}
"4": {increaseParallelism: 4, threadsHigh: 2, threadsLow: 2}
"5": {increaseParallelism: 5, threadsHigh: 3, threadsLow: 2}
"6": {increaseParallelism: 6, threadsHigh: 3, threadsLow: 3}
"7": {increaseParallelism: 7, threadsHigh: 4, threadsLow: 3}
"8": {increaseParallelism: 8, threadsHigh: 4, threadsLow: 4}
` -}}
{{- $key := . | toString -}}
{{- if not (hasKey $presets $key) -}}
{{- fail (printf "presets.cpu: unsupported value %v (must be one of 1-8)" .) -}}
{{- end -}}
{{- index $presets $key | toYaml -}}
{{- end }}

{{/*
cacheSize/blockCacheSize/blobsCfMaxWriteBufferNumber for presets.ram.

presets.ram is closed by values.schema.json, so the `fail` below is defense
in depth for direct template invocations that skip schema validation, not the
primary error path.

Built from a YAML literal (rather than chained `dict` calls) so each row can
carry a plain YAML comment -- Go template actions can't be commented mid-call.
*/}}
{{- define "ovdc.ramPreset" -}}
{{- $s := . | toString -}}
{{- if not (regexMatch "^[0-9]+GB$" $s) -}}
{{- fail (printf "presets.ram: unsupported value %q (must be a whole number of GB from 4GB to 96GB, e.g. \"10GB\")" .) -}}
{{- end -}}
{{- $n := int (trimSuffix "GB" $s) -}}
{{- if or (lt $n 4) (gt $n 96) -}}
{{- fail (printf "presets.ram: unsupported value %q (must be a whole number of GB from 4GB to 96GB, e.g. \"10GB\")" .) -}}
{{- end -}}
{{- /* Block cache in GB at each anchor size, linearly interpolated (integer
       division, rounding down) in between. cacheSize is the remainder, so the
       two always sum to the named value. Above 48GB the block cache takes 25%
       of each extra GB (1G per 4GB), since index/filter blocks grow with the
       dataset and the row cache alone would otherwise absorb all the growth. */ -}}
{{- $anchors := list (list 4 3) (list 6 4) (list 8 4) (list 12 5) (list 16 6) (list 24 8) (list 32 10) (list 48 12) (list 96 24) -}}
{{- $block := 3 -}}
{{- range $i, $hi := $anchors -}}
{{- if $i -}}
{{- $lo := index $anchors (sub $i 1) -}}
{{- if and (gt $n (index $lo 0)) (le $n (index $hi 0)) -}}
{{- $block = add (index $lo 1) (div (mul (sub (index $hi 1) (index $lo 1)) (sub $n (index $lo 0))) (sub (index $hi 0) (index $lo 0))) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- $blobs := ternary 24 32 (lt $n 5) -}}
{{- dict "cacheSize" (printf "%dG" (sub $n $block)) "blockCacheSize" (printf "%dG" $block) "blobsCfMaxWriteBufferNumber" $blobs | toYaml -}}
{{- end }}

{{/*
Render key values; such as annotations
*/}}
{{- define "tplvalues.render" -}}
{{- $value := typeIs "string" .value | ternary .value (.value | toYaml) }}
{{- if contains "{{" (toJson .value) }}
  {{- if .scope }}
      {{- tpl (cat "{{- with $.RelativeScope -}}" $value "{{- end }}") (merge (dict "RelativeScope" .scope) .context) }}
  {{- else }}
    {{- tpl $value .context }}
  {{- end }}
{{- else }}
    {{- $value }}
{{- end }}
{{- end -}}
