// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Bounded, positive-only authorization caching for credential digests.
//!
//! The caller owns credential parsing, digest construction, and the origin
//! probe. This crate retains only a fixed-size digest and an expiry, never raw
//! credentials or response bodies.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

/// Fixed-size identity derived from the complete credential-bearing request.
pub type CredentialDigest = [u8; 32];

// Positive witnesses require sufficient remaining lifetime and expire early.
const MIN_TTL_SECS: i64 = 10;
const EXPIRY_SAFETY_SECS: i64 = 2;

/// Result of checking one credential against its authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeOutcome {
    /// The authority accepted the credential. This is the only cached result.
    Authorized,
    /// The authority accepted this probe, but its response is unsuitable as a
    /// reusable positive witness. Current waiters may proceed; later requests
    /// must probe again.
    AuthorizedUncached,
    /// The authority rejected the credential.
    Rejected,
    /// Authorization could not be established.
    Unavailable,
}

struct Flight {
    id: u64,
    result: watch::Sender<Option<ProbeOutcome>>,
}

#[derive(Default)]
struct State {
    authorized: HashMap<CredentialDigest, i64>,
    expiry_order: VecDeque<(CredentialDigest, i64)>,
    in_flight: HashMap<CredentialDigest, Flight>,
    next_flight_id: u64,
}

/// Process-local positive authorization witnesses with bounded storage.
pub struct AuthorizationCache {
    state: Mutex<State>,
    max_entries: usize,
}

impl AuthorizationCache {
    /// Creates a bounded cache using the established expiry policy.
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            max_entries,
        }
    }

    /// Reuses a positive witness, joins an identical probe, or reserves a new
    /// probe for the caller.
    #[must_use]
    pub fn acquire(self: &Arc<Self>, key: CredentialDigest, now: i64) -> Acquire {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .authorized
            .get(&key)
            .is_some_and(|expires_at| *expires_at > now)
        {
            return Acquire::Authorized;
        }
        state.authorized.remove(&key);

        if let Some(flight) = state.in_flight.get(&key) {
            return Acquire::Wait(Waiter {
                result: flight.result.subscribe(),
            });
        }

        state.next_flight_id = state.next_flight_id.wrapping_add(1);
        let id = state.next_flight_id;
        let (result, _) = watch::channel(None);
        state.in_flight.insert(
            key,
            Flight {
                id,
                result: result.clone(),
            },
        );
        Acquire::Probe(ProbePermit {
            cache: Arc::clone(self),
            key,
            id,
            result,
            completed: false,
        })
    }

    fn complete(
        &self,
        key: CredentialDigest,
        id: u64,
        outcome: ProbeOutcome,
        credential_expires_at: i64,
        now: i64,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.in_flight.get(&key).map(|flight| flight.id) != Some(id) {
            return;
        }
        state.in_flight.remove(&key);

        if outcome == ProbeOutcome::Authorized
            && credential_expires_at.saturating_sub(now) >= MIN_TTL_SECS
        {
            let cache_expires_at = credential_expires_at.saturating_sub(EXPIRY_SAFETY_SECS);
            state.authorized.insert(key, cache_expires_at);
            state.expiry_order.push_back((key, cache_expires_at));
            while state.authorized.len() > self.max_entries {
                let Some((oldest_key, oldest_expiry)) = state.expiry_order.pop_front() else {
                    break;
                };
                if state.authorized.get(&oldest_key) == Some(&oldest_expiry) {
                    state.authorized.remove(&oldest_key);
                }
            }
            if state.expiry_order.len() > self.max_entries.saturating_mul(2) {
                state.expiry_order = state
                    .authorized
                    .iter()
                    .map(|(key, expiry)| (*key, *expiry))
                    .collect();
            }
        }
    }

    fn cancel(&self, key: CredentialDigest, id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.in_flight.get(&key).map(|flight| flight.id) == Some(id) {
            state.in_flight.remove(&key);
        }
    }
}

/// Decision returned when a request seeks authorization.
pub enum Acquire {
    /// An unexpired positive witness exists.
    Authorized,
    /// This caller owns the authority probe.
    Probe(ProbePermit),
    /// Another caller owns the identical authority probe.
    Wait(Waiter),
}

/// Wait handle for an identical in-flight authorization probe.
pub struct Waiter {
    result: watch::Receiver<Option<ProbeOutcome>>,
}

impl Waiter {
    /// Waits for the probe owner, treating cancellation as unavailable.
    pub async fn wait(mut self) -> ProbeOutcome {
        self.result
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|outcome| *outcome)
            .unwrap_or(ProbeOutcome::Unavailable)
    }
}

/// Ownership token for one authority probe.
pub struct ProbePermit {
    cache: Arc<AuthorizationCache>,
    key: CredentialDigest,
    id: u64,
    result: watch::Sender<Option<ProbeOutcome>>,
    completed: bool,
}

impl ProbePermit {
    /// Publishes the probe result and, when authorized, records a positive
    /// witness according to the cache expiry policy.
    pub fn complete(mut self, outcome: ProbeOutcome, credential_expires_at: i64, now: i64) {
        self.cache
            .complete(self.key, self.id, outcome, credential_expires_at, now);
        self.result.send_replace(Some(outcome));
        self.completed = true;
    }
}

