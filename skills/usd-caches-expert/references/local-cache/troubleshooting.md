Up: [local cache](README.md)

# Troubleshooting Hub

Each row runs symptom → cause → fix and is grounded in the cited source. "Inferred" marks conclusions reasoned from code that were not reproduced against a live Hub.

## Clients and discovery

| Symptom | Cause | Fix |
|---|---|---|
| Errors loading files in Kit or Client Library tools after enabling Hub | Client Library older than 2.49.2 (`docs/index.rst` warning). | Upgrade to Client Library ≥ 2.49.2. |
| A newly installed Hub isn't used by a running app | "Hub does not connect to running Kit applications until you restart them" (`docs/index.rst`). | Restart the apps. |
| Windows, Kit ≤ 107.3: "Hub not found" although it is installed | Known issue (`docs/index.rst`). | `mklink /d %APPDATA%\ov %LOCALAPPDATA%\ov`, or upgrade to Kit ≥ 109.0.1. |
| Hub installed with `install.sh -d` / `install.bat -d` isn't found | Shared discovery only scans `paths.library_root` (`hub-client/src/launcher/mod.rs`). | Set `OMNICLIENT_HUB_EXE` to the exe or to the install root directory. |
| An install is ignored, or an older version is picked | `.installed` marker missing; the highest semver with the marker wins (`find_latest_hub`). | Re-run the install script, or remove stale `hub-*` dirs. Updating doesn't remove old versions (`docs/index.rst`). |
| Client error `Using exclusive mode requires providing a cache directory via OMNICLIENT_HUB_CACHE_DIR` | `OMNICLIENT_HUB_MODE=exclusive` without a cache dir (`hub-client/src/client.rs`). | Set `OMNICLIENT_HUB_CACHE_DIR`, or use shared mode. |
| Client error `Unknown value provided for OMNICLIENT_HUB_MODE` | The value isn't `shared` or `exclusive`. | Fix the variable. |
| Client gives up after about 8 s (`Hub failed to launch` / `Tcp connect failed` in client logs) | 40 retries with linear backoff (`client.rs`). The spawned `hub` exited non-zero without writing config, or the advertised host and port are unreachable. | Run `hub --mode=shared --write-config /tmp/h.json` by hand and read its error. Check `transport.tcp.host` (it must be an address the client can reach; Hub binds `127.0.0.1` by default). |
| Two apps use **different** Hub instances or caches | The singleton lock lives inside `cache.path` (`hub/src/hub.rs`). Clients that resolve different `cache.path` values (`OMNICLIENT_HUB_CACHE_DIR`, `HUB__CACHE__PATH`, `hub.toml`, `omniverse.toml`) start separate Hubs, and in shared mode the second one then fails to bind ports 14090/14092/14093. | Make every client resolve the same `cache.path`. Check with `/api/1.0/service/status`. |

## Startup and ports

| Symptom | Cause | Fix |
|---|---|---|
| Hub exits or panics at startup with `Address already in use` | Another process, or a Hub with a different `cache.path`, holds the port. The HTTP bind is `unwrap()`ed (panic), and an RPC bind error ends the task, which shuts Hub down (`hub/src/hub.rs` `select!`). | Free the port, or give this instance distinct `http.port`, `transport.tcp.rpc_port`, and `ext_port` (or use exclusive mode, which defaults to port 0). |
| Panic `Failed to read port file from cache directory` | The lock is held but `hub.port` didn't appear within 2 s (`wait_for_port`, 200×10 ms). The first instance is still starting or can't write. | Retry. Check that `cache.path` is writable. |
| Startup error creating `hub.lock`, `hub-db`, or `hub-files` | `cache.path` isn't writable, or is on a filesystem without advisory locks (`fs::create_dir_all`, `fd-lock`). | Use a local, writable disk (`docs/index.rst`: "Make sure this is a local disk"). |
| Startup error `Cache max size must be > 0` / `Cache cleanup threshold '<n>' must be <= 100` | Invalid `cache.max_size` / `cleanup_threshold` (`hub-cache/src/block/cache.rs` builder). | Fix the value. |
| Startup error `expected configuration value in the form key=value` | A CLI override after `--` is malformed, or its value contains `=` (`hub-util/src/settings/mod.rs`). | Pass that value through `hub.toml` or an env var instead. |
| Config deserialize error at startup | Wrong type in `hub.toml`, a `HUB__*` env var, or an override. | Run `hub --show-config` to locate it. Unknown keys are silently ignored, so also check for typos (see [configuration.md](configuration.md)). |
| `hub --shutdown` / `--write-config` / `--detect-only` say Hub isn't running although it is | They resolve `cache.path` from *their own* settings, and they contact `127.0.0.1` regardless of `http.host` (`hub/src/hub.rs`). | Run them with the same `HUB__CACHE__PATH` or `hub.toml`, and keep `http.host` reachable on loopback. |
| Panic on startup when selecting `[transport.ipc]` | IPC transport is `todo!()` (`hub-util/src/settings/transport.rs`). | Use `[transport.tcp]`. |

