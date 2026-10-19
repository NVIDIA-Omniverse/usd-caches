// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Cursor;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Material {
    pem: String,
    key: String,
    der: CertificateDer<'static>,
}

fn materials() -> &'static [Material; 2] {
    static MATERIALS: OnceLock<[Material; 2]> = OnceLock::new();
    MATERIALS.get_or_init(|| {
        std::array::from_fn(|_| {
            let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
            Material {
                pem: issued.cert.pem(),
                key: issued.signing_key.serialize_pem(),
                der: issued.cert.der().clone(),
            }
        })
    })
}

// Verifies the served identity through an in-memory TLS handshake.
fn certificate(acceptor: &TlsAcceptor) -> CertificateDer<'static> {
    let mut roots = rustls::RootCertStore::empty();
    for material in materials() {
        roots.add(material.der.clone()).unwrap();
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut client =
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap();
    let mut server = rustls::ServerConnection::new(Arc::clone(acceptor.config())).unwrap();
    for _ in 0..10 {
        let mut bytes = Vec::new();
        client.write_tls(&mut bytes).unwrap();
        server.read_tls(&mut Cursor::new(bytes)).unwrap();
        server.process_new_packets().unwrap();
        let mut bytes = Vec::new();
        server.write_tls(&mut bytes).unwrap();
        client.read_tls(&mut Cursor::new(bytes)).unwrap();
        client.process_new_packets().unwrap();
        if !client.is_handshaking() && !server.is_handshaking() {
            assert_eq!(
                client.alpn_protocol(),
                acceptor.config().alpn_protocols.first().map(Vec::as_slice)
            );
            return client.peer_certificates().unwrap()[0].clone();
        }
    }
    panic!("handshake did not complete");
}

#[derive(Debug)]
enum Fault {
    ReadDenied,
    MetadataUnavailable,
    Races {
        remaining: usize,
        initial: usize,
        alternate: usize,
    },
}

struct FaultFiles {
    root: PathBuf,
    fault: Mutex<Option<Fault>>,
    reads: AtomicUsize,
    stats: AtomicUsize,
    injected: AtomicUsize,
}

impl FaultFiles {
    /// Kubernetes rotates a Secret mount by atomically renaming a `..data`
    /// symlink over the old one.
    #[cfg(unix)]
    fn rotate(&self, generation: usize) {
        let temporary = self.root.join("next");
        std::os::unix::fs::symlink(generation.to_string(), &temporary).unwrap();
        std::fs::rename(temporary, self.root.join("current")).unwrap();
    }

