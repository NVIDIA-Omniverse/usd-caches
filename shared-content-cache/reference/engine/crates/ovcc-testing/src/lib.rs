// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Compile-time-gated test control primitives for OVCC.
//!
//! This crate is linked only by the `test-fault-injection` build. It owns one
//! process-local fault slot; effect-specific crates claim and consume that slot.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Highest accepted number of matching requests to pass before injection.
pub const MAX_SKIP_MATCHES: u64 = 10_000;
/// Highest accepted request-body byte offset for the initial upstream fault.
pub const MAX_AFTER_BYTES: u64 = 64 * 1024 * 1024;

/// State of the single process-local fault slot.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum ControllerState {
    /// No rule is configured.
    Idle,
    /// A rule is waiting for matching requests.
    Armed,
    /// One request exclusively owns the rule.
    Claimed,
    /// The owner emitted the configured fault.
    Fired,
}

/// Serializable state returned by the test control API.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    /// Current controller state.
    pub controller_state: ControllerState,
    /// Monotonic rule generation.
    pub generation: u64,
    /// Matching requests that must still pass before one claims the rule.
    pub skips_remaining: u64,
    /// Claims in the current generation; always zero or one.
    pub claims: u8,
    /// Fired effects in the current generation; always zero or one.
    pub fires: u8,
    /// Fired generations since process start.
    pub total_fires: u64,
    /// Configured body offset, absent while idle.
    pub after_bytes: Option<u64>,
    /// Request-body bytes emitted to Hyper before the fault, present only after firing.
    pub body_bytes_emitted: Option<u64>,
}

/// Validation or state-transition failure from the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerError {
    /// A rule is already armed, claimed, or awaiting evidence cleanup.
    Busy,
    /// Clear was requested while no rule existed.
    Idle,
    /// The skip count exceeds [`MAX_SKIP_MATCHES`].
    SkipMatchesOutOfRange,
    /// The byte offset exceeds [`MAX_AFTER_BYTES`].
    AfterBytesOutOfRange,
    /// The generation counter cannot advance.
    GenerationExhausted,
    /// The controller lock was poisoned by a panic.
    Poisoned,
    /// A lease attempted to fire after its generation stopped being claimed.
    StaleLease,
}

impl fmt::Display for ControllerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Busy => "a fault generation is already active",
            Self::Idle => "the fault controller is already idle",
            Self::SkipMatchesOutOfRange => "skip_matches exceeds the supported bound",
            Self::AfterBytesOutOfRange => "after_bytes exceeds the supported bound",
            Self::GenerationExhausted => "the fault generation counter is exhausted",
            Self::Poisoned => "the fault controller lock is poisoned",
            Self::StaleLease => "the fault lease no longer owns the active generation",
        })
    }
}

impl std::error::Error for ControllerError {}

/// Carries the controller failure into the shared error type without losing its
/// rendered detail, so a caller working in [`ovcc_core::Result`] can use `?`.
impl From<ControllerError> for ovcc_core::Error {
    fn from(error: ControllerError) -> Self {
        Self::internal(error.to_string())
    }
}

#[derive(Debug)]
struct Inner {
    state: ControllerState,
    generation: u64,
    skips_remaining: u64,
    claims: u8,
    fires: u8,
    total_fires: u64,
    after_bytes: Option<u64>,
    body_bytes_emitted: Option<u64>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            state: ControllerState::Idle,
            generation: 0,
            skips_remaining: 0,
            claims: 0,
            fires: 0,
            total_fires: 0,
            after_bytes: None,
            body_bytes_emitted: None,
        }
    }
}

impl Inner {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            controller_state: self.state,
            generation: self.generation,
            skips_remaining: self.skips_remaining,
            claims: self.claims,
            fires: self.fires,
            total_fires: self.total_fires,
            after_bytes: self.after_bytes,
            body_bytes_emitted: self.body_bytes_emitted,
        }
    }
}

/// One process-local, concurrency-safe fault slot.
#[derive(Debug, Default)]
pub struct FaultController {
    inner: Mutex<Inner>,
}

/// Result of presenting one matching request to an armed controller.
#[derive(Debug)]
pub enum MatchOutcome {
    /// No armed generation was available to this request.
    NotClaimed,
    /// The request passed through while consuming one configured skip.
    Skipped(Snapshot),
    /// The request exclusively owns the generation and may fire it once.
    Claimed(FaultLease),
}

