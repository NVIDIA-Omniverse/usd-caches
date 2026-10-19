// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

// Docker Bake file for OVDC.
//
// Usage:
//   docker buildx bake -f docker/dockerfile/docker-bake.hcl <target>
//
// Requires a docker-container driver builder (the default "docker" driver
// cannot import/export registry cache or write RUN --mount=type=cache layers):
//   docker buildx create --name ovdc --driver docker-container --use --bootstrap
//
// Override any variable from the command line, e.g.:
//   CACHE_REF=ghcr.io/example/ovdc/buildcache \
//   CARGO_PROFILE=ci-release \
//   docker buildx bake -f docker/dockerfile/docker-bake.hcl out_binary

// ---------- variables ---------------------------------------------------------

// Registry reference used for `type=registry` cache import/export. When empty,
// cache-from/cache-to are disabled so local builds don't try to contact a
// registry that doesn't exist.
variable "CACHE_REF" {
  default = ""
}

// Registry cache manifests are keyed by "<CACHE_REF>:<scope>" so scopes do not
// clobber each other's layer cache.
//
// Only two scopes are exported: "toolchain" and "cook". Both are keyed on
// inputs that rarely change (the Dockerfile's tool versions, and Cargo.lock)
// and both are expensive to rebuild. The `build` and `nextest` scopes were
// dropped: their cache keys include the source tree, so they miss on every
// real change, yet their layers carry all of cook's target/ plus more and so
// dominated registry storage. Measured on a warm run, `build` still took 327s
// and `nextest` 225s with those scopes imported -- they were paying storage
// for nothing.
variable "CACHE_MODE" {
  // "max" exports intermediate stage layers as well as the final image layers;
  // "min" exports only the final ones. "max" is required here so the `cook`
  // stage -- an intermediate that no target names directly -- is exported and
  // can be restored on a later run.
  //
  // Note that no cache exporter ever captures the contents of a RUN
  // --mount=type=cache block; those are builder-local and vanish with the
  // builder. That is why target/ and vendor/ are materialized into real layers
  // rather than left in cache mounts.
  default = "max"
}

variable "CARGO_PROFILE" {
  default = "release"
}

variable "CARGO_AUDITABLE_VERSION" {
  default = "0.7.4"
}

// SBOM scanner image used for the release_container attestation. Built from
// docker/sbom-generator and pinned by tag: the stock buildkit-syft-scanner
// expands /sources.tar.gz and emits ~21k packages with duplicate SPDXIDs, and
// leaves every rust-crate unlicensed.
variable "SBOM_GENERATOR" {
  default = ""
}

// cargo-chef builds the dependency-only `cook` stage that the build stage
// inherits its target/ directory from.
variable "CARGO_CHEF_VERSION" {
  default = "0.1.72"
}

variable "CBINDGEN_VERSION" {
  default = "0.29.4"
}

variable "SYFT_VERSION" {
  default = "1.45.1"
}

// helm is used by test_helm_chart_config in the nextest stage, which renders
// docker/helm and boots a database against the result.
variable "HELM_VERSION" {
  default = "3.16.3"
}

variable "EXE" {
  // Path inside /source/target that the final binary lives at; matches
  // cargo's "target/<profile>/main" convention.
  default = "release/main"
}

variable "BASE" {
  default = "ubuntu:24.04"
}

variable "RUNTIME_UID" {
  default = "1000"
}

variable "RUNTIME_GID" {
  default = "1000"
}

// Rust toolchain base image: rustup + RUST_VERSION's default toolchain
// installed on Ubuntu 24.04 by docker/dockerfile/rust-base.Dockerfile.
// The default is built locally through the public-rust-base Bake target; no private
// registry image or internal CI base-image job is required.
variable "RUST_BASE" {
  default = "public-rust-base"
}

// Rust toolchain version installed via rustup in the Dockerfile `toolchain`
// stage. Single source of truth for the compiler version — keep in sync with
// rust/rust-toolchain.toml. Threaded into the build via _common.args below.
variable "RUST_VERSION" {
  default = "1.96.1"
}

variable "TAG" {
  default = "latest"
}

// Product version compiled into the binary and logged at startup. CI sets it
// from the chart's appVersion plus the short commit SHA; leaving it empty
// yields a binary that reports `dev`, which is what a local build is.
variable "OVDC_VERSION" {
  default = ""
}

// Fully-qualified image ref base for the OVDC release container, e.g.
// "ghcr.io/example/ovdc". The per-arch tag
// "${IMAGE_REF}:${TAG}-${arch}" is what each native CI job pushes; a separate
// "docker buildx imagetools create" step stitches the multi-arch index.
// When empty, container targets only get the short local "container:<tag>"
// tag (handy for local dev).
variable "IMAGE_REF" {
  default = ""
}

