# Getting started with the shared content cache (OVCC)

The shared content cache (also called ovcontentcache or OVCC, formerly UCC) is an HTTP caching reverse proxy for S3, Azure Blob Storage, GCS, and Nucleus. When many clients read the same assets, it retrieves each asset once and serves it to all of them. It deploys to Kubernetes with Helm.

## Requirements

- A Kubernetes cluster and Helm 3, in the same region as the compute that reads the content.
- A StorageClass for the cache volumes. The chart defaults to Azure's `managed-csi-premium-burst`; set your own class on other clouds.
- An NGC API key to pull the chart and image.

## Deploy the prebuilt chart

The [Helm chart](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/helm-charts/ovcontentcache) and [container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/usd-content-cache) are published on NGC. Replace the `your-` placeholders in these commands before running them.

```bash
export NS=ovcc NGC_API_KEY=your-ngc-api-key
kubectl create namespace "$NS"
kubectl -n "$NS" create secret docker-registry ngc-registry \
  --docker-server=nvcr.io --docker-username='$oauthtoken' --docker-password="$NGC_API_KEY"
helm repo add omniverse https://helm.ngc.nvidia.com/nvidia/omniverse \
  --username='$oauthtoken' --password="$NGC_API_KEY"
helm repo update
```

TLS is enabled by default, so create the `ovcc-tls` secret with a certificate that covers the names your clients verify. Then install with an override file for your storage backend:

```bash
kubectl -n "$NS" create secret tls ovcc-tls --key tls.key --cert tls.crt
helm install ovcc omniverse/ovcontentcache -n "$NS" \
  --set image.pullSecrets[0].name=ngc-registry \
  --set persistence.storageClassName=your-storage-class \
  -f values-override.yaml
kubectl -n "$NS" rollout status statefulset/ovcc
```

`values.yaml` is the only supported configuration interface. Clients must route their storage requests through the OVCC service to benefit from the cache.

The [Omniverse Content Cache documentation](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html) covers configuration, deployment, TLS, and runbooks in full.

## Next steps

- {src}`Deployment reference <skills/usd-caches-expert/references/shared-content-cache/deploy.md>` and {src}`every configuration field <skills/usd-caches-expert/references/shared-content-cache/configuration.md>`.
- {src}`Route clients through the cache and verify hits <skills/usd-caches-expert/references/shared-content-cache/api-http.md>`.
- {src}`Helm deployment and operations guide <shared-content-cache/reference/deploy/AGENTS.md>`.
