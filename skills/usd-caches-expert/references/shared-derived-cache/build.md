Up: [shared derived-data cache](README.md)

# Build and test OVDC

Run all commands from `shared-derived-cache/reference/` (the "project root" in `shared-derived-cache/reference/README.md`). The Rust workspace is in `rust/`.

## Native prerequisites

From `shared-derived-cache/reference/README.md` "Prerequisites":

- Rust via rustup, with Cargo's bin directory on `PATH`. The toolchain is pinned by `rust/rust-toolchain.toml` (`channel = "1.96.1"`).
- Git, Make, a C/C++ compiler, CMake, libclang, and `protoc`. These come from the RocksDB and gRPC build scripts. RocksDB is fetched from the public rust-rocksdb Git repository at a pinned rev (`rust/Cargo.toml`).
- The first build compiles RocksDB and takes several minutes. Allow several GB of disk.

Ubuntu 24.04:

```sh
sudo apt-get update
sudo apt-get install -y build-essential git curl ca-certificates cmake clang libclang-dev protobuf-compiler pkg-config
```

macOS:

```sh
xcode-select --install          # skip if already installed
brew install cmake llvm protobuf
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
export DYLD_LIBRARY_PATH="$LIBCLANG_PATH${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
```

Keep those exports in the shell that runs Cargo. The Makefile forwards `LIBCLANG_PATH` as `DYLD_LIBRARY_PATH` because macOS system Make strips `DYLD_*` (`Makefile`). If Linux bindgen cannot find libclang, set `LIBCLANG_PATH`.

Toolchain check:

```sh
(cd rust && rustup show active-toolchain && rustup component add clippy)
protoc --version
```

There are no Git submodules.

## Makefile targets

`Makefile` (`.DEFAULT_GOAL := help`; variables `CARGO ?= cargo`, `ENV_DIR ?= rust/target/env`, `CONFIG ?= rust/ovdc/src/config.toml`):

| Target | Runs (inside `rust/`) |
|---|---|
| `make help` | Prints the target list |
| `make build` | `cargo build --locked --bin main` → `rust/target/debug/main` |
| `make release` | `cargo build --locked --release --bin main` → `rust/target/release/main` (LTO, `codegen-units=1`, `panic='abort'`, per `rust/Cargo.toml`) |
| `make test` | `cargo test --locked --workspace` |
| `make lint` | `cargo clippy --locked --workspace --all-targets -- -D warnings` |
| `make run` | `cargo run --locked --bin main -- "$(abspath ENV_DIR)" "$(abspath CONFIG)"`; override with `make run CONFIG=… ENV_DIR=…` |

Without Make, use the equivalent `cargo` commands inside `rust/` (`shared-derived-cache/reference/README.md` "Build and check").

## Tests

- Server and gRPC integration tests: `rust/ovdc/src/tests.rs`. They use an embedded server (`rust/ovdc/src/embedded.rs`) and cover PUT/GET/TOUCH round trips, chunked and large values, oversized refusal, tenant isolation, JWT tenant/no-nid/expired, last-write-wins/concurrent PUT, `putTimeout`, TLS, and the Helm config contract.
- DB and adapter tests: `rust/ovdc_map/src/tests.rs`, `rust/ovdc_map/src/environment/rocks_adapter/tests.rs`. JWT tests: `rust/ovdc_jwt/src/lib.rs` (synthetic fixtures; regenerate with `cargo run -p ovdc_jwt --example debug_tokens`). Index: `shared-derived-cache/conformance/README.md`.
- `test_helm_chart_config` runs `helm template ovdc-test ../../docker/helm`, boots a DB on the rendered `config.toml`, and does a PUT/GET. It **skips silently if `helm` is not on `PATH`**. Set `OVDC_REQUIRE_HELM=1` to make the skip a failure. It covers default values only (`docker/helm/README.md` "Validation").
- nextest profiles: `rust/.config/nextest.toml` (JUnit `junit.xml`) and `rust/.cargo/nextest.toml` (`serial-native` test group, max 1 thread).

## Docker build, test, and lint (`scripts/docker-check.sh`)

You need Docker with a running daemon and Buildx, plus a `docker-container` driver builder:

