#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build and check OVDC using its existing Dockerfile stages and public bases.
set -euo pipefail

usage() {
  echo "Usage: $0 {build|test|lint|all} [--print]" >&2
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 2
fi
mode=$1
case "$mode" in
  build|test|lint|all) ;;
  *) usage; exit 2 ;;
esac
print_only=false
if [[ $# -eq 2 ]]; then
  [[ $2 == --print ]] || { usage; exit 2; }
  print_only=true
fi

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd -- "$project_root"
command -v docker >/dev/null || { echo 'Docker with Buildx is required' >&2; exit 1; }

case "$(uname -m)" in
  x86_64|amd64) default_platform=linux/amd64 ;;
  arm64|aarch64) default_platform=linux/arm64 ;;
  *) echo 'Unsupported host architecture; use an amd64 or arm64 host' >&2; exit 1 ;;
esac
export PLATFORMS=${OVDC_PLATFORM:-$default_platform}
case "$PLATFORMS" in
  linux/amd64|linux/arm64) ;;
  *) echo 'OVDC_PLATFORM must be linux/amd64 or linux/arm64' >&2; exit 2 ;;
esac

# The overlay replaces image bases; disabling cache references prevents private
# registry access even if the invoking shell carries internal CI variables.
export CACHE_REF=""
export RUST_VERSION
RUST_VERSION=$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust/rust-toolchain.toml)
[[ -n $RUST_VERSION ]] || { echo 'Rust toolchain channel not found' >&2; exit 1; }

run_check() {
  local check=$1 target output_dir status
  case "$check" in
    build) target=out_binary ;;
    test) target=output_nextest ;;
    lint) target=output_clippy ;;
  esac
  local bake=(docker buildx bake
    -f docker/dockerfile/docker-bake.hcl
    -f docker/dockerfile/public-bake.hcl)

  if $print_only; then
    "${bake[@]}" --print "$target"
    return
  fi

  # Fresh directories ensure an earlier successful exit_code cannot mask a
  # failed export. Retain reports and binaries for inspection after every run.
  output_dir=$(mktemp -d "${OVDC_OUTPUT_DIR:-${TMPDIR:-/tmp}}/ovdc-${check}.XXXXXX")
  echo "${check} output: $output_dir"
  "${bake[@]}" --allow "fs.write=$output_dir" \
    --set "$target.output=type=local,dest=$output_dir" "$target"

  if [[ $check == build ]]; then
    [[ -s $output_dir/binary ]] || { echo 'Binary export missing' >&2; return 1; }
    return
  fi
  if [[ ! -f $output_dir/exit_code ]]; then
    echo "${check} exit_code missing: $output_dir" >&2
    return 1
  fi
  status=$(cat -- "$output_dir/exit_code")
  case "$status" in
    0) return ;;
    [1-9]|[1-9][0-9]|1[0-9][0-9]|2[0-4][0-9]|25[0-5])
      echo "${check} failed with exit code $status; reports: $output_dir" >&2
      return "$status"
      ;;
    *) echo "Invalid ${check} exit_code: $output_dir" >&2; return 1 ;;
  esac
}

if [[ $mode == all ]]; then
  for check in build test lint; do
    run_check "$check"
  done
else
  run_check "$mode"
fi
