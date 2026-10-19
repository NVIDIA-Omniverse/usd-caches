// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovdc_bytes::keys;

use crate::count_connections::ConnectionState;

fn str_to_tenant(tenant_str: &str) -> Option<keys::Tenant> {
    if tenant_str.len() > keys::TENANT_KEY_SIZE {
        return None;
    }

    let mut key = [0u8; keys::TENANT_KEY_SIZE];
    key[..tenant_str.len()].copy_from_slice(tenant_str.as_bytes());

    Some(keys::Tenant::new(key))
}

enum TokenVerification {
    None,
    Verified(keys::Tenant),
}

#[allow(clippy::obfuscated_if_else)]
fn verify_token(
    jwk: &ovdc_jwt::Verify,
    request: &mut tonic::Request<()>,
) -> Result<TokenVerification, tonic::Status> {
    let metadata = request
        .metadata()
        .get("authorization")
        .or_else(|| request.metadata().get("jwt"));
    let Some(auth_header) = metadata else {
        return Ok(TokenVerification::None);
    };

    let auth_header = match auth_header.to_str() {
        Ok(x) => x,
        _ => {
            return Err(tonic::Status::invalid_argument(
                "authorization: not a utf8 string",
            ))
        }
    };

    let connection_state = request.extensions().get::<ConnectionState>().unwrap();

    if let Some(tenant) = connection_state
        .with_verififed_token(|token| {
            (token.is_same_token(auth_header) && token.is_valid() && !token.is_expired())
                .then(|| str_to_tenant(token.nca_id()))
                .unwrap_or_default()
        })
        .unwrap_or_default()
    {
        return Ok(TokenVerification::Verified(tenant));
    }

    let token = match jwk.verify(auth_header) {
        Ok(x) => x.to_owned(),
        Err(err) => {
            return Err(tonic::Status::invalid_argument(format!(
                "authorization: {err}"
            )))
        }
    };

    connection_state.try_set_last_verified_token(|| token.to_owned());

    let Some(tenant) = str_to_tenant(token.nca_id()) else {
        return Err(tonic::Status::invalid_argument(
            "authorization: invalid length of tenant",
        ));
    };

    Ok(TokenVerification::Verified(tenant))
}

pub fn handle(
    jwk: &ovdc_jwt::Verify,
    require_jwt: bool,
    request: &mut tonic::Request<()>,
) -> Result<(), tonic::Status> {
    match verify_token(jwk, request)? {
        TokenVerification::None if !require_jwt => return Ok(()),
        TokenVerification::None => {
            return Err(tonic::Status::permission_denied("no jwt"));
        }
        TokenVerification::Verified(tenant) => {
            request.extensions_mut().insert(tenant);
        }
    }

    Ok(())
}
