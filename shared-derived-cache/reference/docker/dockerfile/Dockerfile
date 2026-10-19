# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

ARG RUST_BASE="public-rust-base"
ARG BASE="ubuntu:24.04"
ARG SYFT_VERSION="1.45.1"
ARG HELM_VERSION="3.16.3"

# syft is pulled as an image layer (COPY --from=syft below) instead of being
# curl'd from GitHub releases. This keeps it off the toolchain critical path,
# rides BuildKit's registry/layer cache, and drops the hand-rolled per-arch URL
# mapping — the multi-arch manifest resolves TARGETARCH automatically.
FROM ghcr.io/anchore/syft:v${SYFT_VERSION} AS syft

# helm is needed by test_helm_chart_config, which renders docker/helm and boots
# a database against the result. Pulled as an image layer for the same reasons
# as syft above. Only the nextest stage consumes it.
FROM alpine/helm:${HELM_VERSION} AS helm

FROM ${RUST_BASE} AS toolchain

# BuildKit auto-populates TARGETARCH from --platform (amd64 | arm64).
ARG TARGETARCH
ARG RUST_VERSION="1.96.1"
ARG CARGO_AUDITABLE_VERSION="0.7.4"
ARG CBINDGEN_VERSION="0.29.4"
ARG CARGO_CHEF_VERSION="0.1.72"

