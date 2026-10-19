// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Presigned-URL classification and expiry validation.
//!
//! Two presigned GETs for the same object differ only in their signature/date
//! noise (which `ovcc-cachekey` strips), so they share one cache key. Before
//! serving such a request as a fresh HIT, the proxy must still honor the URL's
//! own expiry — an expired presigned URL is not fresh. This crate is the one
//! place that recognizes a presigning scheme and derives its `expires_at`.
//!
//! Scope covers AWS SigV4 (`X-Amz-*`) and Azure SAS (`sig`/`se`). Everything
//! operates on the raw URL query string; values are percent-decoded only where
//! a timestamp is read.

use ovcc_core::{Error, Result};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

/// AWS SigV4 basic-ISO8601 stamp, e.g. `20250101T000000Z` (`X-Amz-Date`).
const S3_DATE_FORMAT: &[time::format_description::BorrowedFormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

/// Azure also accepts an RFC3339 UTC timestamp without seconds.
const AZURE_MINUTE_FORMAT: &[time::format_description::BorrowedFormatItem<'_>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]Z");

/// Which presigning scheme (if any) a request URL uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresignKind {
    /// Not a presigned URL.
    None,
    /// AWS SigV4 presigned (`X-Amz-*` query params).
    S3,
    /// Azure SAS (`sig`/`se` query params).
    Azure,
}

/// A classified + validated presigned URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presigned {
    pub kind: PresignKind,
    /// Expiry as unix seconds, when determinable from the URL.
    pub expires_at_unix: Option<i64>,
}

/// Classifies a raw URL query string as S3 presigned, Azure SAS, or neither.
///
/// A URL is S3-presigned only when it carries the actual signature
/// (`X-Amz-Signature`). Partial SigV4 noise — e.g. `X-Amz-Algorithm` or
/// `X-Amz-Credential` without a signature — is **not** a presigned URL but a
/// normal request that happens to carry those query args, so it must not be
/// validated (and must not 400). Azure requires **both** `sig` and `se`.
pub fn classify(raw_query: &str) -> PresignKind {
    if has_param(raw_query, "X-Amz-Signature") {
        PresignKind::S3
    } else if has_param(raw_query, "sig") && has_param(raw_query, "se") {
        PresignKind::Azure
    } else {
        PresignKind::None
    }
}

/// Parses the signed time fields without requiring a live credential.
/// Private cache reuse uses this syntax check; upstream still authorizes misses.
pub fn parse_for_private_reuse(raw_query: &str) -> Result<Presigned> {
    let kind = classify(raw_query);
    let expires_at_unix = match kind {
        PresignKind::None => None,
        PresignKind::S3 => Some(expiry_s3(raw_query)?),
        PresignKind::Azure => Some(expiry_azure(raw_query, None)?),
    };
    Ok(Presigned {
        kind,
        expires_at_unix,
    })
}

/// Validates a presigned URL's expiry against `now_unix`.
///
/// Returns the classified kind plus its `expires_at` (unix seconds). Errors
/// when the URL is presigned but its time params are missing/malformed, or
/// when it is already expired (`expires_at < now_unix`). A non-presigned URL
/// is `Ok` with `kind: None` and no expiry.
pub fn validate(raw_query: &str, now_unix: i64) -> Result<Presigned> {
    let kind = classify(raw_query);
    let expires_at_unix = match kind {
        PresignKind::None => {
            return Ok(Presigned {
                kind,
                expires_at_unix: None,
            });
        }
        PresignKind::S3 => expiry_s3(raw_query)?,
        PresignKind::Azure => expiry_azure(raw_query, Some(now_unix))?,
    };

    if expires_at_unix < now_unix {
        return Err(Error::presigned("expired"));
    }
    Ok(Presigned {
        kind,
        expires_at_unix: Some(expires_at_unix),
    })
}

/// S3 expiry = `X-Amz-Date` (basic ISO8601) + `X-Amz-Expires` (seconds).
fn expiry_s3(raw_query: &str) -> Result<i64> {
    let date = param_value(raw_query, "X-Amz-Date")
        .ok_or_else(|| Error::presigned("S3 presigned URL missing X-Amz-Date"))?;
    let expires = param_value(raw_query, "X-Amz-Expires")
        .ok_or_else(|| Error::presigned("S3 presigned URL missing X-Amz-Expires"))?;

    let signed_at = PrimitiveDateTime::parse(&date, &S3_DATE_FORMAT)
        .map_err(|e| Error::presigned(format!("invalid X-Amz-Date {date:?}: {e}")))?
        .assume_utc()
        .unix_timestamp();
    let ttl: i64 = expires
        .parse()
        .map_err(|e| Error::presigned(format!("invalid X-Amz-Expires {expires:?}: {e}")))?;

    signed_at
        .checked_add(ttl)
        .ok_or_else(|| Error::presigned("S3 expiry overflow"))
}

