// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Loads TLS identities and CA bundles independently of transport setup.
//! Server identities support reload; CA bundles load at client construction.

use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_rustls::TlsAcceptor;

mod ca;
pub use ca::{
    PeerCaError, SYSTEM_CA_BUNDLE_PATH, add_peer_ca, load_root_store,
    load_root_store_with_system_policy,
};

/// Reports identity-loading or server-configuration failures.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Restricts negotiation to the selected TLS versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsProtocol {
    Tls12,
    Tls13,
}

/// Uses a generated localhost identity when both PEM paths are absent.
/// Configured identities require both paths.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub certificate_path: Option<PathBuf>,
    pub private_key_path: Option<PathBuf>,
    pub ssl_protocols: Vec<TlsProtocol>,
    pub alpn_protocols: Vec<Vec<u8>>,
    pub auto_reload_secret: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            certificate_path: None,
            private_key_path: None,
            ssl_protocols: vec![TlsProtocol::Tls13],
            alpn_protocols: Vec::new(),
            auto_reload_secret: false,
        }
    }
}

/// Retains the last valid configuration across failed replacements. Acceptor
/// clones survive rotation; failed identities are skipped until metadata changes,
/// including after transient read errors.
pub struct TlsAcceptorProvider(Provider<SystemFiles>);

impl TlsAcceptorProvider {
    /// Loads the configured identity or generates a localhost identity when both
    /// paths are absent; rejects invalid initial TLS configuration.
    pub fn new(options: ServerOptions) -> std::result::Result<Self, BoxError> {
        Provider::new(options, SystemFiles).map(Self)
    }

    /// Checks for replacements when reload is enabled and returns a clone of
    /// the last valid acceptor, retaining it if replacement loading fails.
    pub fn acceptor(&self) -> TlsAcceptor {
        self.0.acceptor()
    }
}

// Allows deterministic filesystem faults while retaining production validation.
trait FileAccess {
    fn identity(&self, path: &Path) -> std::io::Result<TlsFileIdentity>;
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>>;
}

struct SystemFiles;
impl FileAccess for SystemFiles {
    fn identity(&self, path: &Path) -> std::io::Result<TlsFileIdentity> {
        tls_file_identity(path)
    }
    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

struct Provider<F> {
    files: F,
    state: Mutex<TlsAcceptorState>,
    reload: Option<TlsReload>,
}

struct TlsAcceptorState {
    acceptor: TlsAcceptor,
    identity: Option<TlsPairIdentity>,
    failed_identity: Option<TlsPairIdentity>,
    last_failure: Option<String>,
}

/// Identity, length, modification time, and permissions of one PEM file.
type TlsFileIdentity = ovcc_platform::FileStamp;

#[derive(Clone, PartialEq, Eq)]
struct TlsPairIdentity {
    certificate: TlsFileIdentity,
    private_key: TlsFileIdentity,
}

struct LoadedAcceptor {
    acceptor: TlsAcceptor,
    identity: TlsPairIdentity,
}

struct TlsReload {
    certificate_path: PathBuf,
    private_key_path: PathBuf,
    protocols: Vec<TlsProtocol>,
    alpn_protocols: Vec<Vec<u8>>,
}

impl<F: FileAccess> Provider<F> {
    fn new(tls: ServerOptions, files: F) -> std::result::Result<Self, BoxError> {
        let alpn_protocols = tls.alpn_protocols.clone();
        let protocols = tls.ssl_protocols.clone();
        let (acceptor, identity) = build_acceptor(&tls, &alpn_protocols, &files)?;
        let reload = if tls.auto_reload_secret {
            match (&tls.certificate_path, &tls.private_key_path) {
                (Some(certificate), Some(private_key)) => Some(TlsReload {
                    certificate_path: certificate.into(),
                    private_key_path: private_key.into(),
                    protocols,
                    alpn_protocols,
                }),
                _ => None,
            }
        } else {
            None
        };
        Ok(Self {
            files,
            state: Mutex::new(TlsAcceptorState {
                acceptor,
                identity,
                failed_identity: None,
                last_failure: None,
            }),
            reload,
        })
    }

