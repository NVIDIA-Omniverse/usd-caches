// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use ovdc_bytes::human_serde::{HumanByteSize, Interval};

/// How long a single PUT stream may run before it is cut off, when the
/// configuration does not say.
pub const PUT_TIMEOUT_DEFAULT: Duration = Duration::from_secs(60 * 60);

/// The ceiling this binary will accept for `grpc.putTimeout`.
///
/// A PUT holds a write permit and a partially written blob's parts for its
/// whole life, so an unbounded one is a resource leak wearing a slow upload's
/// clothes. A value above this is corrected rather than rejected -- an operator
/// typo should not stop the service from starting.
pub const PUT_TIMEOUT_MAX: Duration = Duration::from_secs(4 * 60 * 60);

/// The last out-of-range `putTimeout` already reported.
///
/// [`Grpc::put_timeout`] is called once per PUT stream, so reporting on every
/// call would turn one bad config line into a log flood. Keying on the value
/// means the correction is reported once, and again if a reload introduces a
/// *different* bad one. `i64::MIN` is the nothing-reported-yet sentinel; no
/// interval parses to it.
static REPORTED_PUT_TIMEOUT: AtomicI64 = AtomicI64::new(i64::MIN);

/// Configure the GRPC HTTP service.
#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grpc {
    /// The port to listen on.
    pub port: u16,

    /// Set the initial HTTP2 stream window size.
    /// Must not be 0.
    pub initial_stream_window_size: u32,

    /// Set the initial HTTP2 connection window size.
    /// Must not be 0.
    pub initial_connection_window_size: u32,

    /// Max number of requests that can be operated on concurrently per connection.
    /// Must not be 0.
    pub connection_concurrency_limit: u64,

    /// Max number of HTTP2 streams per connection.
    /// 0 for unlimited.
    pub max_concurrent_streams: u32,

    /// Max frame size for each HTTP2 data frame.
    /// 0 for HTTP2 default.
    pub max_frame_size: u32,

    /// Max time that a request can take.
    /// Must not be 0.
    pub timeout: Interval,

    /// The amount of time to wait before timing out a proto buf write.
    pub write_timeout: Interval,

    /// The amount of time in seconds before keepalive probes are sent.
    pub tcp_keepalive_after_idle_time: Interval,

    /// The amount of time between each keepalive probe.
    pub tcp_keepalive_interval: Interval,

    /// The number of probes to send before the socket is considered reset.
    pub tcp_keepalive_retries: u32,

    /// The amount of time to wait between http2 keepalive probes.
    pub http2_keepalive_interval: Interval,

    /// The amount of time to wait before connection is considered reset.
    pub http2_keepalive_timeout: Interval,

    /// If true a tenant ID can be set through a string set in the metadata.
    pub tenant_from_metadata: bool,

    /// The max size that the GRPC service will allow.
    pub max_decoding_message_size: u64,

    /// Max time that a single PUT stream can run for.
    ///
    /// Separate from `timeout`, which bounds an ordinary request: a PUT is a
    /// client-driven stream that legitimately runs for minutes, so it needs a
    /// ceiling of its own. Clamped to [`PUT_TIMEOUT_MAX`]; read through
    /// [`Grpc::put_timeout`] rather than directly.
    pub put_timeout: Interval,
}

impl Grpc {
    /// The effective ceiling on one PUT stream, clamped into range.
    ///
    /// A value above [`PUT_TIMEOUT_MAX`] is reported and clamped. A value that
    /// is zero or negative cannot be what the operator meant -- it would refuse
    /// every PUT -- so it is reported and falls back to [`PUT_TIMEOUT_DEFAULT`].
    /// Neither case aborts.
    pub fn put_timeout(&self) -> Duration {
        // `dur()` answers `None` for a negative interval; fold that in with
        // zero, since both describe a limit no PUT could ever satisfy.
        let (effective, configured_ms) = match self.put_timeout.dur() {
            Some(d) if d.is_zero() => (PUT_TIMEOUT_DEFAULT, 0),
            None => (PUT_TIMEOUT_DEFAULT, -1),
            Some(d) if d > PUT_TIMEOUT_MAX => (PUT_TIMEOUT_MAX, d.as_millis() as i64),
            Some(d) => return d,
        };

        if REPORTED_PUT_TIMEOUT.swap(configured_ms, Ordering::Relaxed) != configured_ms {
            tracing::error!(
                configured_ms,
                using_secs = effective.as_secs(),
                max_secs = PUT_TIMEOUT_MAX.as_secs(),
                "grpc.putTimeout is outside the accepted range; using {}s instead",
                effective.as_secs(),
            );
        }

        effective
    }
}

impl Default for Grpc {
    fn default() -> Self {
        Self {
            port: 3010,
            initial_stream_window_size: 512_000,
            initial_connection_window_size: 32_000_000,
            connection_concurrency_limit: 32,
            max_concurrent_streams: 0,
            max_frame_size: 0,
            timeout: 30000.into(),
            write_timeout: 16000.into(),
            tcp_keepalive_after_idle_time: 3000.into(),
            tcp_keepalive_interval: 3000.into(),
            tcp_keepalive_retries: 5,
            http2_keepalive_interval: 3000.into(),
            http2_keepalive_timeout: 20000.into(),
            tenant_from_metadata: true,
            max_decoding_message_size: 4_200_000,
            put_timeout: (PUT_TIMEOUT_DEFAULT.as_millis() as i64).into(),
        }
    }
}

