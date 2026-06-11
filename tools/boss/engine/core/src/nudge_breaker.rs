//! Circuit breaker for the auto-nudge ("produce a PR") loop.
//!
//! Background: when a worker stops without the engine being able to
//! finalize a PR, the on-Stop handler queues a probe ("produce a PR" /
//! "push to the existing PR" / etc.) and waits for the next Stop. If the
//! worker keeps stopping without changing state — e.g. a
//! `ci_remediation` worker whose chore already has a merged PR, so there
//! is genuinely nothing for it to do — the handler would re-queue the
//! same nudge forever. That is exactly the Worf incident
//! (`exec_18b3945c5b7d7e78_1b`): the worker was nudged 20 times and
//! replied with the same already-merged PR URL 19 times before going
//! idle, never accepted, never parked.
//!
//! This breaker caps *consecutive unproductive* nudges per execution.
//! "Unproductive" is defined deterministically by the caller via a
//! `fingerprint` string that encodes the work state at nudge time (the
//! bound PR's head SHA, "no PR", etc.). When two consecutive nudges
//! carry the same fingerprint the worker made no progress, so the count
//! advances; a different fingerprint means progress (new commit, PR
//! opened, transition) and resets the count. Once the cap is exhausted
//! the caller parks the execution instead of nudging again.
//!
//! State is in-memory only, mirroring [`crate::pr_url_capture`]: the
//! probe FIFO it guards also lives in memory, so an engine restart
//! resets the whole nudge loop anyway. Durability across restarts is
//! intentionally not a goal here.

use std::collections::HashMap;
use std::sync::Mutex;

/// Default cap on consecutive unproductive auto-nudges before the
/// breaker trips. After this many nudges that produce no state change,
/// the engine parks the execution rather than nudging again.
pub const DEFAULT_MAX_UNPRODUCTIVE_NUDGES: u32 = 3;

/// Decision returned by [`NudgeBreaker::record`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeDecision {
    /// The nudge is allowed to fire. `count` is the number of
    /// consecutive unproductive nudges including this one (1-based).
    Proceed { count: u32 },
    /// The breaker has tripped: the cap of consecutive unproductive
    /// nudges has already been sent. The caller must park the execution
    /// and must NOT nudge again. `count` is the number of nudges
    /// already sent at this fingerprint (== the configured cap).
    Trip { count: u32 },
}

#[derive(Debug, Clone)]
struct NudgeRecord {
    /// Fingerprint of the work state at the most recent nudge. A nudge
    /// whose fingerprint differs from this means the worker made
    /// progress, so the counter resets.
    fingerprint: String,
    /// Number of consecutive nudges already sent at `fingerprint`.
    count: u32,
}

/// In-memory `execution_id -> (fingerprint, count)` tracker for the
/// auto-nudge circuit breaker. Thread-safe; cheap to clone-share behind
/// an `Arc`.
#[derive(Debug, Default)]
pub struct NudgeBreaker {
    inner: Mutex<HashMap<String, NudgeRecord>>,
}

