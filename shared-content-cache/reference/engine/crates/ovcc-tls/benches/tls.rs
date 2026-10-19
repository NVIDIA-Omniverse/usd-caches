// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use ovcc_tls::{ServerOptions, TlsAcceptorProvider, add_peer_ca, load_root_store};
use std::hint::black_box;
use std::path::Path;

struct Material {
    cert: String,
    key: String,
}
fn material() -> Material {
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    Material {
        cert: issued.cert.pem(),
        key: issued.signing_key.serialize_pem(),
    }
}
#[cfg(unix)]
fn rotate(root: &Path, generation: usize) {
    std::os::unix::fs::symlink(generation.to_string(), root.join("next")).unwrap();
    std::fs::rename(root.join("next"), root.join("current")).unwrap();
}
/// Windows stand-in for the Secret symlink swap; see the unit-test fixture.
#[cfg(windows)]
fn rotate(root: &Path, generation: usize) {
    let next = root.join("next");
    let _ = std::fs::remove_dir_all(&next);
    std::fs::create_dir(&next).unwrap();
    for name in ["tls.crt", "tls.key"] {
        std::fs::hard_link(
            root.join(generation.to_string()).join(name),
            next.join(name),
        )
        .unwrap();
    }
    let current = root.join("current");
    if current.exists() {
        ovcc_platform::exchange_paths(&next, &current).unwrap();
        std::fs::remove_dir_all(&next).unwrap();
    } else {
        std::fs::rename(&next, &current).unwrap();
    }
}
fn setup(
    materials: &[Material; 2],
    reload: bool,
) -> (tempfile::TempDir, ServerOptions, TlsAcceptorProvider) {
    let dir = tempfile::tempdir().unwrap();
    for g in 1..=3 {
        let sub = dir.path().join(g.to_string());
        std::fs::create_dir(&sub).unwrap();
        let material = &materials[(g - 1) % 2];
        std::fs::write(
            sub.join("tls.crt"),
            if g == 3 {
                "invalid PEM"
            } else {
                &material.cert
            },
        )
        .unwrap();
        std::fs::write(sub.join("tls.key"), &material.key).unwrap();
    }
    rotate(dir.path(), 1);
    let options = ServerOptions {
        certificate_path: Some(dir.path().join("current/tls.crt")),
        private_key_path: Some(dir.path().join("current/tls.key")),
        auto_reload_secret: reload,
        ..ServerOptions::default()
    };
    let provider = TlsAcceptorProvider::new(options.clone()).unwrap();
    (dir, options, provider)
}

fn tls(c: &mut Criterion) {
    let materials = [material(), material()];
    let (_disabled_dir, _, disabled) = setup(&materials, false);
    c.bench_function("tls/acceptor/reload_disabled", |b| {
        b.iter(|| black_box(disabled.acceptor()))
    });
    let (dir, options, provider) = setup(&materials, true);
    c.bench_function("tls/acceptor/unchanged_files", |b| {
        b.iter(|| black_box(provider.acceptor()))
    });
    c.bench_function("tls/acceptor/initial_pem_load", |b| {
        b.iter(|| black_box(TlsAcceptorProvider::new(options.clone()).unwrap()))
    });
    for (name, generation) in [("valid_rotation", 2), ("invalid_rotation", 3)] {
        c.bench_function(&format!("tls/acceptor/{name}"), |b| {
            b.iter_batched(
                || {
                    let (dir, _, provider) = setup(&materials, true);
                    rotate(dir.path(), generation);
                    (dir, provider)
                },
                |(dir, provider)| {
                    let acceptor = provider.acceptor();
                    // Defers resource cleanup until after measurement.
                    black_box((dir, provider, acceptor))
                },
                BatchSize::PerIteration,
            )
        });
    }
    rotate(dir.path(), 3);
    let _ = provider.acceptor();
    c.bench_function("tls/acceptor/rejected_identity_reuse", |b| {
        b.iter(|| black_box(provider.acceptor()))
    });
    let ca = dir.path().join("1/tls.crt");
    c.bench_function("tls/ca/origin_bundle", |b| {
        b.iter(|| black_box(load_root_store(&ca).unwrap()))
    });
    c.bench_function("tls/ca/peer_bundle", |b| {
        b.iter(|| {
            let mut roots = rustls::RootCertStore::empty();
            let pem = add_peer_ca(&ca, &mut roots).unwrap();
            black_box((roots, pem))
        })
    });
}

criterion_group!(benches, tls);
criterion_main!(benches);
