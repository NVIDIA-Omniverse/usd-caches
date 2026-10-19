// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use ovcc_cachekey::{KeyPart, LogicalKey};

use super::*;

/// Contract identifier a peer must echo before an optional-key hit is trusted.
pub const OPTIONAL_KEY_CONTRACT: &str = "ovcc.optional-key-parts.v1";
const MAX_PART_BYTES: usize = 8 * 1024;

/// Unknown contracts are a capability outcome, not a transport failure.
pub enum OptionalRequest {
    Legacy,
    Unsupported,
    Supported(LogicalKey),
}

/// The absence of acknowledgement never exposes a legacy COMMITTED response as
/// an optional hit. Callers may retry an independently valid legacy identity.
pub enum OptionalProbeResponse {
    Unsupported,
    Supported {
        response: ProbeResponse,
        omitted_parts: Vec<String>,
    },
}

fn validate_parts(logical: &LogicalKey) -> Result<()> {
    logical
        .validate()
        .map_err(|_| invalid_header("optional_keys", "invalid logical key"))?;
    let mut total = logical.policy().len();
    for part in logical.parts() {
        // These fields have separate ownership. The caller's policy must also
        // whitelist its descriptors; a wire declaration never grants permission
        // to omit authorization or core identity fields.
        if matches!(
            part.name.to_ascii_lowercase().as_str(),
            "generation" | "authorization" | "cookie" | "sig" | "signature" | "token"
        ) {
            return Err(invalid_header("optional_keys", "forbidden descriptor"));
        }
        let value = part.value.as_deref().unwrap_or("");
        validate_bounded("optional_keys.value", value, 0, MAX_PART_BYTES)?;
        total += part.name.len() + value.len();
    }
    if total > MAX_IDENTITY_BYTES {
        return Err(invalid_header(
            "optional_keys",
            "aggregate descriptor limit exceeded",
        ));
    }
    Ok(())
}

/// Validates the wire envelope, then invokes the server-owned policy. The policy
/// must validate the descriptor schema and consistency with the base identity,
/// and insert the server's own cache generation when rendering a storage key.
pub fn validate_optional_request(
    request: &proto::WhoHasRequest,
    accepts: impl FnOnce(&ObjectIdentity, &LogicalKey) -> bool,
) -> Result<OptionalRequest> {
    // Applies the existing validator and base-identity rules to both modes.
    validate_who_has_request(request.clone())?;
    let Some(extension) = &request.optional_keys else {
        return Ok(OptionalRequest::Legacy);
    };
    if extension.contract != OPTIONAL_KEY_CONTRACT {
        return Ok(OptionalRequest::Unsupported);
    }
    if extension.parts.len() > 64 {
        return Err(invalid_header("optional_keys", "too many descriptors"));
    }
    let identity: ObjectIdentity = request
        .identity
        .clone()
        .ok_or_else(|| invalid_header("identity", "missing"))?
        .try_into()?;
    let logical = LogicalKey::new(
        extension.policy.clone(),
        extension
            .parts
            .iter()
            .map(|p| KeyPart {
                name: p.name.clone(),
                value: p.value.clone(),
                optional: p.optional,
            })
            .collect(),
    )
    .map_err(|_| invalid_header("optional_keys", "invalid logical key"))?;
    validate_parts(&logical)?;
    if !accepts(&identity, &logical) {
        return Ok(OptionalRequest::Unsupported);
    }
    Ok(OptionalRequest::Supported(logical))
}