impl NudgeBreaker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an intent to nudge `execution_id` whose current work state
    /// is captured by `fingerprint`, capping at `max` consecutive
    /// unproductive nudges.
    ///
    /// - If the fingerprint differs from the last recorded nudge (the
    ///   worker made progress, or this is the first nudge), reset to a
    ///   single fresh nudge and return `Proceed { count: 1 }`.
    /// - If the fingerprint matches and fewer than `max` nudges have
    ///   been sent at it, increment and return `Proceed { count }`.
    /// - If `max` unproductive nudges have already been sent at this
    ///   fingerprint, return `Trip { count: max }` and do not advance
    ///   the count further.
    ///
    /// With `max = 3` the sequence of identical-fingerprint calls is
    /// `Proceed{1}`, `Proceed{2}`, `Proceed{3}`, `Trip{3}`, `Trip{3}`, …
    /// — three nudges fire, then the breaker trips and stays tripped.
    pub fn record(&self, execution_id: &str, fingerprint: &str, max: u32) -> NudgeDecision {
        let mut guard = self.inner.lock().expect("NudgeBreaker mutex poisoned");
        let entry = guard.entry(execution_id.to_owned()).or_insert_with(|| NudgeRecord {
            fingerprint: fingerprint.to_owned(),
            count: 0,
        });
        if entry.fingerprint != fingerprint {
            // Progress since the last nudge — reset to a fresh cycle.
            entry.fingerprint = fingerprint.to_owned();
            entry.count = 0;
        }
        if entry.count >= max {
            NudgeDecision::Trip { count: entry.count }
        } else {
            entry.count += 1;
            NudgeDecision::Proceed { count: entry.count }
        }
    }

    /// Drop any tracked state for `execution_id`. Called when the worker
    /// makes real progress (a PR is finalized) so a later, unrelated
    /// nudge cycle starts clean. Idempotent.
    pub fn forget(&self, execution_id: &str) {
        self.inner
            .lock()
            .expect("NudgeBreaker mutex poisoned")
            .remove(execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_nudge_proceeds_with_count_one() {
        let breaker = NudgeBreaker::new();
        assert_eq!(
            breaker.record("exec_a", "no_pr", 3),
            NudgeDecision::Proceed { count: 1 },
        );
    }

    #[test]
    fn identical_fingerprint_advances_then_trips_at_cap() {
        // The Worf loop: the same unproductive nudge repeats. Three fire,
        // the fourth and all later attempts trip.
        let breaker = NudgeBreaker::new();
        let fp = "push_existing:https://github.com/spinyfin/mono/pull/869";
        assert_eq!(breaker.record("exec_w", fp, 3), NudgeDecision::Proceed { count: 1 });
        assert_eq!(breaker.record("exec_w", fp, 3), NudgeDecision::Proceed { count: 2 });
        assert_eq!(breaker.record("exec_w", fp, 3), NudgeDecision::Proceed { count: 3 });
        assert_eq!(breaker.record("exec_w", fp, 3), NudgeDecision::Trip { count: 3 });
        assert_eq!(breaker.record("exec_w", fp, 3), NudgeDecision::Trip { count: 3 });
    }

    #[test]
    fn changed_fingerprint_resets_the_count() {
        // Progress (a new commit moves the bound PR head, the PR opens,
        // etc.) changes the fingerprint and gives the worker a fresh
        // budget instead of tripping on stale history.
        let breaker = NudgeBreaker::new();
        breaker.record("exec_b", "no_pr", 3);
        breaker.record("exec_b", "no_pr", 3);
        assert_eq!(
            breaker.record("exec_b", "stale:https://github.com/x/y/pull/1", 3),
            NudgeDecision::Proceed { count: 1 },
            "a different fingerprint must reset the counter",
        );
    }

    #[test]
    fn cap_of_one_trips_after_a_single_nudge() {
        let breaker = NudgeBreaker::new();
        assert_eq!(
            breaker.record("exec_c", "no_pr", 1),
            NudgeDecision::Proceed { count: 1 }
        );
        assert_eq!(breaker.record("exec_c", "no_pr", 1), NudgeDecision::Trip { count: 1 });
    }

    #[test]
    fn executions_are_tracked_independently() {
        let breaker = NudgeBreaker::new();
        breaker.record("exec_a", "no_pr", 3);
        breaker.record("exec_a", "no_pr", 3);
        // A different execution starts its own cycle.
        assert_eq!(
            breaker.record("exec_b", "no_pr", 3),
            NudgeDecision::Proceed { count: 1 },
        );
    }

    #[test]
    fn forget_clears_state_and_allows_a_fresh_cycle() {
        let breaker = NudgeBreaker::new();
        breaker.record("exec_a", "no_pr", 3);
        breaker.record("exec_a", "no_pr", 3);
        breaker.record("exec_a", "no_pr", 3);
        assert_eq!(breaker.record("exec_a", "no_pr", 3), NudgeDecision::Trip { count: 3 });
        breaker.forget("exec_a");
        assert_eq!(
            breaker.record("exec_a", "no_pr", 3),
            NudgeDecision::Proceed { count: 1 },
            "forget must reset the cycle",
        );
    }

    #[test]
    fn forget_is_idempotent() {
        let breaker = NudgeBreaker::new();
        breaker.forget("never-tracked");
        breaker.forget("never-tracked");
        assert_eq!(
            breaker.record("never-tracked", "no_pr", 3),
            NudgeDecision::Proceed { count: 1 },
        );
    }
}