/// Configuration for TLS properties for GRPC.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tls {
    /// If true the tonic server will require TLS.
    pub enabled: bool,

    /// The path for the client certificate to use.
    pub cert: String,
    /// The path for the key of the client certificate.
    pub key: String,

    /// If true the ca root path provided will be used as the ca root for the TLS cert.
    pub include_ca_root: bool,

    /// The path to use for the ca root, only used if `include_ca_root` is true.
    pub ca_root: String,
}

impl Default for Tls {
    fn default() -> Self {
        Self {
            enabled: false,

            cert: "cert.pem".to_owned(),
            key: "cert.key".to_owned(),

            include_ca_root: false,

            ca_root: "ca.pem".to_owned(),
        }
    }
}

/// Configuration for JWT for GRPC.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Jwt {
    /// If true the tonic server will verify JWTs provided to the service.
    pub enabled: bool,

    /// If true, a valid JWT is required for requests to succeed.
    pub require: bool,

    /// Url for public JWK set.
    pub jwk_public_keyset_url: Vec<String>,

    /// The amount of time to wait before requesting a JWK set from the given url.
    pub jwk_update_interval_secs: u64,

    /// The amount of bytes to consume to cache JWTs across connections.
    pub cache_size_mb: u64,

    /// When true the aud claim of the token is verified.
    pub verify_aud: bool,

    /// Tokens will be required to have this value as their audience claim.
    pub aud_claim: String,

    /// When true verifies the expiration time of provided JWT.
    pub verify_exp: bool,
}

impl Default for Jwt {
    fn default() -> Self {
        Self {
            enabled: false,
            require: false,
            jwk_public_keyset_url: vec!["debug".to_owned()],
            jwk_update_interval_secs: 60,
            cache_size_mb: 16,
            verify_aud: false,
            aud_claim: Default::default(),
            verify_exp: false,
        }
    }
}

/// Configure mode in which values are stored.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum EngineMode {
    /// Blobs and values are stored in a single location.
    #[serde(rename = "legacy")]
    Legacy,

    /// Blobs and values are stored in two different locations.
    #[serde(rename = "legacy_and_discrete")]
    LegacyAndDiscrete,

    /// Blobs are stored in a discrete table and values are treated as logs.
    #[serde(rename = "log")]
    Log,
}

#[cfg(test)]
mod put_timeout_tests {
    use super::*;

    fn with(ms: i64) -> Grpc {
        Grpc {
            put_timeout: ms.into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_default_put_timeout_is_one_hour() {
        assert_eq!(Grpc::default().put_timeout(), Duration::from_secs(3600));
        assert_eq!(PUT_TIMEOUT_DEFAULT, Duration::from_secs(3600));
        assert_eq!(PUT_TIMEOUT_MAX, Duration::from_secs(4 * 3600));
    }

    #[test]
    fn test_put_timeout_under_the_cap_is_used_verbatim() {
        // Both ends of the accepted range, so a clamp that fired unconditionally
        // -- or one that used `>=` -- would show up here.
        assert_eq!(with(1).put_timeout(), Duration::from_millis(1));
        assert_eq!(
            with(90 * 60 * 1000).put_timeout(),
            Duration::from_secs(5400)
        );
        assert_eq!(with(4 * 3600 * 1000).put_timeout(), PUT_TIMEOUT_MAX);
    }

    #[test]
    fn test_put_timeout_over_the_cap_is_clamped_not_refused() {
        assert_eq!(with(4 * 3600 * 1000 + 1).put_timeout(), PUT_TIMEOUT_MAX);
        assert_eq!(with(24 * 3600 * 1000).put_timeout(), PUT_TIMEOUT_MAX);
        assert_eq!(with(i64::MAX).put_timeout(), PUT_TIMEOUT_MAX);
    }

    #[test]
    fn test_put_timeout_of_zero_or_negative_falls_back_to_the_default() {
        // Neither can be meant: both would refuse every PUT. Correcting to the
        // default keeps the service serving, which is the same call the
        // over-cap branch makes.
        assert_eq!(with(0).put_timeout(), PUT_TIMEOUT_DEFAULT);
        assert_eq!(with(-1).put_timeout(), PUT_TIMEOUT_DEFAULT);
        assert_eq!(with(i64::MIN).put_timeout(), PUT_TIMEOUT_DEFAULT);
    }

    #[test]
    fn test_human_duration_reaches_interval_as_milliseconds() {
        // The shipped config writes `putTimeout = "1h"`, and the settings layer
        // coerces that string to a number before `Interval`'s transparent i64
        // deserializer sees it. Pin that coercion here; the wiring from the
        // real config file through to `put_timeout()` is covered by
        // `test_shipped_config_put_timeout_is_one_hour` in the crate tests.
        let ms = ovdc_bytes::human_serde::parse_number("1h").expect("\"1h\" should parse");
        assert_eq!(ms, 3_600_000);
        assert_eq!(with(ms).put_timeout(), PUT_TIMEOUT_DEFAULT);

        let over = ovdc_bytes::human_serde::parse_number("5h").expect("\"5h\" should parse");
        assert_eq!(with(over).put_timeout(), PUT_TIMEOUT_MAX);
    }
}

/// Configure additional storage settings.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Store {
    /// The maximum size for the DB to expand to on disk.
    pub max_size: HumanByteSize,

    /// The size after which values will be split up.
    pub blob_cutoff: HumanByteSize,

    /// The size by which to split values by.
    pub blob_chunk_size: HumanByteSize,

    /// Engine to use when storing files.
    pub mode: EngineMode,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            max_size: HumanByteSize(32_000_000),
            blob_cutoff: HumanByteSize(8000),
            blob_chunk_size: HumanByteSize(4_000_000),
            mode: EngineMode::Legacy,
        }
    }
}
