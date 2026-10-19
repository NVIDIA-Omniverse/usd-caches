Up: [local cache](README.md)

# Hub architecture

## Workspace and components

`local-cache/reference/Cargo.toml` is a Cargo workspace with these members. `web/` is a separate npm project.

| Crate / dir | Role |
|---|---|
| `hub` | The service binary (`hub`, `hub.exe`). It parses arguments and settings, runs the HTTP API and static web UI (axum), the MessagePack RPC server, the Protobuf RPC server, and the cache background tasks (`local-cache/reference/hub/src/hub.rs`). |
| `hub-cache` | The storage engine: a sled database (`hub-db/`) plus a pool of data files (`hub-files/`). It implements put, get, close, commit, abort, abandon, stat, clear, GC, and LRU (`local-cache/reference/hub-cache/src/block/cache.rs`, `file.rs`). |
| `hub-rpc` | Wire types. Rust MessagePack enums (`src/hub-rpc.rs`), prost code generated from **only** `proto/nvidia/omniverse/usd_caches/local_cache/v1beta/local_cache.proto` (`hub-rpc/build.rs`), and the conversions between them (`src/conversions.rs`). |
| `hub-client` | A Rust client and C static library (`staticlib`, cbindgen header `hub_client_bindings.h`). It discovers or launches Hub and speaks MessagePack (default) or Protobuf (`local-cache/reference/hub-client/src/client.rs`, `src/launcher/mod.rs`). The source says (`docs/HUB_IN_DOCKER.md`) the Client Library ships a Hub client, but whether that client is this exact crate can't be determined from this repo. |
| `hub-util` | Settings structs and loaders (`hub.toml`, `omniverse.toml`, env, CLI), plus filesystem helpers (`local-cache/reference/hub-util/src/settings/`). |
| `web/` | The settings page (`public/index.html`, `src/main.js`, webpack → `dist/bundle.js`). Hub serves it from the `web/` directory next to the executable, or from `./web` relative to the CWD (`local-cache/reference/hub/src/http/mod.rs`). |

## Process model and modes

`hub --mode=<exclusive|shared>`. The CLI default is **exclusive** (`local-cache/reference/hub/src/args.rs`). The Hub client defaults to **shared** unless `OMNICLIENT_HUB_MODE=exclusive` (`hub-client/src/client.rs` `determine_mode`).

| | Shared | Exclusive |
|---|---|---|
| Ports | Configured defaults: HTTP 14090, MessagePack 14092, Protobuf 14093. | All three default to `0`, so the OS assigns them. Overrides still apply (`hub-util/src/settings/mod.rs` `load_hub_config`, test `hub/src/settings.rs`). |
| Lifetime | Independent daemon that serves any number of clients. | Launched as the client's child with a death signal so it exits with the parent (`hub-client/src/launcher/mod.rs` `command_exclusive`). Hub itself does not watch its parent. |
| Client requirement | None. | The client must supply a cache dir (`OMNICLIENT_HUB_CACHE_DIR`), otherwise `Error::Config` (`hub-client/src/client.rs`). |
| Docker | Containers are "assumed to always be running in shared mode" (`docs/HUB_IN_DOCKER.md`). | — |

**Singleton per cache directory.** At startup Hub creates `<cache.path>/hub.lock` and takes an exclusive advisory lock (`fd-lock`), then writes its PID into the file. If the lock is already held, the new process doesn't serve. It waits up to 2 s (200×10 ms) for `<cache.path>/hub.port`, and then does one of the following (`hub/src/hub.rs`):
- With `--shutdown`: `GET http://127.0.0.1:<port>/api/1.0/shutdown`.
- With `--write-config <file>`: `GET http://127.0.0.1:<port>/api/1.0/service/config` and atomically writes the JSON to `<file>`.
- Otherwise: exits 0.

If the lock is free: `--detect-only` returns the error `Hub is not running`, and `--shutdown` exits 0 with a warning. Otherwise Hub starts.

**Port discovery.** After binding HTTP, Hub writes the bound HTTP port to `<cache.path>/.hub.port` and renames it to `hub.port`. With `--write-config`, it waits until both RPC servers have bound, then writes the full settings JSON (with the actual ports) to that file (`hub/src/hub.rs`).

**Soft restart.** `PUT /api/1.0/service/config` and `GET /api/1.0/service/reset` fire `reset_notify`. `main` then shuts down its tasks, re-reads settings from the same arguments, reopens the cache (possibly at a new path), and rebinds every port without exiting the process (`hub/src/hub.rs` `while keep_running`).

## How clients find and use Hub (Hub client)