/// Azure expiry = `se` (required) parsed as RFC3339/ISO8601, validated against
/// the SAS start time `st`.
///
/// `st` is optional in Azure SAS; when absent it is implicitly "now"
/// (`now_unix`). The signed window must be well-formed — `se` strictly after
/// the start — otherwise the URL is malformed and the caller turns the error
/// into a `400` (an empty/absent `se`, an unparseable stamp, or `se <= st` all
/// qualify). A *well-formed* window whose `se` is merely in the past is not a
/// malformedness error here — that is ordinary expiry, handled by the caller as
/// a refetch.
fn expiry_azure(raw_query: &str, now_unix: Option<i64>) -> Result<i64> {
    let se = param_value(raw_query, "se")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::presigned("Azure SAS missing se"))?;
    let expires_at = parse_azure_timestamp(&se)
        .ok_or_else(|| Error::presigned(format!("invalid Azure se {se:?}")))?;

    // `st` is optional; an absent or empty value means the window starts now.
    let starts_at = match param_value(raw_query, "st").filter(|s| !s.is_empty()) {
        Some(st) => Some(
            parse_azure_timestamp(&st)
                .ok_or_else(|| Error::presigned(format!("invalid Azure st {st:?}")))?,
        ),
        None => now_unix,
    };

    if starts_at.is_some_and(|start| expires_at <= start) {
        return Err(Error::presigned("invalid SAS time window"));
    }
    Ok(expires_at)
}

fn parse_azure_timestamp(value: &str) -> Option<i64> {
    OffsetDateTime::parse(value, &Rfc3339)
        .ok()
        .map(|timestamp| timestamp.unix_timestamp())
        .or_else(|| {
            PrimitiveDateTime::parse(value, &AZURE_MINUTE_FORMAT)
                .ok()
                .map(|timestamp| timestamp.assume_utc().unix_timestamp())
        })
}

/// Checks if `raw_query` carries a `name=...` (or bare `name`) parameter.
/// Scans the raw wire form; keys are matched verbatim (never percent-encoded
/// in practice), keeping classification allocation-free on the hot path.
fn has_param(raw_query: &str, name: &str) -> bool {
    raw_query.split('&').any(|pair| {
        let key = pair.split_once('=').map_or(pair, |(k, _)| k);
        key == name
    })
}

