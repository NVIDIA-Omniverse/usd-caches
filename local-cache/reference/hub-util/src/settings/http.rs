// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

use super::SerializeToToml;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Http {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Default for Http {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
        }
    }
}

pub(super) fn default_host() -> String {
    "127.0.0.1".to_string()
}

pub(super) fn default_port() -> u16 {
    14090
}

impl SerializeToToml for Http {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();

        if self.host != default_host() || include_defaults {
            result.push(format!("host = \"{}\"", self.host));
        }

        if self.port != default_port() || include_defaults {
            result.push(format!("port = {}", self.port));
        }

        // Only add header if we're actually serializing values
        if !result.is_empty() {
            result.insert(0, "[http]".to_owned());
        }

        result
    }
}