/// Resolves an optional request through server-owned local policy and lookup.
/// `None` delegates to the existing exact-only handler. Unsupported contracts
/// return ABSENT without acknowledgement; the lookup closure is never called.
/// Implementations of `lookup` must remain side-effect-free and return omissions
/// from the actual storage match, not from requester assertions.
pub fn optional_who_has_response(
    request: &proto::WhoHasRequest,
    accepts: impl FnOnce(&ObjectIdentity, &LogicalKey) -> bool,
    lookup: impl FnOnce(
        &ObjectIdentity,
        &LogicalKey,
        &ExactValidator,
    ) -> Result<(proto::WhoHasResponse, Vec<String>)>,
) -> Result<Option<proto::WhoHasResponse>> {
    match validate_optional_request(request, accepts)? {
        OptionalRequest::Legacy => Ok(None),
        OptionalRequest::Unsupported => Ok(Some(proto::WhoHasResponse {
            object_status: proto::ObjectStatus::Absent as i32,
            ..Default::default()
        })),
        OptionalRequest::Supported(logical) => {
            let (identity, validator) = validate_who_has_request(request.clone())?;
            let (mut response, omissions) = lookup(&identity, &logical, &validator)?;
            response.optional_keys_ack = OPTIONAL_KEY_CONTRACT.into();
            response.omitted_parts = omissions;
            interpret_response(
                SocketAddr::from(([0, 0, 0, 0], 0)),
                &validator,
                &logical,
                response.clone(),
            )?;
            Ok(Some(response))
        }
    }
}

/// Carries the logical key parts alongside an ordinary probe request.
pub fn optional_who_has_request(
    identity: &ObjectIdentity,
    validator: &ExactValidator,
    logical: &LogicalKey,
) -> Result<proto::WhoHasRequest> {
    validate_parts(logical)?;
    let mut request = who_has_request(identity, validator)?;
    request.optional_keys = Some(proto::OptionalKeyIdentity {
        contract: OPTIONAL_KEY_CONTRACT.into(),
        policy: logical.policy().into(),
        parts: logical
            .parts()
            .iter()
            .map(|p| proto::OptionalKeyPart {
                name: p.name.clone(),
                value: p.value.clone(),
                optional: p.optional,
            })
            .collect(),
    });
    Ok(request)
}

fn interpret_response(
    peer: SocketAddr,
    validator: &ExactValidator,
    logical: &LogicalKey,
    response: proto::WhoHasResponse,
) -> Result<OptionalProbeResponse> {
    if response.optional_keys_ack != OPTIONAL_KEY_CONTRACT {
        return Ok(OptionalProbeResponse::Unsupported);
    }
    if response.omitted_parts.len() > logical.parts().len() {
        return Err(invalid_header("omitted_parts", "too many omissions"));
    }
    let mut seen = HashSet::new();
    for omitted in &response.omitted_parts {
        if !seen.insert(omitted)
            || !logical
                .parts()
                .iter()
                .any(|p| p.optional && p.name == *omitted)
        {
            return Err(invalid_header("omitted_parts", "invalid omission"));
        }
    }
    let object_status = proto::ObjectStatus::try_from(response.object_status)
        .unwrap_or(proto::ObjectStatus::Unspecified);
    let response_validator = response
        .validator
        .map(ExactValidator::try_from)
        .transpose()?;
    if object_status == proto::ObjectStatus::Committed
        && (response_validator.as_ref() != Some(validator)
            || !matches!(response.http_status, 200 | 206))
    {
        return Err(invalid_header(
            "optional_keys",
            "committed response lacks matching validator/status",
        ));
    }
    Ok(OptionalProbeResponse::Supported {
        response: ProbeResponse {
            peer,
            object_status,
            http_status: response.http_status,
            object_size: response.object_size,
            validator: response_validator,
            max_idle_secs: response.max_idle_seconds,
        },
        omitted_parts: response.omitted_parts,
    })
}

