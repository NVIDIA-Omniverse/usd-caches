{{- /*
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
*/ -}}
{{/*
Expand the name of the chart.
*/}}
{{- define "chart.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "chart.fullname" -}}
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
Rendered Kubernetes resource name for StatefulSet, Service, and other release-scoped
resources. When compat.useUccDefaults is enabled, preserve the old UCC chart
resource name (usd-content-cache) so upgrades are near no-op and PVCs are reused.
*/}}
{{- define "chart.compatFullname" -}}
{{- if .Values.compat.useUccDefaults -}}
{{- $oldChartName := .Values.compat.uccDefaults.chartName | default "usd-content-cache" -}}
{{- if contains $oldChartName .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $oldChartName | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- else -}}
{{- include "chart.fullname" . -}}
{{- end -}}
{{- end -}}

{{/*
Aborts when the rendered Service name would make kubelet inject
OVCC_CONFIG_* service-link variables. Those collide with the Rust engine's
configuration override prefix. Use the compatibility-aware name because it is
the actual Service name during UCC-compatible upgrades.
*/}}
{{- define "chart.guardServiceLinkPrefix" -}}
{{- $serviceName := include "chart.compatFullname" . -}}
{{- $envName := $serviceName | upper | replace "-" "_" -}}
{{- if hasPrefix "OVCC_CONFIG" $envName -}}
{{- fail (printf "Release/service name %q would make the kubelet inject OVCC_CONFIG_* service-link vars that collide with the engine config env prefix; rename the release." $serviceName) -}}
{{- end -}}
{{- end -}}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "chart.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "chart.labels" -}}
helm.sh/chart: {{ include "chart.chart" . }}
{{ include "chart.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Application name used in selectors. With compat enabled, keep the UCC chart name so
StatefulSet selectors stay stable across v3.0.3 (hc) -> OVCC upgrades.
*/}}
{{- define "chart.compatName" -}}
{{- if .Values.compat.useUccDefaults -}}
{{- .Values.compat.uccDefaults.chartName | default "usd-content-cache" -}}
{{- else -}}
{{- include "chart.name" . -}}
{{- end -}}
{{- end -}}

{{/*
Selector labels
*/}}
{{- define "chart.selectorLabels" -}}
app.kubernetes.io/name: {{ include "chart.compatName" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "func.toSnakeCaseJson" -}}
{{- $input := . -}}
{{- $output := dict -}}
{{- range $key, $value := $input -}}
    {{- $snakeKey := $key | snakecase -}}
    {{- if kindOf $value | eq "map" -}}
        {{- $_ := set $output $snakeKey (include "func.toSnakeCaseJson" $value | fromJson) -}}
    {{- else -}}
        {{- $_ := set $output $snakeKey $value -}}
    {{- end -}}
{{- end -}}
{{- toJson $output -}}
{{- end -}}

{{/*
The DNS suffix every virtual-host cache address shares. OVCC is always installed
in-cluster, so the suffix is the release namespace's service domain
(<namespace>.svc.cluster.local). A client reaches pypi.org through
pypi.org.<namespace>.svc.cluster.local.
*/}}
{{- define "func.registryCacheSuffix" -}}
{{- printf "%s.svc.cluster.local" .Release.Namespace -}}
{{- end -}}

{{/*
Returns true when a registry cache PVC or volumeClaimTemplate already exists
in the live cluster. Used to keep registry storage on upgrade paths where the
backend was previously enabled, without materializing it on fresh downlevel
upgrades while registry.include stays false.
*/}}
{{- define "func.registryVolumeExistsInCluster" -}}
{{- $found := dict "v" false -}}
{{- $fullName := include "chart.compatFullname" . -}}
{{- $namespace := .Release.Namespace -}}
{{- $statefulSet := lookup "apps/v1" "StatefulSet" $namespace $fullName -}}
{{- if $statefulSet -}}
  {{- range default (list) $statefulSet.spec.volumeClaimTemplates -}}
    {{- if eq .metadata.name "registry" -}}
      {{- $_ := set $found "v" true -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- if not ($found.v) -}}
  {{- range $i := until (int .Values.replicaCount) -}}
    {{- $pvcName := printf "registry-%s-%d" $fullName $i -}}
    {{- if lookup "v1" "PersistentVolumeClaim" $namespace $pvcName -}}
      {{- $_ := set $found "v" true -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- if $found.v -}}true{{- else -}}false{{- end -}}
{{- end -}}