RUN apt update && apt install -y \
    ca-certificates \
    build-essential curl git clang \
    protobuf-compiler libclang-dev \
    cmake gcc-12 g++-12 \
    python3 \
    rsync \
    && rm -rf /var/lib/apt/lists/*

COPY docker/dockerfile/prune-vendor.py /usr/local/bin/prune-vendor
COPY docker/dockerfile/write-third-party-licenses.py /usr/local/bin/write-third-party-licenses
COPY docker/dockerfile/enrich-cargo-licenses.py /usr/local/bin/enrich-cargo-licenses
RUN chmod +x /usr/local/bin/prune-vendor /usr/local/bin/write-third-party-licenses \
    /usr/local/bin/enrich-cargo-licenses

ENV CC=gcc-12
ENV CXX=g++-12

RUN --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    case "${TARGETARCH}" in \
        amd64) RUST_TRIPLE="x86_64-unknown-linux-gnu" ;; \
        arm64) RUST_TRIPLE="aarch64-unknown-linux-gnu" ;; \
        *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && TOOLCHAIN="${RUST_VERSION}-${RUST_TRIPLE}" \
    && rustup toolchain install "${TOOLCHAIN}" \
    && rustup default "${TOOLCHAIN}" \
    && rustup component add --toolchain "${TOOLCHAIN}" rustfmt clippy \
    && cargo install --locked cargo-auditable --version "${CARGO_AUDITABLE_VERSION}" \
    && cargo install --locked cbindgen --version "${CBINDGEN_VERSION}" \
    && cargo install --locked cargo-chef --version "${CARGO_CHEF_VERSION}"

WORKDIR /source

# ---------------------------------------------------------------------------
# Dependency pre-build (cargo-chef)
#
# Compiling third-party crates is ~75% of the build (~29 of 39 minutes), and it
# only changes when Cargo.lock does. The `planner` stage distills the workspace
# down to `recipe.json` -- manifests, lock file, .cargo/config.toml and the
# toolchain file, but no source code -- so editing any .rs file leaves it
# byte-identical. `cook` is keyed on that recipe, so it survives source-only
# changes and is restored from the registry cache instead of recompiling.
#
# `cook` must resolve crates exactly the way `build` does. Cargo treats a
# source-replaced package as a different unit, so building deps against the
# registry here and against vendor/ in `build` would recompile all of them and
# defeat the whole stage. That is why the vendor directory is produced in
# `planner` and copied in: `cargo chef cook` re-hydrates the skeleton and builds
# in one shot, leaving no point at which vendoring could be interleaved.
#
# Only manifests are bind-mounted below -- every workspace member's Cargo.toml,
# plus the workspace Cargo.toml/Cargo.lock/.cargo/config.toml/rust-toolchain.toml
# -- not the `.rs` source tree. A full-tree bind mount (the previous approach)
# makes BuildKit's cache key for this RUN step a function of every file in
# rust/, so it changes on essentially every commit regardless of whether the
# *output* (recipe.json/vendor) is byte-identical -- confirmed against real CI
# cache exports: recipe.json and vendor/ were provably byte-for-byte identical
# (including the fixed mtime below) across independent builds, yet `cook`
# still recompiled every dependency on nearly every run, because the upstream
# `planner` step was never considered a cache hit in the first place. Neither
# `cargo vendor` nor `cargo chef prepare` reads `.rs` files -- confirmed by
# running `cargo vendor` against this workspace with every `.rs` file replaced
# by an empty stub, which succeeded -- so stub entry-point files (src/lib.rs,
# src/main.rs, build.rs, as applicable per crate) are all that's needed to
# satisfy cargo's target-discovery checks. The member list and target shape
# must be kept in sync with rust/Cargo.toml's `[workspace] members` by hand;
# a member added without a matching stub here fails loudly (cargo complains
# about the missing entry file) rather than silently, so drift is cheap to
# catch.
#
# vendor/ is additionally stamped to a fixed mtime: `cargo vendor` writes every
# file with the current wall-clock time on each invocation, so even with this
# narrower bind mount, two planner runs against an identical Cargo.lock would
# otherwise produce byte-identical crate contents but different layer digests.
# ---------------------------------------------------------------------------

FROM toolchain AS planner

ARG TARGETARCH
ARG RUST_PATH="rust/"

WORKDIR /source

RUN --mount=type=bind,source=${RUST_PATH}Cargo.toml,target=/src/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}Cargo.lock,target=/src/Cargo.lock \
    --mount=type=bind,source=${RUST_PATH}rust-toolchain.toml,target=/src/rust-toolchain.toml \
    --mount=type=bind,source=${RUST_PATH}.cargo/config.toml,target=/src/.cargo/config.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc/Cargo.toml,target=/src/ovdc/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_bytes/Cargo.toml,target=/src/ovdc_bytes/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_lru/Cargo.toml,target=/src/ovdc_lru/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_gc/Cargo.toml,target=/src/ovdc_gc/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_map/Cargo.toml,target=/src/ovdc_map/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_settings/Cargo.toml,target=/src/ovdc_settings/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_trace/Cargo.toml,target=/src/ovdc_trace/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_grpc/Cargo.toml,target=/src/ovdc_grpc/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_jwt/Cargo.toml,target=/src/ovdc_jwt/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_sharded_map/Cargo.toml,target=/src/ovdc_sharded_map/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_version/Cargo.toml,target=/src/ovdc_version/Cargo.toml \
    --mount=type=bind,source=${RUST_PATH}ovdc_attributions/Cargo.toml,target=/src/ovdc_attributions/Cargo.toml \
    --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    cp -r /src/. /source/ \
    && for m in ovdc ovdc_bytes ovdc_lru ovdc_gc ovdc_map ovdc_settings ovdc_trace ovdc_grpc ovdc_jwt ovdc_sharded_map ovdc_version ovdc_attributions; do \
         mkdir -p /source/$m/src && touch /source/$m/src/lib.rs; \
       done \
    && mkdir -p /source/ovdc/src/bin && touch /source/ovdc/src/bin/main.rs \
    && touch /source/ovdc_attributions/src/main.rs \
    && for m in ovdc ovdc_map ovdc_grpc ovdc_version; do touch /source/$m/build.rs; done \
    && mkdir -p /source/ovdc_sharded_map/benches && touch /source/ovdc_sharded_map/benches/bench_sharded_map.rs \
    && sed -i 's/^#replace-with/replace-with/' .cargo/config.toml \
    && cargo vendor --versioned-dirs --locked \
    && find vendor -exec touch -h -d @0 {} + \
    && cargo chef prepare --recipe-path /recipe.json

FROM toolchain AS cook

ARG TARGETARCH
ARG CARGO_PROFILE=release

WORKDIR /source

COPY --from=planner /recipe.json /source/recipe.json
COPY --from=planner /source/vendor /source/vendor

# --offline is a guard, not an optimization: if the vendor directory and the
# recipe's .cargo/config.toml ever stop agreeing, this fails loudly here rather
# than silently rebuilding every dependency again in the `build` stage.
RUN --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    cargo chef cook --profile "${CARGO_PROFILE}" --all-targets --offline --recipe-path recipe.json

FROM cook AS build

ARG TARGETARCH
ARG RUST_PATH="rust/"
ARG CARGO_PROFILE=release
ARG EXE="release/main"

# Product version baked into the binary by ovdc_version's build script and
# logged at startup. Declared here rather than in `toolchain`/`cook` on
# purpose: those stages are cached across commits, and this value is not.
# Unset -- any local bake -- yields a binary that reports `dev`.
ARG OVDC_VERSION=""
ENV OVDC_VERSION=${OVDC_VERSION}

WORKDIR /source

# syft from the pinned syft image (top-of-file stage); it's only used here in
# the build stage to emit the SBOMs below.
COPY --from=syft /syft /usr/local/bin/syft

# This layer is invalidated by any source edit -- a bind mount's contents are
# part of the cache key just as a COPY's would be. What makes that cheap is the
# `cook` stage below it: vendor/ and target/ arrive already populated with every
# third-party crate, so cargo only has to rebuild the workspace itself.
#
# rsync keeps /vendor and /target so the cooked artifacts survive; `cargo vendor`
# then re-runs to refresh the tree that the SBOM and sources.tar.gz are cut from.
#
# The `touch` is load-bearing. Cargo decides a workspace unit is fresh by
# comparing source mtimes against the artifact's, and `rsync -a` preserves the
# checkout's mtimes, which are older than anything `cook` just produced.
# cargo-chef deletes the dummy libraries it built, but a lib's unit-test binary
# is named `<crate>-<hash>` with no `lib` prefix and so is not matched by that
# cleanup -- leaving a stub that nextest would happily run as zero passing
# tests. Stamping the tree to now makes every workspace unit unambiguously
# dirty; vendor/ and target/ are pruned because dependency freshness comes from
# lock file checksums, not mtimes.
#
# Deliberately no `cargo sweep` here. It removes artifacts older than its stamp,
# which is now every cooked dependency -- pruning them would both discard the
# work `cook` exists to preserve and force the `nextest` stage (FROM build) to
# recompile them from scratch.
RUN --mount=type=bind,source=${RUST_PATH},target=/src \
    --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    case "${TARGETARCH}" in \
        amd64) RUST_TRIPLE="x86_64-unknown-linux-gnu" ;; \
        arm64) RUST_TRIPLE="aarch64-unknown-linux-gnu" ;; \
        *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && export RUST_TRIPLE \
    && rsync -a --delete --exclude=/vendor --exclude=/target /src/ /source/ \
    && find /source -path /source/vendor -prune -o -path /source/target -prune \
        -o -type f -print0 | xargs -0 -r touch \
    && sed -i 's/^#replace-with/replace-with/' .cargo/config.toml \
    && cargo vendor --versioned-dirs --locked \
    && cargo auditable build --profile "${CARGO_PROFILE}" --all-targets \
    && mkdir -p /out/$(dirname "${EXE}") \
    && cp "target/${EXE}" "/out/${EXE}" \
    && cp -a /source/vendor /out/vendor \
    && prune-vendor /out/vendor \
    && tar -czf /out/sources.tar.gz -C /out vendor \
    && SYFT_CHECK_FOR_APP_UPDATE=false SYFT_LICENSE_CONTENT=all \
        syft "/out/${EXE}" -o spdx-json=/out/binary.spdx.json \
    && SYFT_CHECK_FOR_APP_UPDATE=false SYFT_LICENSE_CONTENT=all \
        syft "/out/${EXE}" -o syft-json=/out/binary.syft.json \
    && SYFT_CHECK_FOR_APP_UPDATE=false SYFT_LICENSE_CONTENT=all \
       SYFT_FILE_METADATA_SELECTION=all \
       SYFT_FILE_CONTENT_GLOBS='**/LICENSE*,**/COPYING*,**/NOTICE*' \
        syft dir:/out/vendor -o spdx-json=/out/sources.spdx.json \
    && SYFT_CHECK_FOR_APP_UPDATE=false SYFT_LICENSE_CONTENT=all \
       SYFT_FILE_METADATA_SELECTION=all \
       SYFT_FILE_CONTENT_GLOBS='**/LICENSE*,**/COPYING*,**/NOTICE*' \
        syft dir:/out/vendor -o syft-json=/out/sources.syft.json \
    && write-third-party-licenses /out/sources.syft.json /out/third_party_licenses.md \
    && cargo metadata --format-version 1 --locked --filter-platform "${RUST_TRIPLE}" \
        > /out/cargo-metadata.json \
    && enrich-cargo-licenses /out/cargo-metadata.json \
        /out/binary.spdx.json /out/binary.syft.json \
        /out/sources.spdx.json /out/sources.syft.json \
    && python3 -c "import json,re,sys; m=json.load(open('/out/cargo-metadata.json')); json.dump({f\"{p['name']}@{p['version']}\": re.sub(r'\s*/\s*',' OR ',p['license'].strip()) for p in m['packages'] if p.get('license')}, open('/out/cargo-licenses.json','w'))"

