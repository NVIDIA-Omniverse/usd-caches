Up: [local cache](README.md)

# Validating a Hub deployment

Run the steps in order. Each one gives the evidence to look for and where that evidence comes from in the source. Linux paths are shown; on Windows use `%LOCALAPPDATA%\ov\cache\hub` and `hub.exe`. Set `C` to the cache dir: `C=$(hub --mode=shared --show-config | sed -n '/^{/,/^}/p' | jq -r .cache.path)`. The `sed` range drops any bootstrap log line around the JSON. Unlike the GNU-only `head -n -1` in `scripts/uninstall.sh`, it works on macOS.

## 1. Installed

- [ ] `ls ~/.local/share/ov/pkg/hub-*/.installed` lists at least one version. Clients pick the highest semver that has the marker (`hub-client/src/launcher/mod.rs` `find_latest_hub`). A custom location only works with `OMNICLIENT_HUB_EXE` set.
- [ ] `<install>/target/release/hub --version` prints `hub <version>`.
- [ ] Client Library is ≥ 2.49.2 (`docs/index.rst`). This repo has no way to check it; use the app's own version reporting.

## 2. Running

Hub is started by the first client, or manually with `hub --mode=shared` ([install-and-run.md](install-and-run.md)).
- [ ] `$C/hub.port` exists and contains the HTTP port. It is written after the HTTP bind (`hub/src/hub.rs`).
- [ ] `$C/hub.lock` contains a PID, and that process is alive. On Linux the process name is `Omniverse Hub` (`pgrep -x 'Omniverse Hub'` or `ps -p "$(cat "$C/hub.lock")"`). For a containerized Hub, `hub.lock` holds the PID inside the container, so check the container with `docker ps` instead.
- [ ] `HUB__ARGS__DETECT_ONLY=true hub --write-config /tmp/h.json && jq .transport /tmp/h.json` exits 0 and prints the running instance's ports. `Error: Hub is not running` means no instance holds the lock for that `cache.path` (`hub/src/hub.rs`).

## 3. Reachable

