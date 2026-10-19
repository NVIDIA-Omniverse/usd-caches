// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Configuration(String),
    #[error(transparent)]
    Cache(#[from] hub_cache::Error),
    #[error("HTTP API error, status code {0}")]
    HttpApi(u16),
}
