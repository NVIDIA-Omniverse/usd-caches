// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Standard options for put

/// The value provided with this option must be a u8 between range 0-1.
/// When no value is provided this option defaults to 1 when interpreted by the server.
pub const OPTION_DURABILITY: u8 = 1;

/// The value provided with this option must be a u64 if provided.
pub const OPTION_ENTRY_TTL: u8 = 2;

/// Options provided by the client for this request.
#[derive(Default, Debug, Clone, Copy)]
pub struct Options {
    /// Dictates to the DB the required durability level for the write operation.
    ///
    /// The DB will contain the specifics on what the durability levels mean. The default for this
    /// option is 1 when interpreted by the server if no value is provided.
    pub durability_level: Option<u8>,

    /// The time to live for a key specified in milliseconds.
    pub ttl: Option<u64>,
}
