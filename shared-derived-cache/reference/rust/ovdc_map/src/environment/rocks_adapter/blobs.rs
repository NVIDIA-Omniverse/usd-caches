// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

mod blob_table;

use crate::{environment::rocks_adapter::value, Blob, BlobProvider, FLAG_DB_LOG_ENABLE};

use super::{log, metrics, multi_part, write_semaphore, Rocks};

impl BlobProvider for Rocks {
    fn blobs(&self) -> &dyn Blob {
        match self.flags & FLAG_DB_LOG_ENABLE != 0 {
            true => unsafe { &*(self as *const Rocks as *const log::Blobs) },
            false => unsafe { &*(self as *const Rocks as *const blob_table::Blobs) },
        }
    }
}
