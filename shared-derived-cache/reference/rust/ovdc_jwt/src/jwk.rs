// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Faculties for getting a JWK set.

use std::{future::Future, pin::Pin, time::Duration};

use jsonwebtoken::jwk::JwkSet;

/// The return type of [`UpdateJwk::update`].
pub type UpdateJwkFuture<'a> = Pin<
    Box<dyn Future<Output = Result<JwkSet, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>,
>;

/// A generic way to fetch a JWK set.
///
/// Allows non HTTP or other methods of getting a JwkSet.
pub trait UpdateJwk {
    /// Returns the JwkSet to use.
    fn update(&self) -> UpdateJwkFuture<'_>;
}

/// [UpdateJwk] as a dynamic heap allocated object.
pub type BoxUpdateJwk = Box<dyn UpdateJwk + Sync + Send + 'static>;

struct UpdateFromCache(Vec<String>);

impl UpdateJwk for UpdateFromCache {
    fn update(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<JwkSet, Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let mut jwk_set = JwkSet { keys: vec![] };
            for url in &self.0 {
                let set = ovdc_trace::err!(
                    get_jwk_set(url).await,
                    "jwk set was not retrieved from {url}"
                )?;
                jwk_set.keys.extend_from_slice(&set.keys);
            }

            Ok(jwk_set)
        })
    }
}

async fn get_jwk_set(url: &str) -> Result<JwkSet, Box<dyn std::error::Error + Send + Sync>> {
    let req_timeout = tokio::time::timeout(Duration::from_secs(5), reqwest::get(url)).await;

    let request_result = ovdc_trace::err!(req_timeout, "request timeout (`{}`)", url)?;
    let raw_jwk_json_request =
        ovdc_trace::err!(request_result, "failed to get jwk set (`{}`)", url)?;

    let raw_jwk_json_bytes = ovdc_trace::err!(
        raw_jwk_json_request.bytes().await,
        "failed to read bytes for jwk set (`{}`)",
        url
    )?;

    let jwk_set: JwkSet = ovdc_trace::err!(
        serde_json::from_slice(&raw_jwk_json_bytes),
        "failed to deserialize the jwk set"
    )?;

    let key_ids: Vec<String> = jwk_set
        .keys
        .iter()
        .filter_map(|y| y.common.key_id.clone())
        .collect();

    tracing::info!(?key_ids, "jwk set retrieved");

    Ok(jwk_set)
}

/// Fetch JWK from the given urls.
pub fn http_get(from: Vec<String>) -> BoxUpdateJwk {
    Box::new(UpdateFromCache(from))
}

/// Use the given JwkSet.
pub fn static_jwk(set: JwkSet) -> BoxUpdateJwk {
    struct StaticJwk(JwkSet);

    impl UpdateJwk for StaticJwk {
        fn update(
            &self,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<JwkSet, Box<dyn std::error::Error + Send + Sync>>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    Box::new(StaticJwk(set))
}

/// Uses a static JwkSet that is only useful for debugging.
pub fn debug_jwk() -> BoxUpdateJwk {
    static_jwk(serde_json::from_str(DEBUG_KEYS).unwrap())
}

/// EC256 Key Pair for debugging and testing ONLY
///
/// -----BEGIN EC PRIVATE KEY-----
/// MHcCAQEEIOFLrpDjnc7qxsakujfyMsJ6KoRtZEvPkIbaEfoLYkkloAoGCCqGSM49
/// AwEHoUQDQgAEa3svaDF09Lj+1tuJA+s8mK4DZfhdGfRuvbNiB9BsezXSLBrFYHH3
/// AJsHc9AZT0soyfGG1Srvfbu3q2LrFFSUYQ==
/// -----END EC PRIVATE KEY-----
///
/// -----BEGIN PUBLIC KEY-----
/// MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEa3svaDF09Lj+1tuJA+s8mK4DZfhd
/// GfRuvbNiB9BsezXSLBrFYHH3AJsHc9AZT0soyfGG1Srvfbu3q2LrFFSUYQ==
/// -----END PUBLIC KEY-----
const DEBUG_KEYS: &str = r#"
{
    "keys": [
        {
          "kid": "debug",
          "kty": "EC",
          "crv": "P-256",
          "alg": "ES256",
          "x": "a3svaDF09Lj-1tuJA-s8mK4DZfhdGfRuvbNiB9BsezU",
          "y": "0iwaxWBx9wCbB3PQGU9LKMnxhtUq7327t6ti6xRUlGE",
          "d": "4UuukOOdzurGxqS6N_IywnoqhG1kS8-QhtoR-gtiSSU"
        }
    ]
}
"#;
