# Getting started with the shared derived-data cache (OVDC)

The shared derived-data cache (also called ovderivedcache or OVDC, formerly DDCS) is a persistent, distributed key-value store on RocksDB. When workers generate the same derived data, one worker stores it and the others retrieve it instead of regenerating it. Clients use a gRPC `KeyValue` service with `Get`, `Put`, and `Touch`. It deploys to Kubernetes with Helm.

```{note}
OVDC can evict or lose entries. Applications must be able to regenerate missing data; keep authoritative content in backing storage.
```

## Requirements

- A Kubernetes cluster and Helm 3, in the same region as the workers that generate derived data.
- A StorageClass that supports `ReadWriteOnce`; each replica gets its own volume.
- An NGC API key to pull the chart and image.

## Deploy the prebuilt chart

The [Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovderivedcache) and [container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/ovderivedcache) are published on NGC. The chart's default image pull secret is named `regcred`. Replace the `your-` placeholder in these commands before running them.

```bash
export NS=ovdc NGC_API_KEY=your-ngc-api-key
kubectl create namespace "$NS"
kubectl -n "$NS" create secret docker-registry regcred \
  --docker-server=nvcr.io --docker-username='$oauthtoken' --docker-password="$NGC_API_KEY"
helm repo add omniverse https://helm.ngc.nvidia.com/nvidia/omniverse \
  --username='$oauthtoken' --password="$NGC_API_KEY"
helm repo update
helm install ovdc omniverse/ovderivedcache -n "$NS"
```

The [Omniverse Derived Cache documentation](https://docs.omniverse.nvidia.com/ovcaches/ovderivedcache/latest/index.html) covers sizing presets, Helm values, TLS, metrics, and runbooks.

## Run locally and smoke-test

From `shared-derived-cache/reference/`, create a small development config and run the server. It listens for gRPC on port 3010 and serves Prometheus metrics on port 3051:

```bash
sed -e 's/cacheSize = "32G"/cacheSize = "64M"/' \
    -e 's/blockCacheSize = "8G"/blockCacheSize = "64M"/' \
    -e 's/exporter = "otlp"/exporter = "none"/g' \
    rust/ovdc/src/config.toml > /tmp/ovdc-local.toml
make run CONFIG=/tmp/ovdc-local.toml ENV_DIR=/tmp/ovdc-local-data
```

With [grpcurl](https://github.com/fullstorydev/grpcurl#installation) installed, store, read, and touch a key (`demo` = `hello`, base64-encoded):

```bash
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"begin":{}} {"declare":{"key":{"key":"ZGVtbw=="},"size":"5","value":"aGVsbG8="}}' \
  localhost:3010 ovdc.KeyValue/Put
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:3010 ovdc.KeyValue/Get
grpcurl -plaintext -import-path rust/ovdc_grpc/proto -proto rpc.proto \
  -d '{"keys":[{"key":"ZGVtbw=="}]}' localhost:3010 ovdc.KeyValue/Touch
```

GET returns `value: "aGVsbG8="` and `size: "5"`, and TOUCH reports `touched: 1`. The reference config disables JWT verification and TLS; use it only on a trusted development machine.

## Next steps

- {src}`Deployment reference <skills/usd-caches-expert/references/shared-derived-cache/deploy.md>`, {src}`every Helm value <skills/usd-caches-expert/references/shared-derived-cache/configuration-helm.md>`, and {src}`every server setting <skills/usd-caches-expert/references/shared-derived-cache/configuration-server.md>`.
- {src}`gRPC API with runnable examples <skills/usd-caches-expert/references/shared-derived-cache/api-grpc.md>`.
- {src}`Source and native setup <shared-derived-cache/reference/README.md>`.