/// Exclusive right for one request to emit the armed fault.
#[derive(Debug)]
pub struct FaultLease {
    controller: Arc<FaultController>,
    generation: u64,
    after_bytes: u64,
}

impl FaultLease {
    /// Generation owned by this lease.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Request-body offset at which this lease should fire.
    pub const fn after_bytes(&self) -> u64 {
        self.after_bytes
    }

    /// Records the terminal fault receipt and consumes this lease.
    pub fn fire(self, body_bytes_emitted: u64) -> Result<Snapshot, ControllerError> {
        let mut inner = self.controller.lock()?;
        if inner.state != ControllerState::Claimed || inner.generation != self.generation {
            return Err(ControllerError::StaleLease);
        }
        let total_fires = inner
            .total_fires
            .checked_add(1)
            .ok_or(ControllerError::GenerationExhausted)?;
        inner.state = ControllerState::Fired;
        inner.fires = 1;
        inner.total_fires = total_fires;
        inner.body_bytes_emitted = Some(body_bytes_emitted);
        Ok(inner.snapshot())
    }
}

impl FaultController {
    fn lock(&self) -> Result<MutexGuard<'_, Inner>, ControllerError> {
        self.inner.lock().map_err(|_| ControllerError::Poisoned)
    }

    /// Arms a new request-body fault generation.
    pub fn arm(&self, skip_matches: u64, after_bytes: u64) -> Result<Snapshot, ControllerError> {
        if skip_matches > MAX_SKIP_MATCHES {
            return Err(ControllerError::SkipMatchesOutOfRange);
        }
        if after_bytes > MAX_AFTER_BYTES {
            return Err(ControllerError::AfterBytesOutOfRange);
        }
        let mut inner = self.lock()?;
        if inner.state != ControllerState::Idle {
            return Err(ControllerError::Busy);
        }
        inner.generation = inner
            .generation
            .checked_add(1)
            .ok_or(ControllerError::GenerationExhausted)?;
        inner.state = ControllerState::Armed;
        inner.skips_remaining = skip_matches;
        inner.claims = 0;
        inner.fires = 0;
        inner.after_bytes = Some(after_bytes);
        inner.body_bytes_emitted = None;
        Ok(inner.snapshot())
    }

    /// Presents one matching request, atomically skipping or claiming it.
    pub fn claim_matching(self: &Arc<Self>) -> Result<MatchOutcome, ControllerError> {
        let mut inner = self.lock()?;
        if inner.state != ControllerState::Armed {
            return Ok(MatchOutcome::NotClaimed);
        }
        if inner.skips_remaining > 0 {
            inner.skips_remaining -= 1;
            return Ok(MatchOutcome::Skipped(inner.snapshot()));
        }
        let after_bytes = inner.after_bytes.ok_or(ControllerError::Poisoned)?;
        inner.state = ControllerState::Claimed;
        inner.claims = 1;
        Ok(MatchOutcome::Claimed(FaultLease {
            controller: Arc::clone(self),
            generation: inner.generation,
            after_bytes,
        }))
    }

    /// Clears the current generation after its evidence has been inspected.
    pub fn clear(&self) -> Result<Snapshot, ControllerError> {
        let mut inner = self.lock()?;
        if inner.state == ControllerState::Idle {
            return Err(ControllerError::Idle);
        }
        inner.state = ControllerState::Idle;
        inner.skips_remaining = 0;
        inner.claims = 0;
        inner.fires = 0;
        inner.after_bytes = None;
        inner.body_bytes_emitted = None;
        Ok(inner.snapshot())
    }

    /// Returns an immutable observation of the current controller state.
    pub fn snapshot(&self) -> Result<Snapshot, ControllerError> {
        Ok(self.lock()?.snapshot())
    }
}

