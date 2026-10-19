// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::{LeaseResponse, Outcome, PROTOCOL_VERSION, read_request, write_response};
use ovcc_platform::local::Listener;
use std::collections::BTreeSet;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Call = Pin<Box<dyn Future<Output = io::Result<LeaseResult>> + Send>>;

enum Reply {
    Ready,
    Rejected,
    Malformed,
}

struct Event {
    call: usize,
    connection: usize,
    reply: oneshot::Sender<Reply>,
}

struct Rig {
    _root: tempfile::TempDir,
    endpoint: PathBuf,
    opened: Arc<AtomicUsize>,
    events: mpsc::UnboundedReceiver<Event>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Rig {
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        #[cfg(target_os = "linux")]
        let endpoint = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
            root.path().join("pool.sock")
        };
        #[cfg(windows)]
        let endpoint = PathBuf::from(format!(
            r"\\.\pipe\ovcc-pool-{}-{}",
            std::process::id(),
            root.path()
                .file_name()
                .ok_or("temp name")?
                .to_string_lossy()
        ));
        let mut listener = Listener::bind(&endpoint)?;
        let opened = Arc::new(AtomicUsize::new(0));
        let counter = opened.clone();
        let (send, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            while let Ok(mut stream) = listener.accept().await {
                let connection = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let send = send.clone();
                children.spawn(async move {
                    while let Ok(request) = read_request(&mut stream).await {
                        let Ok(call) = request.path.trim_start_matches('/').parse::<usize>() else {
                            break;
                        };
                        let (reply, receive) = oneshot::channel();
                        if send
                            .send(Event {
                                call,
                                connection,
                                reply,
                            })
                            .is_err()
                        {
                            break;
                        }
                        let Ok(reply) = receive.await else { break };
                        let rejected = matches!(reply, Reply::Rejected);
                        let result = if !matches!(reply, Reply::Malformed) {
                            write_response(
                                &mut stream,
                                &LeaseResponse {
                                    version: PROTOCOL_VERSION,
                                    outcome: if rejected {
                                        Outcome::Rejected
                                    } else {
                                        Outcome::Ready
                                    },
                                    status: if rejected { 403 } else { 200 },
                                    headers: vec![],
                                    cache_status: Some("HIT".into()),
                                    generation: call as u64,
                                    offset: 0,
                                    length: 0,
                                    path: (!rejected).then(|| {
                                        std::env::temp_dir().join("ovcc-pool-metadata-only.raw")
                                    }),
                                    deadline_unix_millis: u64::MAX,
                                },
                            )
                            .await
                        } else {
                            stream.write_frame(&[0xff]).await
                        };
                        if result.is_err() {
                            break;
                        }
                    }
                });
                while children.try_join_next().is_some() {}
            }
        });
        Ok(Self {
            _root: root,
            endpoint,
            opened,
            events,
            task,
        })
    }

    async fn receive(&mut self, call: &mut Call) -> TestResult<Event> {
        timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = call => Err(format!("call completed before peer reply: {}", result.is_ok()).into()),
                event = self.events.recv() => event.ok_or_else(|| "peer stopped".into()),
            }
        }).await?
    }
}

fn call(client: &SharedClient, id: usize) -> Call {
    let client = client.clone();
    Box::pin(async move {
        client
            .lease(&LeaseRequest::new("pool.test", format!("/{id}")))
            .await
    })
}

async fn poll_once(call: &mut Call) -> Poll<io::Result<LeaseResult>> {
    poll_fn(|cx| Poll::Ready(call.as_mut().poll(cx))).await
}

async fn complete(call: Call, event: Event) -> TestResult<LeaseResult> {
    event.reply.send(Reply::Ready).map_err(|_| "peer stopped")?;
    Ok(timeout(Duration::from_secs(5), call).await??)
}

fn numbered(result: &LeaseResult) -> TestResult<u64> {
    match result {
        LeaseResult::Ready(lease) => Ok(lease.response().generation),
        LeaseResult::Unavailable(_) => Err("expected numbered result".into()),
    }
}

