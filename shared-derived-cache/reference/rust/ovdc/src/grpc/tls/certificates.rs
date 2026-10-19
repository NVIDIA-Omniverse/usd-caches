// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{hash::Hasher as _, io, path::Path};

use rustls::{server::WebPkiClientVerifier, RootCertStore, ServerConfig};
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

pub(crate) async fn certificates(
    cert_path: &Path,
    key_path: &Path,
    ca_root: Option<(&Path, bool)>,
) -> io::Result<(ServerConfig, u64)> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let mut hasher = seahash::SeaHasher::new();

    tracing::trace!(?cert_path, ?key_path, ?ca_root, "try updating certs");

    let certs = ovdc_trace::err!(
        load_certs(&cert_path).await,
        "could not load certificate from {cert_path:?}"
    )?;
    let key = ovdc_trace::err!(
        load_private_key(&key_path).await,
        "failed to load private key from path {key_path:?}"
    )?;

    for cert in &certs {
        hasher.write(cert);
    }
    hasher.write(key.secret_der());

    let len = certs.len();

    let config = match ca_root {
        Some((path, unauthenticated)) => {
            let ca_root = ovdc_trace::err!(
                load_certs(&path).await,
                "failed to load ca root cert from {path:?}"
            )?;

            let mut roots = RootCertStore::empty();
            for ca in ca_root {
                hasher.write(&ca);
                roots.add(ca).expect("cannot use certificate");
            }

            let verifier = if unauthenticated {
                WebPkiClientVerifier::builder(roots.into()).allow_unauthenticated()
            } else {
                WebPkiClientVerifier::builder(roots.into())
            };
            let verifier = ovdc_trace::err!(verifier.build(), "failed to parse ca root")
                .map_err(|_| io::ErrorKind::Other)?;

            ServerConfig::builder().with_client_cert_verifier(verifier)
        }
        None => ServerConfig::builder().with_no_client_auth(),
    };
    let mut certs = ovdc_trace::err!(
        config.with_single_cert(certs, key),
        "TLS cert ({cert_path:?}) or key ({key_path:?}) is invalid"
    )
    .expect("failed to open cert");

    certs.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"http/1.0".to_vec()];

    tracing::info!(
        len,
        is_ca_root = ca_root.is_some(),
        "verified certificate chain"
    );

    Ok((certs, hasher.finish()))
}

async fn load_certs(path: impl AsRef<Path>) -> io::Result<Vec<CertificateDer<'static>>> {
    let buffer = tokio::fs::read(path).await?;
    let mut certs = Vec::new();
    for cert in rustls_pki_types::CertificateDer::pem_slice_iter(&buffer) {
        let cert = cert.expect("invalid public key in certificate chain");
        certs.push(cert);
    }
    Ok(certs)
}

async fn load_private_key(path: impl AsRef<Path>) -> io::Result<PrivateKeyDer<'static>> {
    let buffer = tokio::fs::read(path).await?;
    Ok(rustls_pki_types::PrivateKeyDer::from_pem_slice(&buffer[..]).expect("invalid private key"))
}