# Lint gate. Inherits from `build` so the source tree, vendor/ and target/ are
# already in place; cargo drives clippy through RUSTC_WORKSPACE_WRAPPER, so the
# dependency rlibs `cook` produced are reused as-is and only workspace crates
# actually go through clippy-driver.
#
# `-D warnings` turns every lint into an error. Like the nextest stage, the
# exit code is written to a file rather than failing the bake, so a lint
# failure does not also cost the run its binary artifacts and test report --
# the workflow fails on the exit code after those have been uploaded. The
# diagnostics themselves are kept alongside it, because BuildKit will replay a
# cached stage's log as a single "CACHED" line on a later run.
FROM build AS clippy

ARG TARGETARCH
ARG CARGO_PROFILE=release

RUN --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    mkdir -p /out; \
    set +e; \
    cargo clippy --profile "${CARGO_PROFILE}" --workspace --all-targets --offline \
        -- -D warnings > /out/clippy.log 2>&1; \
    rc=$?; set -e; \
    cat /out/clippy.log; \
    echo "$rc" > /out/exit_code

FROM scratch AS output_clippy
COPY --from=clippy /out/clippy.log /clippy.log
COPY --from=clippy /out/exit_code /exit_code

FROM build AS nextest

ARG HELM_PATH="docker/helm"

