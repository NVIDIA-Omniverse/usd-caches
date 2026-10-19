// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Helpful tools for standard way of logging errors.

/// Logs the expression if it results in an error with a formatted string.
/// The error must implement `Display` so that it can be logged.
///
/// The first argument must be an expression that results in an a `Result<.., T>`
/// where T implements `Display`. The remaining arguments are passed directly to
/// `tracing:error!`.
#[macro_export]
macro_rules! early {
    ($expression:expr, $($arg:tt)*) => {
        match $expression {
            Ok(x) => x,
            Err(error_message) => {
                tracing::error!(%error_message, $($arg)*);
                return Err(error_message.into());
            }
        }
    }
}

/// Logs the expression if it results in an error with a formatted string.
/// The error must implement `Display` so that it can be logged.
///
/// The first argument must be an expression that results in an a `Result<.., T>`
/// where T implements `Display`. The remaining arguments are passed directly to
/// `tracing:error!`.
#[macro_export]
macro_rules! err {
    ($expression:expr, $($arg:tt)*) => {
        match $expression {
            Ok(x) => Ok(x),
            Err(error_message) => {
                tracing::error!(%error_message, $($arg)*);
                Err(error_message)
            }
        }
    }
}

/// Logs the expression if it results in an error with a formatted string.
/// The error must implement `Display` so that it can be logged.
/// The result will be dropped and any error logged.
///
/// The first argument must be an expression that results in an a `Result<.., T>`
/// where T implements `Display`. The remaining arguments are passed directly to
/// `tracing:error!`.
#[macro_export]
macro_rules! drop {
    ($expression:expr, $($arg:tt)*) => {
        if let Err(error_message) = $expression {
            tracing::error!(%error_message, $($arg)*);
        }
    }
}