    #[cfg(unix)]
    fn disk(&self) -> usize {
        std::fs::read_link(self.root.join("current"))
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Windows has no unprivileged directory symlink, so `current` is a real
    /// directory of hard links (preserving each generation's file identity)
    /// that is exchanged into place.
    #[cfg(windows)]
    fn rotate(&self, generation: usize) {
        let next = self.root.join("next");
        let _ = std::fs::remove_dir_all(&next);
        std::fs::create_dir(&next).unwrap();
        let source = self.root.join(generation.to_string());
        for name in ["tls.crt", "tls.key"] {
            std::fs::hard_link(source.join(name), next.join(name)).unwrap();
        }
        std::fs::write(next.join("generation"), generation.to_string()).unwrap();
        let current = self.root.join("current");
        if current.exists() {
            ovcc_platform::exchange_paths(&next, &current).unwrap();
            std::fs::remove_dir_all(&next).unwrap();
        } else {
            std::fs::rename(&next, &current).unwrap();
        }
    }

    #[cfg(windows)]
    fn disk(&self) -> usize {
        std::fs::read_to_string(self.root.join("current").join("generation"))
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl FileAccess for Arc<FaultFiles> {
    fn identity(&self, path: &Path) -> std::io::Result<TlsFileIdentity> {
        self.stats.fetch_add(1, Ordering::Relaxed);
        let mut fault = self.fault.lock().unwrap();
        if matches!(*fault, Some(Fault::MetadataUnavailable)) {
            *fault = None;
            self.injected.fetch_add(1, Ordering::Relaxed);
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "scripted metadata unavailable",
            ));
        }
        tls_file_identity(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut fault = self.fault.lock().unwrap();
        if matches!(*fault, Some(Fault::ReadDenied)) {
            *fault = None;
            self.injected.fetch_add(1, Ordering::Relaxed);
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "scripted read denied",
            ));
        }
        let bytes = std::fs::read(path)?;
        if path.file_name().unwrap() == "tls.crt"
            && let Some(Fault::Races {
                remaining,
                initial,
                alternate,
            }) = fault.as_mut()
        {
            let target = if *remaining == 2 {
                *alternate
            } else {
                *initial
            };
            self.rotate(target);
            self.injected.fetch_add(1, Ordering::Relaxed);
            *remaining -= 1;
            if *remaining == 0 {
                *fault = None;
            }
        }
        Ok(bytes)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    provider: Provider<Arc<FaultFiles>>,
    active: TlsAcceptor,
    held: TlsAcceptor,
    identities: Vec<TlsPairIdentity>,
    result: &'static str,
}

impl Fixture {
    fn new(mismatched: bool, reload: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for generation in 1..=3 {
            let sub = dir.path().join(generation.to_string());
            std::fs::create_dir(&sub).unwrap();
            let material = &materials()[(generation - 1) % 2];
            let pem = if generation == 3 {
                if mismatched {
                    &materials()[1].pem
                } else {
                    "-----BEGIN CERTIFICATE-----\ninvalid!\n-----END CERTIFICATE-----\n"
                }
            } else {
                &material.pem
            };
            std::fs::write(sub.join("tls.crt"), pem).unwrap();
            std::fs::write(sub.join("tls.key"), &material.key).unwrap();
        }
        let files = Arc::new(FaultFiles {
            root: dir.path().to_owned(),
            fault: Mutex::new(None),
            reads: AtomicUsize::new(0),
            stats: AtomicUsize::new(0),
            injected: AtomicUsize::new(0),
        });
        let identities = (1..=3)
            .map(|g| {
                let sub = dir.path().join(g.to_string());
                tls_pair_identity(&sub.join("tls.crt"), &sub.join("tls.key"), &SystemFiles).unwrap()
            })
            .collect();
        files.rotate(1);
        let provider = Provider::new(
            ServerOptions {
                certificate_path: Some(dir.path().join("current/tls.crt")),
                private_key_path: Some(dir.path().join("current/tls.key")),
                auto_reload_secret: reload,
                ..ServerOptions::default()
            },
            files,
        )
        .unwrap();
        let active = provider.state.lock().unwrap().acceptor.clone();
        Self {
            _dir: dir,
            provider,
            held: active.clone(),
            active,
            identities,
            result: "initial",
        }
    }

    fn generation(&self, identity: &TlsPairIdentity) -> usize {
        self.identities
            .iter()
            .position(|item| item == identity)
            .unwrap()
            + 1
    }

    fn cert_generation(&self, acceptor: &TlsAcceptor) -> usize {
        let der = certificate(acceptor);
        materials().iter().position(|item| item.der == der).unwrap() + 1
    }

    fn observe(&self) -> Value {
        let state = self.provider.state.lock().unwrap();
        let active = self.cert_generation(&self.active);
        assert_eq!(active, self.generation(state.identity.as_ref().unwrap()));
        assert!(Arc::ptr_eq(self.active.config(), state.acceptor.config()));
        json!({
            "disk": self.provider.files.disk(), "active": active,
            "failed": state.failed_identity.as_ref().map(|id| self.generation(id)).unwrap_or(0),
            "held": self.cert_generation(&self.held), "result": self.result,
        })
    }