{{/*
Returns true when a persistence.volumes entry should become a mount and storage
claim. The registry volume is gated: it materializes only when the backend is
enabled or the cluster already has registry storage, so downlevel helm upgrades
do not mutate StatefulSet volumeClaimTemplates.
*/}}
{{- define "func.shouldMaterializeCacheVolume" -}}
{{- $root := .root -}}
{{- $vol := .volume -}}
{{- if ne $vol.name "registry" -}}true
{{- else if $root.Values.nginx.backends.registry.include -}}true
{{- else if eq (include "func.registryVolumeExistsInCluster" $root) "true" -}}true
{{- else -}}false
{{- end -}}
{{- end -}}

{{/*
Per-backend version-gated caching policy. Defaults to true when omitted.
*/}}
{{- define "func.backendRequiresVersioning" -}}
{{- if and (hasKey . "requiresVersioning") (not .requiresVersioning) -}}
false
{{- else -}}
true
{{- end -}}
{{- end -}}

{{/*
NGINX variable value ("1" or "0") for $requires_versioning.
*/}}
{{- define "func.backendRequiresVersioningVar" -}}
{{- if and (hasKey . "requiresVersioning") (not .requiresVersioning) -}}0{{- else -}}1{{- end -}}
{{- end -}}

{{/*
Fails when the removed global nginx.proxyCache.requiresVersioning key is still supplied.
*/}}
{{- define "func.validateRequiresVersioningDeprecation" -}}
{{- if hasKey .Values.nginx.proxyCache "requiresVersioning" -}}
{{- fail "OVCC install blocked: nginx.proxyCache.requiresVersioning has been removed. Remove it and set nginx.backends.<name>.requiresVersioning instead (e.g. nginx.backends.s3.requiresVersioning=false for mutable/Artifactory mode, nginx.backends.registry.requiresVersioning=false for package registries)." -}}
{{- end -}}
{{- end -}}

{{/*
Peer fill requires valid discovery and listener configuration. Network isolation
is recommended separately through func.peerNetworkPolicyWarning because an
operator may enforce equivalent isolation outside this chart.
*/}}
{{- define "func.validatePeer" -}}
{{- if hasKey .Values.peer "maxConcurrentServes" -}}
  {{- fail "OVCC install blocked: peer.maxConcurrentServes has been removed. Remove the key; peer body serves now follow measured socket and network backpressure." -}}
{{- end -}}
{{- if .Values.peer.enabled -}}
  {{- if le (int .Values.peer.budgetMs) 0 -}}
    {{- fail "OVCC install blocked: peer.enabled requires peer.budgetMs greater than zero." -}}
  {{- end -}}
  {{- if not .Values.service.headless.enabled -}}
    {{- fail "OVCC install blocked: peer.enabled requires service.headless.enabled=true for replica discovery." -}}
  {{- end -}}
  {{- if or (not .Values.service.peer) (le (int .Values.service.peer.port) 0) (le (int .Values.service.peer.containerPort) 0) -}}
    {{- fail "OVCC install blocked: peer.enabled requires positive service.peer.port and service.peer.containerPort values." -}}
  {{- end -}}
{{- end -}}
{{- end -}}

{{/*
Warns when the unauthenticated peer port is enabled without the chart-managed
namespace isolation. This remains non-blocking because operators may enforce an
equivalent policy outside this chart.
*/}}
{{- define "func.peerNetworkPolicyWarning" -}}
{{- if and .Values.peer.enabled (not .Values.networkPolicy.enabled) -}}
{{- printf "peer.enabled=true while networkPolicy.enabled=false. The unauthenticated peer port is not isolated to the release namespace by this chart and may be reachable by other workloads allowed by the cluster network. Enable networkPolicy.enabled=true unless equivalent isolation is enforced externally." -}}
{{- end -}}
{{- end -}}