# Install unit test harness. nextest can report results in JUnit format, among other
# benefits. The "linux" tarball at get.nexte.st serves the host arch automatically.
ARG NEXTEST_VERSION="0.9.101"
RUN case "${TARGETARCH}" in \
        amd64) NEXTEST_PATH="linux" ;; \
        arm64) NEXTEST_PATH="linux-arm" ;; \
        *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && curl -LsSf "https://get.nexte.st/${NEXTEST_VERSION}/${NEXTEST_PATH}" \
        | tar zxf - -C ${CARGO_HOME:-~/.cargo}/bin

COPY --from=helm /usr/bin/helm /usr/local/bin/helm

# vendor/ and target/ are already materialized on disk by the `build` stage
# while Cargo's registry/git download cache is reused from BuildKit mounts.
#
# The chart is bind-mounted at /docker/helm because test_helm_chart_config
# resolves it as CARGO_MANIFEST_DIR/../../docker/helm, and CARGO_MANIFEST_DIR is
# /source/ovdc. OVDC_REQUIRE_HELM makes that test fail rather than skip if helm
# is missing, so a broken install cannot silently drop the coverage.
RUN --mount=type=bind,source=${HELM_PATH},target=/docker/helm \
    --mount=type=cache,id=ovdc-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ovdc-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    mkdir -p /out; \
    cargo vendor --versioned-dirs --locked; \
    set +e; OVDC_REQUIRE_HELM=1 cargo nextest run --cargo-profile "${CARGO_PROFILE}"; rc=$?; set -e; \
    if [ -f target/nextest/default/junit.xml ]; then \
        cp target/nextest/default/junit.xml /out/junit.xml; \
    fi; \
    echo "$rc" > /out/exit_code

FROM scratch AS output_nextest
COPY --from=nextest /out/junit.xml /junit.xml
COPY --from=nextest /out/exit_code /exit_code

FROM scratch AS out_binary
ARG EXE="release/main"
COPY --from=build /out/$EXE /binary
COPY --from=build /out/binary.spdx.json /binary.spdx.json

FROM scratch AS out_sources
COPY --from=build /out/sources.tar.gz /sources.tar.gz
COPY --from=build /out/sources.spdx.json /sources.spdx.json
COPY --from=build /out/third_party_licenses.md /third_party_licenses.md

FROM ${BASE} AS dev_container
ARG EXE="release/main"
ARG RUNTIME_UID=1000
ARG RUNTIME_GID=1000

COPY --from=build /etc/ssl/certs /etc/ssl/certs
COPY --from=build /out/$EXE /main
COPY --from=build /out/binary.spdx.json /binary.spdx.json
# Read by the SBOM generator (docker/sbom-generator) at attestation time; it
# has no cargo workspace of its own to derive licenses from.
COPY --from=build /out/cargo-licenses.json /cargo-licenses.json
RUN mkdir -p /env && chown ${RUNTIME_UID}:${RUNTIME_GID} /env
USER ${RUNTIME_UID}:${RUNTIME_GID}

FROM dev_container AS release_container
ARG TARGETARCH

COPY --from=build /out/sources.tar.gz /sources.tar.gz
COPY --from=build /out/sources.spdx.json /sources.spdx.json
COPY --from=build /out/third_party_licenses.md /third_party_licenses.md
