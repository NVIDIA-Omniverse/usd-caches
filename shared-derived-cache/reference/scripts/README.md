<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# SPDX license headers

From the repository root, apply headers with Python 3:

```bash
python3 scripts/apply-spdx-headers.py --check
python3 scripts/apply-spdx-headers.py
python3 scripts/apply-spdx-headers.py rust/ovdc scripts
```

The script processes tracked and unignored files, including new files, but skips
symlinks and does not traverse Git submodules. Paths are relative to the repository
root. Files containing either SPDX field are left untouched, including their
existing license and modification time. `--check` lists missing headers without
writing and exits with status 1 if any are missing (2 on errors).

Supported types are `.rs`, `.yaml`, `.yml`, `.py`, `.sh`, `.toml`, `.md`, `.tlp`,
`.tpl`, `.txt`, `.proto`, `.hcl`, `.ext`, `.dockerignore`, `.gitignore`, `.ignore`,
`.gitattributes`, `.helmignore`, `Makefile`, and `Dockerfile`
(including names such as `ci.Dockerfile`). Rust,
Protobuf, HCL, and plain text use `//` comments; Markdown uses an HTML comment;
Helm `.tpl` files, YAML templates, and `templates/NOTES.txt` use Go template comments;
the remaining types use `#`. The header declares NVIDIA's 2026 copyright and
Apache-2.0. Shell shebangs, UTF-8 BOMs, line endings, and file permissions are
preserved. Dockerfile parser directives stay ahead of the header. Ignored build
outputs are excluded.

Ignore files use `#` comments: [Git's ignore format](https://git-scm.com/docs/gitignore),
[Docker's ignore format](https://docs.docker.com/build/concepts/context/#dockerignore-files),
and [ripgrep's `.ignore` format](https://github.com/BurntSushi/ripgrep/blob/master/GUIDE.md)
support them. In this repository, `.ext` is used for OpenSSL certificate extension
configuration (`rust/tests/test_certs/localhost.ext`), whose
[configuration syntax](https://docs.openssl.org/1.1.1/man5/config/) supports `#`
comments. `.ext` support assumes this OpenSSL format; the extension alone does not
define a universal file format.

# Docker build and checks

Run these commands from any working directory, using the script's path:

```bash
./scripts/docker-check.sh build
./scripts/docker-check.sh test
./scripts/docker-check.sh lint
./scripts/docker-check.sh all
```

They use the repository's existing Dockerfiles and Bake targets. The public Bake
overlay builds the Rust base locally on `ubuntu:24.04` and disables registry
caches. The compiler version comes from `rust/rust-toolchain.toml`. No NVIDIA
registry credentials, private images, or GitHub Actions workflows are needed.
Public images and dependencies are downloaded on the first build.

Install Docker with Buildx, start its daemon, and select a docker-container
builder before running the checks:

```bash
docker buildx create --name ovdc-local --driver docker-container --use --bootstrap
```

Reuse that builder on later runs with `docker buildx use ovdc-local`. On Docker
Desktop, allocate sufficient memory and disk for compiling RocksDB and Rust
dependencies. The script selects the host's amd64 or arm64 Linux platform;
`OVDC_PLATFORM=linux/amd64` or `linux/arm64` overrides it.

`build` exports the binary; `test` runs the Dockerfile's nextest stage, including
the Helm chart configuration test; `lint` runs workspace Clippy with warnings
treated as errors. `all` runs those checks in order and stops on the first
failure. Test and lint stages export their status without failing the Docker
build, so the script reads `exit_code` and returns that status. Missing or
malformed status files fail the check.

Every check prints a fresh output directory containing its binary or reports.
Outputs are retained in the system temporary directory; set `OVDC_OUTPUT_DIR`
to an existing directory to choose a different parent. Build errors, test
failures and Clippy failures return nonzero status to the invoking shell.

To inspect resolved Bake targets without starting a daemon or compiling:

```bash
./scripts/docker-check.sh all --print
```

The public export includes this script, these instructions, and
`docker/dockerfile/public-bake.hcl`. Exclude source-repository workflows,
CODEOWNERS, catalogs, `.ovplc/`, and local agent configuration from that export.
Container publication and the Helm chart's release-image default are separate
tasks.