{{/*
Returns a non-empty warning when the registry backend declares contentRewrite
rules while matching is "direct". contentRewrite rewrites in-response URLs to the
cluster-local suffixed form, which only matters when clients reach the cache
through that suffix (suffix and any modes). In direct mode the body filter still
runs per response but nets zero, so it is pure overhead. Surfaced through
NOTES.txt as a warning rather than a hard failure, since the configuration is
wasteful but not broken.
*/}}
{{- define "func.registryContentRewriteMatchingWarning" -}}
{{- $config := .Values.nginx.backends.registry -}}
{{- if and $config.include (eq (default "any" $config.matching) "direct") -}}
{{- $hasRewrite := false -}}
{{- range $config.upstreams -}}
{{- if .contentRewrite -}}{{- $hasRewrite = true -}}{{- end -}}
{{- end -}}
{{- if $hasRewrite -}}
{{- printf "nginx.backends.registry sets contentRewrite while matching=%q. contentRewrite only takes effect when clients use the cluster-local suffix (matching suffix or any); in direct mode the body filter runs per response but nets zero, adding overhead. Remove the contentRewrite rules or set matching to suffix or any." (default "direct" $config.matching) -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Validates the virtual-host cache configuration at render time so misconfiguration
fails the install with an actionable message rather than producing a silently
wrong nginx config. Only runs when nginx.backends.registry.include is true.
Checks:
  1. every upstream has a non-empty name and at least one host;
  2. upstream names are unique;
  3. a host belongs to at most one upstream (no cross-upstream collision
     that would make cache routing ambiguous);
  4. each contentRewrite substitute has a non-empty match and replace, and the
     match references one of that upstream's own hosts (a rewrite whose
     source host is not fronted by this upstream is almost certainly a typo).
*/}}
{{- define "func.validateRegistryCache" -}}
{{- if .Values.nginx.backends.registry.include -}}
{{- $seenNames := dict -}}
{{- $seenHosts := dict -}}
{{- range $idx, $upstream := .Values.nginx.backends.registry.upstreams -}}
  {{- if not $upstream.name -}}
    {{- fail (printf "OVCC install blocked: nginx.backends.registry.upstreams[%d] has no name. Every upstream needs a unique name (for example \"pip\")." $idx) -}}
  {{- end -}}
  {{- if hasKey $seenNames $upstream.name -}}
    {{- fail (printf "OVCC install blocked: duplicate virtual-host cache upstream name %q. Upstream names must be unique." $upstream.name) -}}
  {{- end -}}
  {{- $_ := set $seenNames $upstream.name true -}}
  {{- if not $upstream.hosts -}}
    {{- fail (printf "OVCC install blocked: virtual-host cache upstream %q has no hosts. List at least one real host to cache (for example pypi.org)." $upstream.name) -}}
  {{- end -}}
  {{- range $upstream.hosts -}}
    {{- if hasKey $seenHosts . -}}
      {{- fail (printf "OVCC install blocked: upstream host %q is claimed by more than one virtual-host cache upstream (also %q). Each host may belong to a single upstream." . (get $seenHosts .)) -}}
    {{- end -}}
    {{- $_ := set $seenHosts . $upstream.name -}}
  {{- end -}}
  {{- range $upstream.contentRewrite -}}
    {{- $sub := .substitute -}}
    {{- if or (not $sub) (not $sub.match) (not $sub.replace) -}}
      {{- fail (printf "OVCC install blocked: virtual-host cache upstream %q has a contentRewrite with an empty substitute.match or substitute.replace." $upstream.name) -}}
    {{- end -}}
    {{- $matchesAHost := false -}}
    {{- range $upstream.hosts -}}
      {{- if contains . $sub.match -}}
        {{- $matchesAHost = true -}}
      {{- end -}}
    {{- end -}}
    {{- if not $matchesAHost -}}
      {{- fail (printf "OVCC install blocked: virtual-host cache upstream %q has a contentRewrite whose substitute.match (%q) does not reference any of its hosts (%s). The rewrite source host must be one this upstream fronts." $upstream.name $sub.match (join ", " $upstream.hosts)) -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Outputs either include cache-generation.conf or set $cache_generation "0".
Use the Lua include only when any backend has allowCacheReset enabled.
*/}}
{{- define "func.cacheGeneration" -}}
{{- $cacheReset := dict "enabled" false }}
{{- range $name, $backend := .Values.nginx.backends }}
{{- if $backend.allowCacheReset }}
{{- $_ := set $cacheReset "enabled" true }}
{{- end }}
{{- end }}
{{- if $cacheReset.enabled }}
include /etc/nginx/conf.d/sites/cache-generation.conf;
{{- else }}
set $cache_generation "0";
{{- end }}
{{- end -}}