    fn poll(&mut self) {
        let files = &self.provider.files;
        let before = files.reads.load(Ordering::Relaxed);
        let injected = files.injected.load(Ordering::Relaxed);
        let next = self.provider.acceptor();
        let reads = files.reads.load(Ordering::Relaxed) - before;
        let effects = files.injected.load(Ordering::Relaxed) - injected;
        self.result = if !Arc::ptr_eq(self.active.config(), next.config()) {
            assert_eq!(reads, 2);
            "published"
        } else if reads == 0 {
            if effects == 1 {
                "stat-failed"
            } else {
                "reused"
            }
        } else {
            let state = self.provider.state.lock().unwrap();
            let error = state.last_failure.as_ref().unwrap();
            if error.contains("scripted read denied") {
                assert_eq!(reads, 1);
                "read-failed"
            } else if error.contains("changed while loading") {
                assert_eq!(reads, 4);
                "race-failed"
            } else {
                assert_eq!(reads, 2);
                "rejected"
            }
        };
        self.active = next;
    }

    fn execute(&mut self, action: &Action, witness: Option<&str>) {
        let files = &self.provider.files;
        let before = files.injected.load(Ordering::Relaxed);
        let mut required_effects = 0;
        match action.name.as_str() {
            "Rotate" => {
                files.rotate(action.args[0].as_u64().unwrap() as usize);
                return;
            }
            "Retain" => {
                self.held = self.active.clone();
                return;
            }
            "ReadFailure" => {
                assert!(witness.is_none_or(|w| w == "read-denied"));
                *files.fault.lock().unwrap() = Some(Fault::ReadDenied);
                required_effects = 1;
            }
            "StatFailure" => {
                assert!(witness.is_none_or(|w| w == "metadata-unavailable"));
                *files.fault.lock().unwrap() = Some(Fault::MetadataUnavailable);
                required_effects = 1;
            }
            "RaceFailure" => {
                assert!(witness.is_none_or(|w| w == "two-races"));
                let initial = files.disk();
                *files.fault.lock().unwrap() = Some(Fault::Races {
                    remaining: 2,
                    initial,
                    alternate: if initial == 3 { 2 } else { 3 },
                });
                required_effects = 2;
            }
            "Reject" => {
                assert!(witness.is_none_or(|w| matches!(w, "malformed-pem" | "mismatched-key")))
            }
            "Reuse" | "Publish" => assert!(witness.is_none()),
            name => panic!("unknown model action: {name}"),
        }
        self.poll();
        let files = &self.provider.files;
        assert!(files.fault.lock().unwrap().is_none(), "unconsumed fault");
        assert_eq!(
            files.injected.load(Ordering::Relaxed) - before,
            required_effects
        );
    }
}

#[derive(Deserialize)]
struct Graph {
    initial: usize,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}
#[derive(Deserialize)]
struct Node {
    id: usize,
    state: Value,
}
#[derive(Deserialize)]
struct Edge {
    id: usize,
    from: usize,
    to: usize,
    action: Action,
}
#[derive(Deserialize)]
struct Action {
    name: String,
    args: Vec<Value>,
}
#[derive(Deserialize)]
struct Report {
    raw_edge_count: usize,
    mapped_edge_count: usize,
    unmapped_edge_ids: Vec<usize>,
    scenarios: Vec<Scenario>,
}
#[derive(Deserialize)]
struct Scenario {
    id: usize,
    raw_edge_ids: Vec<usize>,
    representative_edge_id: usize,
    trace_edge_ids: Vec<usize>,
    fault_witness: Option<String>,
    mode: String,
    expected_state: Value,
}
fn graph() -> Graph {
    serde_json::from_str(include_str!(
        "../skills/ovcc-tls-state-machine/fixtures/TrustReload.graph.json"
    ))
    .unwrap()
}
fn report() -> Report {
    serde_json::from_str(include_str!(
        "../skills/ovcc-tls-state-machine/fixtures/TrustReload.scenarios.json"
    ))
    .unwrap()
}

#[test]
fn tls_get_model_graph_integrity() {
    let graph = graph();
    let report = report();
    assert_eq!(report.raw_edge_count, graph.edges.len());
    assert_eq!(report.mapped_edge_count, graph.edges.len());
    assert!(report.unmapped_edge_ids.is_empty());
    let mapped = report
        .scenarios
        .iter()
        .flat_map(|s| s.raw_edge_ids.iter().copied())
        .collect::<BTreeSet<_>>();
    assert_eq!(mapped, (0..graph.edges.len()).collect());
    for (id, node) in graph.nodes.iter().enumerate() {
        assert_eq!(node.id, id);
    }
    for (id, edge) in graph.edges.iter().enumerate() {
        assert_eq!(edge.id, id);
    }
    let witnesses = report
        .scenarios
        .iter()
        .filter_map(|s| s.fault_witness.as_deref())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        witnesses,
        BTreeSet::from([
            "malformed-pem",
            "mismatched-key",
            "read-denied",
            "two-races",
            "metadata-unavailable"
        ])
    );
    for scenario in &report.scenarios {
        let mut state = graph.initial;
        for &id in &scenario.trace_edge_ids {
            let edge = &graph.edges[id];
            assert_eq!(edge.from, state);
            state = edge.to;
        }
        assert_eq!(
            scenario.trace_edge_ids.last(),
            Some(&scenario.representative_edge_id)
        );
        assert_eq!(scenario.expected_state, graph.nodes[state].state);
        assert_eq!(
            scenario.mode,
            if graph.edges[scenario.representative_edge_id].action.name == "Rotate" {
                "environment"
            } else {
                "production"
            }
        );
    }
}

