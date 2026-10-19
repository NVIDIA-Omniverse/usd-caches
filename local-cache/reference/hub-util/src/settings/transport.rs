// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::SerializeToToml;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Tcp {
        #[serde(default = "default_host")]
        host: String,
        /// The Tcp port used for Messagepack
        #[serde(default = "default_rpc_port")]
        rpc_port: u16,
        /// The Tcp port used for Protobuf
        #[serde(default = "default_ext_port")]
        ext_port: u16,
    },
    Ipc {
        #[cfg(unix)]
        #[serde(default = "default_unix_socket")]
        unix_socket: PathBuf,
        #[cfg(windows)]
        #[serde(default = "default_named_pipe")]
        named_pipe: PathBuf,
    },
}

impl Default for Transport {
    fn default() -> Self {
        default_transport()
    }
}

pub(super) fn default_host() -> String {
    "127.0.0.1".to_string()
}

/// Default Tcp port used for MessagePack
pub(super) fn default_rpc_port() -> u16 {
    14092
}

/// Default Tcp port used for protobuf
pub(super) fn default_ext_port() -> u16 {
    14093
}

#[cfg(unix)]
pub(super) fn default_unix_socket() -> PathBuf {
    todo!("not implemented")
}

#[cfg(windows)]
pub(super) fn default_named_pipe() -> PathBuf {
    todo!("not implemented")
}

pub(super) fn default_transport() -> Transport {
    Transport::Tcp {
        host: default_host(),
        rpc_port: default_rpc_port(),
        ext_port: default_ext_port(),
    }
}

impl SerializeToToml for Transport {
    fn serialize_to_toml(&self, include_defaults: bool) -> Vec<String> {
        let mut result = Vec::new();

        match self {
            Transport::Tcp {
                host,
                rpc_port,
                ext_port,
            } => {
                if *host != default_host() || include_defaults {
                    result.push(format!("host = \"{host}\""));
                }
                if *rpc_port != default_rpc_port() || include_defaults {
                    result.push(format!("rpc_port = {rpc_port}"));
                }
                if *ext_port != default_ext_port() || include_defaults {
                    result.push(format!("ext_port = {ext_port}"));
                }

                if !result.is_empty() {
                    result.insert(0, "[transport.tcp]".to_owned());
                }
            },
            #[cfg(unix)]
            Transport::Ipc { unix_socket } => {
                if *unix_socket != default_unix_socket() || include_defaults {
                    result.push(format!("unix_socket = \"{}\"", unix_socket.display()));
                }

                if !result.is_empty() {
                    result.insert(0, "[transport.ipc]".to_owned());
                }
            },
            #[cfg(windows)]
            Transport::Ipc { named_pipe } => {
                if *named_pipe != default_named_pipe() || include_defaults {
                    result.push(format!("named_pipe = \"{}\"", named_pipe.display()));
                }

                if !result.is_empty() {
                    result.insert(0, "[transport.ipc]".to_owned());
                }
            },
        };

        result
    }
}
