Up: [local cache](README.md)

# Hub configuration reference

Every user-changeable input, taken from the structs and loaders in `local-cache/reference/hub-util/src/settings/` (`mod.rs`, `cache.rs`, `http.rs`, `logging.rs`, `runtime.rs`, `transport.rs`, `omniverse.rs`, `hub.rs`), the CLI in `local-cache/reference/hub/src/args.rs`, path resolution in `local-cache/reference/hub/src/settings.rs`, and client variables in `local-cache/reference/hub-client/src/client.rs`. None of the structs use `deny_unknown_fields`, so **unknown keys are silently ignored**, including typos such as `cleanup_threshhold`.

## Sources and precedence

`Settings::load_hub_config`. Later rows override earlier ones.

| Order | Source | Notes |
|---|---|---|
| 1 | Built-in defaults | Exclusive mode first overrides `http.port`, `transport.tcp.rpc_port`, and `transport.tcp.ext_port` to `0`. |
| 2 | `hub.toml` | With `--hub-config <path>`, **only** that file is read (a missing file is not an error). Otherwise Hub reads `<dir of hub exe>/hub.toml`, then `~/.nvidia-omniverse/config/hub.toml`, which wins. |
| 3 | Environment `HUB__<SECTION>__<KEY>` | `config` crate: prefix `HUB`, separator `__`, keys lowercased, empty values ignored, values type-parsed. Examples: `HUB__CACHE__PATH`, `HUB__CACHE__MAX_SIZE=40GB`, `HUB__HTTP__PORT`, `HUB__TRANSPORT__TCP__EXT_PORT`, `HUB__LOGGING__FILE__MAX_LEVEL=trace`. |
| 4 | CLI overrides after `--` | `hub [flags] -- key=value ...`, for example `-- cache.max_size=40GB http.port=15090`. Each item must split on `=` into exactly two parts, so values containing `=` are rejected (`CommandLineSource::new`). |

`omniverse.toml` (`Settings::load_omni_config`): `--omniverse-config <path>` exclusively, otherwise `~/.nvidia-omniverse/config/omniverse.toml`, then env `OMNI__<SECTION>__<KEY>` (for example `OMNI__PATHS__CACHE_ROOT`).

Derived paths (`hub/src/settings.rs`): if `cache.path` is unset, it becomes `<omniverse.paths.cache_root>/hub`. If `logging.path` is unset, it becomes `<omniverse.paths.logs_root>/hub`.

Inspect the effective settings with `hub --show-config` (pretty JSON of the `hub` section, then exit). Without `--mode=shared` it shows exclusive-mode ports of `0`. The early stdout logger may print a trailing log line, which the uninstall scripts strip with GNU `head -n -1` (`local-cache/reference/scripts/uninstall.sh`). In answers, use the portable form `hub --mode=shared --show-config | sed -n '/^{/,/^}/p' | jq .`, which keeps only the JSON block (`serde_json::to_string_pretty`, `hub/src/hub.rs`) and works on macOS.

## `[cache]` (`hub-util/src/settings/cache.rs`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `cache.path` | path | unset → `<paths.cache_root>/hub` (Linux `~/.cache/ov/cache/hub`, Windows `%LOCALAPPDATA%\ov\cache\hub`) | Root for `hub.lock`, `hub.port`, `hub-db/`, and `hub-files/`. Created if missing. Keep it on a local disk (`docs/index.rst`). It also determines singleton identity: clients and `--detect-only` must resolve the same path. |
| `cache.max_size` | ByteSize string or int bytes (`"150 GB"`, `"40GB"`, `"40g"`, `55`) | `150 GB` (`ByteSize::gb(150)`, decimal) | Soft cap. GC evicts when the committed size exceeds it, and a single put larger than it fails with `ERROR_TOO_LARGE`. `0` → startup error `Cache max size must be > 0` (`hub-cache/src/block/cache.rs` `CacheBuilder::max_size`). Serialized as `"150.0 GB"`. |
| `cache.cleanup_threshold` | u64 (percent) | `10` | GC removes LRU entries until size ≤ `max_size × (100 − value)/100`. `> 100` → startup error `InvalidGarbageCollectionThreshold`. The settings page calls this "Cleanup Threshold (%)". |
| `cache.garbage_collection_interval` | u64 seconds | `300` | **Not applied.** It is stored, serialized, and shown in the UI, but `hub/src/context.rs` never passes it to `CacheBuilder`, so GC runs every **900 s** (the `hub-cache` default). |

## `[http]` (`hub-util/src/settings/http.rs`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `http.host` | IP string | `127.0.0.1` | Bind address for the API and settings UI. It must parse as an IP (`IpAddr::from_str`), so hostnames are rejected. `--shutdown`, `--write-config`, and `--detect-only` always contact `127.0.0.1`. |
| `http.port` | u16 | `14090` (exclusive: `0`) | HTTP port. The actually-bound port is written to `<cache.path>/hub.port`. A bind failure panics (`TcpListener::bind(...).unwrap()` in `hub/src/hub.rs`). |

