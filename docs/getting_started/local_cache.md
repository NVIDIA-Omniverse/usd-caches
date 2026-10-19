# Getting started with the local cache (Hub)

Hub runs on a workstation or compute node and keeps the source content and derived data used by Kit-based applications and Client Library tools on local disk, so repeated loads are served locally instead of from upstream storage.

## Requirements

- Omniverse Client Library 2.49.2 or greater in the applications that use Hub. Older versions may fail to load files.
- Local disk space for the cache.

## Install a prebuilt release

Hub is published on NGC as a [collection](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/collections/hub-workstation-cache), with [Windows and Linux installers](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/resources/hub_workstation_cache) and a [container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/hub_workstation_cache).

- **Install scripts:** download the Windows or Linux archive from the installers page, extract it, and run `./scripts/install.sh` or `scripts\install.bat`.
- **From a Kit application:** install and load the Cache Status Indicator extension (3.0.0 or greater), then click the indicator to download and install Hub. Restart running Kit applications afterwards; Hub doesn't connect to applications that were already running.
- **In Docker:** run the container with the cache directory mounted at the same path inside and outside the container, and host networking:

  ```bash
  docker run --rm -v "/var/cache/hub":"/var/cache/hub" -u $(id -u ${USER}):$(id -g ${USER}) --network=host hub_workstation_cache
  ```

  See {src}`Hub in Docker <local-cache/reference/docs/HUB_IN_DOCKER.md>` for client configuration.

The [Hub Workstation Cache documentation](https://docs.omniverse.nvidia.com/utilities/latest/cache/hub-workstation.html) covers installation, uninstallation, and settings in full.

## Configure

Hub reads `~/.nvidia-omniverse/config/hub.toml`. For example, to set the cache location and size:

```toml
[cache]
path = "cache/path"
max_size = "40g"
```

Hub listens on HTTP port 14090, MessagePack RPC port 14092, and Protobuf RPC port 14093 by default. Every setting is listed in the {src}`local cache configuration reference <skills/usd-caches-expert/references/local-cache/configuration.md>`.

## Build from source

From `local-cache/reference/`, `cargo build --release` builds Hub and `cargo run --release --bin hub -- --mode=shared` runs it on the default ports. Without `--mode=shared`, Hub starts in exclusive mode on random ports. On Linux, install `libssl-dev` and `pkg-config` first. See the {src}`source guide <local-cache/reference/README.md>`.

## Next steps

- {src}`Protobuf RPC guide <local-cache/reference/docs/PROTOBUF_RPC.md>` and {src}`Python client examples <local-cache/reference/hub-rpc/examples/python/>`.
- {src}`Validate that Hub is caching <skills/usd-caches-expert/references/local-cache/validate.md>`.