{{- define "func.upstreamKeepaliveResolve" -}}
{{- if .enableUpstreamKeepalive }}
{{- if and (hasKey . "bridgeResolver") .bridgeResolver }}
set $bridge_resolver "{{ .bridgeResolver }}";
{{- else }}
set $bridge_resolver "";
{{- end }}
set $upstream_target "{{ tpl .proxyPass . }}";
set $upstream_keepalive_host "";
set $upstream_keepalive_scheme "";
rewrite_by_lua_block {
    local balancer = require "lua-balancer"
    local peer = balancer.resolve_for_keepalive(ngx.var.upstream_target)
    if peer then
        ngx.ctx.upstream_peer = peer
        ngx.var.upstream_keepalive_host = peer.hostname
        ngx.var.upstream_keepalive_scheme = peer.is_tls and "https" or "http"
    else
        ngx.log(ngx.ERR, "❌ failed to resolve upstream peer for keepalive")
        return ngx.exit(ngx.HTTP_BAD_GATEWAY)
    end
}
{{- end }}
{{- end -}}

{{- define "func.upstreamCacheRequestHeaders" -}}
proxy_set_header If-Match $http_if_match;
proxy_set_header If-Modified-Since $http_if_modified_since;
proxy_set_header If-None-Match $http_if_none_match;
proxy_set_header If-Unmodified-Since $upstream_if_unmodified_since;
proxy_set_header Range $http_range;
proxy_set_header x-ms-if-tags $http_x_ms_if_tags;
proxy_set_header x-ms-lease-id $http_x_ms_lease_id;
{{- end -}}

{{- define "func.upstreamKeepaliveProxyPass" -}}
{{- if .enableUpstreamKeepalive }}
proxy_pass $upstream_keepalive_scheme://dyn_upstream;
proxy_set_header Host $upstream_keepalive_host;  # Otherwise, `dyn_upstream` would be set.
proxy_set_header Connection "";
proxy_pass_request_headers on;
{{- else }}
proxy_pass {{ tpl .proxyPass . }};
{{- end }}
{{- include "func.upstreamCacheRequestHeaders" . | nindent 0 }}
{{- end -}}

{{/*
Returns the configured cache storage mode. PVC remains the default for
backward compatibility with existing values files.
*/}}
{{- define "func.persistenceMode" -}}
{{- default "pvc" .Values.persistence.mode -}}
{{- end -}}

{{/*
Validates the cache storage mode before any workload resources are rendered.
*/}}
{{- define "func.validatePersistenceMode" -}}
{{- $mode := include "func.persistenceMode" . -}}
{{- if not (or (eq $mode "pvc") (eq $mode "ephemeral")) -}}
  {{- fail (printf "OVCC install blocked: persistence.mode must be one of \"pvc\" or \"ephemeral\"; got %q." $mode) -}}
{{- end -}}
{{- end -}}

