// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use tracing::debug;

// remove file, ignore file not found
pub fn remove_file<P>(path: P) -> io::Result<bool>
where
    P: AsRef<Path> + fmt::Debug,
{
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            debug!(?path, ?e, "error removing file");
            Err(e)
        },
    }
}

// remove directory and sub-directories, ignore file not found
pub fn remove_dir_all<P>(path: P) -> io::Result<bool>
where
    P: AsRef<Path> + fmt::Debug,
{
    match fs::remove_dir_all(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            debug!(?path, ?e, "error removing directory");
            Err(e)
        },
    }
}

// create directory, ignore directory already exists
pub fn create_dir<P>(path: P) -> io::Result<bool>
where
    P: AsRef<Path> + fmt::Debug,
{
    match fs::create_dir(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => {
            debug!(?path, ?e, "error creating directory");
            Err(e)
        },
    }
}