/// Percent-decoded value of the first `name=` parameter, if present.
fn param_value(raw_query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(raw_query.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2025-01-01T00:00:00Z
    const T_2025_01_01: i64 = 1_735_689_600;

    const S3_QUERY: &str = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKID%2F20250101\
        &X-Amz-Date=20250101T000000Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host\
        &X-Amz-Signature=deadbeefcafef00d";
    const AZURE_QUERY: &str =
        "sv=2022-11-02&se=2025-01-01T00%3A00%3A00Z&sr=b&sp=r&sig=Zm9vYmFyYmF6";

    #[test]
    fn classify_s3() {
        assert_eq!(classify(S3_QUERY), PresignKind::S3);
        assert_eq!(classify("X-Amz-Signature=abc"), PresignKind::S3);
        // Partial SigV4 noise without a signature is NOT a presigned URL, so it
        // must not be validated (and must not 400). Classifies as None.
        assert_eq!(
            classify("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
            PresignKind::None
        );
        assert_eq!(
            classify(
                "X-Amz-Algorithm=ALG&X-Amz-Credential=CRED&X-Amz-Date=20250101T000000Z&X-Amz-Expires=3600"
            ),
            PresignKind::None
        );
    }

    #[test]
    fn classify_azure() {
        assert_eq!(classify(AZURE_QUERY), PresignKind::Azure);
    }

    #[test]
    fn classify_none() {
        assert_eq!(classify(""), PresignKind::None);
        assert_eq!(classify("list-type=2&prefix=foo"), PresignKind::None);
        // Azure needs BOTH sig and se — one alone is not a SAS.
        assert_eq!(classify("sig=abc"), PresignKind::None);
        assert_eq!(classify("se=2025-01-01T00:00:00Z"), PresignKind::None);
    }

    #[test]
    fn validate_s3_not_expired() {
        // Signed at 2025-01-01T00:00:00Z, +3600s.
        let p = validate(S3_QUERY, T_2025_01_01).unwrap();
        assert_eq!(p.kind, PresignKind::S3);
        assert_eq!(p.expires_at_unix, Some(T_2025_01_01 + 3600));
    }

    #[test]
    fn validate_azure_not_expired() {
        let p = validate(AZURE_QUERY, T_2025_01_01 - 10).unwrap();
        assert_eq!(p.kind, PresignKind::Azure);
        assert_eq!(p.expires_at_unix, Some(T_2025_01_01));
    }

    #[test]
    fn azure_validate_minute_precision_not_expired() {
        let q = "sv=2022-11-02&st=2024-12-31T23%3A59Z&se=2025-01-01T00%3A01Z&sr=b&sp=r&sig=Zm9v";
        let p = validate(q, T_2025_01_01).unwrap();
        assert_eq!(p.kind, PresignKind::Azure);
        assert_eq!(p.expires_at_unix, Some(T_2025_01_01 + 60));
    }

    #[test]
    fn validate_none_is_ok_without_expiry() {
        let p = validate("list-type=2&prefix=foo", T_2025_01_01).unwrap();
        assert_eq!(
            p,
            Presigned {
                kind: PresignKind::None,
                expires_at_unix: None
            }
        );
    }

    #[test]
    fn validate_s3_expired_errs() {
        // now is one second past expiry (signed + 3600).
        let err = validate(S3_QUERY, T_2025_01_01 + 3601).unwrap_err();
        assert_eq!(err.kind(), ovcc_core::ErrorKind::Presigned);
        assert_eq!(err.message(), "expired");
    }

    #[test]
    fn validate_azure_expired_errs() {
        // A well-formed window (explicit st strictly before se) whose se is in
        // the past is ordinary expiry — not a malformed-window 400.
        // st = 2025-01-01T00:00:00Z, se = +1h, now = +2h.
        let q = "sv=2022-11-02&st=2025-01-01T00%3A00%3A00Z&se=2025-01-01T01%3A00%3A00Z&sr=b&sp=r&sig=Zm9v";
        let err = validate(q, T_2025_01_01 + 7200).unwrap_err();
        assert_eq!(err.kind(), ovcc_core::ErrorKind::Presigned);
        assert_eq!(err.message(), "expired");
    }

    #[test]
    fn validate_azure_invalid_window_errs() {
        // se not strictly after st -> malformed window (caller maps to 400),
        // distinct from "expired". Here st = se.
        let q = "sv=2022-11-02&st=2025-01-01T00%3A00%3A00Z&se=2025-01-01T00%3A00%3A00Z&sr=b&sp=r&sig=Zm9v";
        let err = validate(q, T_2025_01_01 - 10).unwrap_err();
        assert_eq!(err.kind(), ovcc_core::ErrorKind::Presigned);
        assert_ne!(err.message(), "expired");
    }

    #[test]
    fn validate_azure_missing_st_defaults_to_now() {
        // No st, so the window starts "now". se in the future -> valid.
        let p = validate(AZURE_QUERY, T_2025_01_01 - 10).unwrap();
        assert_eq!(p.expires_at_unix, Some(T_2025_01_01));
        // An explicitly empty st follows the established request classification.
        let empty_st = "st=&se=2025-01-01T00%3A00%3A00Z&sr=b&sp=r&sig=Zm9vYmFyYmF6";
        let p = validate(empty_st, T_2025_01_01 - 10).unwrap();
        assert_eq!(p.expires_at_unix, Some(T_2025_01_01));
        // No st, se in the past -> se <= now(start) -> invalid window (400),
        // not "expired".
        let err = validate(AZURE_QUERY, T_2025_01_01 + 1).unwrap_err();
        assert_ne!(err.message(), "expired");
    }

    #[test]
    fn validate_s3_malformed_errs() {
        // Missing X-Amz-Expires.
        let q = "X-Amz-Signature=abc&X-Amz-Date=20250101T000000Z";
        assert_eq!(
            validate(q, 0).unwrap_err().kind(),
            ovcc_core::ErrorKind::Presigned
        );
        // Unparseable date.
        let q = "X-Amz-Signature=abc&X-Amz-Date=not-a-date&X-Amz-Expires=3600";
        assert_eq!(
            validate(q, 0).unwrap_err().kind(),
            ovcc_core::ErrorKind::Presigned
        );
        // Non-numeric expires.
        let q = "X-Amz-Signature=abc&X-Amz-Date=20250101T000000Z&X-Amz-Expires=soon";
        assert_eq!(
            validate(q, 0).unwrap_err().kind(),
            ovcc_core::ErrorKind::Presigned
        );
    }

    #[test]
    fn validate_azure_malformed_errs() {
        // se present (so it classifies Azure) but not a timestamp.
        let q = "sig=abc&se=whenever";
        assert_eq!(
            validate(q, 0).unwrap_err().kind(),
            ovcc_core::ErrorKind::Presigned
        );
    }
}

#[cfg(test)]
mod private_reuse_tests {
    use super::*;

    #[test]
    fn azure_get_private_reuse_accepts_expiry_but_rejects_malformed_windows() -> Result<()> {
        // Arrange.
        let expired = "sig=synthetic&se=2000-01-01T00:00:00Z";
        let reversed = "sig=synthetic&st=2001-01-01T00:00:00Z&se=2000-01-01T00:00:00Z";
        // Act.
        let parsed = parse_for_private_reuse(expired)?;
        // Assert.
        assert_eq!(parsed.kind, PresignKind::Azure);
        assert!(validate(expired, 1_700_000_000).is_err());
        assert!(parse_for_private_reuse(reversed).is_err());
        assert!(parse_for_private_reuse("sig=synthetic&se=broken").is_err());
        Ok(())
    }
}
