Up: [local cache](README.md)

# Building Hub from source

Run all commands from `local-cache/reference/`, the Cargo workspace root (`local-cache/reference/Cargo.toml`). The workspace members are `hub-cache`, `hub-client`, `hub-rpc`, `hub-util`, and `hub`. `web/` is a separate npm project that is not built by Cargo.

## Prerequisites

| Need | Detail | Source |
|---|---|---|
| Rust toolchain | `1.92.0`, pinned by `rust-toolchain.toml` (rustup installs it automatically). The workspace declares `rust-version = "1.80.0"` and edition 2021. | `local-cache/reference/rust-toolchain.toml`, `Cargo.toml` |
| Linux packages | `sudo apt install libssl-dev pkg-config` ("to avoid `openssl` errors"). `openssl-sys` reaches the graph through the `curl` dev-dependency of `hub`, so it is needed at least for tests. | `local-cache/reference/README.md` "Notes", `hub/Cargo.toml`, `Cargo.lock` (`curl-sys` → `openssl-sys`) |
| C/C++ toolchain + `make` (Unix) | `hub-rpc/build.rs` builds a vendored `protoc` from source with `protobuf-src 1.1.0+21.5`, which drives the bundled pre-generated `configure` through the `autotools` crate. The repo doesn't list exact packages; `build-essential` is the usual Debian/Ubuntu choice (an assumption). The first build is noticeably slower because of this step. | `hub-rpc/build.rs`, `hub-rpc/Cargo.toml`, `Cargo.lock` |
| `PROTOC` (Windows) | On Windows `protobuf-src` is not used. Set `PROTOC` to a `protoc` executable before building ("Expect "PROTOC" to be set in the environment"). | `local-cache/reference/hub-rpc/build.rs` `configure_protobuf` |
| Windows resources | `hub/build.rs` embeds `hub.ico` with `winresource` on Windows. That needs the Windows SDK resource compiler; the repo doesn't say which. | `local-cache/reference/hub/build.rs` |
| Node.js + npm (web UI only) | `web/package.json` (webpack 5, React 19). The Node version isn't pinned. | `local-cache/reference/web/package.json` |

The published package targets Windows x86_64, Linux x86_64, and Linux aarch64 (`docs/index.rst` 2.0.0: "Added support for Linux AARCH64"). macOS isn't documented. The source has Linux-only and Windows-only `cfg` blocks (`prctl` is an empty crate off Linux) and Unix code paths that would apply to macOS, but no macOS build is described or tested in this repo.

## Commands

From `local-cache/reference/README.md` "Developing":

```bash
cargo build                 # debug (profile.dev uses opt-level = 1)
cargo build --release       # release (lto = true, strip = "debuginfo")
cargo test                  # unit + integration tests, debug
cargo test --release
cargo fmt                   # rules in rustfmt.toml (max_width 120); CI-style check: cargo fmt --all -- --check
cargo clippy                # clippy.toml is empty
cargo clippy --release
cargo run --release --bin hub -- --help
```

Narrower targets:

```bash
cargo build -p hub --release                   # service only
cargo build -p hub-client --release            # Rust lib + C staticlib + header
cargo test  -p hub-cache                        # storage engine unit tests
cargo test  -p hub --test rpc                   # one integration test file (api, rpc, multiple_clients, smoke)
cargo bench -p hub-cache                        # criterion bench hub-cache/benches/cache_benchmark.rs
cargo run   -p hub-client --example client      # needs target/debug/hub to exist first (example sets OMNICLIENT_HUB_EXE=target/debug/hub)
```

Coding standard: "All rust code shall be formatted with `cargo fmt` using the rules in `rustfmt.toml`" (`local-cache/reference/docs/CODING.md`).

## Artifacts

All of these land in `local-cache/reference/target/<debug|release>/`:

| Artifact | Produced by | Notes |
|---|---|---|
| `hub` / `hub.exe` | `hub` bin (`hub/src/hub.rs`) | Hub 2.4.0. The version comes from `hub/Cargo.toml`, and `hub --version` prints it. |
| `libhub_client.a` / `hub_client.lib`, plus `libhub_client.rlib` | `hub-client` (`crate-type = ["lib", "staticlib"]`) | The C API is in `hub-client/src/hub_client_c.rs`. |
| `hub_client_bindings.h` | `hub-client/build.rs` (cbindgen 0.26, prefix `OmniHub`, C with C++ compat) | Written to `<workspace>/target/<PROFILE>/`. |
| prost Rust for v1beta | `hub-rpc/build.rs` → `$OUT_DIR/nvidia.omniverse.usd_caches.local_cache.v1beta.rs` | The v1alpha proto is **not** compiled (see [api-protobuf.md](api-protobuf.md#two-contracts-one-served)). |

The install scripts expect a package root that contains `target/release/hub[.exe]` and copy the whole package into `hub-<version>/` (`scripts/install.sh`, `install.bat`). The Hub client finds `target/release/hub` under an install dir (`hub-client/src/launcher/mod.rs` `match_hub_path`). The NGC package also contains `rpc/proto/...` and `rpc/examples` (`docs/index.rst`, `docs/PROTOBUF_RPC.md`). **The script that assembles the NGC archive is not in this repo**, so the exact archive layout beyond these paths can't be determined.

## Web UI

```bash
cd web
npm ci                      # package-lock.json is present
npm run build               # webpack --mode=production → web/dist/bundle.js
```

Hub serves static files from `<dir of hub exe>/web` if that directory exists, otherwise `./web` relative to the CWD (`hub/src/http/mod.rs`). `index.html`, `main.css`, and the icons live in `web/public/`, while the bundle is written to `web/dist/bundle.js` (`webpack.config.babel.js`), and `index.html` loads `bundle.js` from the same directory. To get a working settings page next to a source-built binary, assemble the directory yourself:

```bash
mkdir -p target/release/web
cp web/public/* web/dist/bundle.js target/release/web/
```

This assembly step is inferred from the paths. The repo has no packaging script for it. Running `cargo run` from `local-cache/reference/` without it serves the raw `web/` source tree, where `/index.html` doesn't exist (it is under `web/public/`).

## Tests

See [validate.md](validate.md#integration-tests-hubtests) for what the integration tests cover and how to run them. Build-relevant facts:

- On Unix the integration fixtures raise `RLIMIT_NOFILE` to 100000 and **panic** if they can't (`hub/tests/common/mod.rs`). Raise the hard limit first (`ulimit -Hn`) in containers or CI.
- The tests launch the freshly built `hub` through `OMNICLIENT_HUB_EXE=$CARGO_BIN_EXE_hub` in **exclusive** mode with a temporary cache dir, so they don't touch a running shared Hub. Their ports are OS-assigned.
- `hub-util` and `hub` unit tests skip the user's `~/.nvidia-omniverse/config/*.toml` (`cfg!(test)` in `hub-util/src/settings/mod.rs`). Integration tests run the real binary, which **does** read the user's `hub.toml`. Overrides there, such as fixed ports, can affect them.

## Docker image

No Dockerfile or image build script is in this repo. `CHANGELOG.md` for 2.0.0 says a Docker build target exists and uses "natively built binaries". The image names in the docs disagree: `docs/index.rst` uses `hub-local`, while `docs/HUB_IN_DOCKER.md` and NGC use `hub_workstation_cache`. Use the NGC container ([install-and-run.md](install-and-run.md#docker)).