## Docker

| Symptom | Cause | Fix |
|---|---|---|
| A bare-metal client gets paths it can't open | Hub returns absolute paths inside its cache. The host and container paths differ (`docs/HUB_IN_DOCKER.md`). | Mount `/var/cache/hub:/var/cache/hub`, the same path on both sides. |
| Container can't write the cache | Directory owner doesn't match the `-u $(id -u):$(id -g)` user. | `chown` the host directory to that user. |
| Client error `Hub is not running` | `HUB__ARGS__DETECT_ONLY=true`, and the client-side `hub` found no lock holder at `HUB__CACHE__PATH`. Either the container isn't running or the paths differ. | Start the container. Make `HUB__CACHE__PATH` equal the mounted path. |
| Client can't connect although the container runs | Not using `--network=host`. The client-side binary calls `127.0.0.1:<hub.port>` (`hub/src/hub.rs`). | Run the container with `--network=host`. |
| Client can't find Hub at all | `OMNICLIENT_HUB_EXE` isn't set. A host-side Hub binary is required even with containerized Hub (`docs/HUB_IN_DOCKER.md`). | Install a `hub` binary and point `OMNICLIENT_HUB_EXE` at it. |

## Settings page

| Symptom | Cause | Fix |
|---|---|---|
| `http://localhost:14090/index.html` shows disabled controls and alerts `Hub port not found` | `web/src/main.js` requires a `port` query parameter. It also builds `new URLSearchParams(window.location)`, which parses the **full URL**, so `?port=14090` as the *first* parameter becomes the key `http://…/index.html?port` and `params.port` is null (inferred from URLSearchParams semantics). | Open `http://localhost:14090/index.html?x=1&port=14090`, or use the HTTP API directly ([api-http.md](api-http.md)). |
| `/index.html` returns 404 for a source build | Hub serves `<exe dir>/web` or `./web`. In the source tree, `index.html` is under `web/public/` and the bundle is under `web/dist/` (`hub/src/http/mod.rs`). | Assemble `target/release/web/` ([build.md](build.md#web-ui)). |
| Apply returns an error, or `PUT /service/config` returns `500 Failed to write toml file` | `~/.nvidia-omniverse/config/` doesn't exist. `tokio::fs::write` doesn't create parent dirs (`service_routes.rs`). | `mkdir -p ~/.nvidia-omniverse/config`, or start Hub with `--hub-config <existing dir>/hub.toml`. |
| A setting changed in the UI or by PUT doesn't take effect | An env (`HUB__*`) or CLI override wins after the restart. `garbage_collection_interval` is never applied. `<exe dir>/hub.toml` has lower precedence than the user file. | Remove the overriding env or CLI value. See [configuration.md](configuration.md). |
| Custom log rotation settings vanish | PUT rewrites `hub.toml` without `logging.file.count`, `logging.file.max_size`, or `logging.file.compression` (`hub-util/src/settings/logging.rs` `serialize_to_toml`). | Re-add them by hand after any UI or PUT change. |

## Disk usage and eviction

| Symptom | Cause | Fix |
|---|---|---|
| The cache directory is larger than `max_size` | `max_size` is a soft cap checked by GC every **900 s**; the configured interval is ignored (`hub/src/context.rs`). Open entries aren't evicted, block files are sparse and preallocated (64 MiB), and orphan files are removed only at GC (`hub-cache/src/block/cache.rs`, `file.rs`). | Wait for GC, or `DELETE /api/1.0/cache/clear`, which runs GC immediately. Lower `max_size`, or raise `cleanup_threshold` to make GC free more each pass. |
| Space isn't reclaimed after client crashes | Writes interrupted by a disconnect become Abandoned. The GC reaper for those rarely runs (inferred; see the defects list below). | `DELETE /api/1.0/cache/clear` doesn't remove open or abandoned records either. Reclaim them with a same-UUID reconnect, or by removing the entries with `CacheAsideRemove` on a reconnected handle. Otherwise deleting the cache dir while Hub is stopped is the only way this source supports. |
| `cache_on_disk_size` stays 0 | Not implemented (`file.rs` TODO). | Use `du` on `hub-files/` and `cache_size`. |
| Stats are all 0 while apps run | The apps aren't using this instance or cache dir, or the Client Library is too old. | Follow [validate.md](validate.md) steps 2–4. |

## RPC result codes and errors

| Result | Meaning (`hub/src/rpc/dispatcher.rs`) | Client action |
|---|---|---|
| `error_info: Unknown client uuid. Client must call connect() first!` | No `ClientConnect` on this connection. | Send `ClientConnect` first on every new connection. |
| `error_info` with a UUID parse error | `client_uuid` isn't a UUID string. | Send `str(uuid.uuid4())`. |
| `error_info: unrecognized message type` | `Request.params` oneof is unset. A common cause in Python is assigning an empty message without `SetInParent()`. | Use `request.params.<field>.SetInParent()` ([api-protobuf.md](api-protobuf.md#runnable-examples)). |
| `RESULT_CODE_ERROR_NOT_FOUND` | Miss, abandoned entry, or a Connect for something no longer there. | Fetch from upstream and write it (cache-aside). |
| `RESULT_CODE_ERROR_PENDING_WRITE` | Another writer has the key open. | Retry later, or read from upstream directly. |
| `RESULT_CODE_ERROR_ALREADY_EXISTS` | The key is committed (immutable) or open for read. | Use the existing entry. To replace it, open it, `CacheAsideRemove`, then write. |
| `RESULT_CODE_ERROR_ABANDONED` | An abandoned put owned by another client UUID. | Reconnect with the original UUID, or treat it as a miss and don't cache. |
| `RESULT_CODE_ERROR_TOO_LARGE` | Item size > `cache.max_size`. | Raise `max_size`, or don't cache the item. |
| `RESULT_CODE_ERROR_HANDLE_NOT_FOUND` | The handle isn't open on **this** connection; handles don't survive reconnects. | Re-register with `CacheAsideConnect` / `BlockCacheConnect`. |
| `RESULT_CODE_ERROR_INTERNAL` | Empty key, `size=0`, I/O error (for example, the file was deleted before `Close`), or another engine error. | Check the Hub log. |
| Connection closed by Hub | A frame > 32 MiB, an empty batch while debug spans are enabled (for example `RUST_LOG=debug`), or a duplicate handle in `*Connect` (panic) (`server_protobuf.rs`, `dispatcher.rs`). | Split batches below 32 MiB, never send empty lists, and use unique handles. |

## Logs, build, and tests

| Symptom | Cause | Fix |
|---|---|---|
| No `hub.log` | `logging.file.enabled=false`, or `logging.path` isn't writable. Default path `~/.nvidia-omniverse/logs/hub` (`hub/src/settings.rs`). | Check `/service/config` `.logging`. |
| `hub.log` lacks per-connection events | Inferred: the configured subscriber is thread-local to the main thread (`set_default` in `hub/src/logging.rs`), while connection tasks run on worker threads. | Run with `logging.stdout.enabled=true` and/or `RUST_LOG=info`, and capture stdout/stderr of a manually started Hub. |
| `openssl` / `pkg-config` build errors on Linux | Missing dev packages (`local-cache/reference/README.md`). | `sudo apt install libssl-dev pkg-config`. |
| Windows build fails in `hub-rpc` | `PROTOC` isn't set (`hub-rpc/build.rs`). | Install `protoc` and set `PROTOC`. |
| Integration tests panic `Could not set NOFILE limit` | The Unix fixture requires an RLIMIT_NOFILE of 100000 (`hub/tests/common/mod.rs`). | Raise the hard limit before running `cargo test`. |
| `uninstall.sh` prints an empty or garbled cache path | `jq` or GNU `head -n -1` is missing (`scripts/uninstall.sh`). | Install `jq` and coreutils. The uninstall itself still proceeds. |
| `simple_client.py` fails on Windows | It uses `mmap.PROT_READ`, which is Unix-only (`hub-rpc/examples/python/simple_client.py`). | On Windows, use `mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)`, or a plain `read()`. |

## Known source defects

Each item is a concrete inconsistency that can be filed on its own. Paths are relative to `local-cache/reference/`.

1. **GC interval not applied.** `hub/src/context.rs` builds the cache without `.garbage_collection_interval(...)`, so `cache.garbage_collection_interval` (default 300) and the UI field "Cache Collection Interval(s)" have no effect. GC uses the `hub-cache` default of 900 s (`hub-cache/src/block/cache.rs` `CacheBuilder::new`).
2. **GC notifier is dead code.** `garbage_collection_notify` is awaited in `gc_task` but never signalled anywhere in the workspace.
3. **Abandoned records are effectively never reaped.** In `gc()`, the 3-pass reaper for `OpenRecord::Abandoned` sits inside the sweep, but the mark phase excludes every key in `open_records`, and abandoned records are held there (`hub-cache/src/block/cache.rs`). Inferred.
4. **Abandoned-put reuse checks the wrong path, keeps the stale record, and ignores ownership.** Any client UUID can take over an abandoned key through a new put; only `*Connect` checks the UUID. In `put1`'s Abandoned branch, `FilePool::file_path(...)` is relative, so `path.exists()` is resolved against the process CWD. When it allocates a new location instead, the open record keeps the old `RecordV1` (old `data_file`/`offset`), and the commit and close later write that old location into the index. The client wrote to the new location, so the index can point at the wrong data. Inferred; not reproduced.
5. **`logging.file.json` is ignored.** `hub/src/logging.rs` always calls `.json()` on the file layer.
6. **PUT `/api/1.0/service/config` loses settings.** `serialize_to_toml` in `hub-util/src/settings/logging.rs` never writes `logging.file.count`, `logging.file.max_size`, or `logging.file.compression`. `cache.max_size` is written through `ByteSize` Display with one decimal place (lossy, for example 40 GiB → `"42.9 GB"`).
7. **Configured logging may not cover worker threads.** `hub/src/logging.rs` installs the subscriber with `set_default` (thread-local) on the main thread, while connection and cache tasks run on Tokio worker threads that fall back to the bootstrap global stdout subscriber from `hub/src/hub.rs`. Inferred.
8. **Empty batch panics.** `#[instrument(... first_key=params.keys[0] / handles[0] / requests[0])]` in `hub/src/rpc/dispatcher.rs` (`block_get`, `block_close`, `block_put`, `block_commit`, `block_abort`, `block_stat`) indexes element 0 whenever the debug span is enabled. The request should be validated instead.
9. **Duplicate handle in Connect panics.** `assert!(res.is_none())` in `cache_connect`/`block_connect` (`dispatcher.rs`) turns bad client input into a task panic instead of a result code.
10. **`rank` is ignored.** `BlockPutItem.rank` is documented as a GC rank (`hub-cache` `PutRequest`), but `put1` stores `rank: 0` and GC ranks by `last_accessed` ("TODO: finalize ranking").
11. **`timeout_us` is ignored.** `BlockCacheGetParams.timeout_us` and `BlockCacheStatParams.timeout_us` are converted but never used by the dispatcher or engine.
12. **`cache_on_disk_size` is always 0,** and `current_size`, `hit_rate`, and `number_of_files` are hard-coded to 0 (`hub-cache/src/block/file.rs` TODO, `hub/src/http/api/cache_routes.rs` "deprecate/remove").
13. **Docs point to the deprecated proto.** `docs/index.rst` ("located at `rpc/proto/nvidia/omniverse/hub/v1alpha/hub.proto`", and it embeds the full v1alpha definition with `HubVersionInfo*`), `docs/PROTOBUF_RPC.md`, and `README.md` all reference `hub/v1alpha/hub.proto`. The server compiles only `usd_caches/local_cache/v1beta/local_cache.proto` (`hub-rpc/build.rs`), and the 2.4.0 release note says it moved.
14. **The deprecated v1alpha proto is still shipped** with "will be removed in the next release" (`hub-rpc/proto/nvidia/omniverse/hub/v1alpha/hub.proto`). Neither proto has a compatibility test.
15. **`hub/api.http` samples are wrong.** The cache PUT body has a trailing comma (invalid JSON). The "Update Transport TCP" body uses `transport_tcp` with a `port` field, which is silently ignored; the correct shape is `{"transport":{"tcp":{"host","rpc_port","ext_port"}}}`. `@host = 0.0.0.0:14090` doesn't match the `127.0.0.1` bind. It covers only 5 of the 12 method+path routes: GET and PUT `/service/config`, `/health`, `/cache/stats`, and `/cache/stats/reset`.
16. **README example config has invalid keys.** `README.md` uses `cleanup_threshhold` (typo, silently ignored) and `[logging.file] color` (not a field). Nothing rejects unknown keys (no `deny_unknown_fields`).
17. **The settings page port parsing is broken.** `web/src/main.js` uses `new URLSearchParams(window.location)` (the full URL, not `.search`), and the published URL `http://localhost:14090/index.html` (`docs/index.rst`) has no `port` parameter at all, so the page disables itself. Inferred from JS semantics.
18. **The Protobuf server logs the wrong protocol name.** `hub/src/rpc/server_protobuf.rs` logs `Messagepack Tcp transport bound to port` when `ext_port` is 0.
19. **Loopback is hard-coded for self-discovery.** `hub/src/hub.rs` uses `http://127.0.0.1:{port}` for `--shutdown` and `--write-config` regardless of `http.host`.
20. **HTTP bind failure panics** (`TcpListener::bind(&http_addr).await.unwrap()` in `hub/src/hub.rs`) instead of returning an error.
21. **State-changing GET endpoints.** `/api/1.0/shutdown` and `/api/1.0/service/reset` are GET with no auth (`hub/src/http/api/*`), so any local process or browser page that can reach the port can trigger them.
22. **Uninstall docs disagree with the scripts.** `docs/index.rst` says `-d` removes the directory "rather than only the contents of the directory", but `scripts/uninstall.sh`/`.bat` remove nothing except `.installed` without `-d`.
23. **Docker image name is inconsistent:** `hub-local` (`docs/index.rst`) vs `hub_workstation_cache` (`docs/HUB_IN_DOCKER.md`, NGC). No Dockerfile is in the repo.
24. **The README overstates the scope.** `README.md` says Hub "provides efficient communication between Omniverse Applications and local or server data storage, such as Nucleus or S3", but the source has no upstream I/O.
25. **Stale workspace URLs.** `Cargo.toml` `homepage` and `repository` point to `.../usd-caches/tree/main/workstation/reference`, but the component lives at `local-cache/reference`.
26. **Python example bugs.** `async_client.py` iterates `pending` without `.items()`, calls `asyncio.sleep(1)` without `await`, and passes the exception class to `set_exception`. `simple_client.py` and `async_client.py` use the Unix-only `mmap.PROT_READ`.
27. **Copy-paste doc comment.** `hub-util/src/settings/logging.rs` documents `logging.path` as "path to cache database and files".
28. **Cache-aside Remove leaks the size and entry counters.** `file_remove_deprecated` (`hub-cache/src/block/cache.rs`) calls `index.remove` for committed records (the Get and not-open branches) without `entries.fetch_sub` or `size.fetch_sub`, unlike `clear()` and `gc()`. `cache_entries` and `cache_size` therefore drift upward until restart, when `open()` recounts the index. GC compares the inflated `size` with `max_size`, so it can evict more than necessary.
29. **Minimal end-to-end config test coverage.** `hub/tests/api.rs` `update_config` is `#[ignore]` because it would overwrite the user's `hub.toml`, so PUT config and soft restart have no integration coverage.