// When "true", container/output targets push to the registry instead of
// loading into the local docker daemon. CI sets PUSH=true.
variable "PUSH" {
  default = "false"
}

// Default to the host arch; CI overrides PLATFORMS to e.g. "linux/amd64" or
// "linux/arm64" per job. Multi-platform values are allowed but each runner
// should normally build a single native arch and push to its own cache tag;
// a separate "imagetools create" step stitches the multi-arch index.
variable "PLATFORMS" {
  default = "linux/amd64"
}

// Suffix appended to every registry cache tag so amd64 and arm64 jobs do not
// race on the same manifest. Defaults to the arch component of PLATFORMS when
// PLATFORMS holds a single platform; CI may override explicitly with e.g.
// CACHE_ARCH_SUFFIX=amd64.
variable "CACHE_ARCH_SUFFIX" {
  default = ""
}

// ---------- helpers -----------------------------------------------------------

function "_arch_suffix" {
  params = []
  result = CACHE_ARCH_SUFFIX != "" ? CACHE_ARCH_SUFFIX : (
    length(split(",", PLATFORMS)) == 1
      ? element(split("/", PLATFORMS), length(split("/", PLATFORMS)) - 1)
      : "multi"
  )
}

// Build the cache-from list for a given scope. Empty list when CACHE_REF
// is unset so "docker buildx bake" doesn't try to dial a bogus registry.
// Tag is "<scope>-<arch>" so per-arch CI jobs don't clobber each other.
function "cache_from" {
  params = [scope]
  result = CACHE_REF == "" ? [] : [
    "type=registry,ref=${CACHE_REF}:${scope}-${_arch_suffix()}",
  ]
}

function "cache_to" {
  params = [scope]
  result = CACHE_REF == "" ? [] : [
    // Deliberately no ignore-error: a failed export used to be silent, and the
    // cache is now load-bearing rather than best-effort. Losing the `cook`
    // manifest costs every later build ~30 minutes of dependency compilation,
    // which is indistinguishable from "the cache never worked" unless the push
    // failure itself is reported. Only pushes to main export, so the blast
    // radius of a flaky registry is one main build, not every pull request.
    "type=registry,ref=${CACHE_REF}:${scope}-${_arch_suffix()},mode=${CACHE_MODE},compression=zstd,image-manifest=true,oci-mediatypes=true",
  ]
}

// Per-arch image tag list for a container target. When IMAGE_REF is unset we
// only emit the short local tag so `docker buildx bake` against a plain docker
// driver still works for local dev.
function "image_tags" {
  params = [short]
  result = IMAGE_REF == "" ? ["${short}:${TAG}"] : [
    "${IMAGE_REF}:${TAG}-${_arch_suffix()}",
  ]
}

// When PUSH=true, export the image straight to the registry; otherwise let
// buildx fall back to its default (load into docker for single-platform).
function "image_output" {
  params = []
  result = PUSH == "true" ? ["type=registry"] : []
}

// ---------- base target -------------------------------------------------------

target "_common" {
  // Bake paths are relative to the invocation directory, not the .hcl file.
  // Run bake from the repo root: `docker buildx bake -f docker/dockerfile/docker-bake.hcl ...`
  context    = "."
  dockerfile = "docker/dockerfile/Dockerfile"
  args = {
    RUST_BASE               = RUST_BASE
    BASE                    = BASE
    EXE                     = EXE
    CARGO_PROFILE           = CARGO_PROFILE
    RUST_VERSION            = RUST_VERSION
    CARGO_AUDITABLE_VERSION = CARGO_AUDITABLE_VERSION
    CARGO_CHEF_VERSION      = CARGO_CHEF_VERSION
    CBINDGEN_VERSION        = CBINDGEN_VERSION
    SYFT_VERSION            = SYFT_VERSION
    HELM_VERSION            = HELM_VERSION
    RUNTIME_UID             = RUNTIME_UID
    RUNTIME_GID             = RUNTIME_GID
    OVDC_VERSION            = OVDC_VERSION
  }
  platforms = split(",", PLATFORMS)
  contexts = RUST_BASE == "public-rust-base" ? {
    "${RUST_BASE}" = "target:public-rust-base"
  } : {}
}

// Build the toolchain locally instead of pulling a private registry image.
target "public-rust-base" {
  context = "."
  dockerfile = "docker/dockerfile/rust-base.Dockerfile"
  args = {
    BASE = BASE
    RUST_VERSION = RUST_VERSION
  }
  platforms = split(",", PLATFORMS)
}