#[test]
fn local_get_shared_pool_graph_integrity() -> TestResult {
    // Arrange.
    let graph: serde_json::Value = serde_json::from_str(include_str!(
        "../../skills/ovcc-local-lease/fixtures/SharedPool.graph.json"
    ))?;
    let report: serde_json::Value = serde_json::from_str(include_str!(
        "../../skills/ovcc-local-lease/fixtures/SharedPool.scenarios.json"
    ))?;
    let edges = graph["edges"].as_array().ok_or("edges")?;
    let mut covered = BTreeSet::new();
    // Act.
    for scenario in report["scenarios"].as_array().ok_or("scenarios")? {
        let mut node = graph["initial"].as_u64().ok_or("initial")?;
        for id in scenario["trace_edge_ids"].as_array().ok_or("trace")? {
            let edge = &edges[id.as_u64().ok_or("edge")? as usize];
            assert_eq!(edge["from"], node);
            node = edge["to"].as_u64().ok_or("target")?;
        }
        for id in scenario["raw_edge_ids"].as_array().ok_or("raw edges")? {
            assert!(covered.insert(id.as_u64().ok_or("edge")? as usize));
        }
    }
    // Assert.
    assert_eq!(covered, (0..edges.len()).collect());
    assert_eq!(report["mapped_edge_count"], edges.len());
    Ok(())
}

#[tokio::test]
async fn local_get_shared_pool_model_conformance() -> TestResult {
    // Arrange.
    let graph: serde_json::Value = serde_json::from_str(include_str!(
        "../../skills/ovcc-local-lease/fixtures/SharedPool.graph.json"
    ))?;
    let report: serde_json::Value = serde_json::from_str(include_str!(
        "../../skills/ovcc-local-lease/fixtures/SharedPool.scenarios.json"
    ))?;
    let edges = graph["edges"].as_array().ok_or("edges")?;
    let nodes = graph["nodes"].as_array().ok_or("nodes")?;
    let mut covered = BTreeSet::new();
    for scenario in report["scenarios"].as_array().ok_or("scenarios")? {
        let mut rig = Rig::new()?;
        let pool = SharedClient::with_endpoint(&rig.endpoint, 1)?;
        let mut calls: [Option<Call>; 2] = [None, None];
        let mut events: [Option<Event>; 2] = [None, None];
        let mut phase = ["new"; 2];
        let mut connection = [0; 2];
        let mut received = [0_u64; 2];
        let mut issued = 0;
        let mut witness = None;
        for id in scenario["trace_edge_ids"].as_array().ok_or("trace")? {
            let edge = &edges[id.as_u64().ok_or("edge id")? as usize];
            let action = edge["action"]["name"].as_str().ok_or("action")?;
            let c = edge["action"]["args"][0].as_u64().ok_or("call")? as usize - 1;
            // Act.
            match action {
                "Start" | "Resume" => {
                    let blocked = pool.inner.capacity.available_permits() == 0;
                    if action == "Start" {
                        calls[c] = Some(call(&pool, c + 1));
                        issued += 1;
                    }
                    let future = calls[c].as_mut().ok_or("call future")?;
                    if action == "Start" && blocked {
                        assert!(poll_once(future).await.is_pending());
                        phase[c] = "waiting";
                    } else {
                        let event = rig.receive(future).await?;
                        assert_eq!(event.call, c + 1);
                        connection[c] = event.connection;
                        events[c] = Some(event);
                        phase[c] = "busy";
                    }
                }
                "Reply" => {
                    let result = complete(
                        calls[c].take().ok_or("call")?,
                        events[c].take().ok_or("event")?,
                    )
                    .await?;
                    received[c] = numbered(&result)?;
                    phase[c] = "done";
                }
                "Fail" => {
                    events[c]
                        .take()
                        .ok_or("event")?
                        .reply
                        .send(Reply::Malformed)
                        .map_err(|_| "peer stopped")?;
                    let result =
                        timeout(Duration::from_secs(5), calls[c].take().ok_or("call")?).await?;
                    assert!(result.is_err_and(|error| error.kind() == io::ErrorKind::InvalidData));
                    phase[c] = "failed";
                    if id == &scenario["representative_edge_id"] {
                        witness = Some("malformed-pool-reply");
                    }
                }
                "Cancel" => {
                    drop(calls[c].take().ok_or("call")?);
                    if let Some(event) = events[c].take() {
                        let _ = event.reply.send(Reply::Ready);
                    }
                    phase[c] = "cancelled";
                    if id == &scenario["representative_edge_id"] {
                        witness = Some("dropped-pool-call");
                    }
                }
                unknown => return Err(format!("unknown action {unknown}").into()),
            }
            // Assert.
            let observed = serde_json::json!({
                "phase": phase, "issued": issued, "idle": !pool.inner.idle().is_empty(),
                "opened": rig.opened.load(Ordering::SeqCst),
                "connection": connection, "received": received,
            });
            let target = edge["to"].as_u64().ok_or("target")? as usize;
            assert_eq!(
                observed, nodes[target]["state"],
                "scenario {} edge {id}",
                scenario["id"]
            );
        }
        assert_eq!(witness, scenario["fault_witness"].as_str());
        for id in scenario["raw_edge_ids"].as_array().ok_or("raw edges")? {
            assert!(covered.insert(id.as_u64().ok_or("raw edge")?));
        }
    }
    assert_eq!(covered.len(), edges.len());
    Ok(())
}

