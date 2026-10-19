// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{Error, Result};

mod atomiccell;
use atomiccell::AtomicCell;

mod cache;
pub use self::cache::{Cache, ConnectRequest, Location, Metadata, PutRequest};

mod file;

macro_rules! record {
    ( $( $i:ident ),* ) => {
        $(
            tracing::Span::current().record(stringify!($i), $i);
        )*
    };
}

use record;
