// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::Cache;
use super::Http;
use super::Logging;
use super::Runtime;
use super::Transport;

pub trait SerializeToToml {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String>;
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Hub {
    #[serde(default)]
    pub cache: Cache,

    #[serde(default)]
    pub http: Http,

    #[serde(default)]
    pub logging: Logging,

    #[serde(default)]
    pub runtime: Runtime,

    #[serde(default)]
    pub transport: Transport,
}

impl SerializeToToml for Hub {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();
        result.append(&mut self.cache.serialize_to_toml(include_defaults));
        result.append(&mut self.http.serialize_to_toml(include_defaults));
        result.append(&mut self.logging.serialize_to_toml(include_defaults));
        result.append(&mut self.runtime.serialize_to_toml(include_defaults));
        result.append(&mut self.transport.serialize_to_toml(include_defaults));

        result
    }
}
