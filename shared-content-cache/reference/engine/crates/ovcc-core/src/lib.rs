// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Shared primitives for OVCC, a unified `Error`/`Result` and the
//! `CacheStatus` surfaced as the `X-Cache-Status` response header.
//!
//! Every other `ovcc-*` crate depends on this one. This crate stays dependency-free.

use std::fmt;

/// Result alias used across all OVCC crates.
pub type Result<T> = std::result::Result<T, Error>;

/// Coarse error category. Fine-grained context goes in the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Config,
    Presigned,
    Storage,
    Cache,
    Upstream,
    Io,
    Internal,
}

/// The OVCC error type, a category, typed transport detail, and message.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    upstream_connection_failure: bool,
    fill_revoked: bool,
}

impl Error {
    /// Constructs an error with an explicit category.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            upstream_connection_failure: false,
            fill_revoked: false,
        }
    }

    /// The error category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Config error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }
    /// Presigned-URL error.
    pub fn presigned(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Presigned, message)
    }
    /// Storage-layer error.
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Storage, message)
    }
    /// Cache-engine error.
    pub fn cache(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Cache, message)
    }
    /// Upstream/origin error.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, message)
    }
    /// Upstream failure caused by an established HTTP connection becoming
    /// unusable before a response was received.
    pub fn upstream_connection(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Upstream,
            message: message.into(),
            upstream_connection_failure: true,
            fill_revoked: false,
        }
    }
    /// Whether this error is an established upstream connection failure.
    pub fn is_upstream_connection_failure(&self) -> bool {
        self.upstream_connection_failure
    }
    /// Storage failure caused by a fill whose key was removed or replaced
    /// before it could commit.
    ///
    /// Keeps `ErrorKind::Storage` and exposes the distinction through a
    /// predicate, following `upstream_connection`. A revoked fill is the
    /// correct outcome of a racing purge rather than an I/O failure, so a
    /// caller can decline to alarm on it.
    pub fn fill_revoked(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Storage,
            message: message.into(),
            upstream_connection_failure: false,
            fill_revoked: true,
        }
    }
    /// Whether this error is a fill revoked by removal or replacement.
    pub fn is_fill_revoked(&self) -> bool {
        self.fill_revoked
    }
    /// Internal invariant violation.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, err.to_string())
    }
}

/// Cache outcome for a request, rendered into the `X-Cache-Status` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Miss,
    Hit,
    Bypass,
    Expired,
    Revalidated,
}

impl CacheStatus {
    /// The stable wire value used by the `X-Cache-Status` header.
    pub fn as_str(&self) -> &'static str {
        match self {
            CacheStatus::Miss => "MISS",
            CacheStatus::Hit => "HIT",
            CacheStatus::Bypass => "BYPASS",
            CacheStatus::Expired => "EXPIRED",
            CacheStatus::Revalidated => "REVALIDATED",
        }
    }
}

impl fmt::Display for CacheStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_status_header_strings() {
        assert_eq!(CacheStatus::Miss.as_str(), "MISS");
        assert_eq!(CacheStatus::Hit.as_str(), "HIT");
        assert_eq!(CacheStatus::Bypass.as_str(), "BYPASS");
        assert_eq!(CacheStatus::Expired.as_str(), "EXPIRED");
        assert_eq!(CacheStatus::Revalidated.as_str(), "REVALIDATED");
    }

    #[test]
    fn error_display_includes_kind_and_message() {
        let err = Error::storage("disk full");
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert_eq!(err.to_string(), "Storage: disk full");
        assert!(!err.is_upstream_connection_failure());
        assert!(!err.is_fill_revoked());
    }

    #[test]
    fn upstream_connection_failure_retains_upstream_kind_and_typed_detail() {
        let err = Error::upstream_connection("connection terminated");
        assert_eq!(err.kind(), ErrorKind::Upstream);
        assert!(err.is_upstream_connection_failure());
    }

    #[test]
    fn fill_revoked_retains_storage_kind_and_typed_detail() {
        // Act.
        let err = Error::fill_revoked("fill was revoked before commit");

        // Assert.
        assert_eq!(err.kind(), ErrorKind::Storage);
        assert!(err.is_fill_revoked());
        assert!(!err.is_upstream_connection_failure());
    }

    #[test]
    fn io_error_maps_to_io_kind() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: Error = io.into();
        assert_eq!(err.kind(), ErrorKind::Io);
    }
}