From `local-cache/reference/hub-client/src/client.rs` and `launcher/mod.rs`:

1. The client takes a per-user discovery lock: `<temp>/hub-<username>.lock`.
2. It finds the executable. With `OMNICLIENT_HUB_EXE` set, the search path is that file, or a directory. A directory is searched for `target/release/hub`, `target/debug/hub`, `release/hub`, `debug/hub`, or `hub`, and in shared mode also for the newest-semver `hub-<ver>/` subdirectory. Otherwise:
   - Shared mode looks for the newest `hub-<semver>/` under `omniverse.toml` `paths.library_root` that contains a `.installed` marker.
   - Exclusive mode looks next to the client dylib, then next to the current exe.
3. It spawns `hub --mode=<m> --write-config <temp>/hub-<user>-<rand>.config.json -- [cache.path=<OMNICLIENT_HUB_CACHE_DIR>] [cache.max_size=<OMNICLIENT_HUB_CACHE_SIZE>]`. If Hub is already running for that cache dir, the spawned process only writes the config of the running instance and exits (see above).
4. It reads the config JSON and connects to `transport.tcp.host`, using `rpc_port` for MessagePack (the default codec) or `ext_port` for Protobuf.
5. It retries up to 40 times with linear backoff of `10 ms × attempt` (`client.rs` `retries: 40`). `HubNotFound` is fatal immediately.