## `[logging]` (`hub-util/src/settings/logging.rs`, applied in `hub/src/logging.rs`)

`Logging` has `path` and three sink sub-tables: `logging.file` (`FileLayer`, `[logging.file]`), `logging.stdout`, and `logging.stderr` (`StdIoLayer`, `[logging.stdout]` / `[logging.stderr]`). Each sub-table defaults as a whole when absent.

| Key | Type | Default | Effect |
|---|---|---|---|
| `logging.path` | path | unset → `<paths.logs_root>/hub` = `~/.nvidia-omniverse/logs/hub` | Directory for the file sink. The whole directory is included in the support bundle. |
| `logging.file.enabled` | bool | `true` | Enables the rotating file sink. |
| `logging.file.json` | bool | `true` | **No effect.** The file layer is always JSON (`.json()` is unconditional in `hub/src/logging.rs`). |
| `logging.file.max_level` | `off`/`error`/`warn`/`info`/`debug`/`trace` | `info` (release), `debug` (debug build) | Default directive for the file sink. `RUST_LOG` (EnvFilter) can override it. |
| `logging.file.file` | path | `hub.log` | Joined to `logging.path` (an absolute path replaces it). |
| `logging.file.count` | usize | `7` | Number of rotated files kept. |
| `logging.file.max_size` | ByteSize | `128 MiB` | Rotates when the file passes this size. |
| `logging.file.compression` | bool | `true` | Compresses rotated files, keeping the most recent one uncompressed (`Compression::OnRotate(1)`). Unix log mode is `0600`. |
| `logging.stdout.enabled` | bool | `false` | Stdout sink. The Hub client launches Hub with stdout set to null. |
| `logging.stdout.color` | bool | `false` | ANSI colors. |
| `logging.stdout.max_level` | level | `debug` | Default directive for stdout. |
| `logging.stderr.enabled` | bool | `false` | Stderr sink. |
| `logging.stderr.color` | bool | `false` | ANSI colors. |
| `logging.stderr.max_level` | level | `debug` | Default directive for stderr. |

Before settings load, a bootstrap stdout logger at `INFO` (overridable with `RUST_LOG`) is active (`hub/src/hub.rs` `main`).

## `[runtime]` (`hub-util/src/settings/runtime.rs`)

| Key | Type | Default | Effect |
|---|---|---|---|
| `runtime.num_worker_threads` | usize | `min(available_parallelism, 4)` | Tokio multi-thread worker count (`hub/src/hub.rs`). |

## `[transport]` (`hub-util/src/settings/transport.rs`)

`Transport` is an externally tagged enum. Exactly one of `[transport.tcp]` or `[transport.ipc]` applies. In JSON it appears as `"transport": {"tcp": {...}}`.

| Key | Type | Default | Effect |
|---|---|---|---|
| `transport.tcp.host` | IP string | `127.0.0.1` | Bind address for **both** RPC servers. It is also returned to clients through `--write-config`, which connect to it. |
| `transport.tcp.rpc_port` | u16 | `14092` (exclusive: `0`) | MessagePack RPC port. |
| `transport.tcp.ext_port` | u16 | `14093` (exclusive: `0`) | Protobuf RPC port. |
| `transport.ipc.unix_socket` (Unix) | path | `todo!()` | **Unimplemented.** The default panics, and the servers `todo!()` on non-TCP transports. Do not use. |
| `transport.ipc.named_pipe` (Windows) | path | `todo!()` | **Unimplemented**, same as above. |

## `omniverse.toml` `[paths]` (`hub-util/src/settings/omniverse.rs`)

