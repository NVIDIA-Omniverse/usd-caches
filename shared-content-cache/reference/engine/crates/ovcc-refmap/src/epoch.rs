// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process-wide epoch-based reclamation for lock-free readers.
//!
//! Pinning stores the observed epoch with `Release` and issues a `SeqCst`
//! fence, retirement issues a `SeqCst` fence before reading the epoch, and the
//! epoch advances only after every pinned participant was seen in the current
//! epoch, so garbage retired in epoch `t` is unreachable once the global epoch
//! reaches `t + 2`. Garbage lives in per-shard lists owned by each map rather
//! than with the retiring thread, so `RefMap::maintain` drains it while that
//! thread sits idle, and retirement never allocates.

use std::cell::Cell;

use crate::sync::{AtomicBool, AtomicU64, Mutex, Ordering, fence};

/// Participant state when not pinned. Pinned states are odd.
const UNPINNED: u64 = 0;

/// Minimum spacing between opportunistic advance attempts, which scan every
/// participant. Maintenance ignores it.
#[cfg(not(loom))]
const ADVANCE_INTERVAL_NANOS: u64 = 100_000;

fn pinned(epoch: u64) -> u64 {
    (epoch << 1) | 1
}

/// Per-thread pin state. Records are leaked and reused by later threads, so
/// their number tracks peak concurrent threads, not threads ever created.
#[repr(align(128))]
struct Participant {
    state: AtomicU64,
    in_use: AtomicBool,
}

struct Domain {
    epoch: crate::sync::CachePadded<AtomicU64>,
    registry: Mutex<Vec<&'static Participant>>,
    #[cfg(not(loom))]
    next_advance: AtomicU64,
}

#[cfg(not(loom))]
static DOMAIN: Domain = Domain {
    epoch: crate::sync::CachePadded(AtomicU64::new(0)),
    registry: Mutex::new(Vec::new()),
    next_advance: AtomicU64::new(0),
};

#[cfg(loom)]
loom::lazy_static! {
    static ref DOMAIN: Domain = Domain {
        epoch: crate::sync::CachePadded(AtomicU64::new(0)),
        registry: Mutex::new(Vec::new()),
    };
}

struct Local {
    participant: &'static Participant,
    depth: Cell<usize>,
}

impl Local {
    fn register() -> Self {
        let mut registry = DOMAIN.registry.lock();
        let reusable = registry
            .iter()
            .copied()
            .find(|participant| !participant.in_use.load(Ordering::Relaxed));
        let participant = match reusable {
            Some(participant) => participant,
            None => {
                let participant: &'static Participant = Box::leak(Box::new(Participant {
                    state: AtomicU64::new(UNPINNED),
                    in_use: AtomicBool::new(false),
                }));
                registry.push(participant);
                participant
            }
        };
        participant.in_use.store(true, Ordering::Relaxed);
        Self {
            participant,
            depth: Cell::new(0),
        }
    }

    fn pin(&self) {
        let depth = self.depth.get();
        if depth == 0 {
            let epoch = DOMAIN.epoch.load(Ordering::Relaxed);
            // Releases the accesses made under earlier pins to an advancer that
            // reads this store instead of the preceding unpin, because only
            // read-modify-writes extend the unpin's release sequence.
            self.participant
                .state
                .store(pinned(epoch), Ordering::Release);
            // Orders the announcement before every pointer load in the
            // critical section.
            fence(Ordering::SeqCst);
        }
        self.depth.set(depth + 1);
    }

    fn unpin(&self) {
        let depth = self.depth.get() - 1;
        self.depth.set(depth);
        if depth == 0 {
            self.participant.state.store(UNPINNED, Ordering::Release);
        }
    }
}

// Under Loom the domain is torn down before the main thread's locals, and a
// thread can only exit unpinned, so records are simply not recycled there.
#[cfg(not(loom))]
impl Drop for Local {
    fn drop(&mut self) {
        let _registry = DOMAIN.registry.lock();
        self.participant.state.store(UNPINNED, Ordering::Release);
        self.participant.in_use.store(false, Ordering::Relaxed);
    }
}

#[cfg(not(loom))]
std::thread_local! {
    static LOCAL: Local = Local::register();
}

#[cfg(loom)]
loom::thread_local! {
    static LOCAL: Local = Local::register();
}

/// Keeps every object reachable from a map's tables allocated while held. It
/// is not `Send` because it must be dropped on the thread that pinned.
pub(crate) struct Guard {
    local: *const Local,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // The thread-local outlives every guard created on this thread, and
        // guards never leave the thread.
        unsafe { (*self.local).unpin() }
    }
}