#[tokio::test]
async fn local_get_shared_pool_uses_idle_capacity_without_round_robin() -> TestResult {
    for cap in [1, 2, 4, 8] {
        // Arrange.
        let mut rig = Rig::new()?;
        let pool = SharedClient::with_endpoint(&rig.endpoint, cap)?;
        let mut active = Vec::new();
        for id in 1..=cap {
            let mut future = call(&pool, id);
            let event = rig.receive(&mut future).await?;
            active.push((future, event));
        }
        let mut queued = call(&pool.clone(), cap + 1);
        // Act.
        assert!(poll_once(&mut queued).await.is_pending());
        assert_eq!(pool.inner.capacity.available_permits(), 0);
        assert_eq!(rig.opened.load(Ordering::SeqCst), cap);
        let (future, event) = active.pop().ok_or("active request")?;
        let reusable = event.connection;
        let held_metadata = complete(future, event).await?;
        let event = rig.receive(&mut queued).await?;
        // Assert.
        assert_eq!(event.connection, reusable);
        assert_eq!(numbered(&complete(queued, event).await?)?, (cap + 1) as u64);
        assert_eq!(numbered(&held_metadata)?, cap as u64);
        for (future, event) in active {
            complete(future, event).await?;
        }
        assert_eq!(rig.opened.load(Ordering::SeqCst), cap);
        assert_eq!(pool.inner.idle().len(), cap);
        assert_eq!(pool.inner.capacity.available_permits(), cap);
    }
    Ok(())
}

#[tokio::test]
async fn local_get_shared_pool_timeouts_release_capacity() -> TestResult {
    // Arrange.
    let mut rig = Rig::new()?;
    let pool = SharedClient::with_endpoint(&rig.endpoint, 1)?;
    let mut first = call(&pool, 1);
    let first_event = rig.receive(&mut first).await?;
    // Act.
    assert!(
        timeout(Duration::from_millis(5), call(&pool, 2))
            .await
            .is_err()
    );
    assert!(timeout(Duration::from_millis(5), first).await.is_err());
    let _ = first_event.reply.send(Reply::Ready);
    let mut next = call(&pool, 3);
    let event = rig.receive(&mut next).await?;
    // Assert.
    assert_eq!(event.connection, 2);
    assert_eq!(event.call, 3);
    assert_eq!(numbered(&complete(next, event).await?)?, 3);
    assert_eq!(pool.inner.capacity.available_permits(), 1);
    assert_eq!(pool.inner.idle().len(), 1);
    Ok(())
}

#[tokio::test]
async fn local_get_shared_pool_denial_keeps_connection_reusable() -> TestResult {
    // Arrange.
    let mut rig = Rig::new()?;
    let pool = SharedClient::with_endpoint(&rig.endpoint, 1)?;
    let mut first = call(&pool, 1);
    let event = rig.receive(&mut first).await?;
    // Act.
    event
        .reply
        .send(Reply::Rejected)
        .map_err(|_| "peer stopped")?;
    let denied = timeout(Duration::from_secs(5), first).await??;
    let mut next = call(&pool, 2);
    let event = rig.receive(&mut next).await?;
    // Assert.
    assert!(
        matches!(denied, LeaseResult::Unavailable(reply) if reply.status == 403 && reply.path.is_none())
    );
    assert_eq!(event.connection, 1);
    assert_eq!(numbered(&complete(next, event).await?)?, 2);
    assert_eq!(rig.opened.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn local_get_shared_pool_connect_failure_does_not_leak_capacity() -> TestResult {
    // Arrange.
    let rig = Rig::new()?;
    let endpoint = rig.endpoint.clone();
    rig.task.abort();
    while !rig.task.is_finished() {
        tokio::task::yield_now().await;
    }
    let pool = SharedClient::with_endpoint(&endpoint, 1)?;
    // Act.
    for _ in 0..2 {
        let result = timeout(Duration::from_secs(5), call(&pool, 1)).await?;
        // Assert.
        assert!(result.is_err());
        assert_eq!(pool.inner.capacity.available_permits(), 1);
        assert!(pool.inner.idle().is_empty());
    }
    Ok(())
}

#[test]
fn local_get_shared_pool_rejects_invalid_limits() {
    // Arrange.
    let limits = [0, Semaphore::MAX_PERMITS + 1];
    // Act.
    for limit in limits {
        let result = SharedClient::new(limit);
        // Assert.
        assert!(result.is_err_and(|error| error.kind() == io::ErrorKind::InvalidInput));
    }
}