The base dirs come from `etcetera::choose_base_strategy` (0.8.0): Known Folders on Windows and XDG everywhere else, including macOS (honours `XDG_CACHE_HOME` and `XDG_DATA_HOME`; `choose_base_strategy` is `create_strategies!(Apple, Xdg)`'s second, CLI-style strategy). The published packages target Windows and Linux only (`docs/index.rst`). If the lookup fails, the fallback is `.nvidia-omniverse/`.

| Key | Default Linux | Default Windows | Used by |
|---|---|---|---|
| `paths.cache_root` | `~/.cache/ov/cache` | `%LOCALAPPDATA%\ov\cache` | Hub, to derive `cache.path`. |
| `paths.logs_root` | `~/.nvidia-omniverse/logs` | `%USERPROFILE%\.nvidia-omniverse\logs` | Hub, to derive `logging.path`. |
| `paths.library_root` | `~/.local/share/ov/pkg` | `%LOCALAPPDATA%\ov\pkg` | Hub client, for shared-mode discovery of `hub-<ver>/` with `.installed`. |
| `paths.data_root` | `~/.local/share/ov/data` | `%APPDATA%\ov\data` | Parsed, but Hub doesn't use it. |

## CLI flags (`hub/src/args.rs`, clap)

| Flag | Default | Effect |
|---|---|---|
| `--hub-config <path>` | — | Load only this `hub.toml`. It is also the file that `PUT /service/config` writes and `/service/reset` deletes. |
| `--omniverse-config <path>` | — | Load only this `omniverse.toml`. |
| `--mode <exclusive\|shared>` | `exclusive` | See [overview.md](overview.md#process-model-and-modes). |
| `--show-config` | off | Print the effective `hub` settings as JSON and exit. |
| `--write-config <path>` | — | Write the effective settings JSON (with the bound ports) to `<path>` atomically. If Hub is already running, write the running instance's `/service/config` instead and exit. |
| `--detect-only` (env `HUB__ARGS__DETECT_ONLY`) | off | Never start a new Hub. If none is running for the resolved `cache.path`, exit with error `Hub is not running`. Used for Docker clients. |
| `--shutdown` | off | If running, call `GET /api/1.0/shutdown` (non-200 → error). If not running, warn and exit 0. |
| `--launch-id <u64, hex ok>` | — | **Deprecated.** It is ignored apart from a warning ("Hub Client manages process launching"). |
| `-- key=value ...` | — | Setting overrides (highest precedence). |
| `-h/--help`, `-V/--version` | — | clap built-ins. `--version` prints `hub <version>`, which the install scripts parse with `awk '{print $2}'`. |

## Client-side variables (Hub client, `hub-client/src/client.rs`)

| Variable | Default | Effect |
|---|---|---|
| `OMNICLIENT_HUB_EXE` | unset | Path to the `hub` executable, or to a directory to search. Required when Hub is installed outside `library_root` (`install.sh -d`) and for Docker setups. |
| `OMNICLIENT_HUB_MODE` | `shared` | `shared` or `exclusive` (case-insensitive). Any other value is a config error. |
| `OMNICLIENT_HUB_CACHE_DIR` | unset | Passed to Hub as `cache.path=<dir>`. **Required** in exclusive mode. |
| `OMNICLIENT_HUB_CACHE_SIZE` | unset | Passed as `cache.max_size=<value>`. |

Docker client environment (`docs/HUB_IN_DOCKER.md`): `HUB__CACHE__PATH=/var/cache/hub`, `HUB__ARGS__DETECT_ONLY=true`, `OMNICLIENT_HUB_EXE=/usr/local/bin/hub`. The Hub client's codec (MessagePack or Protobuf) is a builder option (`ClientBuilder::tcp_codec`, default MessagePack) with no environment variable. Which codec the Client Library or Kit uses can't be determined from this repo.

Settings page (`http://localhost:<http.port>/index.html?port=<http.port>`) fields: Cache Path, Cleanup Threshold (%), Cache Collection Interval(s), and Cache Max Size (GB). The UI sends `max_size` as `<n>000000000`, which is decimal GB (`web/src/main.js`).

## Persistence caveats

`PUT /api/1.0/service/config` merges the target `hub.toml` with the JSON body and rewrites the file with `serialize_to_toml(false)`, which writes only non-default values (`hub/src/http/api/service_routes.rs`). Consequences:
- The serializers never emit `logging.file.count`, `logging.file.max_size`, or `logging.file.compression` (`logging.rs` `serialize_to_toml`). Any custom values for these in that `hub.toml` are **lost** on the first PUT.
- `cache.max_size` is rewritten with `ByteSize`'s `Display`, which rounds to one decimal of a decimal unit: 40 GiB becomes `"42.9 GB"`.
- Env and CLI overrides are not part of the merge, but they still win at the restart that follows.
- The file written is `--hub-config` or `~/.nvidia-omniverse/config/hub.toml`, never `<exe dir>/hub.toml`. `GET /service/reset` deletes that same file.

## Complete example `hub.toml`

All keys are shown with their effective defaults except where noted. Place it at `~/.nvidia-omniverse/config/hub.toml`. The table and key names are verified against the structs above. This is a key reference, not a configuration to recommend. In answers, leave out `garbage_collection_interval` and the other no-effect keys, and state their limitation instead.

```toml
[cache]
path = "/var/cache/hub"            # default: <paths.cache_root>/hub
max_size = "150 GB"                # decimal GB
cleanup_threshold = 10             # percent
garbage_collection_interval = 300  # NOT applied; effective GC interval is 900 s

[http]
host = "127.0.0.1"
port = 14090

[logging]
path = "/home/user/.nvidia-omniverse/logs/hub"   # default: <paths.logs_root>/hub
[logging.file]
enabled = true
json = true             # no effect; file log is always JSON
max_level = "info"
file = "hub.log"
count = 7
max_size = "128 MiB"
compression = true
[logging.stdout]
enabled = false
color = false
max_level = "debug"
[logging.stderr]
enabled = false
color = false
max_level = "debug"

[runtime]
num_worker_threads = 4  # default min(cores, 4)

[transport.tcp]
host = "127.0.0.1"
rpc_port = 14092  # MessagePack
ext_port = 14093  # Protobuf
```

Validate with `hub --mode=shared --show-config`. The JSON should reflect every non-default value. A malformed value, for example `cleanup_threshold = "x"`, makes startup fail with a `config` deserialize error.
