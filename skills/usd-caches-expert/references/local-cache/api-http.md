Up: [local cache](README.md)

# Hub HTTP API

The router is in `local-cache/reference/hub/src/http/mod.rs` (`/api` nested, static files at `/`) and `hub/src/http/api/mod.rs`, with handlers in `health_routes.rs`, `shutdown_routes.rs`, `cache_routes.rs`, and `service_routes.rs`. Request samples are in [`hub/api.http`](../../../../local-cache/reference/hub/api.http). The server binds `http.host:http.port`, by default `127.0.0.1:14090` in shared mode. In exclusive mode the port is random; read it from `<cache.path>/hub.port`. There is no auth, no TLS, and no CORS configuration. Errors from `ApiError` are JSON `{"message": "..."}`. Other failures are bare status codes.

Setup for the examples:

```bash
HUB=http://127.0.0.1:$(cat ~/.cache/ov/cache/hub/hub.port 2>/dev/null || echo 14090)
```

The path shown is the default Linux `cache.path`. On Windows it is `%LOCALAPPDATA%\ov\cache\hub\hub.port`.

## Endpoint index

| Method | Path | Purpose | In `api.http`? |
|---|---|---|---|
| GET | `/api/health` | Liveness and uptime | yes |
| GET | `/api/1.0/shutdown` | Graceful shutdown | no |
| DELETE | `/api/1.0/cache/clear` | Remove all unopened entries and run GC | no |
| GET | `/api/1.0/cache/dumpdb` | Dump the index to the trace log | no |
| GET | `/api/1.0/cache/stats` | Cache statistics | yes |
| DELETE | `/api/1.0/cache/stats/reset` | Reset hit and miss counters | yes |
| GET | `/api/1.0/service/args` | Startup arguments | no |
| GET | `/api/1.0/service/config` | Effective settings | yes |
| PUT | `/api/1.0/service/config` | Merge settings into `hub.toml` and soft-restart | yes (4 samples) |
| GET | `/api/1.0/service/reset` | Delete `hub.toml` and soft-restart | no |
| GET | `/api/1.0/service/status` | Exe path and settings | no |
| GET | `/api/1.0/service/supportbundle` | Zip of status, config, and logs | no |
| GET | `/*` (e.g. `/index.html`) | Static settings UI from the `web/` dir | no |

## Health

`GET /api/health` → `200 {"health":"ok","uptime":<seconds since the router was built>}`. `uptime` resets on soft restart.

```bash
curl -s $HUB/api/health
# {"health":"ok","uptime":42}
```

## Shutdown

`GET /api/1.0/shutdown` → `200`, empty body, then the process exits. It is a **GET**, so a browser prefetch or link can trigger it. `hub --shutdown` uses this endpoint.

```bash
curl -s -o /dev/null -w '%{http_code}\n' $HUB/api/1.0/shutdown   # 200; then /api/health fails to connect
```

## Cache

`DELETE /api/1.0/cache/clear` removes every index entry that **isn't open** (in-use reads and writes stay), flushes, and runs one GC pass immediately to delete orphaned data files. Response: `200`, or `500` on an engine error (`Cache::clear`).

```bash
curl -s -X DELETE -o /dev/null -w '%{http_code}\n' $HUB/api/1.0/cache/clear   # 200
curl -s $HUB/api/1.0/cache/stats | python3 -m json.tool | grep cache_entries  # 0 if nothing was open
```

`GET /api/1.0/cache/dumpdb` → `200`, empty body. It writes every index record and open record at **trace** level, plus one info line `len=… open=…`, to the log. It is only useful with `logging.file.max_level = "trace"` (`Cache::dump_database`, whose file output is TODO).

`GET /api/1.0/cache/stats` → `200` JSON (`hub-cache/src/block/cache.rs` `Stats`, plus 3 legacy keys from `cache_routes.rs`):

| Key | Meaning |
|---|---|
| `cache_entries` | Committed entries (files plus blocks). |
| `cache_size` | Sum of committed entry sizes in bytes. GC compares this to `max_size`. |
| `cache_hits` | Gets that found an entry, since start or the last reset. |
| `cache_misses` | `accesses − hits`. |
| `total_stored_data` | Bytes committed since start. **Not** reset by `/stats/reset`. |
| `reads_during_file_write` | Gets that hit an entry open for write (`ERROR_PENDING_WRITE`). |
| `writes_during_file_write` | Puts on an entry open for write. |
| `cache_on_disk_size` | **Always 0** (`hub-cache/src/block/file.rs` TODO). |
| `database_size` | sled size on disk. |
| `uptime` | Cache engine uptime in seconds. |
| `current_size`, `hit_rate`, `number_of_files` | Always `0`. Launcher compatibility shim, marked "deprecate/remove". |

```bash
curl -s $HUB/api/1.0/cache/stats
# {"cache_entries":2,"cache_hits":2,"cache_misses":1,"cache_on_disk_size":0,"cache_size":53,"current_size":0,
#  "database_size":524288,"hit_rate":0,"number_of_files":0,"reads_during_file_write":0,"total_stored_data":53,
#  "uptime":120,"writes_during_file_write":0}
```

Key order and `database_size` vary.

`DELETE /api/1.0/cache/stats/reset` → `200`. It zeroes `cache_hits`, accesses (and therefore misses), `reads_during_file_write`, and `writes_during_file_write`.

```bash
curl -s -X DELETE -o /dev/null -w '%{http_code}\n' $HUB/api/1.0/cache/stats/reset   # 200
```

## Service

