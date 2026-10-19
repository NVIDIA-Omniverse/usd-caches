# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Rust toolchain image, layered onto public Ubuntu 24.04 instead
# of upstream's `buildpack-deps`. Installs rustup the same way
# rust-lang/docker-rust's Debian template does (see
# https://github.com/rust-lang/docker-rust/blob/master/Dockerfile-debian.template)
# so the main Dockerfile's `toolchain` stage -- which expects `rustup`,
# `cargo`, and `rustc` already on PATH -- behaves identically to when it ran
# on top of the upstream `rust:*-trixie` image.
ARG BASE="ubuntu:24.04"
FROM ${BASE}

LABEL org.opencontainers.image.source=https://github.com/rust-lang/docker-rust

ARG RUST_VERSION
ARG RUSTUP_VERSION=1.29.1

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH \
    RUST_VERSION=${RUST_VERSION}

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*

# rustupSha256 values are the published checksums of the rustup-init binary
# itself for RUSTUP_VERSION -- unrelated to RUST_VERSION -- taken from
# rust-lang/docker-rust's generated Dockerfiles.
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
        'amd64') \
            rustArch='x86_64-unknown-linux-gnu'; \
            rustupSha256='dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71'; \
            ;; \
        'arm64') \
            rustArch='aarch64-unknown-linux-gnu'; \
            rustupSha256='15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433'; \
            ;; \
        *) \
            echo >&2 "unsupported architecture: $arch"; \
            exit 1; \
            ;; \
    esac; \
    url="https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${rustArch}/rustup-init"; \
    wget --progress=dot:giga "$url"; \
    echo "${rustupSha256} *rustup-init" | sha256sum -c -; \
    chmod +x rustup-init; \
    ./rustup-init -y --no-modify-path --profile minimal --default-toolchain "$RUST_VERSION" --default-host "${rustArch}"; \
    rm rustup-init; \
    chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"; \
    rustup --version; \
    cargo --version; \
    rustc --version;