```bash
HUB=http://127.0.0.1:$(cat "$C/hub.port")
curl -s $HUB/api/health                                    # {"health":"ok","uptime":N}
curl -s $HUB/api/1.0/service/status | jq '{exe_path, ports: .config.transport.tcp, http: .config.http}'
nc -z 127.0.0.1 14093 && nc -z 127.0.0.1 14092             # Protobuf and MessagePack listeners (shared-mode defaults)
```
- [ ] `exe_path` is the binary you expect (not an older install).
- [ ] The ports match what clients use. In exclusive mode they are random; use `/service/config`.
- [ ] Protobuf round trip: `python simple_client.py` or `hub_pb_demo.py` from [api-protobuf.md](api-protobuf.md#runnable-examples) exits 0.

## 4. Caching (hit and miss evidence)

Stats come from `GET /api/1.0/cache/stats` (`hub-cache/src/block/cache.rs` `Stats`). A miss is counted only by a cache-aside open-for-read or block get that finds nothing. Stat calls don't count.

Start from a cold cache: use content this cache has never stored, or a fresh `cache.path`, or stop clients and run `curl -s -X DELETE $HUB/api/1.0/cache/clear` (removes unopened entries) before the reset. The reset zeroes only the hit, access, and pending-write counters and keeps cached entries, so content cached earlier makes the first run all hits.

```bash
curl -s -X DELETE $HUB/api/1.0/cache/stats/reset
# ... load the same USD stage twice in Kit / run the same Client Library job twice ...
curl -s $HUB/api/1.0/cache/stats | jq '{cache_entries, cache_size, cache_hits, cache_misses, total_stored_data}'
du -sh "$C/hub-files"; find "$C/hub-files" -type f | wc -l
```
- [ ] The **first** run increases `cache_misses`, `cache_entries`, `cache_size`, and `total_stored_data`, and `hub-files/` grows.
- [ ] The **second identical** run mostly increases `cache_hits`, with little growth in `cache_entries`. This is the core proof that the cache is used.
- [ ] A controlled check without Kit: run `hub_pb_demo.py mykey` twice. The second run shows `read #1: SUCCESS`, `+2` hits, and `+0` misses ([api-protobuf.md](api-protobuf.md#full-cache-aside-and-block-cache-demo)).
- Don't use `cache_on_disk_size` (always 0) or `current_size`, `hit_rate`, and `number_of_files` (always 0) (`hub-cache/src/block/file.rs` TODO, `hub/src/http/api/cache_routes.rs`).
- `du` on `hub-files/` can exceed `cache_size`. Block data files are sparse and preallocated in 64 MiB units, and unreferenced files are deleted only at the next GC pass (every 900 s; see [overview.md](overview.md#eviction-cleanup-gc)).
- If the stats stay at 0 while apps run, the apps aren't using this Hub. They might be using another cache dir or instance, or a Client Library below 2.49.2. See [troubleshooting.md](troubleshooting.md).

## 5. Eviction (optional)

Save the current cap, set a small one, fill past it, and wait for GC:
```bash
OLD_MAX=$(curl -s $HUB/api/1.0/service/config | jq -r .cache.max_size)
curl -s -X PUT -H 'Content-Type: application/json' -d '{"cache":{"max_size":"2 GB"}}' $HUB/api/1.0/service/config
```
After more than `max_size` of new data and up to 900 s, `cache_size` should drop to ≤ `max_size × (100 − cleanup_threshold)/100`. The log shows `garbage collection finished` with `collected` > 0 (`cache.rs` `gc_task`). Restore the saved cap afterwards with a PUT:
```bash
curl -s -X PUT -H 'Content-Type: application/json' -d "{\"cache\":{\"max_size\":\"$OLD_MAX\"}}" $HUB/api/1.0/service/config
```
Don't use `GET /api/1.0/service/reset` for this: it deletes `hub.toml`, so every saved setting returns to its default, not just the cap ([api-http.md](api-http.md)). Hub reports `max_size` with one decimal place (`"150.0 GB"`), so a GiB value comes back slightly rounded. The PUT also drops some logging settings from `hub.toml` ([troubleshooting.md](troubleshooting.md#known-source-defects), item 6). `DELETE /api/1.0/cache/clear` removes all unopened entries immediately.

## 6. Logs

- File: `<logging.path>/hub.log`, default `~/.nvidia-omniverse/logs/hub/hub.log`. It is JSON lines, level `info` in release builds, and rotated (7 files of 128 MiB, older ones gzip-compressed) (`hub/src/logging.rs`).
- Startup lines to look for: `cache started` with `entries` and `size`, `HTTP server bound to port …`, and `Starting garage collection task` (sic) (`hub-cache/src/block/cache.rs`, `hub/src/hub.rs`).
- Per-connection lines: `Received new connection from …` and `Connection from … closed` (`server_protobuf.rs`, `server_msgpack.rs`). These are emitted on Tokio worker threads. The configured file subscriber is installed with the thread-local `set_default` on the main thread, while a bootstrap global stdout subscriber also exists (`hub/src/hub.rs`, `hub/src/logging.rs`). Worker-thread events may therefore go to the bootstrap stdout logger, not `hub.log`. This is inferred from source; check it on a live system before relying on `hub.log` for connection events.
- Support bundle: `curl -o b.zip $HUB/api/1.0/service/supportbundle` collects `status.json`, `hub.toml`, `omniverse.toml`, and `logs/` ([api-http.md](api-http.md#service)).

## 7. Docker deployments

- [ ] On the host: `ls -l /var/cache/hub/hub.port /var/cache/hub/hub.lock` exist and are owned by the user that runs the container.
- [ ] Client env: `OMNICLIENT_HUB_EXE`, `HUB__CACHE__PATH=/var/cache/hub`, and `HUB__ARGS__DETECT_ONLY=true` (`docs/HUB_IN_DOCKER.md`). Then `"$OMNICLIENT_HUB_EXE" --write-config /tmp/h.json` succeeds.
- [ ] A path returned by Hub (for example `local_file` from `hub_pb_demo.py`) is readable **from the client**. That proves the path parity required for bare-metal clients.

## Integration tests (`hub/tests`)

Run them from `local-cache/reference/`. They build `hub` and launch it in exclusive mode with temporary cache dirs (`hub/tests/common/mod.rs`). On Unix, the fixtures raise the open-file soft limit to 100000 and panic (`Could not set NOFILE limit`) if they can't. The `rlimit` crate caps the request at the hard limit, and on macOS also at `kern.maxfilesperproc` (`hub/tests/common/mod.rs`). So before running the tests:
- **Linux:** the hard limit must be ≥ 100000. Check with `ulimit -Hn`, and raise it with `sudo prlimit --pid $$ --nofile=100000:100000`, in `/etc/security/limits.conf`, or with `--ulimit nofile=100000:100000` for a container.
- **macOS:** `kern.maxfilesperproc` must also be ≥ 100000. Check with `sysctl kern.maxfiles kern.maxfilesperproc`, and raise it until the next reboot with `sudo sysctl -w kern.maxfiles=200000 kern.maxfilesperproc=100000`.

```bash
ulimit -n 100000                          # fails if the hard limit above is lower
cargo test -p hub --tests                 # all integration tests (debug)
cargo test -p hub --test rpc              # one file
cargo test -p hub --test rpc -- read_write --nocapture
RUST_LOG=debug cargo test -p hub --test api -- --nocapture   # test-log prints tracing output
cargo test -p hub --test multiple_clients -- --ignored       # 10/100/1000-client variants (slow)
```

| File | Covers | Codecs |
|---|---|---|
| `smoke.rs` | Empty harness check. | — |
| `api.rs` | `port_file` (`hub.port` matches the client-reported HTTP port), `health`, `status` (cache path and exe path), and `cache_stats`, which asserts exact stats after miss, write, pending-write read and write, close, hit, and already-exists: `entries=1, hits=1, misses=2, reads_during_file_write=1, writes_during_file_write=1`. `update_config` is `#[ignore]` because it would overwrite the user's `hub.toml`. | MessagePack only |
| `rpc.rs` | `get_version`, cache-aside `read_write`, `close_on_disconnect`, `close_multiple`, `remove_multiple`, restart and reclaim (`restart_open_read`, `restart_open_write`, `restart_read_write`, `restart_client_also_crashes`), blocks (`read_write_block`, `abort_block`, `close_multiple_block`, `commit_multiple_block`, `abort_multiple_block`, `commit_and_abort_block`, `restart_read_write_blocks`, `restart_abandon_block_put_finished`, `restart_abandon_block_put_pending`, `restart_block_client_also_crashes`), and `launch_exclusive_with_nonexistent_cache_path`. `#[ignore]`: `launch_exclusive_with_nonexistent_exe_path` and `abort_put_on_disconnect`. | MessagePack **and** Protobuf (`rstest` cases) |
| `multiple_clients.rs` | Concurrent clients for file create, block cache, restart-open, and client-crash scenarios. The 2-client case runs by default; 10, 100, and 1000 are `#[ignore]`. File counts come from `#[values(1, 10)]`. | MessagePack only (fixtures `shared_test_service` and `shared_restart_test_service` wrap `MsgPackTestService`) |

Unit tests: `cargo test -p hub-cache` covers the engine (put, get, GC, abandon, file pool), and `cargo test -p hub` covers the HTTP routes, the settings precedence tests in `hub/src/settings.rs`, and the dispatcher. `hub/src/logging.rs` `log_rotation` is `#[ignore]` and must run alone (`cargo test -p hub log_rotation -- --ignored --test-threads=1`). The tests exist in the repo, but nothing here records a passing run for your environment (`docs/README.md`).
