// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

#[derive(Error, Debug, Clone)]
pub enum Error {
    /// error from AddrParse
    #[error(transparent)]
    AddrParse(#[from] std::net::AddrParseError),
    /// Error creating request in client
    #[error("Failed to send request to Hub Client thread: {0}]")]
    ChannelSend(String),
    /// Internal error
    #[error("Internal Hub error: {0}")]
    Internal(String),
    /// Invalid argument
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    /// error from io
    #[error("Io Error: {0}")]
    Io(String),
    /// error from tokio channel receiver
    #[error(transparent)]
    ChannelRecv(#[from] flume::RecvError),
    /// Client has been shutdown
    #[error("Client has been shutdown. Reason: {0}")]
    Shutdown(String),
    /// Unexpected response type from server
    #[error("Unexpected response type from server")]
    UnexpectedResponseType,
    /// UTF-8 error
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    /// From UTF-8 error
    #[error(transparent)]
    FromUtf8(#[from] std::string::FromUtf8Error),
    /// Version mismatch with server
    #[error("Version Incompatibility: {0}")]
    Version(String),
    /// Client failed launching Hub
    #[error("Launch error: {0:?}")]
    Launch(Option<i32>),
    /// serde_json error
    #[error("Serde Json Error: {0:?}")]
    SerdeJson(String),
    /// Configuration error
    #[error("Configuration Error: {0:?}")]
    Config(String),
    /// Hub not found error
    #[error("Hub not found")]
    HubNotFound,
    /// Error from the logging system
    #[error("Logging Error: {0:?}")]
    Logging(String),
}

pub type Result<T, E = Error> = ::core::result::Result<T, E>;

impl<T> From<flume::SendError<T>> for Error {
    fn from(value: flume::SendError<T>) -> Self {
        Error::ChannelSend(value.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value.to_string())
    }
}

impl From<&std::io::Error> for Error {
    fn from(value: &std::io::Error) -> Self {
        Error::Io(value.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::SerdeJson(value.to_string())
    }
}

impl From<tracing_subscriber::reload::Error> for Error {
    fn from(value: tracing_subscriber::reload::Error) -> Self {
        Error::Logging(value.to_string())
    }
}

/// Possible results from Hub Client Operations
/// cbindgen:prefix-with-name
#[repr(C)]
#[derive(Debug)]
pub enum ClientResultCode {
    /// Success
    Success = 0,
    /// The client has not yet been initialized
    ClientNotInitialized,
    /// The client has already been initialized
    AlreadyInitialized,
    /// An invalid argument (such as a null pointer) was provided
    InvalidArgument,
    /// Internal error
    InternalError,
    /// Failed to parse a provided IP address
    AddrParseError,
    /// Failed to create the request in the Hub Client
    ChannelSendError,
    /// Generic IO error
    Io,
    /// Client has been shutdown
    Shutdown,
    /// Failed to receive response in the Hub Client
    RecvError,
    /// The server sent an incorrect response type for an operation
    UnexpectedResponseType,
    /// The cache path has invalid UTF-8 characters
    Utf8Error,
    /// From UTF-8 conversion error
    FromUtf8Error,
    /// Version incompatibility with the server
    Version,
    /// Error launching Hub process
    Launch,
    /// Error parsing json config
    SerdeJson,
    /// Configuration error
    Config,
    /// Hub binary not found
    HubNotFound,
    /// Logging error
    Logging,
}

impl From<&Error> for ClientResultCode {
    fn from(value: &Error) -> Self {
        match value {
            Error::AddrParse(_) => Self::AddrParseError,
            Error::ChannelSend(_) => Self::ChannelSendError,
            Error::Internal(_) => Self::InternalError,
            Error::InvalidArgument(_) => Self::InvalidArgument,
            Error::Io(_) => Self::Io,
            Error::ChannelRecv(_) => Self::RecvError,
            Error::Shutdown(_) => Self::Shutdown,
            Error::UnexpectedResponseType => Self::UnexpectedResponseType,
            Error::Utf8(_) => Self::Utf8Error,
            Error::FromUtf8(_) => Self::FromUtf8Error,
            Error::Version(_) => Self::Version,
            Error::Launch(_) => Self::Launch,
            Error::SerdeJson(_) => Self::SerdeJson,
            Error::Config(_) => Self::Config,
            Error::HubNotFound => Self::HubNotFound,
            Error::Logging(_) => Self::Logging,
        }
    }
}