`GET /api/1.0/service/args` → the serializable startup args: `{"hub_config":<path|null>,"omniverse_config":<path|null>,"mode":"Exclusive"|"Shared"}`. The other flags are `serde(skip)`.

```bash
curl -s $HUB/api/1.0/service/args   # {"hub_config":null,"mode":"Shared","omniverse_config":null}
```

`GET /api/1.0/service/config` → the effective `hub` settings, with the **actually bound** ports substituted:

```bash
curl -s $HUB/api/1.0/service/config
# {"cache":{"cleanup_threshold":10,"garbage_collection_interval":300,"max_size":"150.0 GB","path":"/home/u/.cache/ov/cache/hub"},
#  "http":{"host":"127.0.0.1","port":14090},
#  "logging":{"file":{"compression":true,"count":7,"enabled":true,"file":"hub.log","json":true,"max_level":"info","max_size":"134.2 MB"},
#             "path":"/home/u/.nvidia-omniverse/logs/hub","stderr":{...},"stdout":{...}},
#  "runtime":{"num_worker_threads":4},"transport":{"tcp":{"ext_port":14093,"host":"127.0.0.1","rpc_port":14092}}}
```

The Hub client consumes this shape through `--write-config` (`hub-client/src/client.rs` deserializes `Hub`).

`PUT /api/1.0/service/config` takes a JSON body with any subset of the `hub.toml` structure (see [configuration.md](configuration.md)). It requires `Content-Type: application/json`. Behavior (`service_routes.rs` `update_config`):
1. Merge the target `hub.toml` (`--hub-config`, else `~/.nvidia-omniverse/config/hub.toml`) with the body.
2. Deserialize. Failure → `400 {"message":"Failed to deserialize settings: …"}`. An empty or non-JSON body → `400` from the extractor.
3. Rewrite that file with non-default values only. Failure (for example, the config directory doesn't exist) → `500 {"message":"Failed to write toml file: …"}`.
4. Return `200` and **soft-restart**: all listeners close and rebind with the new settings, and the cache reopens. Existing RPC connections drop.

```bash
curl -s -X PUT -H 'Content-Type: application/json' -w '%{http_code}\n' \
  -d '{"cache":{"max_size":"40 GB","cleanup_threshold":20}}' $HUB/api/1.0/service/config   # 200
sleep 1; curl -s $HUB/api/1.0/service/config | python3 -c 'import sys,json; print(json.load(sys.stdin)["cache"])'
# {'cleanup_threshold': 20, 'garbage_collection_interval': 300, 'max_size': '40.0 GB', 'path': '...'}
```

The four `api.http` samples, with corrections:
- `cache` (path, max_size, cleanup_threshold, garbage_collection_interval): valid, except the sample has a trailing comma, which is invalid JSON and returns `400`. `garbage_collection_interval` is accepted but has no effect ([configuration.md](configuration.md)).
- `http` (`port`, `host`): valid. A changed port takes effect after the restart, and `hub.port` is rewritten.
- `runtime.num_worker_threads`: valid.
- `transport_tcp` `{host, port}`: **wrong shape**. Hub ignores the unknown key, so the PUT returns 200 and changes nothing. The correct body is `{"transport":{"tcp":{"host":"127.0.0.1","rpc_port":14092,"ext_port":14093}}}` (unit test `update_transport_config`).
- `api.http` also sets `@host = 0.0.0.0:14090`. Hub binds `http.host`, which defaults to `127.0.0.1`. Use the configured address, `127.0.0.1` by default; connecting to `0.0.0.0` works on Linux but not on Windows.

`GET /api/1.0/service/reset` deletes the target `hub.toml` (a missing file is fine), returns `200`, and soft-restarts with defaults plus env and CLI overrides. `500 {"message":"Failed to remove …"}` on an I/O error. It is a **GET**.

```bash
curl -s -o /dev/null -w '%{http_code}\n' $HUB/api/1.0/service/reset   # 200
```

`GET /api/1.0/service/status` → `200 {"exe_path":"<abs path of hub>","config":{…same as /service/config…}}`, or `500` if the exe path can't be resolved.

```bash
curl -s $HUB/api/1.0/service/status | python3 -c 'import sys,json; print(json.load(sys.stdin)["exe_path"])'
```

`GET /api/1.0/service/supportbundle` → `200`, `Content-Type: application/zip, application/octet-stream`, `Content-Disposition: attachment; filename="…"`. Built in a temp dir (`hub-supportbundle-*`, `hub/src/support_bundle.rs`), it contains `status.json` (the status above), `hub.toml` (the target file), `omniverse.toml` (always `~/.nvidia-omniverse/config/omniverse.toml`), and `logs/` (the `logging.path` dir). Missing or unreadable input files are skipped silently (`is_file()` check), so a bundle without `hub.toml` is normal when defaults are in use. Database dumps are not included (TODO in source). The temp dir is deleted when the handler returns while the file is still being streamed. That is fine on Unix (unlinked files stay readable). Windows behavior can't be determined from source.

```bash
curl -s -o hub-support.zip $HUB/api/1.0/service/supportbundle && unzip -l hub-support.zip
```

## Static settings UI

`GET /` serves files from `<dir of hub exe>/web` if that directory exists, otherwise `./web` relative to the CWD (`http/mod.rs` `web_dir`). The page (`web/public/index.html` + webpack `bundle.js`) reads the API port from the `port` query parameter. Without it, every control is disabled and it alerts `Hub port not found` (`web/src/main.js`). The published URL is `http://localhost:14090/index.html` (`docs/index.rst`). See [troubleshooting.md](troubleshooting.md#settings-page) for the query-string parsing caveat.
