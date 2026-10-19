Up: [usd-caches-expert](../../SKILL.md)

# Local cache (Hub, Hub Workstation Cache)

Hub is a per-node Rust service (`hub` binary, version 2.4.0) that stores source-content files (cache-aside API) and derived-data blocks (block-cache API) on local disk for Kit-based applications and Client Library tools. It does not fetch from storage itself. The client fetches from upstream and then writes into Hub, and Hub hands back **local file paths**. Clients therefore must share Hub's filesystem, so Hub can't serve remote machines. By default it binds `127.0.0.1` (`http.host`, `transport.tcp.host`): HTTP `14090`, MessagePack RPC `14092`, and Protobuf RPC `14093` in shared mode. Any other bind address exposes these listeners without authentication or TLS. Sources: `local-cache/reference/hub/src/rpc/dispatcher.rs`, `local-cache/reference/hub-util/src/settings/{http,transport}.rs`.

Hub is the **only** cache that helps a single machine. It is also the only usd-caches component that keeps derived data local. Put it on every node that runs Kit or Client Library workloads. Add the shared caches (OVCC, OVDC) only when several nodes repeat the same work. Hub requires Omniverse Client Library 2.49.2 or later (`local-cache/reference/docs/index.rst`).

Primary sources: [published docs source `index.rst`](../../../../local-cache/reference/docs/index.rst) ([published page](https://docs.omniverse.nvidia.com/utilities/latest/cache/hub-workstation.html)), [settings structs](../../../../local-cache/reference/hub-util/src/settings/), [CLI args](../../../../local-cache/reference/hub/src/args.rs), [proto](../../../../local-cache/reference/hub-rpc/proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto). NGC: [Hub Workstation Cache collection](https://catalog.ngc.nvidia.com/orgs/nvidia/omniverse/collections/hub-workstation-cache/-), [installers](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/resources/hub_workstation_cache), [container](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/hub_workstation_cache).

## Files in this reference

| File | Read it when |
|---|---|
| [overview.md](overview.md) | You need the architecture: modes, ports, how clients discover and launch Hub, on-disk layout, the file and block caches, eviction and GC, per-connection state, limits, and what Hub does not do. |
| [build.md](build.md) | You are building, testing, linting, or packaging from `local-cache/reference/`, including the web UI and the C client library. |
| [install-and-run.md](install-and-run.md) | You are installing the NGC package (scripts or the Kit Cache Status Indicator), running from source, running in Docker, pointing clients at Hub, or uninstalling. Also covers Windows and Linux differences. |
| [configuration.md](configuration.md) | You need the meaning, default, or source of any `hub.toml` key, `omniverse.toml` path, CLI flag, `HUB__*`/`OMNI__*`/`OMNICLIENT_HUB_*` variable, or a complete example `hub.toml`. |
| [api-protobuf.md](api-protobuf.md) | You are writing a client against the Protobuf RPC: framing, session workflow, every message and result code, and runnable Python examples. |
| [api-http.md](api-http.md) | You are calling the HTTP API: health, stats, config, clear, reset, shutdown, and support bundle, with curl examples. |
| [validate.md](validate.md) | You are proving Hub is installed, running, reachable, and actually caching, or running the integration tests. |
| [troubleshooting.md](troubleshooting.md) | Something fails: Client Library version, Hub not found, port conflicts, Docker paths, settings page, GC not freeing space, or RPC error results. |