{{/*
Blocks unsafe storage-mode changes during helm upgrade. StatefulSet volume
claim templates are not safely mutable in-place, and switching from PVCs to
emptyDir would silently discard cache data.
*/}}
{{- define "func.validatePersistenceModeUpgrade" -}}
{{- $requestedMode := include "func.persistenceMode" . -}}
{{- $fullName := include "chart.compatFullname" . -}}
{{- $namespace := .Release.Namespace -}}
{{- $liveMode := "" -}}
{{- $statefulSet := lookup "apps/v1" "StatefulSet" $namespace $fullName -}}
{{- if $statefulSet -}}
  {{- $claimTemplates := default (list) $statefulSet.spec.volumeClaimTemplates -}}
  {{- if gt (len $claimTemplates) 0 -}}
    {{- $liveMode = "pvc" -}}
  {{- else -}}
    {{- $cacheVolumes := dict -}}
    {{- range .Values.persistence.volumes -}}
      {{- $_ := set $cacheVolumes .name true -}}
    {{- end -}}
    {{- range (default (list) $statefulSet.spec.template.spec.volumes) -}}
      {{- if and (hasKey $cacheVolumes .name) (hasKey . "emptyDir") -}}
        {{- $liveMode = "ephemeral" -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
{{- else -}}
  {{- range $vol := .Values.persistence.volumes -}}
    {{- range $i := until (int $.Values.replicaCount) -}}
      {{- $pvcName := printf "%s-%s-%d" $vol.name $fullName $i -}}
      {{- if lookup "v1" "PersistentVolumeClaim" $namespace $pvcName -}}
        {{- $liveMode = "pvc" -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
{{- end -}}
{{- if and $liveMode (ne $liveMode $requestedMode) -}}
  {{- fail (printf "OVCC upgrade blocked: existing release %q in namespace %q uses cache storage mode %q, but requested persistence.mode=%q.\n\nChanging between PVC-backed cache storage and ephemeral emptyDir storage is an intentional migration, not a safe in-place helm upgrade. To migrate from PVC to ephemeral storage, remove the old release/StatefulSet state and explicitly delete the old cache PVCs; otherwise those PVCs remain and continue to incur storage cost. The chart will not delete PVCs or switch storage modes automatically." .Release.Name $namespace $liveMode $requestedMode) -}}
{{- end -}}
{{- end -}}

{{/*
Validates that enabled backends have the cache zone and volume wiring needed by
NGINX. Disabled backends may omit either entry because no server block should
reference their cache zone.
*/}}
{{- define "func.validateBackendCacheCompatibility" -}}
{{- $cachePathsByName := dict -}}
{{- range .Values.nginx.proxyCache.paths }}
  {{- $_ := set $cachePathsByName .name . }}
{{- end }}
{{- $volumesByName := dict -}}
{{- range .Values.persistence.volumes }}
  {{- $_ := set $volumesByName .name . }}
{{- end }}
{{- range $backendName, $backend := .Values.nginx.backends }}
  {{- if $backend.include }}
    {{- if not $backend.proxyCacheName }}
      {{- fail (printf "OVCC upgrade blocked: the %q backend is enabled, but it does not declare which cache storage to use.\n\nMissing value:\n  nginx.backends.%s.proxyCacheName\n\nTo upgrade successfully, either set nginx.backends.%s.proxyCacheName or disable this backend with nginx.backends.%s.include=false." $backendName $backendName $backendName $backendName) }}
    {{- end }}
    {{- $volume := get $volumesByName $backend.proxyCacheName }}
    {{- $cachePath := get $cachePathsByName $backend.proxyCacheName }}
    {{- if not $cachePath }}
      {{- $suggestedCachePath := printf "/proxy_cache_%s/data" $backend.proxyCacheName }}
      {{- if $volume }}
        {{- $suggestedCachePath = printf "%s/data" ($volume.path | toString | trimSuffix "/") }}
      {{- end }}
      {{- fail (printf "OVCC upgrade blocked: the %q backend is enabled, but its cache path is not configured.\n\nThis can happen when upgrading from an older OVCC release that did not include this backend. OVCC cannot safely start an enabled backend unless its cache storage and cache path are configured.\n\nTo upgrade successfully, choose one:\n  1. If you do not use this backend, disable it:\n     nginx.backends.%s.include=false\n\n  2. If you use this backend, add a cache path entry named %q:\n     nginx.proxyCache.paths:\n       - name: %s\n         path: %s" $backendName $backendName $backend.proxyCacheName $backend.proxyCacheName $suggestedCachePath) }}
    {{- end }}
    {{- if not $volume }}
      {{- $suggestedVolumePath := trimSuffix "/data" ($cachePath.path | toString) }}
      {{- fail (printf "OVCC upgrade blocked: the %q backend is enabled, but no cache storage is configured for it.\n\nThis can happen when upgrading from an older OVCC release that did not include this backend. OVCC cannot safely start an enabled backend unless its cache storage and cache path are configured.\n\nTo upgrade successfully, choose one:\n  1. If you do not use this backend, disable it:\n     nginx.backends.%s.include=false\n\n  2. If you use this backend, add storage configuration named %q:\n     persistence.volumes:\n       - name: %s\n         path: %s\n         sizeGi: 2048" $backendName $backendName $backend.proxyCacheName $backend.proxyCacheName $suggestedVolumePath) }}
    {{- end }}
    {{- if and $cachePath $volume }}
      {{- $cacheDir := $cachePath.path | toString }}
      {{- $volumeDir := $volume.path | toString | trimSuffix "/" }}
      {{- if and $cacheDir $volumeDir (not (or (eq $cacheDir $volumeDir) (hasPrefix (printf "%s/" $volumeDir) $cacheDir))) }}
        {{- fail (printf "OVCC upgrade blocked: the %q backend cache path is outside its configured storage volume.\n\nCurrent values:\n  persistence.volumes[name=%s].path: %s\n  nginx.proxyCache.paths[name=%s].path: %s\n\nTo upgrade successfully, set nginx.proxyCache.paths[name=%s].path under persistence.volumes[name=%s].path, or disable this backend with nginx.backends.%s.include=false." $backendName $backend.proxyCacheName $volume.path $backend.proxyCacheName $cacheDir $backend.proxyCacheName $backend.proxyCacheName $backendName) }}
      {{- end }}
    {{- end }}
  {{- end }}
{{- end }}
{{- end -}}

{{/*
TLS Secret for the workload. With compat enabled, use the UCC v3.0.3 default unless
tls.secretName was explicitly set away from the OVCC default (ovcc-tls).
*/}}
{{- define "func.effectiveTlsSecretName" -}}
{{- $ovccTlsDefault := "ovcc-tls" -}}
{{- if and .Values.compat.useUccDefaults (eq .Values.tls.secretName $ovccTlsDefault) -}}
{{- .Values.compat.uccDefaults.tlsSecretName -}}
{{- else -}}
{{- .Values.tls.secretName -}}
{{- end -}}
{{- end -}}

{{/*
Cache path rendered into NGINX. With compat enabled, preserve the UCC v3.0.3
default cache directory suffix for default OVCC paths.
*/}}
{{- define "func.effectiveProxyCachePath" -}}
{{- $root := .root -}}
{{- $path := .cachePath.path | toString -}}
{{- if and $root.Values.compat.useUccDefaults (hasSuffix "/data" $path) -}}
{{- printf "%s/ucc_data" (trimSuffix "/data" $path) -}}
{{- else -}}
{{- $path -}}
{{- end -}}
{{- end -}}

{{/*
OVCC runtime defaults. These are fixed by the chart; not tunable via values.
*/}}
{{- define "func.cacheResetPath" -}}/__/cache/reset{{- end -}}
{{- define "func.instanceHeader" -}}CC-Instance{{- end -}}

{{/*
Metric prefix. Switches to the UCC v3.0.3 (hc) prefix when compat is enabled.
*/}}
{{- define "func.metricPrefix" -}}
{{- if .Values.compat.useUccDefaults -}}
{{- .Values.compat.uccDefaults.metricPrefix -}}
{{- else -}}
cc_
{{- end -}}
{{- end -}}

{{/*
Minimum free capacity in GiB for a cache volume. PVC-backed volumes reserve the
configured percentage with a 1 GiB floor. Ephemeral volumes reserve 10 GiB,
clamped to their declared size limit.
Args: dict "root" $ "volume" <persistence.volumes entry>.
*/}}
{{- define "func.cacheMinFreeGi" -}}
{{- $root := .root -}}
{{- $volume := .volume -}}
{{- if eq (include "func.persistenceMode" $root) "ephemeral" -}}
{{- int (minf 10.0 (float64 $volume.sizeGi)) -}}
{{- else -}}
{{- $sizeGi := float64 $volume.sizeGi -}}
{{- $percentage := default 7.0 (float64 $volume.minFreeSizePercentage) -}}
{{- int (max (mulf (divf $sizeGi 100.0) $percentage) 1.0) -}}
{{- end -}}
{{- end -}}

{{/*
Private Rust cache root for one backend. The backend's proxyCacheName links to
the existing persistence volume; Rust uses a sibling "rust" directory so it
never reads or mutates OpenResty's data/ucc_data trees.
Args: dict "root" $ "cacheName" <backend.proxyCacheName>.
*/}}
{{- define "func.engineBackendCacheRoot" -}}
{{- $root := .root -}}
{{- $cacheName := .cacheName | toString -}}
{{- $path := "" -}}
{{- range $root.Values.persistence.volumes -}}
{{- if eq (.name | toString) $cacheName -}}
{{- $path = printf "%s/rust" (trimSuffix "/" (.path | toString)) -}}
{{- end -}}
{{- end -}}
{{- $path -}}
{{- end -}}

{{/*
Legacy single-root fallback required by the current ovcc-config schema. Selects
the first enabled backend's isolated root deterministically; the per-backend
cacheRoot fields in the private config are the future multi-store contract.
When every backend is disabled, use the first declared persistence volume so
the required field remains derived from the canonical values.
*/}}
{{- define "func.engineCacheRoot" -}}
{{- $selected := dict "path" "" -}}
{{- range $name, $backend := .Values.nginx.backends -}}
{{- if and (not (get $selected "path")) (kindIs "map" $backend) (hasKey $backend "include") $backend.include (get $backend "proxyCacheName") -}}
{{- $_ := set $selected "path" (include "func.engineBackendCacheRoot" (dict "root" $ "cacheName" (get $backend "proxyCacheName"))) -}}
{{- end -}}
{{- end -}}
{{- if not (get $selected "path") -}}
{{- range .Values.persistence.volumes -}}
{{- if not (get $selected "path") -}}
{{- $_ := set $selected "path" (printf "%s/rust" (trimSuffix "/" (.path | toString))) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- get $selected "path" -}}
{{- end -}}

{{/*
Legacy cache-reset URL dual-emitted when compat.emitLegacyCacheReset is true.
Empty string when disabled.
*/}}
{{- define "func.legacyCacheResetPath" -}}
{{- if .Values.compat.emitLegacyCacheReset -}}/__ucc/cache/reset{{- end -}}
{{- end -}}

{{/*
Legacy instance header dual-emitted on the cache-reset response when
compat.emitLegacyCacheReset is true. Empty string when disabled.
*/}}
{{- define "func.legacyInstanceHeader" -}}
{{- if .Values.compat.emitLegacyCacheReset -}}UCC-Instance{{- end -}}
{{- end -}}

{{/*
Blocks upgrades from the last UCC chart (usd-content-cache) unless legacy runtime
compatibility is enabled. Detects the prior StatefulSet name (<release>-usd-content-cache).
*/}}
{{- define "func.validateUccUpgradeCompat" -}}
{{- if not .Values.compat.useUccDefaults -}}
{{- $namespace := .Release.Namespace -}}
{{- $releaseName := .Release.Name -}}
{{- $oldChartName := "usd-content-cache" -}}
{{- $oldFullName := "" -}}
{{- if contains $oldChartName $releaseName -}}
{{- $oldFullName = $releaseName | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $oldFullName = printf "%s-%s" $releaseName $oldChartName | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- $oldSts := lookup "apps/v1" "StatefulSet" $namespace $oldFullName -}}
{{- if $oldSts -}}
  {{- fail (printf "OVCC upgrade blocked: existing release %q in namespace %q is still backed by the UCC chart (%q StatefulSet).\n\nUpgrading to OVCC changes runtime names (cache reset path, instance header, metric prefix) and cache data paths unless you enable legacy compatibility.\n\nTo upgrade a default UCC install, re-run with:\n  --set compat.useUccDefaults=true\n\nIf you customized TLS secrets, service ports, or other values on the old chart, also pass the explicit overrides documented in deploy/AGENTS.md under \"Upgrade From UCC / usd-content-cache\"." $releaseName $namespace $oldFullName) -}}
{{- end -}}
{{- end -}}
{{- end -}}
