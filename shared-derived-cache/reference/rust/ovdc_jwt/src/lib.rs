// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

#![deny(unused, missing_docs)]

//! Verifies JSON Web Tokens and extracts the tenant account ID.
//!
//! Tokens require an `assertion` object containing string fields `ncaId`,
//! `functionId`, and `functionVersionId`. The verified `ncaId` identifies the tenant.
//! Signing keys are supplied as a JWK set; audience and expiration checks are configurable.
//!
//! See [JSON Web Token (RFC 7519)](https://www.rfc-editor.org/rfc/rfc7519)
//! and [JSON Web Key (RFC 7517)](https://www.rfc-editor.org/rfc/rfc7517).

pub mod jwk;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use jsonwebtoken::{jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

const BEARER_PREFIX: &str = "Bearer ";

/// An error related to verification of a token
#[derive(Debug)]
pub enum Error {
    /// The required JWK used to validate the JWT is not valid.
    InvalidJwk(jsonwebtoken::errors::Error),

    /// The provided JWT could not be verified.
    InvalidJwt(jsonwebtoken::errors::Error),

    /// Verification of the JWT header failed.
    InvalidHeader(jsonwebtoken::errors::Error),

    /// A key is not available from the JWK set to verify the JWT.
    NoKeyAvailable,

    /// The algorithm required to verify the JWT is not supported.
    UnsupportedAlgorithm,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidJwk(e) => write!(f, "jwk was not valid: {}", e),
            Error::InvalidJwt(e) => write!(f, "jwt was not valid: {}", e),
            Error::InvalidHeader(e) => write!(f, "jwt header is not valid: {}", e),
            Error::NoKeyAvailable => write!(f, "no key was available to validate the jwt"),
            Error::UnsupportedAlgorithm => write!(f, "algorithm required by JWT/JWK not supported"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::InvalidJwk(e) | Error::InvalidJwt(e) | Error::InvalidHeader(e) => Some(e),
            _ => None,
        }
    }
}

/// A JWT that has already been verified. Can be used to cheaply verify the same token if used
/// again.
#[derive(Clone)]
pub struct VerifiedToken {
    jwt: Arc<String>,
    expire_secs: Option<u64>,
    reverify_secs: u64,
    claim: Arc<NcaClaim>,
}

impl ovdc_sharded_map::Weight for VerifiedToken {
    #[inline]
    fn weight(&self) -> u64 {
        (self.claim.nid.len() + self.jwt.len()) as u64
    }
}

impl VerifiedToken {
    /// Check if this token is the same as another.
    ///
    /// If the input string begins with `Bearer `, the prefix is ignored.
    pub fn is_same_token(&self, token: &str) -> bool {
        if self.jwt.as_ref() == token {
            return true;
        }

        let jwt_stripped = self
            .jwt
            .as_ref()
            .strip_prefix(BEARER_PREFIX)
            .unwrap_or(&self.jwt);
        let token_stripped = token.strip_prefix(BEARER_PREFIX).unwrap_or(token);

        jwt_stripped == token_stripped
    }

    /// Returns false if the JWT must be re-verified.
    pub fn is_valid(&self) -> bool {
        self.reverify_secs > now()
    }

    /// Returns true if the JWT is expired.
    pub fn is_expired(&self) -> bool {
        self.expire_secs.map(|y| y <= now()).unwrap_or_default()
    }

    /// The NCA ID of the token.
    pub fn nca_id(&self) -> &str {
        &self.claim.nid
    }
}

/// Used to verify Json Web Tokens (JWT) and source the NCA account ID from the token.
///
/// This NCA ID can be used as a verified tenant.
pub struct Verify {
    set: Arc<parking_lot::RwLock<Option<Arc<JwkSet>>>>,
    task: JoinHandle<()>,
    map: ovdc_sharded_map::ShardedMap<String, VerifiedToken>,

    aud: Option<Vec<String>>,
    require_exp: bool,
}

