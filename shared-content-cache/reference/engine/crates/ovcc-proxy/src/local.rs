// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Local requests share the ordinary policy pipeline and retain its exact commit.

use std::cell::RefCell;
use std::io;

use ovcc_local::{LeaseRequest, LeaseResponse, Outcome, PROTOCOL_VERSION};
use ovcc_platform::local::{Connection, Listener};

use super::*;

tokio::task_local! {
    static COMMITTED: RefCell<Option<Lease>>;
    static WORKERS: RefCell<Vec<JoinHandle<()>>>;
    static SOURCE: RefCell<Option<ovcc_storage::LocalPathSource>>;
}

pub(super) async fn blocking<F, T>(operation: F) -> io::Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if WORKERS.try_with(|_| ()).is_ok() {
        let (sender, receiver) = oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let _ = sender.send(operation());
        });
        WORKERS.with(|workers| workers.borrow_mut().push(worker));
        receiver.await.map_err(io::Error::other)
    } else {
        tokio::task::spawn_blocking(operation)
            .await
            .map_err(io::Error::other)
    }
}

pub(super) fn retain_committed(lease: Lease) {
    // Only local requests retain a commit. An ordinary HTTP request drops the
    // descriptor at the same commit boundary as before.
    let _ = COMMITTED.try_with(|slot| *slot.borrow_mut() = Some(lease));
}

pub(super) fn response_headers(mut response: Response, headers: &[(String, String)]) -> Response {
    if COMMITTED.try_with(|_| ()).is_ok() {
        response.head.headers = headers.to_vec();
    }
    response
}

pub(super) fn is_request() -> bool {
    COMMITTED.try_with(|_| ()).is_ok()
}

pub(super) fn retain_source(source: impl FnOnce() -> Option<ovcc_storage::LocalPathSource>) {
    let _ = SOURCE.try_with(|slot| *slot.borrow_mut() = source());
}

#[cfg(test)]
struct LookupGate {
    key: String,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static LOOKUP_GATE: std::sync::Mutex<Option<LookupGate>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) fn hold_lookup_for_test(key: &str) {
    let gate = {
        let mut hook = LOOKUP_GATE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if hook.as_ref().is_some_and(|gate| gate.key == key) {
            hook.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        let _ = gate.entered.send(());
        let _ = gate.release.recv();
    }
}

pub(super) async fn accept(
    listener: &mut Option<Listener>,
    retry_at: &mut tokio::time::Instant,
) -> io::Result<Connection> {
    match listener {
        Some(listener) => accept_after(retry_at, listener.accept()).await,
        None => std::future::pending().await,
    }
}

async fn accept_after<T>(
    retry_at: &mut tokio::time::Instant,
    accept: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::sleep_until(*retry_at).await;
    let result = accept.await;
    if let Err(error) = &result {
        *retry_at = tokio::time::Instant::now() + Duration::from_millis(100);
        tracing::debug!(event = "local_accept_failed", kind = ?error.kind());
    }
    result
}

fn request_head(mut request: LeaseRequest) -> io::Result<RequestHead> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "invalid local GET identity");
    if request.version != PROTOCOL_VERSION
        || request.lease_duration_millis == 0
        || request.lease_duration_millis > ovcc_storage::MAX_PATH_LEASE_MILLIS
        || !request.path.starts_with('/')
        || request.path.contains(['?', '#', '\r', '\n'])
        || request.query.contains(['#', '\r', '\n'])
        || request.authority.parse::<http::uri::Authority>().is_err()
        || request.authority.contains('@')
    {
        return Err(invalid());
    }
    let target = if request.query.is_empty() {
        request.path.clone()
    } else {
        format!("{}?{}", request.path, request.query)
    };
    target.parse::<http::Uri>().map_err(|_| invalid())?;
    for (name, value) in &mut request.headers {
        let parsed =
            http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        http::HeaderValue::from_str(value).map_err(|_| invalid())?;
        *value = value.trim().to_owned();
        if matches!(
            parsed.as_str(),
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        ) || parsed == http::header::CONTENT_LENGTH
            || name.to_ascii_lowercase().starts_with("x-ovcc-")
            || (parsed == http::header::HOST && value.as_str() != request.authority.as_str())
        {
            return Err(invalid());
        }
    }
    request
        .headers
        .retain(|(name, _)| !name.eq_ignore_ascii_case("host"));
    request
        .headers
        .push(("Host".to_owned(), request.authority.clone()));
    Ok(RequestHead {
        method: "GET".to_owned(),
        path: request.path,
        query_present: !request.query.is_empty(),
        query: request.query,
        http_version: "LOCAL/1".to_owned(),
        host: Some(request.authority),
        headers: request.headers,
        content_length: 0,
        head_length: 0,
    })
}