#[test]
fn tls_get_model_scenarios_conform_to_production() {
    let graph = graph();
    for scenario in report().scenarios {
        let mut fixture = Fixture::new(
            scenario.fault_witness.as_deref() == Some("mismatched-key"),
            true,
        );
        assert_eq!(fixture.observe(), graph.nodes[graph.initial].state);
        for id in scenario.trace_edge_ids {
            let edge = &graph.edges[id];
            let witness = (id == scenario.representative_edge_id)
                .then_some(scenario.fault_witness.as_deref())
                .flatten();
            fixture.execute(&edge.action, witness);
            assert_eq!(
                fixture.observe(),
                graph.nodes[edge.to].state,
                "scenario {} edge {} action {}",
                scenario.id,
                id,
                edge.action.name
            );
        }
    }
}

#[test]
fn tls_get_disabled_reload_does_not_touch_files() {
    let mut fixture = Fixture::new(false, false);
    fixture.provider.files.rotate(2);
    let stats = fixture.provider.files.stats.load(Ordering::Relaxed);
    let reads = fixture.provider.files.reads.load(Ordering::Relaxed);
    fixture.poll();
    assert_eq!(fixture.cert_generation(&fixture.active), 1);
    assert_eq!(fixture.provider.files.stats.load(Ordering::Relaxed), stats);
    assert_eq!(fixture.provider.files.reads.load(Ordering::Relaxed), reads);
}

#[test]
fn tls_get_concurrent_rotation_loads_once_and_preserves_snapshot() {
    let fixture = Fixture::new(false, true);
    fixture.provider.files.rotate(2);
    let reads = fixture.provider.files.reads.load(Ordering::Relaxed);
    std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| scope.spawn(|| fixture.provider.acceptor()))
            .collect::<Vec<_>>();
        let snapshots = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        for snapshot in &snapshots {
            assert!(Arc::ptr_eq(snapshot.config(), snapshots[0].config()));
            assert_eq!(certificate(snapshot), materials()[1].der);
        }
    });
    assert_eq!(
        fixture.provider.files.reads.load(Ordering::Relaxed) - reads,
        2
    );
    assert_eq!(certificate(&fixture.held), materials()[0].der);
}

