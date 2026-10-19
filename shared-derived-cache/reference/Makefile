# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Native development helpers. Run from the repository root.
.DEFAULT_GOAL := help
CARGO ?= cargo
ENV_DIR ?= rust/target/env
CONFIG ?= rust/ovdc/src/config.toml

# macOS system Make/shells strip inherited DYLD_* variables. Set the path
# immediately before Cargo so bindgen build scripts can load libclang.
ifneq ($(strip $(LIBCLANG_PATH)),)
CARGO_ENV = DYLD_LIBRARY_PATH="$(LIBCLANG_PATH)"
endif

.PHONY: help build release test lint run
help:
	@printf '%s\n' 'make build       Build the debug server' 'make release     Build the release server' 'make test        Run workspace tests' 'make lint        Run Clippy with warnings denied' 'make run         Run the server (override CONFIG and ENV_DIR as needed)'

build:
	cd rust && $(CARGO_ENV) $(CARGO) build --locked --bin main

release:
	cd rust && $(CARGO_ENV) $(CARGO) build --locked --release --bin main

test:
	cd rust && $(CARGO_ENV) $(CARGO) test --locked --workspace

lint:
	cd rust && $(CARGO_ENV) $(CARGO) clippy --locked --workspace --all-targets -- -D warnings

run:
	cd rust && $(CARGO_ENV) $(CARGO) run --locked --bin main -- "$(abspath $(ENV_DIR))" "$(abspath $(CONFIG))"