/// Shared controller used by the deployed test-enabled process.
pub fn global_controller() -> &'static Arc<FaultController> {
    static CONTROLLER: OnceLock<Arc<FaultController>> = OnceLock::new();
    CONTROLLER.get_or_init(|| Arc::new(FaultController::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::{BTreeMap, BTreeSet};
    use std::thread;

    const GRAPH_JSON: &str =
        include_str!("../skills/ovcc-fault-arming/fixtures/FaultArming.graph.json");
    const SCENARIOS_JSON: &str =
        include_str!("../skills/ovcc-fault-arming/fixtures/FaultArming.scenarios.json");

    #[derive(Debug, Deserialize)]
    struct Graph {
        model: String,
        initial: usize,
        edge_count: usize,
        nodes: Vec<Node>,
        edges: Vec<Edge>,
    }

    #[derive(Debug, Deserialize)]
    struct Node {
        id: usize,
        state: ModelState,
    }

    #[derive(Clone, Debug, Deserialize)]
    struct Edge {
        id: usize,
        from: usize,
        to: usize,
        action: Action,
    }

    #[derive(Clone, Debug, Deserialize)]
    struct Action {
        name: String,
        args: Vec<u64>,
    }

    #[derive(Debug, Deserialize)]
    struct ScenarioReport {
        model: String,
        raw_edge_count: usize,
        mapped_edge_count: usize,
        unmapped_edge_ids: Vec<usize>,
        action_summary: BTreeMap<String, ActionSummary>,
        scenarios: Vec<Scenario>,
    }

    #[derive(Debug, Deserialize)]
    struct ActionSummary {
        mode: String,
        raw_edges: usize,
        representative_scenarios: usize,
        fault_witnesses: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct Scenario {
        id: usize,
        raw_edge_ids: Vec<usize>,
        representative_edge_id: usize,
        trace_edge_ids: Vec<usize>,
        mode: String,
        fault_witness: Option<String>,
        expected_state: ModelState,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
    struct ModelState {
        controller_state: ControllerState,
        generation: u64,
        skips_remaining: u64,
        claims: u8,
        fires: u8,
        total_fires: u64,
    }

    struct Adapter {
        controller: Arc<FaultController>,
        lease: Option<FaultLease>,
    }

    impl Adapter {
        fn new() -> Self {
            Self {
                controller: Arc::new(FaultController::default()),
                lease: None,
            }
        }

        fn execute(&mut self, action: &Action) -> &'static str {
            match action.name.as_str() {
                "Arm" => {
                    self.controller
                        .arm(action.args[0], 4)
                        .expect("arm modeled generation");
                    "production"
                }
                "SkipMatch" => {
                    assert!(matches!(
                        self.controller.claim_matching(),
                        Ok(MatchOutcome::Skipped(_))
                    ));
                    "production"
                }
                "ClaimMatch" => {
                    let Ok(MatchOutcome::Claimed(lease)) = self.controller.claim_matching() else {
                        panic!("modeled request did not claim the armed generation");
                    };
                    self.lease = Some(lease);
                    "production"
                }
                "Fire" => {
                    self.lease
                        .take()
                        .expect("claimed modeled lease")
                        .fire(4)
                        .expect("fire modeled generation");
                    "environment"
                }
                "Clear" => {
                    self.lease = None;
                    self.controller.clear().expect("clear modeled generation");
                    "production"
                }
                other => panic!("unknown FaultArming action {other}"),
            }
        }

        fn observe(&self) -> ModelState {
            let snapshot = self.controller.snapshot().expect("observe controller");
            ModelState {
                controller_state: snapshot.controller_state,
                generation: snapshot.generation,
                skips_remaining: snapshot.skips_remaining,
                claims: snapshot.claims,
                fires: snapshot.fires,
                total_fires: snapshot.total_fires,
            }
        }
    }

    #[test]
    fn faultarming_model_scenarios_conform_to_controller() {
        // Arrange.
        let graph: Graph = serde_json::from_str(GRAPH_JSON).expect("valid canonical graph");
        let report: ScenarioReport =
            serde_json::from_str(SCENARIOS_JSON).expect("valid scenario report");

        // Act.
        let mapped = report
            .scenarios
            .iter()
            .flat_map(|scenario| scenario.raw_edge_ids.iter().copied())
            .collect::<BTreeSet<_>>();

        // Assert.
        assert_eq!(graph.model, "FaultArming");
        assert_eq!(report.model, graph.model);
        assert_eq!(graph.edge_count, graph.edges.len());
        assert_eq!(report.raw_edge_count, graph.edge_count);
        assert_eq!(report.mapped_edge_count, graph.edge_count);
        assert!(report.unmapped_edge_ids.is_empty());
        assert_eq!(
            graph.nodes.iter().map(|node| node.id).collect::<Vec<_>>(),
            (0..graph.nodes.len()).collect::<Vec<_>>()
        );
        assert_eq!(mapped, (0..graph.edge_count).collect());
        assert_eq!(report.scenarios.len(), 36);
        assert_eq!(report.action_summary["Fire"].fault_witnesses.len(), 1);
        assert!(
            report
                .action_summary
                .values()
                .all(|summary| summary.mode != "model-only"
                    && summary.raw_edges == summary.representative_scenarios)
        );

        for scenario in &report.scenarios {
            let mut adapter = Adapter::new();
            assert_eq!(adapter.observe(), graph.nodes[graph.initial].state);
            let mut current = graph.initial;
            let mut final_kind = None;
            for edge_id in &scenario.trace_edge_ids {
                let edge = &graph.edges[*edge_id];
                assert_eq!(edge.id, *edge_id);
                assert_eq!(edge.from, current, "scenario {} continuity", scenario.id);
                final_kind = Some(adapter.execute(&edge.action));
                assert_eq!(
                    adapter.observe(),
                    graph.nodes[edge.to].state,
                    "scenario {} edge {}",
                    scenario.id,
                    edge.id
                );
                current = edge.to;
            }
            assert_eq!(final_kind, Some(scenario.mode.as_str()));
            if scenario.mode == "environment" {
                assert_eq!(
                    scenario.fault_witness.as_deref(),
                    Some("upstream-request-body-error"),
                    "scenario {} must carry a deterministic fault witness",
                    scenario.id
                );
            } else {
                assert!(
                    scenario.fault_witness.is_none(),
                    "scenario {} must not carry a fault witness",
                    scenario.id
                );
            }
            let target = graph.edges[scenario.representative_edge_id].to;
            assert_eq!(scenario.expected_state, graph.nodes[target].state);
        }
    }

    #[test]
    fn fault_controller_rejects_overwrite_until_clear() {
        // Arrange.
        let controller = FaultController::default();

        // Act.
        controller.arm(0, 1).expect("initial arm");
        let second = controller.arm(0, 1);

        // Assert.
        assert!(matches!(second, Err(ControllerError::Busy)));
    }

    #[test]
    fn fault_controller_concurrent_matches_have_one_owner() {
        // Arrange.
        let controller = Arc::new(FaultController::default());
        controller.arm(0, 1).expect("arm concurrent fault");
        let barrier = Arc::new(std::sync::Barrier::new(17));

        // Act.
        // Collection starts every claimant before any join; a lazy chain would
        // serialize the supposedly concurrent ownership test.
        #[allow(clippy::needless_collect)]
        let handles = (0..16)
            .map(|_| {
                let controller = Arc::clone(&controller);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    controller.claim_matching()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().expect("join claimant"))
            .collect::<Result<Vec<_>, _>>()
            .expect("claim outcomes");

        // Assert.
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, MatchOutcome::Claimed(_)))
                .count(),
            1
        );
        assert_eq!(controller.snapshot().expect("snapshot").claims, 1);
    }

    #[test]
    fn fault_controller_skips_exactly_the_configured_matches() {
        // Arrange.
        let controller = Arc::new(FaultController::default());
        controller.arm(2, 1).expect("arm skipped fault");

        // Act.
        let first = controller.claim_matching().expect("first match");
        let second = controller.claim_matching().expect("second match");
        let third = controller.claim_matching().expect("third match");

        // Assert.
        assert!(matches!(first, MatchOutcome::Skipped(_)));
        assert!(matches!(second, MatchOutcome::Skipped(_)));
        assert!(matches!(third, MatchOutcome::Claimed(_)));
        assert_eq!(controller.snapshot().expect("snapshot").claims, 1);
    }

    #[test]
    fn fault_controller_rejects_parameters_outside_bounded_contract() {
        // Arrange.
        let controller = FaultController::default();

        // Act.
        let skips = controller.arm(MAX_SKIP_MATCHES + 1, 0);
        let bytes = controller.arm(0, MAX_AFTER_BYTES + 1);

        // Assert.
        assert!(matches!(skips, Err(ControllerError::SkipMatchesOutOfRange)));
        assert!(matches!(bytes, Err(ControllerError::AfterBytesOutOfRange)));
    }

    #[test]
    fn controller_failure_bridges_into_the_internal_error_kind() {
        // Arrange.
        let error = ControllerError::Busy;

        // Act.
        let bridged = ovcc_core::Error::from(error);

        // Assert.
        assert_eq!(bridged.kind(), ovcc_core::ErrorKind::Internal);
        assert_eq!(bridged.message(), "a fault generation is already active");
    }
}