/// Pins the current thread, or returns `None` while its thread-locals are
/// being destroyed, in which case the caller must use the locked path.
pub(crate) fn pin() -> Option<Guard> {
    LOCAL
        .try_with(|local| {
            local.pin();
            Guard {
                local: local as *const Local,
            }
        })
        .ok()
}

/// Reads the epoch to tag an object that was just made unreachable.
pub(crate) fn retire_epoch() -> u64 {
    fence(Ordering::SeqCst);
    // Orders the free of any list three epochs old that retirement hands over,
    // which unwinding from a panicking `Eq` in `link` performs before the
    // reclamation path's own Acquire load in `collect_epoch` runs.
    DOMAIN.epoch.load(Ordering::Acquire)
}

/// Reads the epoch to decide which retired objects may be freed.
pub(crate) fn collect_epoch() -> u64 {
    DOMAIN.epoch.load(Ordering::Acquire)
}

/// Whether garbage retired in `retired` is unreachable by every reader once
/// the global epoch is `global`.
pub(crate) fn is_expired(retired: u64, global: u64) -> bool {
    global.wrapping_sub(retired) >= 2
}

/// Advances the global epoch when every pinned participant has observed it.
/// Never blocks, and is a no-op while another thread advances, registers, or
/// exits.
pub(crate) fn try_advance() -> bool {
    let Some(registry) = DOMAIN.registry.try_lock() else {
        return false;
    };
    let epoch = DOMAIN.epoch.load(Ordering::Relaxed);
    fence(Ordering::SeqCst);
    let current = pinned(epoch);
    for participant in registry.iter() {
        let state = participant.state.load(Ordering::Relaxed);
        if state != UNPINNED && state != current {
            return false;
        }
    }
    fence(Ordering::Acquire);
    // Advances are serialized by the registry lock, so this cannot move the
    // epoch backwards.
    DOMAIN.epoch.store(epoch.wrapping_add(1), Ordering::Release);
    true
}

/// Rate-limited `try_advance` for the retirement path.
pub(crate) fn maybe_advance() {
    #[cfg(not(loom))]
    {
        let now = now_nanos();
        let deadline = DOMAIN.next_advance.load(Ordering::Relaxed);
        if now < deadline
            || DOMAIN
                .next_advance
                .compare_exchange(
                    deadline,
                    now.saturating_add(ADVANCE_INTERVAL_NANOS),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }
    }
    try_advance();
}

#[cfg(not(loom))]
fn now_nanos() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let elapsed = START.get_or_init(std::time::Instant::now).elapsed();
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

/// Current global epoch, for tests and diagnostics.
#[cfg(all(test, not(loom)))]
pub(crate) fn current() -> u64 {
    DOMAIN.epoch.load(Ordering::Acquire)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    // Tests share the process-wide domain, so they assert relative movement.

    #[test]
    fn refmap_epoch_pins_nest_and_unpin_at_depth_zero() {
        // Arrange.
        let outer = pin().expect("thread-local available");
        let inner = pin().expect("thread-local available");

        // Act.
        drop(inner);
        let nested = LOCAL.with(|local| {
            (
                local.depth.get(),
                local.participant.state.load(Ordering::Relaxed),
            )
        });
        drop(outer);
        let released = LOCAL.with(|local| {
            (
                local.depth.get(),
                local.participant.state.load(Ordering::Relaxed),
            )
        });

        // Assert.
        assert_eq!(nested.0, 1);
        assert_ne!(nested.1, UNPINNED);
        assert_eq!(released, (0, UNPINNED));
    }

    #[test]
    fn refmap_epoch_pinned_participant_blocks_a_second_advance() {
        // Arrange.
        // A pinned thread lets the epoch move at most one step past it,
        // whatever other tests do concurrently.
        let guard = pin().expect("thread-local available");
        let pinned_at = LOCAL.with(|local| local.participant.state.load(Ordering::Relaxed)) >> 1;

        // Act.
        let mut advanced = 0;
        for _ in 0..1_000 {
            if try_advance() {
                advanced += 1;
            }
            if current().wrapping_sub(pinned_at) >= 2 {
                break;
            }
        }
        let distance = current().wrapping_sub(pinned_at);
        drop(guard);

        // Assert.
        assert!(
            distance <= 1,
            "epoch moved {distance} steps past a pinned participant after {advanced} advances"
        );
    }

    #[test]
    fn refmap_epoch_expiry_requires_two_advances() {
        // Arrange.
        let retired = 10;

        // Act.
        let observed = [10, 11, 12].map(|global| is_expired(retired, global));

        // Assert.
        assert_eq!(observed, [false, false, true]);
        assert!(is_expired(u64::MAX, 1), "expiry survives wrapping");
    }
}