#[test]
fn tls_get_second_stable_read_attempt_can_publish() {
    let fixture = Fixture::new(false, true);
    fixture.provider.files.rotate(2);
    // Restores generation 1 during a read to exercise the second load attempt.
    *fixture.provider.files.fault.lock().unwrap() = Some(Fault::Races {
        remaining: 1,
        initial: 1,
        alternate: 2,
    });
    let reads = fixture.provider.files.reads.load(Ordering::Relaxed);
    let next = fixture.provider.acceptor();
    assert_eq!(certificate(&next), materials()[0].der);
    assert_eq!(
        fixture.provider.files.reads.load(Ordering::Relaxed) - reads,
        4
    );
    assert_eq!(fixture.provider.files.injected.load(Ordering::Relaxed), 1);
    assert!(fixture.provider.files.fault.lock().unwrap().is_none());
    assert!(
        fixture
            .provider
            .state
            .lock()
            .unwrap()
            .failed_identity
            .is_none()
    );
}

#[test]
fn tls_get_server_policy_preserves_alpn_and_ktls() {
    let fixture = Fixture::new(false, true);
    let options = ServerOptions {
        certificate_path: Some(fixture._dir.path().join("1/tls.crt")),
        private_key_path: Some(fixture._dir.path().join("1/tls.key")),
        alpn_protocols: vec![b"h2".to_vec()],
        ssl_protocols: vec![TlsProtocol::Tls12, TlsProtocol::Tls13],
        ..ServerOptions::default()
    };
    let provider = TlsAcceptorProvider::new(options).unwrap();
    let acceptor = provider.acceptor();
    assert!(acceptor.config().enable_secret_extraction);
    assert_eq!(certificate(&acceptor), materials()[0].der);
}

#[test]
fn tls_get_ca_loading_preserves_exclusive_and_additive_trust() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ca.pem");
    std::fs::write(&path, &materials()[0].pem).unwrap();
    let origin = load_root_store(&path).unwrap();
    let mut peer = rustls::RootCertStore::empty();
    peer.add(materials()[1].der.clone()).unwrap();
    let pem = add_peer_ca(&path, &mut peer).unwrap();
    assert_eq!(origin.len(), 1);
    assert_eq!(peer.len(), 2);
    assert_eq!(pem, materials()[0].pem.as_bytes());
    std::fs::write(&path, &materials()[1].pem).unwrap();
    assert_ne!(origin.roots, load_root_store(&path).unwrap().roots);
}

#[cfg(windows)]
#[test]
fn tls_get_ca_default_bundle_falls_back_to_windows_roots_only_when_absent() {
    // The Linux default path is absent on Windows, so it means system trust.
    assert!(!Path::new(SYSTEM_CA_BUNDLE_PATH).try_exists().unwrap());
    assert!(
        !load_root_store(Path::new(SYSTEM_CA_BUNDLE_PATH))
            .unwrap()
            .is_empty()
    );
    // Any other missing path still fails closed.
    let dir = tempfile::tempdir().unwrap();
    assert!(load_root_store(&dir.path().join("ca-certificates.crt")).is_err());
}

#[test]
fn tls_get_ca_loading_rejects_missing_empty_and_malformed_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.pem");
    assert!(load_root_store(&path).is_err());
    assert!(matches!(
        add_peer_ca(&path, &mut rustls::RootCertStore::empty()),
        Err(PeerCaError::Io(_))
    ));
    for contents in [
        "# empty",
        "-----BEGIN CERTIFICATE-----\ninvalid!\n-----END CERTIFICATE-----",
        "-----BEGIN CERTIFICATE-----\nAA==\n-----END CERTIFICATE-----",
    ] {
        std::fs::write(&path, contents).unwrap();
        assert!(load_root_store(&path).is_err());
        assert!(matches!(
            add_peer_ca(&path, &mut rustls::RootCertStore::empty()),
            Err(PeerCaError::Tls(_))
        ));
    }
}
