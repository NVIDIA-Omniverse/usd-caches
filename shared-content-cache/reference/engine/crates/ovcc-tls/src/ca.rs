// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Applies origin and peer trust policies during client construction.
use ovcc_core::{Error, Result};
use rustls::pki_types::{CertificateDer, pem::PemObject as _};
use std::path::Path;

/// The distro trust bundle, which is also the configured default origin CA.
pub const SYSTEM_CA_BUNDLE_PATH: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Builds origin trust exclusively from the configured PEM bundle.
///
/// On Windows the default [`SYSTEM_CA_BUNDLE_PATH`] does not exist; when it is
/// configured and confirmed absent, the system `ROOT` store supplies the same
/// "system trust" meaning. A default path that cannot be inspected fails
/// closed, and any other path is still loaded exclusively.
pub fn load_root_store(path: &Path) -> Result<rustls::RootCertStore> {
    load_root_store_with_system_policy(path).map(|(roots, _)| roots)
}

/// Loads the configured roots and reports whether each accepted origin chain
/// must also pass the operating system's certificate policy.
pub fn load_root_store_with_system_policy(path: &Path) -> Result<(rustls::RootCertStore, bool)> {
    #[cfg(windows)]
    if path == Path::new(SYSTEM_CA_BUNDLE_PATH) {
        match path.try_exists() {
            Ok(false) => return load_system_root_store().map(|roots| (roots, true)),
            Ok(true) => {}
            Err(error) => {
                return Err(Error::config(format!(
                    "failed to inspect origin CA bundle {}: {error}",
                    path.display()
                )));
            }
        }
    }
    let pem = std::fs::read(path).map_err(|error| {
        Error::config(format!(
            "failed to read origin CA bundle {}: {error}",
            path.display()
        ))
    })?;
    let mut roots = rustls::RootCertStore::empty();
    for (index, cert) in CertificateDer::pem_slice_iter(&pem).enumerate() {
        let cert = cert.map_err(|error| {
            Error::config(format!(
                "malformed certificate {} in origin CA bundle {}: {error}",
                index + 1,
                path.display()
            ))
        })?;
        roots.add(cert).map_err(|error| {
            Error::config(format!(
                "invalid certificate {} in origin CA bundle {}: {error}",
                index + 1,
                path.display()
            ))
        })?;
    }
    if roots.is_empty() {
        return Err(Error::config(format!(
            "origin CA bundle {} contains no certificates",
            path.display()
        )));
    }
    Ok((roots, false))
}

#[cfg(windows)]
fn load_system_root_store() -> Result<rustls::RootCertStore> {
    let certificates = ovcc_platform::system_root_certificates().map_err(|error| {
        Error::config(format!("failed to read the Windows ROOT store: {error}"))
    })?;
    let mut roots = rustls::RootCertStore::empty();
    // The OS store may hold roots webpki cannot parse (e.g. legacy
    // encodings); skip those rather than refusing all system trust.
    roots.add_parsable_certificates(certificates.into_iter().map(CertificateDer::from));
    if roots.is_empty() {
        return Err(Error::config(
            "the Windows ROOT store contains no usable certificates",
        ));
    }
    Ok(roots)
}

/// Distinguishes filesystem failures from invalid peer trust material.
#[derive(Debug)]
pub enum PeerCaError {
    Io(std::io::Error),
    Tls(String),
}

impl std::fmt::Display for PeerCaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Tls(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for PeerCaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Tls(_) => None,
        }
    }
}

/// Preserves filesystem versus peer-trust classification when crossing into
/// the shared OVCC error type.
impl From<PeerCaError> for Error {
    fn from(error: PeerCaError) -> Self {
        match error {
            PeerCaError::Io(error) => error.into(),
            PeerCaError::Tls(message) => Self::config(message),
        }
    }
}

/// Extends peer trust and returns the same PEM bytes for tonic.
pub fn add_peer_ca(
    path: &Path,
    roots: &mut rustls::RootCertStore,
) -> std::result::Result<Vec<u8>, PeerCaError> {
    let pem = std::fs::read(path).map_err(PeerCaError::Io)?;
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| PeerCaError::Tls(format!("parsing CA PEM at {path:?}: {error}")))?;
    if certificates.is_empty() {
        return Err(PeerCaError::Tls(format!(
            "CA PEM at {path:?} contains no certificates"
        )));
    }
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| PeerCaError::Tls(format!("adding CA from {path:?}: {error}")))?;
    }
    Ok(pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_ca_failures_preserve_io_and_config_classification() {
        // Arrange.
        let io = PeerCaError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "missing peer CA",
        ));
        let tls = PeerCaError::Tls("invalid peer CA".to_owned());

        // Act.
        let io = Error::from(io);
        let tls = Error::from(tls);

        // Assert.
        assert_eq!(io.kind(), ovcc_core::ErrorKind::Io);
        assert_eq!(io.message(), "missing peer CA");
        assert_eq!(tls.kind(), ovcc_core::ErrorKind::Config);
        assert_eq!(tls.message(), "invalid peer CA");
    }
}