pub(super) async fn handle(
    connection: Connection,
    context: ConnCtx,
    scheme: &'static str,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    WORKERS
        .scope(RefCell::new(Vec::new()), async {
            let result = if *shutdown.borrow() {
                Ok(())
            } else {
                tokio::select! {
                    result = handle_inner(connection, context, scheme) => result,
                    _ = shutdown.changed() => Ok(()),
                }
            };
            // Cancelling a JoinHandle await does not stop spawn_blocking. The
            // request stays owned until every worker has dropped its cache owner.
            let workers = WORKERS.with(|workers| std::mem::take(&mut *workers.borrow_mut()));
            let mut failed = false;
            for worker in workers {
                failed |= worker.await.is_err();
            }
            if failed {
                Err(io::Error::other("local storage worker failed"))
            } else {
                result
            }
        })
        .await
}

async fn handle_inner(
    mut connection: Connection,
    context: ConnCtx,
    scheme: &'static str,
) -> io::Result<()> {
    let connection_id = CONNECTION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut requests = 0;
    loop {
        // Idle same-user connections remain reusable until disconnect or shutdown.
        let request = match ovcc_local::read_request(&mut connection).await {
            Ok(request) => request,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        };
        requests += 1;
        handle_request(
            &mut connection,
            request,
            &context,
            scheme,
            RequestConnection {
                id: connection_id,
                requests,
            },
        )
        .await?;
        // Unfinished workers remain owned by the outer shutdown join.
        WORKERS.with(|workers| workers.borrow_mut().retain(|worker| !worker.is_finished()));
    }
}

