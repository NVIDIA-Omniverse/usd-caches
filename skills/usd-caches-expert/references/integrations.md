# Using the caches from Kit and ovstorage

## Kit

### Local cache (Hub)

Kit uses Hub through the Omniverse Client Library. Nothing in a Kit app points at Hub by address; the Client Library finds the newest installed Hub and launches it on demand in shared mode ([install-and-run.md](local-cache/install-and-run.md)).

1. **Install Hub.** Use one of these:
   - In the Kit app, install and enable the **Cache Status Indicator** extension, version 3.0.0 or later (Option B in [install-and-run.md](local-cache/install-and-run.md)).
   - Use the NGC installers from the [Hub Workstation Cache collection](https://catalog.ngc.nvidia.com/orgs/nvidia/omniverse/collections/hub-workstation-cache/-) (Option A).
2. **Check the Client Library version.** It must be 2.49.2 or later; older versions cause load errors ([troubleshooting.md](local-cache/troubleshooting.md)).
3. **Restart running Kit apps.** "Hub does not connect to running Kit applications until you restart them" (`local-cache/reference/docs/index.rst`).
4. **Known issue on Windows with Kit 107.3 or earlier:** Kit may not find an installed Hub. The workaround is `mklink /d %APPDATA%\ov %LOCALAPPDATA%\ov`; Kit 109.0.1 and later fix it.
5. **Validate.** The Cache Status Indicator shows a green **SET**. Load the same stage twice, and `cache_hits` in `curl -s http://127.0.0.1:14090/api/1.0/cache/stats` rises on the second load ([validate.md](local-cache/validate.md)).

Configure Hub itself (size, path) through `hub.toml`, the Hub Settings page opened from the indicator, or `OMNICLIENT_HUB_*` environment variables ([configuration.md](local-cache/configuration.md)).

### Shared content cache (OVCC) and shared derived-data cache (OVDC)

This repo doesn't say how to point Kit at OVCC or OVDC. Don't invent Kit settings, extension names, or environment variables for them. State what is known and what has to come from elsewhere:

- **OVCC:** Kit reads storage through the Client Library. OVCC caches those reads only if the requests reach the OVCC Service with the original storage hostname in `Host` and SNI, and only if they carry `If-Match` (with the default `requiresVersioning: true`). Transparent routing through cluster DNS needs no Kit change; whether the Client Library sends `If-Match` is not in this repo ([choosing-caches.md](choosing-caches.md#gaps-to-state-in-recommendations)).
- **OVDC:** RTX (UJITSO) is the client. The settings that point it at OVDC, and how it spreads keys across pods, are not in this repo. The server still accepts the legacy gRPC service name `ddcs.KeyValue` and rewrites it to `ovdc.KeyValue`, so clients built against the older DDCS proto keep working (`shared-derived-cache/reference/rust/ovdc/src/lib.rs`, `LEGACY_KEY_VALUE_SERVICE`).
- **Where to go next:** the published [OVCC](https://docs.omniverse.nvidia.com/ovcaches/ovcontentcache/latest/index.html) and [OVDC](https://docs.omniverse.nvidia.com/ovcaches/ovderivedcache/latest/index.html) docs. For connecting Kit to Content Services, use the `usd-content-workspace-expert` ([related-experts.md](related-experts.md#usd-content-workspace)).

## ovstorage `hub_cache` layer

ovstorage can cache object bytes in Hub through its **experimental** `hub_cache` wrapper layer. Its configuration keys and behavior may change between releases. This summary is from ovstorage 0.4.0. For anything beyond the Hub side, ask the ovstorage expert ([related-experts.md](related-experts.md#ovstorage)).

Source, in the ovstorage repo: `docs/public/configuration.md` ("Standard layer kinds").

### What it does

- It wraps another layer (`inner`). On a read, it checks the object's current validator with `inner`, looks up the key in Hub, and serves a hit from Hub's files. On a miss, it fills Hub while the bytes stream to the caller unchanged. `materialize` returns Hub's own file, which gives a local file to backends that only stream bytes.
- It caches only objects whose validator (ETag) is declared strong. Weak or missing validators bypass Hub. The built-in `file` backend has only weak validators, so a file-backed stack never uses Hub; the typical inner layer is a remote backend such as `http` or `s3`.
- Any Hub failure, including no Hub running, degrades to a plain read from `inner`. Writes pass through, and the validator-based keys mean entries never need invalidating.
- It is connect-only: ovstorage never starts Hub. Hub must already be running in **shared** mode, because exclusive mode uses OS-assigned ports. Start it with `hub --mode=shared`, or let a Kit or Client Library process launch it ([install-and-run.md](local-cache/install-and-run.md)).

### Configuration

| Key | Default | Meaning |
| --- | --- | --- |
| `endpoint` | `127.0.0.1:14093` | Hub's shared-mode Protobuf listener. Must not be empty. |
| `partition` | `local` | Cache-sharing boundary for entries under the `ovstorage` key scheme. |
| `key_scheme` | `ovstorage` | `ovstorage` keys entries per partition. `omni-client-compat` shares entries with omni.client (Kit and the Client Library) on the same machine. |
| `max_object_bytes` | unset (no limit) | Larger objects are served but never stored in Hub. |
| `max_streaming_fills` | `64` | Concurrent fills; `0` turns filling off. |
| `timeout_ms` | `1000` | Deadline for each Hub call. |

Python, adapted from ovstorage's `tools/ovtasks/smoke_bundled_plugins.py` and not run here:

```python
import asyncio

import ovstorage
from ovstorage import ConfigValue, ConnectionRequest, hub_cache, plugin


async def main() -> None:
    registry = ovstorage.PluginRegistry([ovstorage.bundled_plugins_dir()])
    request = ConnectionRequest("http")
    request.add_config("root_url", ConfigValue.string("https://example.com/"))
    stack = await (
        ovstorage.Stack(root="cached")
        .with_registry(registry)
        .wrapper(hub_cache.HubCache("cached", "origin",
                                    {"endpoint": ConfigValue.string("127.0.0.1:14093")}))
        .backend(plugin.PluginBackend("http", name="origin"))
        .connection("origin", request)
        .build()
    )
    data, _info = await stack.read("https://example.com/scene.usd")
    print(len(data))


asyncio.run(main())
```

The same stack as an ovstorage TOML file, written from the layer and connection rules in `docs/public/configuration.md` (not run). A layer's `kind` defaults to its name, and a connection attaches to the layer named by `target`, which defaults to `backend_kind`. Naming the backend layer `http` lets the connection attach without a `target`. If you name it differently, for example `origin`, also set `kind = "http"` on that layer, point the wrapper's `inner` at `origin`, and add `target = "origin"` to the connection. Without `kind`, the layer's kind defaults to its name and the stack fails to load with `unknown layer kind 'origin'`. Load the plugins before building the stack.

```toml
[ovstorage]
root = "cached"

[ovstorage.layers.cached]
kind = "hub_cache"
inner = "http"
endpoint = "127.0.0.1:14093"

[ovstorage.layers.http]

[[ovstorage.connections]]
backend_kind = "http"
config = { root_url = "https://assets.example.com/" }
```

### Security and key-scheme warnings

- Hub accepts writes from any local process without authentication, so `hub_cache` puts every local process inside the stack's trust domain. Enable it only where local processes are already trusted with the cached objects (`docs/public/configuration.md`).
- Under the default `ovstorage` scheme, entries carry no provable content identity: a local process that derives a key can plant bytes the layer will serve.
- Under `omni-client-compat`, two objects that declare the same content digest share one entry, even at different addresses, because the key contains neither the URL nor the partition. Entries are checked against their bytes before they are served. Use this scheme only to share a cache with omni.client, and not for content whose digests you don't control (0.3.0 release notes, known issue).

### Validate

- On a second read of the same object, Hub's `cache_hits` rises (`curl -s http://127.0.0.1:14090/api/1.0/cache/stats`).
- ovstorage exports `ovstorage_hub_cache_requests_total`, `ovstorage_hub_cache_lookup_duration_seconds`, and `ovstorage_hub_cache_fills_total`.
- If nothing rises, check that the backend declares a strong ETag and that Hub is running in shared mode on the configured endpoint.

## ovstorage through OVCC (`[ovstorage.content_cache]`)

ovstorage can send storage-origin reads through OVCC as a forward proxy. One host-level table configures it for every connection and layer that supports it. Everything in this section comes from the "Content cache (usd-content-cache)" section of the ovstorage repo's `docs/public/configuration.md`; nothing here was tested against an OVCC deployment.

```toml
[ovstorage.content_cache]
url = "https://ovcc.<ns>.svc:14128"   # the OVCC Service; https so OVCC fetches the origin over TLS
tls_verify = true                     # verifies the certificate on the ovstorage-to-OVCC hop only
```

- **Environment variables:** `OVSTORAGE__CONTENT_CACHE__URL` and `OVSTORAGE__CONTENT_CACHE__TLS_VERIFY` for the CLI, MCP server, and file-backed Python loader. The broker uses the `OVSTORAGE_BROKER__OVSTORAGE__CONTENT_CACHE__*` spellings; the REST gateway uses `OVSTORAGE_REST__…`.
- **Only the host table.** A connection that sets `content_cache_url` or `content_cache_tls_verify` directly is refused at startup.
- **Use `https://`.** ovstorage sends an absolute-form proxy request, and OVCC fetches the origin with the scheme of the client's connection to it. With an `http://` cache URL, credentials in the request, including presigned signatures, cross the cache hop in cleartext.
- **Certificate trust.** With `tls_verify = true`, OVCC's certificate must chain to a CA the ovstorage host trusts and carry a SAN for the host in `url`. For a self-signed test certificate from [deploy.md](shared-content-cache/deploy.md), either add it to the ovstorage host's trust store or set `tls_verify = false`, which is for trusted test setups only. How ovstorage picks its CA bundle is a question for the ovstorage expert.

**What gets cached:**

| Request | Route | Cached by OVCC? |
| --- | --- | --- |
| Payload read pinned with `if_match` (S3, or Azure with Shared Key or SAS) | Cache; the ETag travels as `If-Match` beside the presigned or SAS URL | Yes, under the default `requiresVersioning: true` |
| Unpinned payload read | Cache, without `If-Match` | No: `BYPASS` (stored only if `requiresVersioning: false`) |
| Payload read authorized by a header (bearer, Basic, Shared Key, Entra OAuth, HTTP `secret_headers`) | Direct to origin | No |
| Stat, list, and other metadata | Cache, with `Cache-Control: no-cache, no-store` | No |
| Writes, deletes, token calls, plain-`http://` origins | Direct | No |
| `file`, `opendal`, `nucleus`, services-client, and broker-client backends | Excluded | No |

- **S3 addressing must match OVCC's `serverName`.** OVCC's default S3 `serverName` matches only virtual-hosted regional hosts, `<bucket>.s3.<region>.amazonaws.com`. ovstorage's `s3` backend uses virtual-hosted addressing on AWS unless `force_path_style` is set, a non-AWS compatibility profile requires path-style, or the bucket name contains dots. Path-style requests pass through OVCC uncached.
- **Shared entries.** With the cache configured, ovstorage signs S3 presigned GETs on 300 s epochs, each valid for 600 s from the start of its epoch, and OVCC strips signature parameters from its key. Clients that read the same object and ETag therefore share one entry.
- **Revocation lag.** OVCC caches a presigned URL's successful authorization until the URL expires (minus 2 s), so access revoked at the origin keeps working through OVCC until that URL expires. With the cache configured, each URL is valid until the end of the epoch after the one it was signed in, so the lag is 5 to 10 minutes depending on when in the epoch it was signed.
- **Validate on the OVCC pod:** two pinned reads of the same object show `cc_http_requests_total{cache_status="HIT"}` rising and `"upstream_cache_status":"HIT"` in the access log ([validate.md](shared-content-cache/validate.md)). ovstorage counts `ovstorage_content_cache_responses_total{status=...}` and logs `content_cache.status`.
- **Ask the ovstorage expert** which read API sets `if_match` in your host language, and about the cache failure and cool-down policy.
