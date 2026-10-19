Up: [local cache](README.md)

# Installing, running, and uninstalling Hub

Requirement for every path: clients need **Omniverse Client Library 2.49.2 or greater**. "If using older versions of Client Library, you may experience errors when loading files" (`local-cache/reference/docs/index.rst`).

Key behavior: installing Hub does **not** start it. A Hub client (Client Library, Kit) finds the newest installed Hub and launches it on demand in **shared** mode. If one is already running for that cache dir, the client attaches to it (`hub-client/src/client.rs`, `launcher/mod.rs`; see [overview.md](overview.md#how-clients-find-and-use-hub-hub-client)). Running Kit apps don't pick up a newly installed Hub: "Hub does not connect to running Kit applications until you restart them" (`docs/index.rst`).

## Option A: NGC archive and install scripts

1. Download the Windows or Linux archive from the [NGC resource](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/resources/hub_workstation_cache): *File Browser* → choose the package → *Actions* (⋯) → download. The [collection](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/collections/hub-workstation-cache) groups the installers and the container.
2. Extract it, then run the install script from the extracted root (`docs/index.rst`):

```bash
./scripts/install.sh            # Linux  → $HOME/.local/share/ov/pkg/hub-<version>/
./scripts/install.sh -d /opt/ov # custom root → /opt/ov/hub-<version>/  (experimental)
```
```bat
scripts\install.bat             :: Windows → %LOCALAPPDATA%\ov\pkg\hub-<version>\
scripts\install.bat -d D:\ov    :: custom root (experimental)
```

What the scripts do (`local-cache/reference/scripts/install.sh`, `install.bat`):
- They read `<version>` from `target/release/hub[.exe] --version` in the archive.
- If `<root>/hub-<version>` already exists, they print `Destination directory … already exists.` and exit **without changes**. The exit status is 0.
- Otherwise they copy the whole archive there and create the `.installed` marker. Shared-mode discovery ignores installs that lack this marker (`launcher/mod.rs` `find_latest_hub`).
- The default roots match `omniverse.toml` `paths.library_root` (`hub-util/src/settings/omniverse.rs`). With `-d`, "the environment variable 'OMNICLIENT_HUB_EXE' must be set to this same path so that Hub can be found by clients". The Hub client accepts either the exe path (`<root>/hub-<ver>/target/release/hub`) or a directory. For a directory it searches `target/release/hub` and similar paths, and in shared mode the newest `hub-<semver>/` subdir, **without** requiring `.installed` (`discover_binary_shared`).
- Multiple versions can coexist. The client picks the highest semver with `.installed`. "Updating of Hub Workstation Cache does not remove the old versions that were installed. Workaround: Delete the old version … after updating" (`docs/index.rst` Known Issues).

## Option B: Kit Cache Status Indicator

From `docs/index.rst`. This requires **Cache Status Indicator 3.0.0 or greater**.
1. In the Kit app, search Extensions for *Cache Status Indicator*, install and load it, and enable autoload.
2. Click the indicator in the upper-right corner. It downloads and installs Hub and shows *Downloading...*.
3. Restart the Kit apps when prompted. The indicator then shows a green **SET**. Clicking it opens Hub Settings.

The extension isn't in this repo, so where it installs Hub and how it does so can't be determined here.

## Option C: run from source

Build first ([build.md](build.md)). The CLI default mode is **exclusive**, with OS-assigned ports (`hub/src/args.rs`), so for a workstation daemon pass `--mode=shared`:

```bash
cd local-cache/reference
cargo run --release --bin hub -- --mode=shared                       # 14090/14092/14093 on 127.0.0.1
cargo run --release --bin hub -- --mode=shared -- cache.path=/data/hubcache cache.max_size=200GB
./target/release/hub --mode=shared --show-config                     # effective settings, then exit
```

Point clients at the source build with `OMNICLIENT_HUB_EXE=$PWD/target/release/hub`, or with the `local-cache/reference` directory (the client checks `target/release/hub`). Stop it with `./target/release/hub --mode=shared --shutdown`. This only works if it resolves the same `cache.path`, so pass the same `cache.path` override if you used one. Alternatives are Ctrl-C (`signal::ctrl_c` in `hub/src/hub.rs`) or `curl http://127.0.0.1:14090/api/1.0/shutdown`.

Exclusive mode is meant for a parent process such as CI or a single app. The parent passes `--write-config <file>` and reads the bound ports from it. The Hub client does this when `OMNICLIENT_HUB_MODE=exclusive` and `OMNICLIENT_HUB_CACHE_DIR` are set (`docs/PROTOBUF_RPC.md` "Execution", `hub-client/src/client.rs`).

## Docker

From `local-cache/reference/docs/HUB_IN_DOCKER.md` and `docs/index.rst` "Hub as a Docker Container" (added in 2.0.0). The Hub container is "useful for deploying Hub on a single-user workstation, or on a server that is not running Omniverse". Containerized Hub "is assumed to always be running in shared mode".

Requirements:
- A host directory for the cache, **owned by the user that runs the container**. Use the same path for every client; `/var/cache/hub` is strongly recommended.
- `--network=host`. The container serves 14090 (HTTP), 14092 (MessagePack), and 14093 (Protobuf). On Docker Desktop (macOS, Windows), host networking needs version 4.34 or later, signed in to Docker Desktop, with **Enable host networking** checked under Settings → Resources → Network ([Docker docs](https://docs.docker.com/engine/network/drivers/host/)). Without it, `--network=host` joins the Docker Desktop VM's network, and clients on the host get `Connection refused`.
- **Bare-metal clients:** the cache path must be identical on the host and in the container (`/var/cache/hub` on both), because Hub returns absolute file paths and coordinates through `hub.lock` and `hub.port` in that directory.
- **Containerized clients only:** the host directory can live anywhere (for example `$HOME/.cache/ov/hub`) as long as it is mounted at `/var/cache/hub` in Hub and in every client container.

```bash
TAG=2.3.0   # a tag listed on the NGC container page

# with bare-metal clients
sudo mkdir -p /var/cache/hub && sudo chown "$(id -u):$(id -g)" /var/cache/hub
docker run --rm -v /var/cache/hub:/var/cache/hub -u "$(id -u):$(id -g)" --network=host "nvcr.io/nvidia/omniverse/hub_workstation_cache:$TAG"

# containerized clients only
docker run --rm -v "$HOME/.cache/ov/hub":/var/cache/hub -u "$(id -u):$(id -g)" --network=host "nvcr.io/nvidia/omniverse/hub_workstation_cache:$TAG"
```

Take `TAG` from the [NGC container page](https://catalog.ngc.nvidia.com/orgs/nvidia/teams/omniverse/containers/hub_workstation_cache); the image was renamed from `omni_hub` in 2.0.0 (`CHANGELOG.md`). The repo docs use the local tags `hub_workstation_cache` (`HUB_IN_DOCKER.md`) and `hub-local` (`index.rst`) instead, and the repo has no Dockerfile. The published 2.3.0 image's entrypoint is `/app/hub --mode shared --hub-config /app/hub.toml`, and that file sets `cache.path = "/var/cache/hub"` and the default loopback ports, so the commands pass no arguments.

**Client side** (every client host or container). The Hub client always runs a local `hub` executable to learn the running instance's ports (`hub-client/src/client.rs` → `launcher.launch`):

```bash
# install a hub binary (any location), e.g. /usr/local/bin/hub, then:
export OMNICLIENT_HUB_EXE=/usr/local/bin/hub   # where the client finds the hub executable
export HUB__CACHE__PATH=/var/cache/hub         # so that hub finds the container's hub.lock / hub.port
export HUB__ARGS__DETECT_ONLY=true             # never start a local Hub; fail if none is running
```

The client-side binary only reads `hub.port` and calls `http://127.0.0.1:<port>/api/1.0/service/config` (`hub/src/hub.rs`), so the container must share the host network. If the container isn't running, `--detect-only` fails with `Hub is not running` and the client gets a launch failure, which it retries up to its limit.

## Uninstall

From `docs/index.rst` and `scripts/uninstall.sh`/`uninstall.bat`:
1. Shut down all Omniverse applications.
2. Run the script **inside the installed directory**: `<root>/hub-<ver>/scripts/uninstall.sh [-d]` or `uninstall.bat [-d]`.

The scripts:
- Delete `.installed`, so discovery stops choosing this version.
- Wait 1 s, then run `hub --shutdown`, which stops a running instance that uses the default-resolved cache path.
- With `-d`, delete the whole `hub-<ver>` directory.
- Print `Hub <ver> uninstalled from <dir>. Cache is still present in <cache path>.`

The **cache is never deleted**. Remove `cache.path` yourself (default Linux `~/.cache/ov/cache/hub`, Windows `%LOCALAPPDATA%\ov\cache\hub`), plus `~/.nvidia-omniverse/config/hub.toml` and `~/.nvidia-omniverse/logs/hub` if you want them gone.

Notes:
- `uninstall.sh` needs `jq` and GNU `head` (`head -n -1`). The cache-path message breaks without them, but the uninstall continues.
- Without `-d`, the install files stay on disk; only the marker is removed. `docs/index.rst` says `-d` removes "the hub directory … as well, rather than only the contents of the directory", but neither script removes contents without `-d`.

## Windows vs Linux

| Aspect | Linux | Windows | Source |
|---|---|---|---|
| Install root | `$HOME/.local/share/ov/pkg` | `%LOCALAPPDATA%\ov\pkg` (changed in 1.0.2) | `scripts/install.*`, `omniverse.rs`, `docs/index.rst` |
| Default cache | `~/.cache/ov/cache/hub` (XDG) | `%LOCALAPPDATA%\ov\cache\hub` | `omniverse.rs`, `hub/src/settings.rs` |
| Executable | `target/release/hub` | `target\release\hub.exe` | `launcher/mod.rs` `hub_exe` |
| Exclusive child lifetime | `PR_SET_PDEATHSIG(SIGTERM)` | Job object with `KILL_ON_JOB_CLOSE` | `hub-client/src/launcher/deathsignal.rs` |
| Process name | `Omniverse Hub` (prctl) | From embedded resources and icon | `hub/src/hub.rs`, `hub/build.rs` |
| Docker | Documented | Not documented | `docs/HUB_IN_DOCKER.md` |
| Known issue | — | Kit ≤ 107.3 may not find an installed Hub. Workaround: `mklink /d %APPDATA%\ov %LOCALAPPDATA%\ov`. Fixed in Kit 109.0.1+. | `docs/index.rst` |
| Architectures | x86_64, aarch64 (2.0.0+) | x86_64 (not stated otherwise) | `docs/index.rst` |