impl PeerClient {
    /// Uses the same retained channel and deadline as legacy WhoHas. Capability
    /// absence is returned normally and must not enter the peer failure cache.
    pub async fn probe_optional(
        &self,
        peer: SocketAddr,
        identity: &ObjectIdentity,
        validator: &ExactValidator,
        logical: &LogicalKey,
    ) -> Result<OptionalProbeResponse> {
        let request = optional_who_has_request(identity, validator, logical)?;
        timeout(self.options.probe_timeout, async {
            let channel = self.grpc_channel(peer).await?;
            let response = proto::peer_control_client::PeerControlClient::new(channel)
                .who_has(request)
                .await
                .map_err(|error| PeerError::Http(format!("WhoHas RPC to {peer}: {error}")))?
                .into_inner();
            interpret_response(peer, validator, logical, response)
        })
        .await
        .map_err(|_| PeerError::Timeout(TimeoutStage::Probe))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    // Frozen 0.1.7 message fields, from 56fad41:crates/ovcc-peer/proto/peer.proto.
    // Nested messages are frozen too, so changing a base field's number cannot
    // silently change both sides of this compatibility test.
    #[derive(Clone, PartialEq, Message)]
    struct OldIdentity {
        #[prost(string, tag = "1")]
        host: String,
        #[prost(string, tag = "2")]
        method: String,
        #[prost(string, tag = "3")]
        uri: String,
        #[prost(string, tag = "4")]
        filtered_query: String,
        #[prost(string, tag = "5")]
        if_match: String,
        #[prost(string, tag = "6")]
        range: String,
    }

    impl From<&ObjectIdentity> for OldIdentity {
        fn from(identity: &ObjectIdentity) -> Self {
            Self {
                host: identity.host.clone(),
                method: identity.method.to_string(),
                uri: identity.uri.clone(),
                filtered_query: identity.args.clone(),
                if_match: identity.if_match.clone(),
                range: identity.range.clone(),
            }
        }
    }

    #[derive(Clone, PartialEq, prost::Oneof)]
    enum OldValidatorValue {
        #[prost(string, tag = "1")]
        Etag(String),
        #[prost(string, tag = "2")]
        LastModified(String),
    }

    #[derive(Clone, PartialEq, Message)]
    struct OldValidator {
        #[prost(oneof = "OldValidatorValue", tags = "1, 2")]
        value: Option<OldValidatorValue>,
    }

    impl From<&ExactValidator> for OldValidator {
        fn from(validator: &ExactValidator) -> Self {
            Self {
                value: Some(match validator {
                    ExactValidator::Etag(value) => OldValidatorValue::Etag(value.clone()),
                    ExactValidator::LastModified(value) => {
                        OldValidatorValue::LastModified(value.clone())
                    }
                }),
            }
        }
    }

    #[derive(Clone, PartialEq, Message)]
    struct OldRequest {
        #[prost(message, optional, tag = "1")]
        identity: Option<OldIdentity>,
        #[prost(message, optional, tag = "2")]
        validator: Option<OldValidator>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct OldResponse {
        #[prost(int32, tag = "1")]
        object_status: i32,
        #[prost(uint32, tag = "2")]
        http_status: u32,
        #[prost(uint64, tag = "3")]
        object_size: u64,
        #[prost(message, optional, tag = "4")]
        validator: Option<OldValidator>,
        #[prost(uint64, optional, tag = "5")]
        max_idle_seconds: Option<u64>,
    }

    fn identity() -> ObjectIdentity {
        ObjectIdentity {
            host: "test.local".into(),
            method: Method::GET,
            uri: "/object".into(),
            args: String::new(),
            if_match: "\"v1\"".into(),
            range: String::new(),
        }
    }

    fn logical() -> std::result::Result<LogicalKey, ovcc_cachekey::LogicalKeyError> {
        LogicalKey::new(
            "test.v1".into(),
            vec![
                KeyPart {
                    name: "representation".into(),
                    value: Some("A".into()),
                    optional: true,
                },
                KeyPart {
                    name: "tenant".into(),
                    value: Some("one".into()),
                    optional: false,
                },
            ],
        )
    }

    fn validator() -> ExactValidator {
        ExactValidator::Etag("\"v1\"".into())
    }
    fn peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 14130))
    }
    fn committed() -> proto::WhoHasResponse {
        proto::WhoHasResponse {
            object_status: proto::ObjectStatus::Committed as i32,
            http_status: 200,
            object_size: 5,
            validator: Some((&validator()).into()),
            max_idle_seconds: Some(60),
            ..Default::default()
        }
    }

    #[test]
    fn general_grpc_optional_keys_old_to_old_and_old_to_new_stay_exact() -> TestResult {
        // Arrange.
        let mut complete_identity = identity();
        complete_identity.args = "query=value".into();
        complete_identity.range = "bytes=0-4".into();
        let old = OldRequest {
            identity: Some((&complete_identity).into()),
            validator: Some((&validator()).into()),
        };
        // Act.
        let old_roundtrip = OldRequest::decode(old.encode_to_vec().as_slice())?;
        let new = proto::WhoHasRequest::decode(old.encode_to_vec().as_slice())?;
        let reply = optional_who_has_response(
            &new,
            |_, _| panic!("legacy request reached optional policy"),
            |_, _, _| panic!("legacy request reached optional lookup"),
        )?;
        // Assert.
        assert_eq!(old, old_roundtrip);
        assert_eq!(validate_who_has_request(new.clone())?.0, complete_identity);
        assert!(reply.is_none());
        assert!(matches!(
            validate_optional_request(&new, |_, _| false)?,
            OptionalRequest::Legacy
        ));
        let legacy_response = OldResponse::decode(committed().encode_to_vec().as_slice())?;
        assert_eq!(
            legacy_response.object_status,
            proto::ObjectStatus::Committed as i32
        );
        assert_eq!(legacy_response.max_idle_seconds, Some(60));
        Ok(())
    }

    #[test]
    fn general_grpc_optional_keys_new_to_old_ignored_fields_cannot_hit() -> TestResult {
        // Arrange.
        let key = logical()?;
        let new = optional_who_has_request(&identity(), &validator(), &key)?;
        // Act.
        let old = OldRequest::decode(new.encode_to_vec().as_slice())?;
        let old_reply = OldResponse {
            object_status: proto::ObjectStatus::Committed as i32,
            http_status: 200,
            object_size: 5,
            validator: old.validator.clone(),
            max_idle_seconds: Some(60),
        };
        let decoded = proto::WhoHasResponse::decode(old_reply.encode_to_vec().as_slice())?;
        let outcome = interpret_response(peer(), &validator(), &key, decoded)?;
        // Assert.
        assert_eq!(
            old.identity.map(|v| v.encode_to_vec()),
            new.identity.map(|v| v.encode_to_vec())
        );
        assert_eq!(
            old.validator.map(|v| v.encode_to_vec()),
            new.validator.map(|v| v.encode_to_vec())
        );
        assert!(matches!(outcome, OptionalProbeResponse::Unsupported));
        Ok(())
    }

    #[test]
    fn general_grpc_optional_keys_new_to_new_requires_policy_and_ack() -> TestResult {
        // Arrange.
        let key = logical()?;
        let request = optional_who_has_request(&identity(), &validator(), &key)?;
        // Act.
        let wire = proto::WhoHasRequest::decode(request.encode_to_vec().as_slice())?;
        let reply = optional_who_has_response(
            &wire,
            |base, logical| base == &identity() && logical.policy() == "test.v1",
            |base, logical, exact| {
                assert_eq!(base, &identity());
                assert!(
                    logical
                        .parts()
                        .iter()
                        .any(|p| p.name == "tenant" && !p.optional)
                );
                assert_eq!(exact, &validator());
                Ok((committed(), vec!["representation".into()]))
            },
        )?
        .ok_or("expected optional response")?;
        let wire_reply = proto::WhoHasResponse::decode(reply.encode_to_vec().as_slice())?;
        let outcome = interpret_response(peer(), &validator(), &key, wire_reply)?;
        // Assert.
        let OptionalProbeResponse::Supported {
            response,
            omitted_parts,
        } = outcome
        else {
            return Err("acknowledged contract was not accepted".into());
        };
        assert_eq!(response.object_status, proto::ObjectStatus::Committed);
        assert_eq!(omitted_parts, ["representation"]);
        Ok(())
    }

    #[test]
    fn general_grpc_optional_keys_disabled_and_unknown_contract_do_not_lookup() -> TestResult {
        // Arrange.
        let key = logical()?;
        let mut request = optional_who_has_request(&identity(), &validator(), &key)?;
        // Act.
        let disabled = optional_who_has_response(
            &request,
            |_, _| false,
            |_, _, _| panic!("disabled policy performed a lookup"),
        )?
        .ok_or("missing disabled response")?;
        request
            .optional_keys
            .as_mut()
            .ok_or("missing extension")?
            .contract = "future.v99".into();
        let unknown = optional_who_has_response(
            &request,
            |_, _| panic!("unknown contract reached policy"),
            |_, _, _| panic!("unknown contract performed a lookup"),
        )?
        .ok_or("missing unknown response")?;
        // Assert.
        for response in [disabled, unknown] {
            assert!(response.optional_keys_ack.is_empty());
            assert!(matches!(
                interpret_response(peer(), &validator(), &key, response)?,
                OptionalProbeResponse::Unsupported
            ));
        }
        Ok(())
    }

    #[test]
    fn general_grpc_optional_keys_rejects_duplicates_required_omissions_and_bad_validator()
    -> TestResult {
        // Arrange.
        let key = logical()?;
        let mut duplicate = optional_who_has_request(&identity(), &validator(), &key)?;
        let extension = duplicate
            .optional_keys
            .as_mut()
            .ok_or("missing extension")?;
        extension.parts.push(extension.parts[0].clone());
        let mut bad_omission = committed();
        bad_omission.optional_keys_ack = OPTIONAL_KEY_CONTRACT.into();
        bad_omission.omitted_parts = vec!["tenant".into()];
        let mut bad_validator = committed();
        bad_validator.optional_keys_ack = OPTIONAL_KEY_CONTRACT.into();
        bad_validator.validator = Some((&ExactValidator::Etag("different".into())).into());
        // Act.
        let duplicate_result = validate_optional_request(&duplicate, |_, _| true);
        let omission_result = interpret_response(peer(), &validator(), &key, bad_omission);
        let validator_result = interpret_response(peer(), &validator(), &key, bad_validator);
        // Assert.
        assert!(duplicate_result.is_err());
        assert!(omission_result.is_err());
        assert!(validator_result.is_err());
        Ok(())
    }

    #[test]
    fn general_grpc_optional_keys_preserves_absent_vs_empty_and_bounds() -> TestResult {
        // Arrange.
        for value in [None, Some("")] {
            let key = LogicalKey::new(
                "test.v1".into(),
                vec![KeyPart {
                    name: "representation".into(),
                    value: value.map(str::to_owned),
                    optional: true,
                }],
            )?;
            // Act.
            let request = optional_who_has_request(&identity(), &validator(), &key)?;
            let decoded = proto::WhoHasRequest::decode(request.encode_to_vec().as_slice())?;
            // Assert.
            let OptionalRequest::Supported(actual) =
                validate_optional_request(&decoded, |_, _| true)?
            else {
                return Err("supported key did not decode".into());
            };
            assert_eq!(actual, key);
        }
        // Arrange.
        let hostile = LogicalKey::new(
            "test.v1".into(),
            vec![KeyPart {
                name: "generation".into(),
                value: Some("private".into()),
                optional: true,
            }],
        )?;
        // Act.
        let rejected = optional_who_has_request(&identity(), &validator(), &hostile);
        // Assert.
        assert!(rejected.is_err());
        Ok(())
    }
}
