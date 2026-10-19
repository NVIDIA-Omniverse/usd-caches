// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! GET-equivalent local object leases. The server authorizes and commits the
//! object before granting an immutable path and deadline; object bytes never
//! travel over the control connection.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ovcc_platform::local::Connection;
use serde::{Deserialize, Serialize};

mod pool;
pub use pool::{DEFAULT_MAX_CONNECTIONS, SharedClient};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_LEASE_MILLIS: u64 = 30_000;

/// The same origin identity and credentials used by an ordinary GET.
/// Deliberately omits Debug so signed queries and headers are not logged.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub version: u32,
    pub authority: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub nucleus: bool,
    pub lease_duration_millis: u64,
}

impl LeaseRequest {
    pub fn new(authority: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            authority: authority.into(),
            path: path.into(),
            query: String::new(),
            headers: Vec::new(),
            nucleus: false,
            lease_duration_millis: DEFAULT_LEASE_MILLIS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ready,
    NotLeasable,
    UnsupportedLayout,
    Rejected,
    Expired,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseResponse {
    pub version: u32,
    pub outcome: Outcome,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub cache_status: Option<String>,
    pub generation: u64,
    pub offset: u64,
    pub length: u64,
    pub path: Option<PathBuf>,
    pub deadline_unix_millis: u64,
}

/// A path/deadline promise. Dropping it sends no release message.
pub struct ObjectLease {
    response: LeaseResponse,
}

impl ObjectLease {
    pub fn path(&self) -> Option<&Path> {
        self.response.path.as_deref()
    }
    pub fn is_expired(&self) -> io::Result<bool> {
        Ok(now_unix_millis()? >= self.response.deadline_unix_millis)
    }
    pub fn response(&self) -> &LeaseResponse {
        &self.response
    }

    pub fn into_response(self) -> LeaseResponse {
        self.response
    }
}

pub enum LeaseResult {
    Ready(ObjectLease),
    Unavailable(LeaseResponse),
}

pub async fn read_request(connection: &mut Connection) -> io::Result<LeaseRequest> {
    let request: LeaseRequest = decode(&connection.read_frame().await?)?;
    if request.version != PROTOCOL_VERSION {
        return Err(invalid("unsupported lease protocol"));
    }
    Ok(request)
}

pub async fn write_request(connection: &mut Connection, request: &LeaseRequest) -> io::Result<()> {
    connection
        .write_frame(&postcard::to_stdvec(request).map_err(invalid)?)
        .await
}

pub async fn write_response(
    connection: &mut Connection,
    response: &LeaseResponse,
) -> io::Result<()> {
    connection
        .write_frame(&postcard::to_stdvec(response).map_err(invalid)?)
        .await
}

/// One dedicated connection, with at most one outstanding request.
pub struct Client {
    connection: Option<Connection>,
}

impl Client {
    /// Connects to this user's local OVCC without transport configuration.
    pub async fn connect_local() -> io::Result<Self> {
        Ok(Self {
            connection: Some(Connection::connect_default().await?),
        })
    }

    pub async fn connect(endpoint: &Path) -> io::Result<Self> {
        Ok(Self {
            connection: Some(Connection::connect(endpoint).await?),
        })
    }

    /// Reports cancellation/error poisoning, not peer liveness.
    pub fn is_usable(&self) -> bool {
        self.connection.is_some()
    }

    pub async fn lease(&mut self, request: &LeaseRequest) -> io::Result<LeaseResult> {
        // An interrupted exchange drops its stream so a late reply cannot be reused.
        let mut connection = self.connection.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "local connection is no longer usable",
            )
        })?;
        write_request(&mut connection, request).await?;
        let response = decode(&connection.read_frame().await?)?;
        let result = validate_response(response)?;
        self.connection = Some(connection);
        Ok(result)
    }
}

/// Acquires once; callers doing repeated requests retain a Client instead.
pub async fn lease(endpoint: &Path, request: &LeaseRequest) -> io::Result<LeaseResult> {
    Client::connect(endpoint).await?.lease(request).await
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    // The leading postcard u32 is stable even when a later payload schema changes.
    let (version, _) = postcard::take_from_bytes::<u32>(bytes).map_err(invalid)?;
    if version != PROTOCOL_VERSION {
        return Err(invalid("unsupported lease protocol"));
    }
    let (value, remaining) = postcard::take_from_bytes(bytes).map_err(invalid)?;
    if !remaining.is_empty() {
        return Err(invalid("trailing local protocol bytes"));
    }
    Ok(value)
}

fn validate_response(mut response: LeaseResponse) -> io::Result<LeaseResult> {
    if response.version != PROTOCOL_VERSION {
        return Err(invalid("unsupported lease protocol"));
    }
    if response.outcome != Outcome::Ready {
        return Ok(LeaseResult::Unavailable(response));
    }
    if !matches!(response.status, 200 | 206)
        || !response.path.as_ref().is_some_and(|p| p.is_absolute())
    {
        return Err(invalid("invalid path lease response"));
    }
    response
        .offset
        .checked_add(response.length)
        .ok_or_else(|| invalid("file range overflow"))?;
    if response.deadline_unix_millis <= now_unix_millis()? {
        response.outcome = Outcome::Expired;
        response.path = None;
        return Ok(LeaseResult::Unavailable(response));
    }
    Ok(LeaseResult::Ready(ObjectLease { response }))
}

pub fn now_unix_millis() -> io::Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(invalid)?
            .as_millis(),
    )
    .map_err(invalid)
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod wire_v1_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_get_wire_request_has_stable_binary_encoding() -> io::Result<()> {
        // Arrange.
        let mut request = LeaseRequest::new("a", "/");
        request.lease_duration_millis = 1;
        // Act.
        let bytes = postcard::to_stdvec(&request).map_err(invalid)?;
        let decoded: LeaseRequest = decode(&bytes)?;
        // Assert.
        assert_eq!(bytes, [1, 1, b'a', 1, b'/', 0, 0, 0, 1]);
        assert_eq!(decoded.authority, "a");
        assert_eq!(decoded.lease_duration_millis, 1);
        Ok(())
    }

    #[test]
    fn local_get_wire_request_rejects_json_truncation_and_trailing_bytes() -> io::Result<()> {
        // Arrange.
        let request = LeaseRequest::new("objects.test", "/foo.usd");
        let mut bytes = postcard::to_stdvec(&request).map_err(invalid)?;
        // Act.
        let json = decode::<LeaseRequest>(br#"{"version":1,"authority":"objects.test"}"#);
        let truncated = decode::<LeaseRequest>(&bytes[..bytes.len() - 1]);
        bytes.push(0);
        let trailing = decode::<LeaseRequest>(&bytes);
        // Assert.
        assert!(json.is_err());
        assert!(truncated.is_err());
        assert!(trailing.is_err());
        Ok(())
    }
}