    fn acceptor(&self) -> TlsAcceptor {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(reload) = &self.reload {
            match tls_pair_identity(
                &reload.certificate_path,
                &reload.private_key_path,
                &self.files,
            ) {
                Ok(identity)
                    if state.identity.as_ref() != Some(&identity)
                        && state.failed_identity.as_ref() != Some(&identity) =>
                {
                    match load_pem_acceptor(
                        &reload.certificate_path,
                        &reload.private_key_path,
                        &reload.protocols,
                        &reload.alpn_protocols,
                        &self.files,
                    ) {
                        Ok(loaded) => {
                            state.acceptor = loaded.acceptor;
                            state.identity = Some(loaded.identity);
                            state.failed_identity = None;
                            state.last_failure = None;
                            tracing::info!(target: "ovcc_proxy", "ovcc-proxy reloaded TLS Secret");
                        }
                        Err(err) => {
                            state.failed_identity = Some(identity);
                            log_tls_reload_failure(&mut state, err.to_string());
                        }
                    }
                }
                Ok(_) => {}
                Err(err) => log_tls_reload_failure(&mut state, err.to_string()),
            }
        }
        state.acceptor.clone()
    }
}

fn log_tls_reload_failure(state: &mut TlsAcceptorState, failure: String) {
    if state.last_failure.as_ref() != Some(&failure) {
        tracing::warn!(
            target: "ovcc_proxy",
            "ovcc-proxy TLS Secret reload failed; retaining last valid configuration: {failure}"
        );
        state.last_failure = Some(failure);
    }
}

fn build_acceptor(
    tls: &ServerOptions,
    alpn_protocols: &[Vec<u8>],
    files: &impl FileAccess,
) -> std::result::Result<(TlsAcceptor, Option<TlsPairIdentity>), BoxError> {
    match (&tls.certificate_path, &tls.private_key_path) {
        (Some(certificate), Some(private_key)) => {
            let loaded = load_pem_acceptor(
                Path::new(certificate),
                Path::new(private_key),
                &tls.ssl_protocols,
                alpn_protocols,
                files,
            )?;
            Ok((loaded.acceptor, Some(loaded.identity)))
        }
        (None, None) => {
            let issued = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
            let cert_der: CertificateDer<'static> = issued.cert.der().clone();
            let key_der =
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(issued.signing_key.serialize_der()));
            Ok((
                acceptor_from_der(vec![cert_der], key_der, &tls.ssl_protocols, alpn_protocols)?,
                None,
            ))
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "TLS certificate_path and private_key_path must be configured together",
        )
        .into()),
    }
}

fn load_pem_acceptor(
    certificate_path: &Path,
    private_key_path: &Path,
    protocols: &[TlsProtocol],
    alpn_protocols: &[Vec<u8>],
    files: &impl FileAccess,
) -> std::result::Result<LoadedAcceptor, BoxError> {
    let mut last_race = None;
    for _ in 0..2 {
        let before = tls_pair_identity(certificate_path, private_key_path, files)?;
        let certificate_pem = files.read(certificate_path)?;
        let private_key_pem = files.read(private_key_path)?;
        let after = tls_pair_identity(certificate_path, private_key_path, files)?;
        if before != after {
            last_race = Some(std::io::Error::other(
                "TLS Secret changed while loading certificate pair",
            ));
            continue;
        }

        let certificates = CertificateDer::pem_slice_iter(&certificate_pem)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if certificates.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "TLS certificate file contains no PEM certificates",
            )
            .into());
        }
        let private_key = PrivateKeyDer::from_pem_slice(&private_key_pem)?;
        return Ok(LoadedAcceptor {
            acceptor: acceptor_from_der(certificates, private_key, protocols, alpn_protocols)?,
            identity: after,
        });
    }
    Err(last_race
        .unwrap_or_else(|| std::io::Error::other("TLS Secret could not be loaded"))
        .into())
}

fn tls_pair_identity(
    certificate_path: &Path,
    private_key_path: &Path,
    files: &impl FileAccess,
) -> std::io::Result<TlsPairIdentity> {
    Ok(TlsPairIdentity {
        certificate: files.identity(certificate_path)?,
        private_key: files.identity(private_key_path)?,
    })
}

fn tls_file_identity(path: &Path) -> std::io::Result<TlsFileIdentity> {
    ovcc_platform::file_stamp(path)
}

fn acceptor_from_der(
    certificates: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
    protocols: &[TlsProtocol],
    alpn_protocols: &[Vec<u8>],
) -> std::result::Result<TlsAcceptor, BoxError> {
    let protocol_versions = protocols
        .iter()
        .map(|protocol| match protocol {
            TlsProtocol::Tls12 => &rustls::version::TLS12,
            TlsProtocol::Tls13 => &rustls::version::TLS13,
        })
        .collect::<Vec<_>>();
    let mut server_config =
        rustls::ServerConfig::builder_with_protocol_versions(&protocol_versions)
            .with_no_client_auth()
            .with_single_cert(certificates, private_key)?;
    // Enables session-key extraction for the kTLS handoff.
    server_config.enable_secret_extraction = true;
    server_config.alpn_protocols = alpn_protocols.to_vec();
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

#[cfg(test)]
mod tests;
