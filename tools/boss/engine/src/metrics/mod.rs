//! Engine counter / gauge metrics framework (phase 1).
//!
//! Declaring a new metric is a one- or two-line change at the call
//! site via [`register_counter!`] / [`register_gauge!`]. Values are
//! held in in-memory atomics for the hot path and flushed to
//! `state.db` every 30 seconds (and on graceful shutdown). On engine
//! startup the framework reads the persisted rows back so monotonic
//! counter totals are continuous across restarts.
//!
//! Per the framework design (see
//! `tools/boss/docs/designs/engine-counter-metrics-framework.md`,
//! §"Risks / open questions" item 7) the [`Registry`] is plumbed
//! explicitly as `Arc<Registry>` rather than stashed in a global —
//! every call site takes a `&Registry` so unit tests can construct a
//! local registry without leaking state across tests.
//!
//! Phase 1 ships the registry, the primitives, the `state.db`
//! tables, the flush task and the startup-rehydrate path. Phase 4
//! migrates `DispatcherStats` onto the framework. The `bossctl
//! metrics` surfacing verbs land in a subsequent phase.

pub mod persistence;
pub mod registry;

pub use persistence::{flush_all, seed_from_db, spawn_flush_task};
pub use registry::{CounterHandle, GaugeHandle, Registry};

/// Force registration of every counter / gauge handle the engine
/// declares.
///
/// `LazyLock`-style registration would let a counter living in a
/// rarely-loaded module miss its first flush window (and would push
/// the duplicate-name panic from boot into the middle of a busy
/// sweep — see design §"Risks / open questions" item 6, which is
/// load-bearing for item 2). The cure is this single function that
/// touches every handle so registration happens once, deterministically,
/// at engine startup.
///
/// As each new counter module lands, add one line here to register
/// its handles so duplicate-name panics surface at boot rather than
/// at the first increment (design §"Risks / open questions" item 6).
pub fn init_all(registry: &Registry) {
    // Phase 3: PR URL capture path counters.
    crate::completion::register_metrics(registry);
    // Phase 3: Dependency-unblock sweep gauge.
    crate::dep_unblock_sweep::register_metrics(registry);
    // Phase 3: Cube workspace lease counters.
    crate::coordinator::register_metrics(registry);
    // Phase 4: DispatcherStats counters migrated to the framework.
    crate::live_status_loop::register_metrics(registry);
    // Phase 5: SweepOutcome / merge_poller counters.
    crate::merge_poller::init(registry);
    // External tracker reconciler pass counters.
    crate::external_tracker::reconcile::register_metrics(registry);
}