// ---------- individual targets ------------------------------------------------

// Import one consistent cache graph for every target in the bake. The cook
// mode=max export includes the upstream toolchain records. Importing the
// overlapping toolchain scope as well caused dependency recompilation on
// fresh builders with unchanged inputs; cook-only imports reused them.
// Keep exports in both scopes for compatibility with existing consumers.

target "toolchain" {
  inherits   = ["_common"]
  target     = "toolchain"
  tags       = image_tags("toolchain")
  output     = image_output()
  cache-from = cache_from("cook")
  cache-to   = cache_to("toolchain")
}

// Third-party dependency compilation, keyed on recipe.json (manifests + lock
// file) rather than on source. Its own cache scope on purpose: the "build" tag
// is rewritten by every push to main, whereas this layer only needs to change
// when Cargo.lock does, and it is the expensive one to lose.
target "cook" {
  inherits   = ["_common"]
  target     = "cook"
  cache-from = cache_from("cook")
  cache-to   = cache_to("cook")
}

// Lint gate. Like nextest it inherits from `build`, so the "cook" scope is what
// spares it a cold cargo compile.
target "clippy" {
  inherits   = ["_common"]
  target     = "clippy"
  tags       = ["clippy:${TAG}"]
  cache-from = cache_from("cook")
}

// Export the clippy log and its exit code to ./clippy-output on the host. The
// stage records the exit code instead of failing, so the workflow reads it here
// and fails the job itself -- after the build artifacts have been uploaded.
target "output_clippy" {
  inherits   = ["_common"]
  target     = "output_clippy"
  output     = ["type=local,dest=./clippy-output"]
  cache-from = cache_from("cook")
}

target "nextest" {
  inherits   = ["_common"]
  target     = "nextest"
  tags       = ["nextest:${TAG}"]
  // Nextest inherits from `build`, which inherits from `cook`, so the "cook"
  // scope is what spares it a cold cargo compile.
  cache-from = cache_from("cook")
}

// Export target/nextest/default/junit.xml to ./nextest-output/junit.xml on
// the host. Honors the test exit code: a test failure fails the bake, but
// junit.xml is still copied to /out/junit.xml inside the image first so the
// XML can be retrieved from the build cache for diagnosis.
target "output_nextest" {
  inherits   = ["_common"]
  target     = "output_nextest"
  output     = ["type=local,dest=./nextest-output"]
  cache-from = cache_from("cook")
}

// The heavy stage: cargo vendor + cargo auditable build + nextest archive.
// Separate cache scope ("build") so its giant Rust cache doesn't mix with
// the toolchain layer cache.
target "build" {
  inherits   = ["_common"]
  target     = "build"
  cache-from = cache_from("cook")
}

target "out_binary" {
  inherits   = ["_common"]
  target     = "out_binary"
  output     = ["type=local,dest=./binary"]
  cache-from = cache_from("cook")
}

target "out_sources" {
  inherits   = ["_common"]
  target     = "out_sources"
  output     = ["type=local,dest=./sources"]
  cache-from = cache_from("cook")
}

target "dev_container" {
  inherits   = ["_common"]
  target     = "dev_container"
  tags       = image_tags("container")
  output     = image_output()
  cache-from = cache_from("cook")
}

// Same image as `dev_container`, exported as a `docker load`-able tarball
// instead of pushed to a registry. CI uploads this as a short-lived (7-day)
// build artifact per branch/PR run -- cheaper than pushing a throwaway tag to
// GHCR on every push, at the cost of needing `docker load` instead of
// `docker pull` to run it. Shares the same "cook"/"toolchain" cache as
// out_binary/out_sources, so it costs no extra compile time to also build.
target "out_dev_container" {
  inherits   = ["_common"]
  target     = "dev_container"
  tags       = image_tags("container")
  output     = ["type=docker,dest=./container/dev_container.tar"]
  cache-from = cache_from("cook")
}

target "release_container" {
  inherits   = ["_common"]
  target     = "release_container"
  tags       = image_tags("container-release")
  output     = image_output()
  attest     = SBOM_GENERATOR == "" ? [] : ["type=sbom,generator=${SBOM_GENERATOR}"]
  cache-from = cache_from("cook")
}

// ---------- groups ------------------------------------------------------------

// `docker buildx bake default` builds everything.
group "default" {
  targets = ["out_binary", "dev_container"]
}

group "all" {
  targets = ["toolchain", "cook", "clippy", "output_clippy", "nextest", "output_nextest", "out_binary", "out_sources", "dev_container", "out_dev_container", "release_container"]
}