impl Drop for Verify {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Verify {
    /// Creates a new [Verify].
    ///
    /// Arg `update` is called to obtain the first JWK set.
    /// However if it fails, an empty set is used and the error is ignored.
    pub async fn new(
        update: jwk::BoxUpdateJwk,
        map_size_max: u64,
        interval: Duration,
        aud: Option<Vec<String>>,
        require_exp: bool,
    ) -> Self {
        let jwk_set = ovdc_trace::err!(
            update.update().await,
            "failed to get jwk set, will retry in background"
        )
        .ok()
        .map(Arc::new);

        let is_setup = jwk_set.is_none();
        let mut last = is_setup.then(Instant::now);

        let set: Arc<parking_lot::RwLock<Option<Arc<JwkSet>>>> =
            Arc::new(parking_lot::RwLock::new(jwk_set));

        let bg_set = set.clone();
        let task = tokio::spawn(async move {
            let mut failures = is_setup.then_some(1);

            loop {
                let sleep = failures
                    .map(|count| Duration::from_millis(2u64.pow(count)))
                    .unwrap_or(interval);
                tokio::time::sleep(sleep).await;

                if last.map(|y| y.elapsed().as_secs()).unwrap_or_default() >= 86400 {
                    panic!("24h has passed since jwk has been updated");
                }

                let jwk_set = match update.update().await {
                    Ok(jwk_set) => jwk_set,
                    Err(error_message) => {
                        tracing::error!(%error_message, "failed to update JWK set");

                        failures = failures
                            .map(|y| {
                                if y == 20 {
                                    return 1;
                                }
                                y + 1
                            })
                            .or(Some(1));
                        continue;
                    }
                };

                tracing::info!("jwk set updated");
                *bg_set.write() = Some(Arc::new(jwk_set));
                last = Some(Instant::now());
                failures = None;
            }
        });

        Self {
            set,
            task,
            map: ovdc_sharded_map::ShardedMap::new(map_size_max, None),
            aud,
            require_exp,
        }
    }

    /// Verify and cache the given JWT.
    ///
    /// If the JWT begins with `Bearer `, the prefix is ignored.
    ///
    /// For the given period of time (to the nearest second) if another JWT is received that is bit
    /// equivalent, the JWT will not be decoded/checked. The previous attestation of the JWT will allow
    /// future uses the same JWT to skip crypto and instead only perform an equivalency check.
    pub fn verify(&self, mut token: &str) -> Result<VerifiedToken, Error> {
        if let Some(stripped) = token.trim().strip_prefix(BEARER_PREFIX) {
            token = stripped;
        }

        if let Some(cached) = self.map.get(token) {
            if cached.is_valid() {
                return Ok(cached.clone());
            }
        }

        let (token_first_2, token_last_2) = first_two_last_two(token);

        let header = jsonwebtoken::decode_header(token).map_err(|e| {
            let err_str = convert_err_kind(e.kind());
            tracing::error!(
                token_first_2,
                token_last_2,
                "failed to decode hader: {}",
                err_str
            );
            Error::InvalidHeader(e)
        })?;

        let set = self.set.read().clone();
        let jwk = set.clone().ok_or_else(|| {
            tracing::error!("cannot verify jwt: no JWK set present");
            Error::NoKeyAvailable
        })?;

        let jwk = jwk
            .keys
            .iter()
            .find(|y| y.common.key_id.as_ref() == header.kid.as_ref())
            .ok_or_else(|| {
                tracing::error!(
                    token_first_2,
                    token_last_2,
                    kid = header.kid,
                    "jwk set does not contain requested key id"
                );
                Error::NoKeyAvailable
            })?;

        let algo = match jwk.common.key_algorithm {
            Some(jsonwebtoken::jwk::KeyAlgorithm::HS256) => Algorithm::HS256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::HS384) => Algorithm::HS384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::HS512) => Algorithm::HS512,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES256) => Algorithm::ES256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::ES384) => Algorithm::ES384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS256) => Algorithm::RS256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS384) => Algorithm::RS384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::RS512) => Algorithm::RS512,
            Some(jsonwebtoken::jwk::KeyAlgorithm::PS256) => Algorithm::PS256,
            Some(jsonwebtoken::jwk::KeyAlgorithm::PS384) => Algorithm::PS384,
            Some(jsonwebtoken::jwk::KeyAlgorithm::PS512) => Algorithm::PS512,
            Some(jsonwebtoken::jwk::KeyAlgorithm::EdDSA) => Algorithm::EdDSA,
            _ => {
                tracing::error!(
                    token_first_2,
                    token_last_2,
                    alg = ?header.alg,
                    "jwt requested use of jwk with unsupported algorithm"
                );
                return Err(Error::UnsupportedAlgorithm);
            }
        };

        let key = DecodingKey::from_jwk(jwk).map_err(Error::InvalidJwk)?;

        let mut validation = Validation::new(algo);
        validation.validate_aud = false;

        if let Some(aud) = &self.aud {
            validation.validate_aud = true;
            validation.set_audience(aud);
        }

        validation.validate_exp = self.require_exp;
        if !self.require_exp {
            validation.required_spec_claims.remove("exp");
        }

        let claims: jsonwebtoken::TokenData<Claims> =
            jsonwebtoken::decode(token, &key, &validation).map_err(|e| {
                let err_str = convert_err_kind(e.kind());
                tracing::error!(
                    token_first_2,
                    token_last_2,
                    "failed to verify jwt: {}",
                    err_str
                );
                Error::InvalidJwt(e)
            })?;

        tracing::info!(
            nid = { &claims.claims.assertion.nid },
            fid = { &claims.claims.assertion.function_id },
            fver = { &claims.claims.assertion.function_version },
            "token verified"
        );

        let claim = Arc::new(claims.claims.assertion);

        let cached = VerifiedToken {
            jwt: Arc::new(token.to_owned()),
            claim: claim.clone(),
            expire_secs: claims.claims.exp,
            // Reverify this token every 15 minutes
            reverify_secs: now() + 60 * 15,
        };

        self.map.insert(token.to_owned(), cached.clone());

        Ok(cached)
    }
}