async fn handle_request(
    connection: &mut Connection,
    request: LeaseRequest,
    context: &ConnCtx,
    scheme: &'static str,
    request_connection: RequestConnection,
) -> io::Result<()> {
    let started = Instant::now();
    let duration = request.lease_duration_millis;
    let listener = if request.nucleus {
        ListenerKind::Nucleus
    } else {
        ListenerKind::Cache
    };
    let head = match request_head(request) {
        Ok(head) => head,
        Err(_) => {
            return ovcc_local::write_response(
                connection,
                &LeaseResponse {
                    version: PROTOCOL_VERSION,
                    outcome: Outcome::Rejected,
                    status: 400,
                    headers: Vec::new(),
                    cache_status: None,
                    generation: 0,
                    offset: 0,
                    length: 0,
                    path: None,
                    deadline_unix_millis: 0,
                },
            )
            .await;
        }
    };
    let host = head.host.as_deref().unwrap_or_default();
    let mut flags = RequestFlags::default();
    let (result, committed, source) = SOURCE
        .scope(
            RefCell::new(None),
            COMMITTED.scope(RefCell::new(None), async {
                let mut stream = ClientStream::Local(scheme);
                let result = with_downstream_request_state(compute_response(
                    &mut stream,
                    &head,
                    host,
                    Vec::new(),
                    context,
                    &mut flags,
                    listener,
                ))
                .await;
                (
                    result,
                    COMMITTED.with(|s| s.borrow_mut().take()),
                    SOURCE.with(|s| s.borrow_mut().take()),
                )
            }),
        )
        .await;
    let failed = result.is_err();
    let response = match result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                event = "local_response_failed",
                kind = ?error.downcast_ref::<Error>().map(Error::kind),
                io_kind = ?error.downcast_ref::<io::Error>().map(io::Error::kind),
            );
            Response::head(500, "Internal Server Error", None, Vec::new(), None)
        }
    };
    let mut access = build_access_log(
        &head,
        host,
        AccessConnection {
            remote_addr: "local",
            scheme,
            ssl_protocol: "",
            request: request_connection,
        },
        &flags,
        &response,
    );
    // Object bytes are read by the application, never transferred over IPC.
    access.bytes_sent = 0;
    let mut reply = LeaseResponse {
        version: PROTOCOL_VERSION,
        outcome: if failed {
            Outcome::Rejected
        } else {
            Outcome::NotLeasable
        },
        status: response.head.status,
        headers: response.head.headers,
        cache_status: response.head.cache_status.map(str::to_owned),
        generation: 0,
        offset: 0,
        length: 0,
        path: None,
        deadline_unix_millis: 0,
    };
    if matches!(reply.status, 200 | 206) {
        let candidate = match response.body {
            BodySource::LocalPath(candidate) => Some(candidate),
            other => {
                let selected = match other {
                    BodySource::Sendfile(lease) => Some(lease),
                    _ => committed,
                };
                match (selected, source) {
                    (Some(selected), Some(source)) => {
                        let expected = selected.meta.revision;
                        match blocking(move || source.prepare()).await {
                            Ok(Ok(ovcc_storage::LocalPathLookup::Hit(candidate)))
                                if candidate.meta.revision == expected =>
                            {
                                Some(candidate)
                            }
                            Ok(Err(error)) => {
                                tracing::warn!(event = "local_prepare_failed", kind = ?error.kind());
                                None
                            }
                            Err(error) => {
                                tracing::warn!(event = "local_prepare_task_failed", kind = ?error.kind());
                                None
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                }
            }
        };
        if let Some(candidate) = candidate {
            if !candidate.is_supported() {
                reply.outcome = Outcome::UnsupportedLayout;
            } else if candidate.meta.status == reply.status
                && let Some(grant) = match grant_path(&candidate, duration).await {
                    Ok(grant) => grant,
                    Err(error) => {
                        tracing::warn!(event = "local_grant_failed", kind = ?error.kind());
                        None
                    }
                }
            {
                reply.outcome = Outcome::Ready;
                reply.generation = candidate.meta.revision;
                reply.length = candidate.meta.size;
                reply.path = Some(grant.path);
                reply.deadline_unix_millis = grant.deadline_unix_millis;
                if reply.headers.is_empty() {
                    reply.headers = candidate.meta.headers.clone();
                }
            }
        }
    }
    let written = timeout(
        Duration::from_secs(10),
        ovcc_local::write_response(connection, &reply),
    )
    .await;
    access.request_time = started.elapsed().as_secs_f64();
    ovcc_telemetry::metrics::record(&access);
    ovcc_telemetry::access::emit(&access);
    written??;
    Ok(())
}

async fn grant_path(
    candidate: &ovcc_storage::PathCandidate,
    duration: u64,
) -> ovcc_core::Result<Option<ovcc_storage::PathLeaseGrant>> {
    if let Some(grant) = candidate.grant(duration)? {
        return Ok(Some(grant));
    }
    // Cleanup may run after preparation yields back to the IPC task.
    let candidate = candidate.clone();
    blocking(move || candidate.reprepare_and_grant(duration)).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_get_path_grant_survives_cleanup() -> std::result::Result<(), BoxError> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let store = ovcc_storage::DiskStorage::open(root.path())?;
        let mut writer = store.begin_fill("object", StoredMeta::default())?;
        writer.write_chunk(b"complete body")?;
        let committed = writer.commit()?;
        let ovcc_storage::LocalPathLookup::Hit(candidate) = store.prepare_local_path("object")?
        else {
            panic!("missing committed candidate");
        };
        assert_eq!(candidate.meta.revision, committed.meta.revision);

        // Act.
        store.maintain_local_leases()?;
        assert!(candidate.grant(30_000)?.is_none());
        let grant = grant_path(&candidate, 30_000)
            .await?
            .ok_or("grant denied")?;
        store.maintain_local_leases()?;

        // Assert.
        assert_eq!(std::fs::read(grant.path)?, b"complete body");
        Ok(())
    }

    #[tokio::test]
    async fn local_get_path_grant_recovers_after_alias_io_error()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let root = tempfile::tempdir()?;
        let store = ovcc_storage::DiskStorage::open(root.path())?;
        let mut writer = store.begin_fill("object", StoredMeta::default())?;
        writer.write_chunk(b"complete body")?;
        drop(writer.commit()?);
        let ovcc_storage::LocalPathLookup::Hit(candidate) = store.prepare_local_path("object")?
        else {
            return Err("missing candidate".into());
        };
        store.maintain_local_leases()?;
        let directory = root.path().join("local-leases");
        std::fs::remove_dir(&directory)?;
        std::fs::write(&directory, b"blocks alias creation")?;
        // Act.
        let failed = grant_path(&candidate, 30_000).await;
        std::fs::remove_file(&directory)?;
        let recovered = grant_path(&candidate, 30_000).await?;
        // Assert.
        assert!(
            failed.is_err(),
            "filesystem errors must not become an ordinary denial"
        );
        assert_eq!(
            std::fs::read(recovered.ok_or("grant denied after repair")?.path)?,
            b"complete body"
        );
        Ok(())
    }

    #[derive(Default)]
    struct RecoveryClock {
        gate: Mutex<Option<RecoveryGate>>,
    }

    struct RecoveryGate {
        skip: usize,
        entered: oneshot::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    }

    impl ovcc_storage::LeaseClock for RecoveryClock {
        fn now_millis(&self) -> u64 {
            let gate = {
                let mut gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(pending) = gate.as_mut()
                    && pending.skip > 0
                {
                    pending.skip -= 1;
                    return 0;
                }
                gate.take()
            };
            if let Some(gate) = gate {
                let _ = gate.entered.send(());
                let _ = gate.resume.recv();
            }
            0
        }
    }

    #[tokio::test]
    async fn local_get_cancelled_recovery_keeps_worker_until_joined()
    -> std::result::Result<(), BoxError> {
        // Arrange. Cleanup removes the ungranted alias before the request enters recovery.
        let root = tempfile::tempdir()?;
        let clock = Arc::new(RecoveryClock::default());
        let store = ovcc_storage::DiskStorage::open_with_lease_clock(root.path(), clock.clone())?;
        let mut writer = store.begin_fill("object", StoredMeta::default())?;
        writer.write_chunk(b"complete body")?;
        drop(writer.commit()?);
        let ovcc_storage::LocalPathLookup::Hit(candidate) = store.prepare_local_path("object")?
        else {
            return Err("missing candidate".into());
        };
        store.maintain_local_leases()?;
        let (entered, at_grant) = oneshot::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        // The first clock read is the failed fast grant; only recovery is gated.
        assert!(candidate.grant(30_000)?.is_none());
        *clock.gate.lock().map_err(|_| "gate poisoned")? = Some(RecoveryGate {
            skip: 1,
            entered,
            resume: paused,
        });

        WORKERS
            .scope(RefCell::new(Vec::new()), async {
                let unblock = resume;
                // Act. Cancellation drops the request while its blocking worker owns storage.
                let mut request = Box::pin(grant_path(&candidate, 30_000));
                tokio::select! {
                    _ = &mut request => return Err("request finished before recovery gate".into()),
                    entered = timeout(Duration::from_secs(10), at_grant) => entered??,
                }
                drop(request);
                drop(store);
                let workers = WORKERS.with(|workers| std::mem::take(&mut *workers.borrow_mut()));
                // Assert. Shutdown still owns the cancelled request's real storage worker.
                assert_eq!(workers.len(), 1);
                assert!(!workers[0].is_finished());
                assert!(ovcc_storage::DiskStorage::open(root.path()).is_err());
                unblock.send(())?;
                for worker in workers {
                    worker.await?;
                }
                drop(ovcc_storage::DiskStorage::open(root.path())?);
                Ok::<_, BoxError>(())
            })
            .await
    }

    #[test]
    fn local_get_authority_rejects_userinfo_and_accepts_host_ports() {
        // Arrange.
        for (authority, valid) in [
            ("user@localhost", false),
            ("user:password@localhost:80", false),
            ("localhost:80", true),
            ("[::1]:80", true),
        ] {
            let mut request = LeaseRequest::new(authority, "/object");
            request.headers.push(("If-Match".into(), "\"v1\"".into()));
            // Act.
            let result = request_head(request);
            // Assert.
            assert_eq!(result.is_ok(), valid);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn local_accept_error_backoff_keeps_other_work_ready() {
        // Arrange.
        let mut retry_at = tokio::time::Instant::now();
        let failure = async { Err::<(), _>(io::ErrorKind::Other.into()) };
        // Act.
        assert!(accept_after(&mut retry_at, failure).await.is_err());
        let other_ready = tokio::select! {
            biased;
            _ = accept_after(&mut retry_at, async { Ok::<_, io::Error>(()) }) => false,
            _ = std::future::ready(()) => true,
        };
        // Assert.
        assert!(other_ready);
        assert_eq!(
            retry_at - tokio::time::Instant::now(),
            Duration::from_millis(100)
        );
    }

    #[tokio::test]
    async fn local_get_shutdown_joins_blocking_lookup_before_reopen()
    -> std::result::Result<(), BoxError> {
        // Arrange.
        let root = tempfile::tempdir()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
        }
        #[cfg(target_os = "linux")]
        let endpoint = root.path().join("local.sock");
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\ovcc-shutdown-{:032x}",
            rand::random::<u128>()
        ));
        let cache_root = root.path().join("cache");
        let mut config = Config::default();
        config.tls.enabled = false;
        config.service.address = "127.0.0.1".into();
        config.service.port = 0;
        config.service.local_mode = true;
        config.service.local_endpoint = Some(endpoint.to_string_lossy().into_owned());
        config.backends = vec![Backend {
            name: "local".into(),
            host: "lease-shutdown.test".into(),
            upstream: "http://127.0.0.1:9".into(),
            requires_versioning: true,
            ..Backend::default()
        }];
        let serving = serve(config, cache_root.clone()).await?;
        let mut request = LeaseRequest::new("lease-shutdown.test", "/object");
        request.headers.push(("If-Match".into(), "\"v1\"".into()));
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, receiver) = std::sync::mpsc::channel();
        *LOOKUP_GATE.lock().map_err(|_| "lookup gate poisoned")? = Some(LookupGate {
            key: "0|lease-shutdown.test|GET|/object||\"v1\"|".into(),
            entered,
            release: receiver,
        });
        let client = tokio::spawn(async move { ovcc_local::lease(&endpoint, &request).await });
        timeout(Duration::from_secs(5), started).await??;
        // Act.
        let mut stopped = tokio::spawn(serving.shutdown());
        let early_return = timeout(Duration::from_millis(100), &mut stopped).await;
        release.send(())?;
        // Assert.
        if early_return.is_err() {
            timeout(Duration::from_secs(10), stopped).await??;
        }
        let _ = timeout(Duration::from_secs(10), client).await?;
        assert!(
            early_return.is_err(),
            "shutdown detached a cache-owning lookup"
        );
        drop(DiskStorage::open(&cache_root)?);
        Ok(())
    }
}
