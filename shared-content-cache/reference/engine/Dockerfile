# syntax=docker/dockerfile:1.7
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Builds the engine/ Rust workspace. The dependency stages deliberately keep
# target/ in normal image layers so a registry cache can restore compiled crates
# on a fresh BuildKit worker.

ARG RUST_IMAGE=rust:1.96-bookworm
ARG DISTROLESS_CC_REPOSITORY=nvcr.io/nvidia/distroless
ARG DISTROLESS_CC_VERSION=v3.3.2

FROM ${RUST_IMAGE} AS toolchain

ARG CARGO_CHEF_VERSION=0.1.72
ARG RUST_VERSION=1.96.0

# cargo-chef starts before it rehydrates rust-toolchain.toml. Activating the
# exact workspace toolchain here prevents a floating patch release in the base
# image from producing artifacts that the real-source build must reject.
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    clang \
    perl \
    pkg-config \
    rsync \
    && rm -rf /var/lib/apt/lists/* \
    && rustup toolchain install "${RUST_VERSION}" --profile minimal --component clippy,rustfmt \
    && cargo install --locked cargo-chef --version "${CARGO_CHEF_VERSION}"

WORKDIR /build

FROM toolchain AS planner

# A bind mount keeps source bytes out of the planner layer. recipe.json changes
# only when dependency-defining workspace inputs change.
RUN --mount=type=bind,source=.,target=/source \
    rsync -a --delete --exclude=/target /source/ /build/ \
    && cargo chef prepare --recipe-path /recipe.json

FROM toolchain AS cook-debug

COPY --from=planner /recipe.json /recipe.json
RUN cargo chef cook --profile test --workspace --all-targets --recipe-path /recipe.json \
    2>&1 | tee /cook-debug.log \
    && cargo clippy --profile test --workspace --all-targets \
    2>&1 | tee -a /cook-debug.log

FROM toolchain AS cook-release

COPY --from=planner /recipe.json /recipe.json
RUN cargo chef cook --release -p ovcc --recipe-path /recipe.json \
    2>&1 | tee /cook-release.log \
    && cargo chef cook --profile dist -p ovcc --recipe-path /recipe.json \
    2>&1 | tee -a /cook-release.log

FROM cook-release AS real-source

COPY . .
# cargo-chef creates stub workspace targets. Checkout mtimes can be older than
# those artifacts, so source files are stamped to force only workspace units
# through the compiler.
RUN find crates -type f \( -name '*.rs' -o -name 'build.rs' \) -print0 \
    | xargs -0 -r touch

FROM real-source AS engine-build

RUN cargo build --profile dist -p ovcc 2>&1 | tee /engine-build.log \
    && cp target/dist/ovcc /usr/local/bin/ovcc

# A separate, non-publishable binary for the isolated fault-injection scenario.
# It keeps release optimizations but links the testing crate through an explicit
# non-default feature. The ordinary runtime stage below never copies this file.
FROM real-source AS engine-build-test

RUN cargo build --release -p ovcc --features test-fault-injection \
      2>&1 | tee /engine-build-test.log \
    && cp target/release/ovcc /usr/local/bin/ovcc-test

# Standalone local upgrade evidence; this target is never a release image.
FROM real-source AS engine-build-optional-keys
RUN cargo build --release -p ovcc --features test-optional-keys \
    && cargo build --release -p ovcc-storage --features test-fixture-inspection \
         --example inspect_optional_key_fixture

FROM cook-debug AS checks-source

COPY . .
RUN find crates -type f \( -name '*.rs' -o -name 'build.rs' \) -print0 \
    | xargs -0 -r touch

FROM checks-source AS checks-compile
RUN { \
      cargo fmt --all -- --check \
      && cargo clippy --profile test --workspace --all-targets \
      && cargo clippy --profile test -p ovcc-upstream -p ovcc-proxy \
           --all-targets --features test-fault-injection \
      && cargo test --workspace --no-run \
      && cargo test -p ovcc-proxy --features test-fault-injection \
           --test fault_injection --no-run; \
    } 2>&1 | tee /checks-compile.log

FROM checks-compile AS checks-run

# CI supplies a unique value for every invocation. Compilation remains fresh,
# while this execution layer cannot substitute a cached test result.
ARG TEST_RUN_NONCE=local
RUN { \
      echo "test run ${TEST_RUN_NONCE}" \
      && env \
          -u OTEL_EXPORTER_OTLP_ENDPOINT \
          -u OTEL_EXPORTER_OTLP_TRACES_ENDPOINT \
          -u OTEL_SDK_DISABLED \
          -u OVCC_OTLP \
          cargo test --workspace \
      && env \
          -u OTEL_EXPORTER_OTLP_ENDPOINT \
          -u OTEL_EXPORTER_OTLP_TRACES_ENDPOINT \
          -u OTEL_SDK_DISABLED \
          -u OVCC_OTLP \
          cargo test -p ovcc-upstream --features test-fault-injection \
      && env \
          -u OTEL_EXPORTER_OTLP_ENDPOINT \
          -u OTEL_EXPORTER_OTLP_TRACES_ENDPOINT \
          -u OTEL_SDK_DISABLED \
          -u OVCC_OTLP \
          cargo test -p ovcc-proxy --features test-fault-injection \
            --test fault_injection; \
    } 2>&1 | tee /checks-run.log

FROM ${DISTROLESS_CC_REPOSITORY}/cc:${DISTROLESS_CC_VERSION} AS test-runtime

COPY --from=engine-build-test /usr/local/bin/ovcc-test /usr/local/bin/ovcc

ENV HOME=/tmp \
    TMPDIR=/tmp
USER 1000:1000

ENTRYPOINT ["/usr/local/bin/ovcc"]
CMD ["/etc/ovcc/values.yaml"]

FROM ${DISTROLESS_CC_REPOSITORY}/cc:${DISTROLESS_CC_VERSION} AS optional-keys-test-runtime
ARG OVCC_SOURCE_REVISION
ARG OVCC_SOURCE_FINGERPRINT
LABEL org.opencontainers.image.revision=${OVCC_SOURCE_REVISION} \
      com.nvidia.ovcc.source-fingerprint=${OVCC_SOURCE_FINGERPRINT} \
      com.nvidia.ovcc.test-policy="ovcc.test.optional-x-ms-version.v1"
COPY --from=engine-build-optional-keys /build/target/release/ovcc /usr/local/bin/ovcc
COPY --from=engine-build-optional-keys /build/target/release/examples/inspect_optional_key_fixture /usr/local/bin/inspect-optional-key-fixture
ENV HOME=/tmp TMPDIR=/tmp
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/ovcc"]
CMD ["/etc/ovcc/values.yaml"]

# Offline metadata inspection runs in an init container, outside the cache runtime.
FROM real-source AS metadata-inspector-build
RUN cargo build --release -p ovcc-storage --features test-fixture-inspection \
    --example inspect_optional_key_fixture

FROM ${DISTROLESS_CC_REPOSITORY}/cc:${DISTROLESS_CC_VERSION} AS metadata-inspector
ARG OVCC_SOURCE_REVISION
ARG OVCC_SOURCE_FINGERPRINT
LABEL org.opencontainers.image.revision=${OVCC_SOURCE_REVISION} \
      com.nvidia.ovcc.source-fingerprint=${OVCC_SOURCE_FINGERPRINT}
COPY --from=metadata-inspector-build /build/target/release/examples/inspect_optional_key_fixture /usr/local/bin/inspect-optional-key-fixture
ENV HOME=/tmp TMPDIR=/tmp
USER 1000:1000
ENTRYPOINT ["/usr/local/bin/inspect-optional-key-fixture"]

# Keep the golden runtime last: an ordinary Docker build always produces the
# binary whose dependency graph excludes `ovcc-testing`.
FROM ${DISTROLESS_CC_REPOSITORY}/cc:${DISTROLESS_CC_VERSION} AS runtime
ARG OVCC_SOURCE_REVISION
ARG OVCC_SOURCE_FINGERPRINT
LABEL org.opencontainers.image.revision=${OVCC_SOURCE_REVISION} \
      com.nvidia.ovcc.source-fingerprint=${OVCC_SOURCE_FINGERPRINT}

COPY --from=engine-build /usr/local/bin/ovcc /usr/local/bin/ovcc

ENV HOME=/tmp \
    TMPDIR=/tmp
USER 1000:1000

ENTRYPOINT ["/usr/local/bin/ovcc"]
CMD ["/etc/ovcc/values.yaml"]