```sh
docker buildx create --name ovdc-local --driver docker-container --use --bootstrap
# later runs: docker buildx use ovdc-local
```

```sh
./scripts/docker-check.sh build     # exports the release binary
./scripts/docker-check.sh test      # nextest stage, OVDC_REQUIRE_HELM=1, exports junit.xml + exit_code
./scripts/docker-check.sh lint      # clippy -D warnings, exports clippy.log + exit_code
./scripts/docker-check.sh all       # build, test, lint in order; stops at first failure
./scripts/docker-check.sh all --print   # validate Bake config only; no daemon or compile
```

Behavior, from `scripts/docker-check.sh` and `scripts/README.md` "Docker build and checks":

- The script invokes `docker buildx bake -f docker/dockerfile/docker-bake.hcl -f docker/dockerfile/public-bake.hcl <target>`. Targets: `build` → `out_binary`, `test` → `output_nextest`, `lint` → `output_clippy`.
- The public overlay builds the Rust base locally on `docker.io/library/ubuntu:24.04` (`docker/dockerfile/rust-base.Dockerfile`) and sets `CACHE_REF=""`. It needs no NVIDIA registry credentials.
- `RUST_VERSION` is read from `rust/rust-toolchain.toml`. The platform defaults to the host (`linux/amd64` or `linux/arm64`); override with `OVDC_PLATFORM`.
- Each check writes to a fresh `mktemp -d` directory under `${OVDC_OUTPUT_DIR:-$TMPDIR}` and prints its path. For `build`, the script requires a non-empty `binary`. For `test` and `lint`, it reads `exit_code` and returns that status; a missing or malformed file fails.
- On Docker Desktop, allocate enough memory and disk for RocksDB. The Dockerfile notes that dependency compilation is about 75% of build time (about 29 of 39 min cold).

## Container image (`docker/dockerfile/Dockerfile`)

Stages: `toolchain` → `planner` (cargo vendor + cargo-chef recipe) → `cook` (dependencies only) → `build` (`cargo auditable build`, SBOMs via syft, `third_party_licenses.md`) → `clippy`/`nextest` → exports. Final images:

| Bake target | Image contents |
|---|---|
| `dev_container` | `FROM ${BASE}` (`docker.io/library/ubuntu:24.04`). Contains `/main`, `/etc/ssl/certs`, `/binary.spdx.json`, `/cargo-licenses.json`, and an `/env` dir owned by 1000:1000. Runs as `USER 1000:1000`. |
| `release_container` | `dev_container` plus `/sources.tar.gz`, `/sources.spdx.json`, `/third_party_licenses.md` |
| `out_dev_container` | The same image as a `docker load` tarball at `./container/dev_container.tar` |

The image declares **no `ENTRYPOINT`/`CMD` and no `EXPOSE`**. You must pass the command, for example `/main /env/env /config/config.toml`, which is what the chart uses (`docker/helm/templates/statefulset.yml`).

Local image build (not wrapped by `docker-check.sh`; use the same public overlay so no private base is needed):

```sh
CACHE_REF="" docker buildx bake \
  -f docker/dockerfile/docker-bake.hcl -f docker/dockerfile/public-bake.hcl \
  --set dev_container.output=type=docker dev_container
# tags as container:latest (image_tags("container") with TAG=latest, IMAGE_REF="")
```

Bake variables you are likely to set (`docker/dockerfile/docker-bake.hcl`): `TAG` (default `latest`), `IMAGE_REF` (registry/repo; empty gives a local `container:<TAG>` tag), `PUSH` (`true` pushes), `PLATFORMS`, `CARGO_PROFILE` (`release`), and `OVDC_VERSION`. `OVDC_VERSION` is the version baked in and logged at startup; empty reports `dev`. The `--set …output=type=docker` form above is standard Buildx syntax and is not exercised by the repo's scripts.

## Quick render check of the chart

```sh
helm lint --strict ./docker/helm
helm template ovdc ./docker/helm | kubectl apply --dry-run=client -f -
```

(`docker/helm/README.md` "Validation".) `values.schema.json` is closed for chart-owned objects, so an unknown key fails the render.
