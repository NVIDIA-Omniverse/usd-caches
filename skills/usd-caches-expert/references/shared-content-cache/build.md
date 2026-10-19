Up: [shared content cache](README.md)

# Building and testing OVCC

The Helm chart at [`deploy/`](../../../../shared-content-cache/reference/deploy/) is what you lint and render from this repository.

The chart's container image (`nvcr.io/nvidia/omniverse/usd-content-cache:3.0.2`) **cannot be built from this repo**. `values.yaml` refers to `/docker/openresty.Dockerfile`, which is not present. The only version pins for it are in `shared-content-cache/reference/vendor/versions.yml` (OpenResty `1.31.1.1`, NGINX `1.31.1`, nginx-otel `0.1.2`, lua-upstream-cache-nginx-module `0.1.1`). Pull that image from NGC instead (see [deploy.md](deploy.md)).

## Helm chart checks

Chart: `deploy/Chart.yaml` (`name: ovcontentcache`, `version: 4.0.4`, `appVersion: "3.0.2"`). `helm` was not installed in the authoring environment, so the commands below are not run-verified.

```bash
cd shared-content-cache/reference
helm lint ./deploy
helm template t ./deploy --namespace ovcc > /tmp/ovcc.yaml          # default render
helm template t ./deploy -f my-values.yaml --debug                   # see validation failures
```

- Validation in `deploy/templates/validation.yaml` runs during `template`.
- `lookup`-based guards (PVC size, storage-mode change, UCC upgrade detection) are no-ops without a cluster (`AGENTS.md` [PVC Size Guard](../../../../shared-content-cache/reference/deploy/AGENTS.md#pvc-size-guard)). Use `helm upgrade --dry-run=server` against a cluster to exercise them.
- Lua files listed in `nginx.lua.files` but missing from `deploy/files/lua/` are silently skipped (`configmap-lua.yaml`, `statefulset.yaml`).
- `deploy/bom/` holds only a README; per that README, the SBOM is generated in CI.