In Docker, a host-side Hub executable is still required as the "config oracle". Point `OMNICLIENT_HUB_EXE` at it and set `HUB__CACHE__PATH` (same path as the container's cache, so the lock and `hub.port` match) and `HUB__ARGS__DETECT_ONLY=true` (`docs/HUB_IN_DOCKER.md`).

The NGC installer scripts don't start Hub. The client launches it on demand (shared mode) from the install directory. See [install-and-run.md](install-and-run.md).

## Cache data model

One engine serves both APIs (`hub-cache/src/block/cache.rs`, `hub/src/rpc/dispatcher.rs`):

- **Index:** sled DB at `<cache.path>/hub-db` (LowSpace mode, zstd compression level 1, 128 MiB page cache). The trees are `system` (uuid, on-disk format version `1.0.0`) and `index` (key → `RecordV1`: flags, data file id, offset, size, last_accessed, accesses, rank, client uuid).
- **Data:** `<cache.path>/hub-files/<XXX>/<016X>[.ext]`. `XXX` is the low 12 bits of the file id in hex. A legacy `hub-pending/` directory is deleted at startup (`file.rs` `FilePool::new`).
- **Blocks (derived data):** packed into shared, sparsely preallocated data files of 64 MiB (`data_file_size`; a file may grow to about 2× to absorb overflow). A block larger than the data file size gets its own file. Clients write and read at `(path, offset, size)` relative to `base_path`.
- **Files (cache-aside):** always get their own file, with the extension appended, created empty at offset 0. On close-after-write, the committed size is the file's actual length from `fs::metadata`, not the declared `size`.
- **Key encoding:** block key = `<key>\0`, file key = `<key>\0<extension>`. The same key with different extensions gives distinct entries (fixed in 2.4.0, `CHANGELOG.md`). A file with an empty extension can collide with a block that has the same key (`dispatcher.rs` NOTE).
- **`provider`** (cache-aside) is accepted and ignored. It isn't part of the key.

Record lifecycle: `put` → open-for-put (written to the index, not committed) → `commit`/`close` sets COMMITTED and adds the record to the size and entry counters. `BlockCacheAbort` deletes the pending record (`abort1_deprecated`). A **disconnect** during a put marks the record **Abandoned** (`dispatcher.rs` `Drop`). A reconnect from the **same client UUID** (`*Connect` with `open_for_write`/`open_for_put`, or a re-`put` of the same size) reclaims it. Abandoned records are treated as not-found for reads. Uncommitted records found at startup are loaded as Abandoned.

## Per-connection state

Each TCP connection (on either RPC port) gets its own `Dispatcher` holding the client name, client UUID, and a map of `handle → open key`. Handles are per-connection integers starting at 1 (`dispatcher.rs`). All cache operations except version queries require `ClientConnect` first. Without it the server returns the error `Unknown client uuid. Client must call connect() first!`.

Requests on one connection are processed **sequentially**: each request is decoded, executed synchronously, and answered before the next frame is read (`hub/src/rpc/server_protobuf.rs` `handle_connection`). Concurrency comes from multiple connections across `runtime.num_worker_threads` Tokio workers.

**On disconnect** (the Dispatcher is dropped while the server is not exiting): open reads are closed, open block and file writes are **abandoned**, so a reconnecting client can reclaim them with `CacheAsideConnect`/`BlockCacheConnect` (`dispatcher.rs` `Drop`).

## Eviction, cleanup, GC

`hub-cache/src/block/cache.rs` `gc_task`/`gc`:
- **Trigger:** a timer, plus `DELETE /api/1.0/cache/clear`. **Interval caveat:** `cache.garbage_collection_interval` in `hub.toml` is never passed to the engine (`hub/src/context.rs` sets only `max_size` and `cleanup_threshold`). The effective interval is the engine default of **900 s**. The on-demand notifier (`garbage_collection_notify`) is never signalled anywhere.
- **Eviction** runs only when the tracked committed size exceeds `cache.max_size`. Records that aren't open are ranked by `last_accessed` (LRU, second granularity), and the oldest are removed until size ≤ `max_size × (100 − cleanup_threshold)/100`. Open (in-use) records are never evicted. The abandoned-record reaper (remove on the 3rd pass) sits inside the over-size sweep, which only iterates keys that were **not** in the open-record map at mark time. Abandoned records live in that map, so in practice they are only reaped if they become abandoned between the mark and sweep phases. Otherwise they persist until a same-UUID client reclaims them, `CacheAsideRemove` removes them, or the process restarts (they reload as Abandoned). This is inferred from `gc()`; see [troubleshooting.md](troubleshooting.md#known-source-defects). The `rank` supplied in `BlockCachePut` is stored as 0 and **not used** for ranking.
- **Every pass:** data files not referenced by any index record are deleted (except the current active block file), and unknown files under `hub-files/` are deleted.
- **LRU updates** (`last_accessed`, `accesses`) are queued from get, stat, and connect and applied by a background task. A 1 s clock task supplies `now`.
- `max_size` is a soft limit. The cache can exceed it until the next GC pass. A single put larger than `max_size` fails with `RESULT_CODE_ERROR_TOO_LARGE`.

## Ports and network

| Port | Default (shared) | Setting | Protocol |
|---|---|---|---|
| 14090 | `127.0.0.1` | `[http] host/port` | HTTP/1.1 and HTTP/2 (axum): API under `/api`, static UI at `/` |
| 14092 | `127.0.0.1` | `[transport.tcp] host/rpc_port` | MessagePack RPC (legacy) |
| 14093 | `127.0.0.1` | `[transport.tcp] host/ext_port` | Protobuf RPC |

Both RPC servers use 4-byte length-prefixed frames with a 32 MiB maximum frame length and `TCP_NODELAY` (`server_protobuf.rs`, `server_msgpack.rs`). `--shutdown`, `--write-config`, and `--detect-only` always contact the running instance at `127.0.0.1`, whatever `http.host` is set to (`hub/src/hub.rs`). There is no TLS and no authentication on any port.

## Limits and scaling

- **Scope:** one node. One serving Hub per cache directory (enforced by the lock). In shared mode, one Hub per host per port set: a second instance with a different cache dir but the same ports fails to bind.
- **Clients must share the filesystem.** Responses carry absolute local paths, and clients `mmap`/read/write those files directly (`hub-rpc/examples/python/*.py`).
- **Container path parity:** bare-metal clients of a containerized Hub need the cache at the same path on the host (`/var/cache/hub` recommended) (`docs/HUB_IN_DOCKER.md`).
- Worker threads: `min(available cores, 4)` by default (`hub-util/src/settings/runtime.rs`).
- The source has no replication, no peer sharing, and no upstream fetch.
- **Unimplemented:** the IPC transport (`[transport.ipc]`: Unix socket or named pipe) has `todo!()` defaults and server paths, so selecting it panics (`hub-util/src/settings/transport.rs`, `server_*.rs`).

## What Hub does not do

- It does not talk to Nucleus, S3, OVCC, or OVDC. Nothing in the source connects to any upstream. `local-cache/reference/README.md` describes Hub as providing "communication between Omniverse Applications and local or server data storage, such as Nucleus or S3". The code implements only a local cache-aside and block store.
- No remote or multi-node serving, no auth, no TLS, no metrics endpoint (only JSON stats), no per-entry TTL or invalidation. Freshness is the client's responsibility: keys are opaque, and a committed key can't be overwritten (`ERROR_ALREADY_EXISTS`) until it is removed or evicted.
- No Cache Cleaner. It was removed in 2.4.0 (`docs/index.rst` release notes).
- Settings pushed through HTTP don't persist the `logging.file.count`, `logging.file.max_size`, or `logging.file.compression` keys (see [configuration.md](configuration.md#persistence-caveats)).