impl Drop for ProbePermit {
    fn drop(&mut self) {
        if !self.completed {
            self.cache.cancel(self.key, self.id);
            self.result.send_replace(Some(ProbeOutcome::Unavailable));
        }
    }
}

#[cfg(test)]
mod presigned_authorization_state_machine_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_cache_acquire_positive_witness_expires_early() {
        let cache = Arc::new(AuthorizationCache::new(8));
        let allowed = [1_u8; 32];
        let denied = [2_u8; 32];

        let Acquire::Probe(allowed_probe) = cache.acquire(allowed, 100) else {
            panic!("new credential did not reserve an authorization probe");
        };
        allowed_probe.complete(ProbeOutcome::Authorized, 200, 100);
        let allowed_cached = cache.acquire(allowed, 101);
        let allowed_after_safety_window = cache.acquire(allowed, 198);

        let Acquire::Probe(denied_probe) = cache.acquire(denied, 100) else {
            panic!("new denied credential did not reserve an authorization probe");
        };
        denied_probe.complete(ProbeOutcome::Rejected, 200, 100);
        let denied_again = cache.acquire(denied, 101);

        assert!(matches!(allowed_cached, Acquire::Authorized));
        assert!(matches!(allowed_after_safety_window, Acquire::Probe(_)));
        assert!(matches!(denied_again, Acquire::Probe(_)));
    }

    #[test]
    fn auth_cache_complete_expiry_policy_matches_boundaries() {
        let cache = Arc::new(AuthorizationCache::new(8));
        let too_short = [8_u8; 32];
        let threshold = [9_u8; 32];

        let Acquire::Probe(too_short_probe) = cache.acquire(too_short, 100) else {
            panic!("short-lived credential did not reserve a probe");
        };
        too_short_probe.complete(ProbeOutcome::Authorized, 109, 100);

        let Acquire::Probe(threshold_probe) = cache.acquire(threshold, 100) else {
            panic!("threshold credential did not reserve a probe");
        };
        threshold_probe.complete(ProbeOutcome::Authorized, 110, 100);

        assert!(matches!(cache.acquire(too_short, 101), Acquire::Probe(_)));
        assert!(matches!(cache.acquire(threshold, 107), Acquire::Authorized));
        assert!(matches!(cache.acquire(threshold, 108), Acquire::Probe(_)));
    }

    #[tokio::test]
    async fn auth_cache_acquire_identical_credentials_share_probe() {
        let cache = Arc::new(AuthorizationCache::new(8));
        let key = [3_u8; 32];
        let Acquire::Probe(probe) = cache.acquire(key, 100) else {
            panic!("first request did not own the probe");
        };
        let Acquire::Wait(waiter) = cache.acquire(key, 100) else {
            panic!("second request did not wait for the probe");
        };

        probe.complete(ProbeOutcome::Authorized, 200, 100);

        assert_eq!(waiter.wait().await, ProbeOutcome::Authorized);
        assert!(matches!(cache.acquire(key, 101), Acquire::Authorized));
    }

    #[tokio::test]
    async fn auth_cache_wait_cancelled_probe_returns_unavailable() {
        let cache = Arc::new(AuthorizationCache::new(8));
        let key = [4_u8; 32];
        let Acquire::Probe(probe) = cache.acquire(key, 100) else {
            panic!("first request did not own the probe");
        };
        let Acquire::Wait(waiter) = cache.acquire(key, 100) else {
            panic!("second request did not wait for the probe");
        };

        drop(probe);

        assert_eq!(waiter.wait().await, ProbeOutcome::Unavailable);
        assert!(matches!(cache.acquire(key, 101), Acquire::Probe(_)));
    }

    #[tokio::test]
    async fn auth_cache_wait_uncached_authorization_requires_new_probe() {
        let cache = Arc::new(AuthorizationCache::new(8));
        let key = [5_u8; 32];
        let Acquire::Probe(probe) = cache.acquire(key, 100) else {
            panic!("first request did not own the probe");
        };
        let Acquire::Wait(waiter) = cache.acquire(key, 100) else {
            panic!("second request did not wait for the probe");
        };

        probe.complete(ProbeOutcome::AuthorizedUncached, 200, 100);

        assert_eq!(waiter.wait().await, ProbeOutcome::AuthorizedUncached);
        assert!(matches!(cache.acquire(key, 101), Acquire::Probe(_)));
    }

    #[test]
    fn auth_cache_complete_capacity_evicts_oldest_witness() {
        let cache = Arc::new(AuthorizationCache::new(1));
        let first = [6_u8; 32];
        let second = [7_u8; 32];
        let Acquire::Probe(first_probe) = cache.acquire(first, 100) else {
            panic!("first credential did not reserve a probe");
        };
        first_probe.complete(ProbeOutcome::Authorized, 200, 100);
        let Acquire::Probe(second_probe) = cache.acquire(second, 100) else {
            panic!("second credential did not reserve a probe");
        };
        second_probe.complete(ProbeOutcome::Authorized, 200, 100);

        assert!(matches!(cache.acquire(first, 101), Acquire::Probe(_)));
        assert!(matches!(cache.acquire(second, 101), Acquire::Authorized));
    }
}