fn first_two_last_two(token: &str) -> (&str, &str) {
    if token.len() < 4 || !token.is_ascii() {
        return ("", "");
    }

    (&token[..2], &token[token.len() - 2..])
}

fn convert_err_kind(kind: &jsonwebtoken::errors::ErrorKind) -> &'static str {
    match kind {
        jsonwebtoken::errors::ErrorKind::InvalidToken => "token is cannot be read",
        jsonwebtoken::errors::ErrorKind::InvalidSignature => "token signature doesn't match",
        jsonwebtoken::errors::ErrorKind::InvalidEcdsaKey => "secret is an invalid ECDSA key",
        jsonwebtoken::errors::ErrorKind::InvalidRsaKey(_) => "secret is an invalid RSA key",
        jsonwebtoken::errors::ErrorKind::RsaFailedSigning => "RSA signing failed",
        jsonwebtoken::errors::ErrorKind::InvalidAlgorithmName => "algorithm name is invalid",
        jsonwebtoken::errors::ErrorKind::InvalidKeyFormat => {
            "a key was provided wiht an invalid format"
        }
        jsonwebtoken::errors::ErrorKind::MissingRequiredClaim(_) => "jwt is missing required claim",
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => "`exp` claim is expired",
        jsonwebtoken::errors::ErrorKind::InvalidIssuer => {
            "`iss` claim does not match expected value"
        }
        jsonwebtoken::errors::ErrorKind::InvalidAudience => {
            "`aud` claim does not match expected value"
        }
        jsonwebtoken::errors::ErrorKind::InvalidSubject => {
            "`sub` claim does not match expected value"
        }
        jsonwebtoken::errors::ErrorKind::ImmatureSignature => "`nbf` future time",
        jsonwebtoken::errors::ErrorKind::InvalidAlgorithm => "algorithm mismatch",
        jsonwebtoken::errors::ErrorKind::MissingAlgorithm => "no algorithm presented",
        jsonwebtoken::errors::ErrorKind::Base64(_) => "invalid base64 supplied",
        jsonwebtoken::errors::ErrorKind::Json(_) => {
            "failed to deserialize JSON, maybe missing nid claim"
        }
        jsonwebtoken::errors::ErrorKind::Utf8(_) => "jwt did not contain fully vaild UTF8",
        _ => "unspecified",
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct NcaClaim {
    #[serde(rename = "ncaId")]
    pub nid: String,

    #[serde(rename = "functionId")]
    pub function_id: String,

    #[serde(rename = "functionVersionId")]
    pub function_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    assertion: NcaClaim,
    exp: Option<u64>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs()
}

/// A synthetic debug token accepted by [`jwk::debug_jwk`].
///
/// Regenerate these fixtures with `cargo run -p ovdc_jwt --example debug_tokens`.
/// Signatures may change between runs; the payloads use only fictional identities.
/// These tokens and their published signing key are for debugging/testing only.
///
/// ```json
/// {
///   "iss": "https://issuer.example.com",
///   "sub": "example-service",
///   "aud": "ovdc-test",
///   "assertion": {
///     "ncaId": "example-tenant-a",
///     "functionId": "example-function",
///     "functionVersionId": "example-version"
///   },
///   "iat": 1700000000,
///   "jti": "example-token-example-tenant-a"
/// }
/// ```
pub const _DEBUG_JWT_0: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiIsImtpZCI6ImRlYnVnIn0.eyJhc3NlcnRpb24iOnsiZnVuY3Rpb25JZCI6ImV4YW1wbGUtZnVuY3Rpb24iLCJmdW5jdGlvblZlcnNpb25JZCI6ImV4YW1wbGUtdmVyc2lvbiIsIm5jYUlkIjoiZXhhbXBsZS10ZW5hbnQtYSJ9LCJhdWQiOiJvdmRjLXRlc3QiLCJpYXQiOjE3MDAwMDAwMDAsImlzcyI6Imh0dHBzOi8vaXNzdWVyLmV4YW1wbGUuY29tIiwianRpIjoiZXhhbXBsZS10b2tlbi1leGFtcGxlLXRlbmFudC1hIiwic3ViIjoiZXhhbXBsZS1zZXJ2aWNlIn0.N309wl6_xmzlXCTBJHqj61lUvg1rSRI2DMQBug3yZvLZiUKpWXtYj3IiUuKEk3VQtUyiXQuQ8qC-oaYOmZhcbg";

/// A synthetic debug token for a second tenant, accepted by [`jwk::debug_jwk`].
pub const _DEBUG_JWT_1: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiIsImtpZCI6ImRlYnVnIn0.eyJhc3NlcnRpb24iOnsiZnVuY3Rpb25JZCI6ImV4YW1wbGUtZnVuY3Rpb24iLCJmdW5jdGlvblZlcnNpb25JZCI6ImV4YW1wbGUtdmVyc2lvbiIsIm5jYUlkIjoiZXhhbXBsZS10ZW5hbnQtYiJ9LCJhdWQiOiJvdmRjLXRlc3QiLCJpYXQiOjE3MDAwMDAwMDAsImlzcyI6Imh0dHBzOi8vaXNzdWVyLmV4YW1wbGUuY29tIiwianRpIjoiZXhhbXBsZS10b2tlbi1leGFtcGxlLXRlbmFudC1iIiwic3ViIjoiZXhhbXBsZS1zZXJ2aWNlIn0.KgERrd3H7XudIQmVtr0HyuYsbm-4wUrJDDlcUjiWsx1mZA_PCYKwn02zYEuCkBz4XPJraoBkISjrf1Qtv1wmCg";

/// A signed synthetic debug token missing the required `assertion.ncaId` claim.
pub const _DEBUG_BAD_JWT_MISSING_NID: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiIsImtpZCI6ImRlYnVnIn0.eyJhc3NlcnRpb24iOnsiZnVuY3Rpb25JZCI6ImV4YW1wbGUtZnVuY3Rpb24iLCJmdW5jdGlvblZlcnNpb25JZCI6ImV4YW1wbGUtdmVyc2lvbiJ9LCJhdWQiOiJvdmRjLXRlc3QiLCJleHAiOjQxMDI0NDQ4MDAsImlhdCI6MTcwMDAwMDAwMCwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZS5jb20iLCJqdGkiOiJleGFtcGxlLXRva2VuLWV4YW1wbGUtdGVuYW50LWEiLCJzdWIiOiJleGFtcGxlLXNlcnZpY2UifQ.SScM1YVLF6TCEX0X6UDmNIa61hP_q0tF88FbSciJronccFDg8BUj03Lj85CyvlmXyEujljovWg_-EOQJbKg5Hw";

/// A signed synthetic debug token with complete claims and an expired `exp` claim.
pub const _DEBUG_BAD_JWT_EXPIRED: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiIsImtpZCI6ImRlYnVnIn0.eyJhc3NlcnRpb24iOnsiZnVuY3Rpb25JZCI6ImV4YW1wbGUtZnVuY3Rpb24iLCJmdW5jdGlvblZlcnNpb25JZCI6ImV4YW1wbGUtdmVyc2lvbiIsIm5jYUlkIjoiZXhhbXBsZS10ZW5hbnQtYSJ9LCJhdWQiOiJvdmRjLXRlc3QiLCJleHAiOjE3MDAwMDAwMDAsImlhdCI6MTcwMDAwMDAwMCwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZS5jb20iLCJqdGkiOiJleGFtcGxlLXRva2VuLWV4YW1wbGUtdGVuYW50LWEiLCJzdWIiOiJleGFtcGxlLXNlcnZpY2UifQ.l85BeFF7QKaQ0hO2DdZE_2aMpSCbVm9X4Wk0Yz_QPM1opH01Ecp_aKOybfjOyq4pBJ2XFhgl5vr7oiPswyCpMw";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_verify_debug_jwk() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            None,
            false,
        )
        .await;

        let verified = verify.verify(_DEBUG_JWT_0).unwrap();
        verify.verify(_DEBUG_JWT_0).unwrap();

        assert_eq!(verified.nca_id(), "example-tenant-a");
    }

    #[tokio::test]
    async fn test_missing_nid() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            None,
            false,
        )
        .await;
        assert!(verify.verify(_DEBUG_BAD_JWT_MISSING_NID).is_err());
    }

    #[tokio::test]
    async fn test_expired() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            None,
            true,
        )
        .await;
        assert!(matches!(
            verify.verify(_DEBUG_BAD_JWT_EXPIRED),
            Err(Error::InvalidJwt(error))
                if matches!(error.kind(), jsonwebtoken::errors::ErrorKind::ExpiredSignature)
        ));
    }

    #[tokio::test]
    async fn test_verify_missing_aud() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            Some(vec!["other-audience".to_owned()]),
            false,
        )
        .await;

        assert!(matches!(
            verify.verify(_DEBUG_JWT_0),
            Err(Error::InvalidJwt(error))
                if matches!(error.kind(), jsonwebtoken::errors::ErrorKind::InvalidAudience)
        ));
    }

    #[tokio::test]
    async fn test_verify_matching_aud() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            Some(vec!["ovdc-test".to_owned()]),
            false,
        )
        .await;
        assert_eq!(
            verify.verify(_DEBUG_JWT_0).unwrap().nca_id(),
            "example-tenant-a"
        );
        assert_eq!(
            verify.verify(_DEBUG_JWT_1).unwrap().nca_id(),
            "example-tenant-b"
        );
    }

    #[tokio::test]
    async fn test_is_expired() {
        let token = VerifiedToken {
            jwt: Arc::new("example".to_owned()),
            expire_secs: Some(now()),
            claim: Arc::new(NcaClaim {
                nid: Default::default(),
                function_id: Default::default(),
                function_version: Default::default(),
            }),
            reverify_secs: 0,
        };

        assert!(token.is_same_token("example"));
        assert!(token.is_same_token("Bearer example"));
        assert!(token.is_expired());
    }

    #[tokio::test]
    async fn test_jwt_is_cached() {
        let verify = Verify::new(
            jwk::debug_jwk(),
            64_000_000,
            Duration::from_secs(10),
            None,
            false,
        )
        .await;

        let v_0 = verify.verify(_DEBUG_JWT_0).unwrap();
        let v_1 = verify.verify(_DEBUG_JWT_0).unwrap();

        assert!(Arc::ptr_eq(&v_0.claim, &v_1.claim));
    }
}
