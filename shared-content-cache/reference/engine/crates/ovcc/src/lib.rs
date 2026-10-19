// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! `ovcc` — the Omniverse Content Cache reverse-proxy.
//!
//! This crate is a thin wiring layer that loads configuration via
//! [`ovcc_config`] and serves the reverse-proxy cache via [`ovcc_proxy`]. All
//! real request-path logic lives in the `ovcc-*` building-block crates; see
//! `engine/AGENTS.md`.

pub use ovcc_config::{Config, load};
pub use ovcc_proxy::{Serving, run, serve};
