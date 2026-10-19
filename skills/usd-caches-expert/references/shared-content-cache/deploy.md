Up: [shared content cache](README.md)

# Deploying OVCC with Helm

Supported interface: `values.yaml` only. Do not edit templates, Lua, or backend confs ([AGENTS.md Critical Constraints](../../../../shared-content-cache/reference/deploy/AGENTS.md#critical-constraints)). Field reference: [configuration.md](configuration.md). Published guide sections: Configure, Deployment, TLS Configuration, Runbooks ([docs](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html)).

## 1. Prerequisites

- A Kubernetes cluster and Helm 3 (chart `apiVersion: v2`).
- A StorageClass. The default `persistence.storageClassName` is Azure's `managed-csi-premium-burst`. On AWS or GKE, set your own class or `""` for the cluster default. Recommended classes: AWS gp3, GCP pd-ssd or `hyperdisk-balanced` on c4, both with `WaitForFirstConsumer` ([AGENTS.md Storage Class Recommendations](../../../../shared-content-cache/reference/deploy/AGENTS.md#storage-class-recommendations)).
- Egress TCP 443 to the origins and a CA bundle for outbound TLS ([AGENTS.md GCP Infrastructure](../../../../shared-content-cache/reference/deploy/AGENTS.md#gcp-infrastructure)).
- An NGC API key for the Helm repo and the `nvcr.io` image.

## 2. Namespace, image pull secret, Helm repo

The commands below follow the standard NGC conventions (`$oauthtoken` user) and match [`docs/getting_started/shared_content_cache.md`](../../../../docs/getting_started/shared_content_cache.md); the chart name comes from `Chart.yaml`. Replace the `your-` placeholders before running the commands on this page.

```bash
export NS=ovcc NGC_API_KEY=your-ngc-api-key
kubectl create namespace "$NS"
kubectl -n "$NS" create secret docker-registry ngc-registry \
  --docker-server=nvcr.io --docker-username='$oauthtoken' --docker-password="$NGC_API_KEY"
helm repo add omniverse https://helm.ngc.nvidia.com/nvidia/omniverse \
  --username='$oauthtoken' --password="$NGC_API_KEY"
helm repo update
helm search repo omniverse/ovcontentcache --versions
```

The default `image.pullSecrets` is `[{name: gitlab-registry-secret}]` (`values.yaml`). Override it to the secret you created: `--set 'image.pullSecrets[0].name=ngc-registry'`.

## 3. TLS secret

`tls.enabled` defaults to `true`. The pod mounts Secret `tls.secretName` (default `ovcc-tls`; `usd-content-cache-tls` under compat) at `/etc/nginx/ssl`, and NGINX reads `tls.crt` and `tls.key` from it (`deploy/files/nginx.conf`).

```bash
# -addext needs OpenSSL 1.1.1+ or LibreSSL 3.1+. List every name your clients verify.
openssl req -x509 -nodes -days 365 -newkey rsa:2048 -keyout /tmp/tls.key -out /tmp/tls.crt \
  -subj "/CN=ovcc.$NS.svc/O=MyOrg" \
  -addext "subjectAltName=DNS:ovcc.$NS.svc,DNS:ovcc.$NS.svc.cluster.local,DNS:your-bucket.s3.your-region.amazonaws.com"
kubectl -n "$NS" create secret tls ovcc-tls --key /tmp/tls.key --cert /tmp/tls.crt
rm /tmp/tls.key /tmp/tls.crt
```

The `values.yaml` example uses only `-subj "/CN=ovcc.local.test/O=MyOrg"`. A certificate without SANs works only with `curl -k`; clients that verify hostnames reject it. Use a CA-issued certificate in production.

- Clients normally connect to OVCC while presenting the storage hostname (SNI and Host). The certificate must therefore cover the names clients verify: the transparent DNS names, or the OVCC Service name on the opt-in path ([AGENTS.md S3 Client Access Patterns](../../../../shared-content-cache/reference/deploy/AGENTS.md#s3-client-access-patterns)). The source does not prescribe a SAN list.
- Use clients that support TLS 1.3.
- `allowHttp2Clients` requires TLS (Azure note in `AGENTS.md`).

## 4. Install

Minimum override, assuming a single S3 backend. First save the [single-backend S3 example](configuration.md#single-backend-s3-values-s3yaml) as `values-s3.yaml`. It enables a ServiceMonitor, so set `metrics.prometheus.serviceMonitor.enabled: false` in it unless the cluster has the Prometheus Operator CRDs:

```bash
helm install ovcc omniverse/ovcontentcache -n "$NS" \
  --set 'image.pullSecrets[0].name=ngc-registry' \
  --set persistence.storageClassName=gp3 \
  -f values-s3.yaml
kubectl -n "$NS" rollout status statefulset/ovcc
```

Naming: with release `ovcc`, resources are `ovcc`, `ovcc-nucleus`, `ovcc-metrics`, pod `ovcc-0`, and PVCs `s3-ovcc-0` etc. With another release name `R`, everything is `R-ovcc*`. `nameOverride` changes the `ovcc` root. `fullnameOverride` replaces the whole name (use sparingly). Full matrix: [AGENTS.md Rendered resource names by scenario](../../../../shared-content-cache/reference/deploy/AGENTS.md#rendered-resource-names-by-scenario).

Naming rules:
- Don't use a release name or `nameOverride` that makes the Service name start with `ovcc-config`.
- Names are truncated to 63 characters.

## 5. Upgrade

- After a config-only change, make sure the pods restarted. If they didn't, run `kubectl -n "$NS" rollout restart statefulset/ovcc`.
- Guards that fail `helm upgrade` before any change ([AGENTS.md PVC Size Guard](../../../../shared-content-cache/reference/deploy/AGENTS.md#pvc-size-guard)):
  - Any existing PVC smaller than `sizeGi`. Fix: delete that PVC, or set `sizeGi` to match.
  - Changing `persistence.mode` (PVC ↔ ephemeral). Fix: uninstall, delete the StatefulSet and PVCs, then reinstall.
  - An enabled backend without matching `nginx.proxyCache.paths[]` and `persistence.volumes[]`, or whose path is outside its volume.
  - A UCC-era StatefulSet present without `compat.useUccDefaults=true`.
- Image pin: `--set image.registry=nvcr.io --set image.repository=nvidia/omniverse/usd-content-cache --set image.tag=3.0.2`, using the tag you want to pin ([AGENTS.md Optional image pin](../../../../shared-content-cache/reference/deploy/AGENTS.md#optional-image-pin)).

### Upgrade from UCC (`usd-content-cache` chart v3.0.3)

```bash
RELEASE=your-ucc-release NS=your-namespace
helm upgrade "$RELEASE" omniverse/ovcontentcache -n "$NS" --set compat.useUccDefaults=true
# Customized UCC installs also carry over their old values, for example:
#   --set tls.secretName=your-tls-secret --set service.cache.port=OLD_UCC_PORT \
#   --set service.cache.containerPort=OLD_UCC_CONTAINER_PORT
```

`compat.useUccDefaults=true` keeps the `<release>-usd-content-cache` StatefulSet, Service, and selector names, so the old PVCs rebind. It also keeps TLS secret `usd-content-cache-tls`, cache paths `/proxy_cache_*/ucc_data`, and metric prefix `ucc_`. The `ucc_` prefix requires the matching dashboard variant. Separately, `compat.emitLegacyCacheReset` (default `true`) keeps `/__ucc/cache/reset` and the `UCC-Instance` header live. Its planned removal is in OVCC 5.0.0. The `service.ucc` key is renamed to `service.cache`. Full procedure: [AGENTS.md Upgrade From UCC / usd-content-cache](../../../../shared-content-cache/reference/deploy/AGENTS.md#upgrade-from-ucc--usd-content-cache).

## 6. Uninstall

```bash
helm uninstall ovcc -n "$NS"
kubectl -n "$NS" get pvc            # StatefulSet PVCs are NOT deleted by Helm
kubectl -n "$NS" delete pvc -l app.kubernetes.io/instance=ovcc   # see note
```

Note: the StatefulSet controller labels each PVC with the selector labels, so the selector above deletes the release's PVCs. PVC names are `<volume>-<sts>-<ordinal>`, e.g. `s3-ovcc-0`. Deleting them is required when changing storage mode or cutting cost ([AGENTS.md Storage Mode](../../../../shared-content-cache/reference/deploy/AGENTS.md#storage-mode)).

## 7. Persistence modes

| `persistence.mode` | Backing | `min_free` reserve (`func.cacheMinFreeGi`) |
|---|---|---|
| `pvc` (default) | `volumeClaimTemplates`, RWO, `sizeGi` per backend per replica | `max(sizeGi × minFreeSizePercentage / 100, 1)` GiB, integer |
| `ephemeral` | `emptyDir` with `sizeLimit: <sizeGi>Gi`; data lost on pod or node recreate | flat `min(10, sizeGi)` GiB; `minFreeSizePercentage` ignored |

When using ephemeral mode, set `sizeGi` to fit the real node disk (for example local SSD). The default of 2048 is only an upper cap. Each `persistence.volumes[]` entry is its own claim, or its own `emptyDir`. The AGENTS.md Single PVC Option example fails validation, because every included backend needs a volume named after its `proxyCacheName`.

## 8. Monitoring

- Metrics endpoint: `:9145/cache_metrics`, served through Service `<fullname>-metrics` (label `metrics: 'true'`, port name `cache-metrics`). The metric prefix is `cc_`, or `ucc_` with compat.
- Prometheus Operator: set `metrics.prometheus.serviceMonitor.enabled=true`. It defaults to `false`; without it, dashboards show "No data" ([AGENTS.md Monitoring mutable mode](../../../../shared-content-cache/reference/deploy/AGENTS.md#monitoring-mutable-mode)). The template selects Services labelled `metrics: 'true'` in the release namespace.
- GKE Managed Prometheus: set `metrics.prometheus.podMonitoring.enabled=true` (`monitoring.googleapis.com/v1` PodMonitoring). The ServiceMonitor CRD does not exist on GKE.
- Dashboards:
  - Grafana: don't use `metrics.dashboard.include=true`. The API server rejects its ConfigMaps, because the names contain `_` and the default label value is a boolean. Create the ConfigMap yourself and label it for your Grafana sidecar, for example `kubectl -n "$NS" create configmap ovcc-grafana-dashboard --from-file=deploy/dashboards/grafana_dashboard.json` and then `kubectl -n "$NS" label configmap ovcc-grafana-dashboard grafana_dashboard=1`.
  - GCM: `gcloud monitoring dashboards create --config-from-file=deploy/dashboards/gcloud_dashboard.json` ([AGENTS.md GCP Infrastructure](../../../../shared-content-cache/reference/deploy/AGENTS.md#gcp-infrastructure)).
  - Both dashboards hardcode the `cc_` prefix.
- OpenTelemetry: wiring is inconsistent with `values.yaml`; see [configuration.md](configuration.md#otel). `otel.enabled` only adds `otel_*` fields to access logs. The OTel module loads only on the undeclared `traces.enabled`. The sidecar collector renders only on the undeclared `otel.sidecar`.
- Logs: JSON to stdout. Formats are `cloud_backend_log` (S3/Azure/GCS), `nucleus_log`, and `default_log` (passthrough). Buffered with `nginx.logging.buffer`/`flushInterval` ([AGENTS.md Logging](../../../../shared-content-cache/reference/deploy/AGENTS.md#logging)).

## 9. Client routing (required for any cache benefit)

Clients must send requests to the OVCC Service while keeping the original storage `Host`. Use one of these:
- Transparent: cluster DNS maps bucket or account hostnames to the OVCC Service. A Nucleus `coredns-overrides` addon is referenced in `values.yaml` and `AGENTS.md`, but it is not part of this chart.
- Opt-in: point the SDK connection URL at the OVCC Service while keeping the signed `Host`, e.g. a boto3 `before-send.s3` hook.

Details and examples: [api-http.md](api-http.md).
