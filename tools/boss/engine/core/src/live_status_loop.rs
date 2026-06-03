//! Per-slot trigger fan-in for the live-status summarizer.
//!
//! [`LiveStatusManager`] owns one tokio task per active worker slot.
//! The task wakes up on any of:
//!
//! 1. **Stop hook** — the worker just finished a turn; transcript is
//!    fresh.
//! 2. **Every K-th PostToolUse** (default K = 5) — catches long
//!    working stretches that don't hit a Stop.
//! 3. **Activity transition** — the moment `activity` flips to
//!    `WaitingForInput` or `Errored` we write the literal label
//!    directly so the UI never lies about "what the worker is doing".
//! 4. **Timer floor** — every 60s if the worker is `Working` and none
//!    of (1)–(3) has fired in that window. Catches a slow turn with
//!    no tool activity.
//!
//! And does *not* fire while `activity` is `Spawning` or
//! `Terminated`. `Idle` is special: we summarize once on the
//! transition (so the card shows "what the worker was doing" rather
//! than the stale prior text), then clear the field after a 30s grace
//! period.
//!
//! Rate limit per slot: at most one summary call in flight at a time,
//! and at most one completed summary per 15s of wall clock. If a
//! Stop arrives during the cool-down the trigger is coalesced — we
//! drop the work, the next event will pick up.
//!
//! Failure modes are silent. A summarizer that times out or returns
//! an empty string leaves the prior `live_status` in place and does
//! not advance `last_success_at`, so the staleness UI takes over
//! after 5 min.
//!
//! See `tools/boss/docs/designs/worker-live-status.md` (Q2 cadence,
//! Q3 budget, Q4 quiet states) for the policy that shapes this loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use boss_protocol::WorkerActivity;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::live_status::{self, SummarizerOutcome};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::metrics::Registry;
use crate::transcript_tail::TranscriptTail;

/// Per-slot diagnostic state captured by the trigger fan-in. The
/// `bossctl live-status debug` verb reads this to give a one-shot view
/// of the pipeline without log-diving — see the chore description for
/// the field-by-field contract. Nothing in this struct is wired into
/// the cooldown/retry logic; it's strictly for observability.
#[derive(Debug, Clone, Default)]
pub struct SlotDebugSnapshot {
    /// Last trigger the per-slot loop received (any variant).
    ///
    /// **Ambiguous on its own.** Both real hook fan-outs and the
    /// per-slot loop's synthetic 60-second timer write this field
    /// with the same `post_tool_use` label. To distinguish, read
    /// `last_real_trigger_*` and `last_synthetic_trigger_at` — these
    /// were added by the 2026-05-12 follow-up to PR #366 specifically
    /// because that ambiguity made it look like real hooks were
    /// arriving when only the synthetic timer was firing.
    pub last_trigger_kind: Option<String>,
    pub last_trigger_at_epoch_s: Option<i64>,
    /// Last trigger received from a real hook fan-out (i.e. a
    /// `notify()` call from `dispatch_live_worker_state`). Excludes
    /// the synthetic timer-floor firings, so a `None` here while
    /// `last_trigger_kind` is `Some` is the smoking gun for
    /// "the synthetic timer is firing but no hooks ever reach the
    /// slot loop".
    pub last_real_trigger_kind: Option<String>,
    pub last_real_trigger_at_epoch_s: Option<i64>,
    pub last_synthetic_trigger_at_epoch_s: Option<i64>,
    /// Outcome tag of the most recent summarizer call (the four
    /// distinguishable cases from [`SummarizerOutcome::tag`]).
    pub last_outcome_tag: Option<String>,
    /// Human-readable detail for the most recent summarizer call.
    pub last_outcome_detail: Option<String>,
    pub last_outcome_at_epoch_s: Option<i64>,
    /// Timestamp of the most recent `Success` outcome.
    pub last_success_at_epoch_s: Option<i64>,
    /// First 80 chars of the most recent successful summary text.
    pub last_success_text: Option<String>,
    /// Resolved transcript path the loop is tailing, or `None` if the
    /// resolver has not returned a path yet.
    pub transcript_path: Option<String>,
    /// Bytes of redacted prompt text fed to the most recent
    /// summarizer call. Helpful for telling "transcript is empty" from
    /// "transcript is huge".
    pub last_redacted_bytes: Option<usize>,
    /// True if the disabled-slot toggle was active at the time of the
    /// last loop iteration that observed it.
    pub disabled: bool,
}

/// Manager-level diagnostic store. Mutated by per-slot tasks; read by
/// the `live-status debug` RPC handler. Lives behind a single mutex
/// shared by reference; per-slot writes are short and not on the hot
/// path of any user-facing latency budget, so the contention cost is
/// negligible.
#[derive(Default)]
pub struct LiveStatusDebugStore {
    inner: StdMutex<HashMap<u8, SlotDebugSnapshot>>,
}

impl LiveStatusDebugStore {
    pub fn snapshot_for(&self, slot_id: u8) -> SlotDebugSnapshot {
        self.inner
            .lock()
            .expect("debug store mutex poisoned")
            .get(&slot_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn snapshot_all(&self) -> HashMap<u8, SlotDebugSnapshot> {
        self.inner
            .lock()
            .expect("debug store mutex poisoned")
            .clone()
    }

    fn update<F: FnOnce(&mut SlotDebugSnapshot)>(&self, slot_id: u8, f: F) {
        let mut guard = self.inner.lock().expect("debug store mutex poisoned");
        let entry = guard.entry(slot_id).or_default();
        f(entry);
    }

    fn forget(&self, slot_id: u8) {
        self.inner
            .lock()
            .expect("debug store mutex poisoned")
            .remove(&slot_id);
    }
}

fn epoch_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Phase-4 counter handles for DispatcherStats. Registered via
// `register_metrics` which is called from `metrics::init_all` at
// engine startup.
crate::register_counter!(
    DISPATCHER_HOOK_EVENTS_TOTAL,
    "dispatcher.hook_events.total",
    "Total hook events received by dispatch_live_worker_state."
);
crate::register_counter!(
    DISPATCHER_HOOK_EVENTS_DROPPED_MISSING_RUN_ID,
    "dispatcher.hook_events.dropped_missing_run_id",
    "Hook events dropped because no run_id could be resolved."
);
crate::register_counter!(
    DISPATCHER_HOOK_EVENTS_WITH_TRANSCRIPT_PATH,
    "dispatcher.hook_events.with_transcript_path",
    "Hook events whose payload carried a non-empty transcript_path."
);
crate::register_counter!(
    DISPATCHER_HOOK_EVENTS_WITHOUT_TRANSCRIPT_PATH,
    "dispatcher.hook_events.without_transcript_path",
    "Hook events whose payload lacked transcript_path (cache may cover)."
);
crate::register_counter!(
    DISPATCHER_TRANSCRIPT_PATH_PERSIST_UPDATED,
    "dispatcher.transcript_path_persist.updated",
    "set_run_transcript_path_if_unset calls that updated a work_runs row."
);
crate::register_counter!(
    DISPATCHER_TRANSCRIPT_PATH_PERSIST_NOOP,
    "dispatcher.transcript_path_persist.noop",
    "Persist calls where the row already had transcript_path set."
);
crate::register_counter!(
    DISPATCHER_TRANSCRIPT_PATH_PERSIST_ROW_MISSING,
    "dispatcher.transcript_path_persist.row_missing",
    "Persist calls where no matching work_runs row exists yet."
);
crate::register_counter!(
    DISPATCHER_TRANSCRIPT_PATH_PERSIST_ERR,
    "dispatcher.transcript_path_persist.err",
    "Persist calls that returned Err (DB write failed)."
);
crate::register_counter!(
    DISPATCHER_TRANSCRIPT_PATH_PERSIST_FROM_CACHE,
    "dispatcher.transcript_path_persist.from_cache",
    "Persist calls that resolved transcript_path from the in-memory cache."
);

/// Register every DispatcherStats counter handle with `registry`.
/// Called once from `metrics::init_all` at engine startup so duplicate-name
/// panics surface at boot rather than on the first increment.
pub fn register_metrics(registry: &Registry) {
    registry.register_counter(&DISPATCHER_HOOK_EVENTS_TOTAL);
    registry.register_counter(&DISPATCHER_HOOK_EVENTS_DROPPED_MISSING_RUN_ID);
    registry.register_counter(&DISPATCHER_HOOK_EVENTS_WITH_TRANSCRIPT_PATH);
    registry.register_counter(&DISPATCHER_HOOK_EVENTS_WITHOUT_TRANSCRIPT_PATH);
    registry.register_counter(&DISPATCHER_TRANSCRIPT_PATH_PERSIST_UPDATED);
    registry.register_counter(&DISPATCHER_TRANSCRIPT_PATH_PERSIST_NOOP);
    registry.register_counter(&DISPATCHER_TRANSCRIPT_PATH_PERSIST_ROW_MISSING);
    registry.register_counter(&DISPATCHER_TRANSCRIPT_PATH_PERSIST_ERR);
    registry.register_counter(&DISPATCHER_TRANSCRIPT_PATH_PERSIST_FROM_CACHE);
}

/// Engine-wide counters for the hook-event dispatcher. One instance
/// is shared by `Arc` from `ServerState` so every call into
/// `dispatch_live_worker_state` can bump the appropriate counters.
///
/// After phase-4 migration the per-counter state lives in the framework
/// registry (`self.metrics`). The `inc_*` methods are kept as one-release
/// compat shims so call sites compile without change; a follow-up chore
/// will delete them once call sites move to the handles directly.
pub struct DispatcherStats {
    metrics: Arc<Registry>,
    /// Last hook event the dispatcher processed. Held behind a mutex
    /// rather than atomics because the run id is a String.
    last_hook: StdMutex<Option<LastHookSnapshot>>,
}

#[derive(Debug, Clone)]
pub struct LastHookSnapshot {
    pub run_id: String,
    pub kind: String,
    pub epoch_s: i64,
}

impl DispatcherStats {
    pub fn new(metrics: Arc<Registry>) -> Self {
        Self {
            metrics,
            last_hook: StdMutex::new(None),
        }
    }

    // One-release compat shims. Each delegates to the framework handle
    // so both `snapshot()` and `registry.counter_value(name)` reflect
    // the same value. A follow-up chore removes these wrappers once
    // call sites migrate to the handles directly.

    pub fn inc_hook_events_total(&self) {
        DISPATCHER_HOOK_EVENTS_TOTAL.inc(&self.metrics);
    }
    pub fn inc_dropped_missing_run_id(&self) {
        DISPATCHER_HOOK_EVENTS_DROPPED_MISSING_RUN_ID.inc(&self.metrics);
    }
    pub fn inc_with_transcript_path(&self) {
        DISPATCHER_HOOK_EVENTS_WITH_TRANSCRIPT_PATH.inc(&self.metrics);
    }
    pub fn inc_without_transcript_path(&self) {
        DISPATCHER_HOOK_EVENTS_WITHOUT_TRANSCRIPT_PATH.inc(&self.metrics);
    }
    pub fn inc_persist_updated(&self) {
        DISPATCHER_TRANSCRIPT_PATH_PERSIST_UPDATED.inc(&self.metrics);
    }
    pub fn inc_persist_noop(&self) {
        DISPATCHER_TRANSCRIPT_PATH_PERSIST_NOOP.inc(&self.metrics);
    }
    pub fn inc_persist_row_missing(&self) {
        DISPATCHER_TRANSCRIPT_PATH_PERSIST_ROW_MISSING.inc(&self.metrics);
    }
    pub fn inc_persist_err(&self) {
        DISPATCHER_TRANSCRIPT_PATH_PERSIST_ERR.inc(&self.metrics);
    }
    pub fn inc_persist_from_cache(&self) {
        DISPATCHER_TRANSCRIPT_PATH_PERSIST_FROM_CACHE.inc(&self.metrics);
    }

    pub fn record_last_hook(&self, run_id: &str, kind: &str) {
        let mut guard = self.last_hook.lock().expect("last_hook mutex poisoned");
        *guard = Some(LastHookSnapshot {
            run_id: run_id.to_owned(),
            kind: kind.to_owned(),
            epoch_s: epoch_now(),
        });
    }

    pub fn last_hook(&self) -> Option<LastHookSnapshot> {
        self.last_hook
            .lock()
            .expect("last_hook mutex poisoned")
            .clone()
    }

    /// Read-only snapshot of every counter as plain `u64`. Populated
    /// from the framework registry so `snapshot()` and
    /// `registry.counter_value(name)` always agree.
    pub fn snapshot(&self) -> DispatcherStatsSnapshot {
        DispatcherStatsSnapshot {
            hook_events_total: self
                .metrics
                .counter_value(DISPATCHER_HOOK_EVENTS_TOTAL.name())
                .unwrap_or(0),
            hook_events_dropped_missing_run_id: self
                .metrics
                .counter_value(DISPATCHER_HOOK_EVENTS_DROPPED_MISSING_RUN_ID.name())
                .unwrap_or(0),
            hook_events_with_transcript_path_in_payload: self
                .metrics
                .counter_value(DISPATCHER_HOOK_EVENTS_WITH_TRANSCRIPT_PATH.name())
                .unwrap_or(0),
            hook_events_without_transcript_path_in_payload: self
                .metrics
                .counter_value(DISPATCHER_HOOK_EVENTS_WITHOUT_TRANSCRIPT_PATH.name())
                .unwrap_or(0),
            transcript_path_persist_updated: self
                .metrics
                .counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_UPDATED.name())
                .unwrap_or(0),
            transcript_path_persist_noop: self
                .metrics
                .counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_NOOP.name())
                .unwrap_or(0),
            transcript_path_persist_row_missing: self
                .metrics
                .counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_ROW_MISSING.name())
                .unwrap_or(0),
            transcript_path_persist_err: self
                .metrics
                .counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_ERR.name())
                .unwrap_or(0),
            transcript_path_persist_from_cache: self
                .metrics
                .counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_FROM_CACHE.name())
                .unwrap_or(0),
            last_hook: self.last_hook(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DispatcherStatsSnapshot {
    pub hook_events_total: u64,
    pub hook_events_dropped_missing_run_id: u64,
    pub hook_events_with_transcript_path_in_payload: u64,
    pub hook_events_without_transcript_path_in_payload: u64,
    pub transcript_path_persist_updated: u64,
    pub transcript_path_persist_noop: u64,
    pub transcript_path_persist_row_missing: u64,
    pub transcript_path_persist_err: u64,
    pub transcript_path_persist_from_cache: u64,
    pub last_hook: Option<LastHookSnapshot>,
}

/// Engine-wide in-memory cache of the most recent `transcript_path`
/// the dispatcher learned for a given `run_id`. Populated whenever a
/// hook event arrives with a non-empty `transcript_path` field; read
/// whenever a hook event arrives WITHOUT one so the dispatcher can
/// still call `set_run_transcript_path_if_unset` with a known-good
/// path.
///
/// This is the structural fix for the failure mode where claude
/// emits `transcript_path` on `SessionStart` / `Stop` but not on
/// `PostToolUse` / `PreToolUse` / `UserPromptSubmit` — without this
/// cache, the dispatcher would skip the persist on every event that
/// happens to lack the field, and if the work_runs row was inserted
/// AFTER the SessionStart fired (the chore's reported reproduction)
/// the persist call from SessionStart was an `UPDATE … WHERE id=?`
/// that affected zero rows. The cache lets the next hook for the
/// same run finally win.
#[derive(Default)]
pub struct TranscriptPathCache {
    inner: StdMutex<HashMap<String, String>>,
}

impl TranscriptPathCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the path for `run_id` if it's not already cached.
    /// Returns whether the cache was updated (true) or already had a
    /// value (false). Cache is first-writer-wins to match the
    /// idempotency of `set_run_transcript_path_if_unset` — a later
    /// SessionStart/resume must NOT clobber the path the tail
    /// watcher has already opened against the original session.
    pub fn record_if_unset(&self, run_id: &str, path: &str) -> bool {
        let mut guard = self.inner.lock().expect("transcript path cache poisoned");
        if guard.contains_key(run_id) {
            return false;
        }
        guard.insert(run_id.to_owned(), path.to_owned());
        true
    }

    pub fn get(&self, run_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("transcript path cache poisoned")
            .get(run_id)
            .cloned()
    }

    /// Drop the cache entry for `run_id`. Called when a run is
    /// released so the cache doesn't grow without bound.
    pub fn forget(&self, run_id: &str) {
        self.inner
            .lock()
            .expect("transcript path cache poisoned")
            .remove(run_id);
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("transcript path cache poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn trigger_kind(t: &Trigger) -> &'static str {
    match t {
        Trigger::Stop => "stop",
        Trigger::PostToolUse => "post_tool_use",
        Trigger::ActivityChanged(_) => "activity_changed",
        Trigger::Shutdown => "shutdown",
    }
}

/// Default PostToolUse counter modulus before a refresh is requested.
const POST_TOOL_USE_K: u32 = 5;

/// Lower bound on the gap between successful summarizer calls per
/// slot.
const SUCCESS_COOLDOWN: Duration = Duration::from_secs(15);

/// Wall-clock cap on how long a slot can sit in `Working` without
/// a refresh.
const WORKING_TIMER_FLOOR: Duration = Duration::from_secs(60);

/// After this much time in `Idle`, clear the prior `live_status`.
const IDLE_CLEAR_AFTER: Duration = Duration::from_secs(30);

/// Cap on the number of transcript JSONL entries the per-slot loop
/// keeps buffered between ticks. The summarizer module trims further
/// to its input-byte cap, but we'd rather not let a chatty worker
/// grow this vector unboundedly.
const TRANSCRIPT_BUFFER_CAP: usize = 60;

/// Trigger sent into a per-slot task from the events dispatcher (and
/// from the manager itself for lifecycle bookkeeping).
#[derive(Debug, Clone)]
pub enum Trigger {
    /// A Stop hook arrived. Refresh on the next allowed slot.
    Stop,
    /// A PostToolUse hook arrived. The task counts these and refreshes
    /// every Kth one.
    PostToolUse,
    /// Activity transitioned to a new state. The task may write a
    /// literal label directly (`Errored`, `WaitingForInput`) without
    /// calling the model, or schedule a clear after `Idle` settles.
    ActivityChanged(WorkerActivity),
    /// Manager-initiated shutdown for this slot.
    Shutdown,
}

/// Trait the per-slot task uses to broadcast registry changes. In
/// production this is `Arc<ServerState>`; tests use a counting stub.
#[async_trait]
pub trait LiveStatusBroadcaster: Send + Sync {
    async fn broadcast_live_worker_states(&self);
}

/// Trait the per-slot task uses to resolve the transcript path for a
/// run. In production this hits `WorkDb`; tests use an in-memory map.
#[async_trait]
pub trait TranscriptPathResolver: Send + Sync {
    async fn transcript_path(&self, run_id: &str) -> Option<PathBuf>;
}

/// Per-slot handle stored on the manager. Dropping the sender closes
/// the channel; the task receives `None` from `recv()` and exits.
struct SlotHandle {
    sender: mpsc::UnboundedSender<Trigger>,
    join: Option<JoinHandle<()>>,
}

/// Configuration captured at slot start. Carried by value into the
/// task so the manager can drop the entry on `stop_slot` without
/// waiting for the task.
struct SlotConfig {
    slot_id: u8,
    run_id: String,
    api_key: Option<String>,
    registry: Arc<LiveWorkerStateRegistry>,
    broadcaster: Arc<dyn LiveStatusBroadcaster>,
    resolver: Arc<dyn TranscriptPathResolver>,
    disabled: Arc<DisabledSlots>,
    debug_store: Arc<LiveStatusDebugStore>,
}

/// Shared, lock-protected list of slots whose summarizer has been
/// manually disabled by the human (Q9 per-worker toggle). The manager
/// reads it whenever it considers a refresh; a frontend RPC mutates
/// it. Holding the slot in here makes the running per-slot task skip
/// the model call and clear its existing `live_status`. Persistence
/// is handled by the engine layer (metadata KV) and threaded through
/// `LiveStatusManager::set_initial_disabled_slots` at startup.
#[derive(Default)]
pub struct DisabledSlots(StdMutex<HashSet<u8>>);

impl DisabledSlots {
    pub fn is_disabled(&self, slot_id: u8) -> bool {
        self.0
            .lock()
            .expect("disabled-slots mutex poisoned")
            .contains(&slot_id)
    }

    fn set(&self, slot_id: u8, disabled: bool) -> bool {
        let mut guard = self.0.lock().expect("disabled-slots mutex poisoned");
        if disabled {
            guard.insert(slot_id)
        } else {
            guard.remove(&slot_id)
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self
            .0
            .lock()
            .expect("disabled-slots mutex poisoned")
            .iter()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }

    fn load(&self, slots: impl IntoIterator<Item = u8>) {
        let mut guard = self.0.lock().expect("disabled-slots mutex poisoned");
        guard.clear();
        guard.extend(slots);
    }
}

pub struct LiveStatusManager {
    slots: StdMutex<HashMap<u8, SlotHandle>>,
    disabled: Arc<DisabledSlots>,
    debug_store: Arc<LiveStatusDebugStore>,
}

impl Default for LiveStatusManager {
    fn default() -> Self {
        Self {
            slots: StdMutex::new(HashMap::new()),
            disabled: Arc::new(DisabledSlots::default()),
            debug_store: Arc::new(LiveStatusDebugStore::default()),
        }
    }
}

impl LiveStatusManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Shared handle to the disabled-slot set, used by the engine's
    /// frontend RPC handler to mutate the toggle without taking the
    /// manager's slots mutex. The set is also passed into every
    /// per-slot task so the task can consult it on each refresh.
    pub fn disabled_slots(&self) -> Arc<DisabledSlots> {
        self.disabled.clone()
    }

    /// Seed the disabled set from persisted engine state at startup.
    /// Replaces the current set wholesale — callers should pass the
    /// full list from the metadata KV.
    pub fn set_initial_disabled_slots(&self, slot_ids: impl IntoIterator<Item = u8>) {
        self.disabled.load(slot_ids);
    }

    /// Flip the disabled state for `slot_id`. Returns the new state
    /// so callers can persist a delta. If `enabled` is false the
    /// running task picks the change up on its next tick — see
    /// the disable-arm in [`run_slot_loop`].
    pub fn set_enabled(&self, slot_id: u8, enabled: bool) -> bool {
        let changed = self.disabled.set(slot_id, !enabled);
        if changed {
            // Wake the per-slot task so a freshly-enabled slot
            // catches up immediately and a freshly-disabled one
            // clears its prior status without waiting for the next
            // hook event.
            self.notify(slot_id, Trigger::PostToolUse);
        }
        enabled
    }

    /// Snapshot of slot ids currently disabled. Used for the
    /// `ListLiveStatusDisabledSlots` RPC and for persistence in the
    /// metadata KV.
    pub fn disabled_snapshot(&self) -> Vec<u8> {
        self.disabled.snapshot()
    }

    /// Shared handle to the diagnostic store. The `live-status debug`
    /// RPC handler reads this; per-slot tasks already hold their own
    /// clone via `SlotConfig`.
    pub fn debug_store(&self) -> Arc<LiveStatusDebugStore> {
        self.debug_store.clone()
    }

    /// Slot ids currently running a per-slot task. Distinct from
    /// `disabled_snapshot()` — a slot can have a running task that is
    /// also disabled, and both pieces of information are surfaced
    /// separately in the debug verb.
    pub fn active_slot_ids(&self) -> Vec<u8> {
        let mut v: Vec<u8> = self
            .slots
            .lock()
            .expect("manager mutex poisoned")
            .keys()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }

    /// Spawn (or replace) the per-slot task. Idempotent in the
    /// re-spawn case: a slot whose prior task is still alive gets
    /// torn down first so we never have two summarizer loops racing
    /// against the same slot.
    pub fn start_slot(
        &self,
        slot_id: u8,
        run_id: String,
        api_key: Option<String>,
        registry: Arc<LiveWorkerStateRegistry>,
        broadcaster: Arc<dyn LiveStatusBroadcaster>,
        resolver: Arc<dyn TranscriptPathResolver>,
    ) {
        self.stop_slot(slot_id);
        tracing::info!(
            slot_id,
            run_id = %run_id,
            has_api_key = api_key.is_some(),
            "live_status: start_slot — spawning per-slot summarizer task",
        );
        let (sender, receiver) = mpsc::unbounded_channel();
        // Reset the diagnostic snapshot for this slot — the prior
        // entry, if any, belongs to a previous run that just
        // released. The disabled flag is sticky across re-spawns so
        // it gets read from the live set on first iteration.
        self.debug_store.update(slot_id, |snap| {
            *snap = SlotDebugSnapshot {
                disabled: self.disabled.is_disabled(slot_id),
                ..SlotDebugSnapshot::default()
            };
        });
        let cfg = SlotConfig {
            slot_id,
            run_id,
            api_key,
            registry,
            broadcaster,
            resolver,
            disabled: self.disabled.clone(),
            debug_store: self.debug_store.clone(),
        };
        let join = tokio::spawn(run_slot_loop(cfg, receiver));
        let mut guard = self.slots.lock().expect("manager mutex poisoned");
        guard.insert(slot_id, SlotHandle {
            sender,
            join: Some(join),
        });
    }

    /// Send `Shutdown` to the slot's task (if any). The task will
    /// drain any queued triggers, then exit. Caller does not wait —
    /// the JoinHandle is dropped on the floor so a stuck summarizer
    /// HTTP call cannot block `release_worker_pane`.
    pub fn stop_slot(&self, slot_id: u8) {
        let handle = self
            .slots
            .lock()
            .expect("manager mutex poisoned")
            .remove(&slot_id);
        if let Some(mut h) = handle {
            tracing::info!(slot_id, "live_status: stop_slot — tearing down per-slot task");
            // Best-effort: send Shutdown, then drop the sender so the
            // receiver returns None even if the task was holding the
            // sleep at the time.
            let _ = h.sender.send(Trigger::Shutdown);
            drop(h.sender);
            // The JoinHandle is moved out so dropping `h` doesn't
            // implicitly abort the task. We deliberately don't await
            // it: a wedged Anthropic call must not block the release
            // path.
            let _ = h.join.take();
            self.debug_store.forget(slot_id);
        }
    }

    /// Forward `trigger` to the task running `slot_id`. Returns
    /// `true` if the trigger was delivered, `false` if no task is
    /// running for that slot.
    ///
    /// A drop here used to be silent — one of the suspected silent-
    /// failure modes the chore exists to surface. Now we log every
    /// drop at `warn` with the slot/trigger so a hook event that
    /// races ahead of `start_slot` (or arrives after `stop_slot`) is
    /// visible in engine stderr.
    pub fn notify(&self, slot_id: u8, trigger: Trigger) -> bool {
        let kind = trigger_kind(&trigger);
        let guard = self.slots.lock().expect("manager mutex poisoned");
        let Some(handle) = guard.get(&slot_id) else {
            tracing::warn!(
                slot_id,
                trigger = kind,
                "live_status: notify dropped — no per-slot task running for this slot",
            );
            return false;
        };
        let sent = handle.sender.send(trigger).is_ok();
        if !sent {
            tracing::warn!(
                slot_id,
                trigger = kind,
                "live_status: notify dropped — receiver closed (slot task already exiting)",
            );
        }
        sent
    }

    /// True iff the manager currently has a task for `slot_id`. Used
    /// by tests; the production dispatcher just calls [`Self::notify`]
    /// and lets a missing slot fall through.
    #[cfg(test)]
    pub fn has_slot(&self, slot_id: u8) -> bool {
        self.slots
            .lock()
            .expect("manager mutex poisoned")
            .contains_key(&slot_id)
    }
}

/// Per-slot loop body. Receives triggers on `rx`, runs the
/// summarizer when appropriate, and writes the result back into the
/// registry + broadcasts.
///
/// Internal state:
///
/// - `transcript_buffer` — accumulated JSONL entries from the
///   transcript tail since startup. Bounded by `TRANSCRIPT_BUFFER_CAP`.
/// - `post_tool_use_count` — modulo `POST_TOOL_USE_K`.
/// - `last_success_at` — wall-clock of the last successful set;
///   summary calls inside `SUCCESS_COOLDOWN` of this are coalesced.
/// - `last_activity` — the most recent activity we've been told about.
/// - `in_flight` — true while a summarizer HTTP call is outstanding.
async fn run_slot_loop(cfg: SlotConfig, mut rx: mpsc::UnboundedReceiver<Trigger>) {
    let SlotConfig {
        slot_id,
        run_id,
        api_key,
        registry,
        broadcaster,
        resolver,
        disabled,
        debug_store,
    } = cfg;
    let mut tail: Option<TranscriptTail> = None;
    let mut transcript_buffer: Vec<Value> = Vec::new();
    let mut post_tool_use_count: u32 = 0;
    let mut last_success_at: Option<Instant> = None;
    let mut last_activity = WorkerActivity::Spawning;
    let mut idle_since: Option<Instant> = None;

    loop {
        // The select! arm for the timer floor only matters while we're
        // in `Working` and have a `last_success_at` to count against.
        // Outside of that, idle out the loop on the channel only.
        let timer_remaining = compute_timer_delay(last_activity, last_success_at, idle_since);
        let (trigger, synthetic) = tokio::select! {
            t = rx.recv() => match t {
                Some(t) => (t, false),
                None => return,
            },
            _ = tokio::time::sleep(timer_remaining) => (Trigger::PostToolUse, true),
        };

        let kind = trigger_kind(&trigger);
        tracing::debug!(
            slot_id,
            trigger = kind,
            synthetic,
            activity = last_activity.as_str(),
            "live_status: slot loop received trigger",
        );
        let now_epoch = epoch_now();
        debug_store.update(slot_id, |snap| {
            snap.last_trigger_kind = Some(kind.to_owned());
            snap.last_trigger_at_epoch_s = Some(now_epoch);
            if synthetic {
                snap.last_synthetic_trigger_at_epoch_s = Some(now_epoch);
            } else {
                snap.last_real_trigger_kind = Some(kind.to_owned());
                snap.last_real_trigger_at_epoch_s = Some(now_epoch);
            }
        });

        match trigger {
            Trigger::Shutdown => return,
            Trigger::ActivityChanged(new_activity) => {
                last_activity = new_activity;
                idle_since = match new_activity {
                    WorkerActivity::Idle => Some(Instant::now()),
                    _ => None,
                };
                match new_activity {
                    WorkerActivity::Errored => {
                        if registry
                            .set_live_status(slot_id, Some(live_status::ERRORED_LITERAL.to_owned()))
                        {
                            broadcaster.broadcast_live_worker_states().await;
                        }
                        continue;
                    }
                    WorkerActivity::WaitingForInput => {
                        // Per Q4: keep the prior sentence if there
                        // was one, otherwise write the literal so the
                        // card is not misleading.
                        let prior = registry.get(slot_id).and_then(|s| s.live_status);
                        if prior.is_none()
                            && registry.set_live_status(
                                slot_id,
                                Some(live_status::AWAITING_INPUT_LITERAL.to_owned()),
                            ) {
                                broadcaster.broadcast_live_worker_states().await;
                            }
                        continue;
                    }
                    WorkerActivity::Spawning | WorkerActivity::Terminated => {
                        // No refresh, and (for Terminated) the manager
                        // will tear us down momentarily.
                        continue;
                    }
                    WorkerActivity::Working | WorkerActivity::Idle => {
                        // Fall through to the refresh path below.
                    }
                }
            }
            Trigger::Stop => {
                // No filter — Stop is the cleanest refresh boundary.
            }
            Trigger::PostToolUse => {
                post_tool_use_count = post_tool_use_count.wrapping_add(1);
                if !post_tool_use_count.is_multiple_of(POST_TOOL_USE_K) {
                    continue;
                }
            }
        }

        // Quiet states never refresh.
        if matches!(
            last_activity,
            WorkerActivity::Spawning | WorkerActivity::Terminated
        ) {
            tracing::debug!(
                slot_id,
                activity = last_activity.as_str(),
                "live_status: skip — quiet activity state",
            );
            continue;
        }

        // Per-slot off-switch (Q9). When the human has disabled this
        // slot, drop any prior `live_status` (so the UI falls back to
        // pane_summary) and skip the model call until the toggle
        // flips back on. The toggle flip itself sends a wake-up
        // trigger so this branch fires within a tick of the change.
        let disabled_now = disabled.is_disabled(slot_id);
        debug_store.update(slot_id, |snap| {
            snap.disabled = disabled_now;
        });
        if disabled_now {
            tracing::info!(
                slot_id,
                "live_status: skip — slot disabled by per-slot toggle",
            );
            if registry.set_live_status(slot_id, None) {
                broadcaster.broadcast_live_worker_states().await;
            }
            continue;
        }

        // Idle-clear: if we've been idle longer than IDLE_CLEAR_AFTER
        // and a prior `live_status` is still set, drop it.
        if last_activity == WorkerActivity::Idle
            && let Some(idle_at) = idle_since
                && idle_at.elapsed() >= IDLE_CLEAR_AFTER {
                    tracing::info!(
                        slot_id,
                        idle_for_s = idle_at.elapsed().as_secs(),
                        "live_status: clearing live_status — idle grace expired",
                    );
                    if registry.set_live_status(slot_id, None) {
                        broadcaster.broadcast_live_worker_states().await;
                    }
                    idle_since = None;
                    continue;
                }
            // Within the 30s grace — fall through and let the summary
            // path describe the last action before settling.

        // Rate limit. The single-task design ensures only one
        // summarize call is outstanding at a time — the channel
        // accumulates triggers during the `await`, and the loop
        // services them after each call.
        if let Some(at) = last_success_at {
            let elapsed = at.elapsed();
            if elapsed < SUCCESS_COOLDOWN {
                tracing::debug!(
                    slot_id,
                    cooldown_remaining_ms =
                        SUCCESS_COOLDOWN.saturating_sub(elapsed).as_millis() as u64,
                    "live_status: skip — within success cooldown",
                );
                continue;
            }
        }

        // Resolve the transcript path lazily on the first tick (the
        // path is recorded on the WorkRun row some time after spawn).
        if tail.is_none() {
            if let Some(path) = resolver.transcript_path(&run_id).await {
                let path_str = path.display().to_string();
                tracing::info!(
                    slot_id,
                    run_id = %run_id,
                    transcript_path = %path_str,
                    "live_status: resolved transcript path; tail started",
                );
                debug_store.update(slot_id, |snap| {
                    snap.transcript_path = Some(path_str);
                });
                tail = Some(TranscriptTail::new(path));
            } else {
                tracing::info!(
                    slot_id,
                    run_id = %run_id,
                    "live_status: skip — no transcript path yet (work_runs row has NULL)",
                );
                continue;
            }
        }

        // Pull any new transcript content into the buffer.
        let mut new_lines_count = 0usize;
        if let Some(t) = tail.as_mut() {
            match t.poll().await {
                Ok(new_lines) => {
                    new_lines_count = new_lines.len();
                    transcript_buffer.extend(new_lines);
                }
                Err(err) => {
                    tracing::warn!(slot_id, ?err, "live_status: transcript tail error");
                }
            }
        }
        if transcript_buffer.len() > TRANSCRIPT_BUFFER_CAP {
            let drop_n = transcript_buffer.len() - TRANSCRIPT_BUFFER_CAP;
            transcript_buffer.drain(0..drop_n);
        }
        if transcript_buffer.is_empty() {
            tracing::debug!(
                slot_id,
                "live_status: skip — transcript buffer empty after poll",
            );
            continue;
        }

        // Pre-compute the redacted payload bytes for the debug store
        // — we'd rather pay this cost twice on the rare diagnostic
        // path than thread the byte count out of the summarizer's
        // private helper.
        let redacted_bytes =
            live_status::redact_and_assemble(&transcript_buffer).len();
        debug_store.update(slot_id, |snap| {
            snap.last_redacted_bytes = Some(redacted_bytes);
        });
        tracing::debug!(
            slot_id,
            buffer_lines = transcript_buffer.len(),
            new_lines = new_lines_count,
            redacted_bytes,
            "live_status: calling summarizer",
        );

        let outcome =
            live_status::summarize_transcript(api_key.as_deref(), &transcript_buffer).await;

        // Always update the debug store with the outcome so the
        // verb can show "last attempt" even when the loop keeps
        // retrying the same failure.
        let outcome_tag = outcome.tag().to_owned();
        let outcome_detail = outcome.detail();
        let now_epoch = epoch_now();
        debug_store.update(slot_id, |snap| {
            snap.last_outcome_tag = Some(outcome_tag.clone());
            snap.last_outcome_detail = Some(outcome_detail.clone());
            snap.last_outcome_at_epoch_s = Some(now_epoch);
        });
        tracing::info!(
            slot_id,
            outcome = %outcome_tag,
            detail = %outcome_detail,
            "live_status: summarizer outcome",
        );

        match outcome {
            SummarizerOutcome::Success(text) => {
                let prior = registry.get(slot_id).and_then(|s| s.live_status);
                let transition = match (&prior, &Some(text.clone())) {
                    (None, Some(_)) => "none->some",
                    (Some(p), Some(t)) if p == t => "some->some_same",
                    (Some(_), Some(_)) => "some->some_diff",
                    _ => "noop",
                };
                tracing::info!(
                    slot_id,
                    transition,
                    summary_prefix = %text.chars().take(80).collect::<String>(),
                    "live_status: set_live_status broadcasting",
                );
                let preview: String = text.chars().take(80).collect();
                debug_store.update(slot_id, |snap| {
                    snap.last_success_at_epoch_s = Some(now_epoch);
                    snap.last_success_text = Some(preview);
                });
                if registry.set_live_status(slot_id, Some(text)) {
                    broadcaster.broadcast_live_worker_states().await;
                }
                last_success_at = Some(Instant::now());
            }
            // On any failure, deliberately do NOT advance
            // last_success_at so the next tick can retry immediately
            // and the staleness UI sees the stamp freeze. The outcome
            // is already in the debug store + tracing above.
            SummarizerOutcome::NoApiKey
            | SummarizerOutcome::EmptyAfterRedaction
            | SummarizerOutcome::ApiError { .. }
            | SummarizerOutcome::Transport(_)
            | SummarizerOutcome::PostFilterDropped => {}
        }
    }
}

/// Decide how long until the next forced tick on the slot loop.
///
/// - `Working` with no prior success → fire after `WORKING_TIMER_FLOOR`.
/// - `Working` with a recent success → fire after the cooldown
///   completes, so the timer floor doesn't shorten the cooldown.
/// - `Idle` with the grace period running → fire when the grace
///   would expire so the clear-on-30s rule lands without waiting for
///   another hook.
/// - Any other state → effectively park (we wait on the channel
///   only); 1 hour is far longer than any real workflow window and
///   any hook event will pre-empt the sleep.
fn compute_timer_delay(
    activity: WorkerActivity,
    last_success_at: Option<Instant>,
    idle_since: Option<Instant>,
) -> Duration {
    match activity {
        WorkerActivity::Working => {
            let elapsed = last_success_at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO);
            WORKING_TIMER_FLOOR.saturating_sub(elapsed).max(Duration::from_millis(50))
        }
        WorkerActivity::Idle => {
            if let Some(t) = idle_since {
                IDLE_CLEAR_AFTER.saturating_sub(t.elapsed()).max(Duration::from_millis(50))
            } else {
                Duration::from_secs(3_600)
            }
        }
        _ => Duration::from_secs(3_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex as TokioMutex;

    #[derive(Default)]
    struct CountingBroadcaster {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LiveStatusBroadcaster for CountingBroadcaster {
        async fn broadcast_live_worker_states(&self) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct CannedResolver {
        path: TokioMutex<Option<PathBuf>>,
    }

    impl CannedResolver {
        fn new(path: Option<PathBuf>) -> Self {
            Self {
                path: TokioMutex::new(path),
            }
        }
    }

    #[async_trait]
    impl TranscriptPathResolver for CannedResolver {
        async fn transcript_path(&self, _run_id: &str) -> Option<PathBuf> {
            self.path.lock().await.clone()
        }
    }

    #[test]
    fn manager_start_replaces_existing_slot() {
        // start_slot on a slot that's already running tears down the
        // prior task before installing the new one. Otherwise a
        // re-spawn would leave the old loop alive and racing.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mgr = LiveStatusManager::new();
            let registry = Arc::new(LiveWorkerStateRegistry::new());
            registry.register_spawn(3, "run-a", "claude-opus-4-7", 0, None);
            let bc: Arc<dyn LiveStatusBroadcaster> =
                Arc::new(CountingBroadcaster::default());
            let res: Arc<dyn TranscriptPathResolver> = Arc::new(CannedResolver::new(None));
            mgr.start_slot(3, "run-a".into(), None, registry.clone(), bc.clone(), res.clone());
            assert!(mgr.has_slot(3));
            mgr.start_slot(3, "run-b".into(), None, registry, bc, res);
            assert!(mgr.has_slot(3));
            mgr.stop_slot(3);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!mgr.has_slot(3));
        });
    }

    #[tokio::test]
    async fn errored_transition_writes_literal_without_model_call() {
        // Errored → "errored — check logs" written directly. The
        // resolver has no transcript path, so any path that called
        // the model would block — this test passes only if the
        // literal short-circuit fires first.
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(1, "run-1", "claude-opus-4-7", 0, None);
        let bc = Arc::new(CountingBroadcaster::default());
        let res = Arc::new(CannedResolver::new(None));
        let bc_dyn: Arc<dyn LiveStatusBroadcaster> = bc.clone();
        let res_dyn: Arc<dyn TranscriptPathResolver> = res.clone();
        mgr.start_slot(1, "run-1".into(), None, registry.clone(), bc_dyn, res_dyn);
        mgr.notify(1, Trigger::ActivityChanged(WorkerActivity::Errored));
        // Let the task pick up the trigger and write the literal.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let state = registry.get(1).unwrap();
        assert_eq!(
            state.live_status.as_deref(),
            Some(live_status::ERRORED_LITERAL),
        );
        assert!(bc.calls.load(Ordering::Relaxed) >= 1);
        mgr.stop_slot(1);
    }

    #[tokio::test]
    async fn waiting_for_input_writes_literal_only_when_no_prior() {
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(2, "run-2", "claude-opus-4-7", 0, None);
        let bc = Arc::new(CountingBroadcaster::default());
        let res = Arc::new(CannedResolver::new(None));
        let bc_dyn: Arc<dyn LiveStatusBroadcaster> = bc.clone();
        let res_dyn: Arc<dyn TranscriptPathResolver> = res.clone();
        mgr.start_slot(2, "run-2".into(), None, registry.clone(), bc_dyn, res_dyn);
        // No prior status → literal lands.
        mgr.notify(2, Trigger::ActivityChanged(WorkerActivity::WaitingForInput));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            registry.get(2).unwrap().live_status.as_deref(),
            Some(live_status::AWAITING_INPUT_LITERAL),
        );

        // Now stamp a prior sentence and re-fire — the literal must
        // not overwrite it.
        registry.set_live_status(2, Some("investigating the bug".into()));
        mgr.notify(2, Trigger::ActivityChanged(WorkerActivity::WaitingForInput));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            registry.get(2).unwrap().live_status.as_deref(),
            Some("investigating the bug"),
        );
        mgr.stop_slot(2);
    }

    #[tokio::test]
    async fn shutdown_trigger_terminates_loop_quickly() {
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(4, "run-4", "claude-opus-4-7", 0, None);
        let bc: Arc<dyn LiveStatusBroadcaster> =
            Arc::new(CountingBroadcaster::default());
        let res: Arc<dyn TranscriptPathResolver> = Arc::new(CannedResolver::new(None));
        mgr.start_slot(4, "run-4".into(), None, registry, bc, res);
        assert!(mgr.has_slot(4));
        mgr.stop_slot(4);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!mgr.has_slot(4));
    }

    #[test]
    fn timer_delay_working_uses_floor_until_cooldown_satisfied() {
        // First Working tick with no prior success: full timer floor.
        let d = compute_timer_delay(WorkerActivity::Working, None, None);
        assert!(d >= Duration::from_secs(55), "expected ~60s, got {d:?}");
        // Just after a successful set: timer ticks down.
        let recent = Instant::now() - Duration::from_secs(10);
        let d = compute_timer_delay(WorkerActivity::Working, Some(recent), None);
        assert!(d <= Duration::from_secs(50) && d >= Duration::from_secs(45));
    }

    #[test]
    fn timer_delay_idle_clamps_to_grace_remaining() {
        let recent = Instant::now() - Duration::from_secs(5);
        let d = compute_timer_delay(WorkerActivity::Idle, None, Some(recent));
        assert!(d <= Duration::from_secs(26) && d >= Duration::from_secs(20));
    }

    #[test]
    fn timer_delay_parks_in_quiet_states() {
        // Spawning / Terminated / Errored / WaitingForInput all park
        // the loop on the channel.
        for activity in [
            WorkerActivity::Spawning,
            WorkerActivity::Terminated,
            WorkerActivity::Errored,
            WorkerActivity::WaitingForInput,
        ] {
            let d = compute_timer_delay(activity, None, None);
            assert!(d >= Duration::from_secs(60), "{activity:?}: {d:?}");
        }
    }

    #[tokio::test]
    async fn disabled_slot_clears_prior_status_and_skips_model_call() {
        // Toggle the slot off; the loop should clear any prior
        // status, broadcast the change, and (since there's no
        // transcript path) not even attempt a model call. With
        // the slot disabled, repeated triggers must not lead to
        // a non-None status reappearing.
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(6, "run-6", "claude-opus-4-7", 0, None);
        registry.set_live_status(6, Some("doing a thing".into()));
        let bc = Arc::new(CountingBroadcaster::default());
        let res = Arc::new(CannedResolver::new(None));
        let bc_dyn: Arc<dyn LiveStatusBroadcaster> = bc.clone();
        let res_dyn: Arc<dyn TranscriptPathResolver> = res.clone();
        mgr.start_slot(6, "run-6".into(), None, registry.clone(), bc_dyn, res_dyn);
        // Mark Working so the disable arm is the only barrier.
        mgr.notify(6, Trigger::ActivityChanged(WorkerActivity::Working));
        mgr.set_enabled(6, false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(registry.get(6).unwrap().live_status.is_none());
        // Subsequent triggers stay quiet.
        mgr.notify(6, Trigger::Stop);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(registry.get(6).unwrap().live_status.is_none());
        mgr.stop_slot(6);
    }

    #[test]
    fn disabled_snapshot_returns_sorted_slot_ids() {
        let mgr = LiveStatusManager::new();
        mgr.set_initial_disabled_slots([3, 1, 7]);
        assert_eq!(mgr.disabled_snapshot(), vec![1, 3, 7]);
    }

    #[test]
    fn set_initial_disabled_slots_replaces_prior_set() {
        let mgr = LiveStatusManager::new();
        mgr.set_initial_disabled_slots([1, 2, 3]);
        mgr.set_initial_disabled_slots([5]);
        assert_eq!(mgr.disabled_snapshot(), vec![5]);
    }

    #[test]
    fn notify_returns_false_for_unknown_slot() {
        // Hook events that arrive before `start_slot` (or after
        // `stop_slot`) used to be silently dropped — one of the
        // suspected silent-failure modes the chore exists to surface.
        // The current implementation logs a `warn` on the drop; we
        // still assert the return value so a future refactor can't
        // accidentally swallow the false without us noticing.
        let mgr = LiveStatusManager::new();
        let delivered = mgr.notify(99, Trigger::Stop);
        assert!(!delivered, "notify on a slot with no task must return false");
    }

    #[tokio::test]
    async fn debug_store_records_last_trigger_kind() {
        // The `bossctl live-status debug` verb reads
        // `LiveStatusManager::debug_store()`. Confirm that a notify
        // round-trips into the per-slot snapshot — without this, the
        // verb would always report "no trigger received yet" even on
        // an actively-running worker.
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(7, "run-7", "claude-opus-4-7", 0, None);
        let bc: Arc<dyn LiveStatusBroadcaster> =
            Arc::new(CountingBroadcaster::default());
        let res: Arc<dyn TranscriptPathResolver> = Arc::new(CannedResolver::new(None));
        mgr.start_slot(7, "run-7".into(), None, registry, bc, res);
        mgr.notify(7, Trigger::Stop);
        tokio::time::sleep(Duration::from_millis(40)).await;
        let snap = mgr.debug_store().snapshot_for(7);
        assert_eq!(snap.last_trigger_kind.as_deref(), Some("stop"));
        assert!(snap.last_trigger_at_epoch_s.is_some());
        mgr.stop_slot(7);
    }

    #[test]
    fn stop_slot_forgets_debug_snapshot() {
        // The snapshot for a released slot must not leak into a
        // subsequent re-spawn.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mgr = LiveStatusManager::new();
            let registry = Arc::new(LiveWorkerStateRegistry::new());
            registry.register_spawn(8, "run-8", "claude-opus-4-7", 0, None);
            let bc: Arc<dyn LiveStatusBroadcaster> =
                Arc::new(CountingBroadcaster::default());
            let res: Arc<dyn TranscriptPathResolver> = Arc::new(CannedResolver::new(None));
            mgr.start_slot(8, "run-8".into(), None, registry, bc, res);
            mgr.notify(8, Trigger::Stop);
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(mgr.debug_store().snapshot_for(8).last_trigger_kind.is_some());
            mgr.stop_slot(8);
            // After stop_slot the snapshot is wiped.
            let snap = mgr.debug_store().snapshot_for(8);
            assert!(snap.last_trigger_kind.is_none());
        });
    }

    #[tokio::test]
    async fn budget_smoke_post_tool_use_only_every_k_triggers_summarize() {
        // Acceptance hook from chore 5: at the rate limit, K-1 of
        // every K PostToolUse triggers must be coalesced. We can't
        // exercise the model end-to-end in unit tests without a wire
        // mock, but we can prove the post-tool-use counter is the
        // only thing fronting the model call by checking that K-1
        // notifies do not advance the broadcast count past the
        // ActivityChanged → WaitingForInput literal that started it.
        let mgr = LiveStatusManager::new();
        let registry = Arc::new(LiveWorkerStateRegistry::new());
        registry.register_spawn(5, "run-5", "claude-opus-4-7", 0, None);
        let bc = Arc::new(CountingBroadcaster::default());
        let res = Arc::new(CannedResolver::new(None));
        let bc_dyn: Arc<dyn LiveStatusBroadcaster> = bc.clone();
        let res_dyn: Arc<dyn TranscriptPathResolver> = res.clone();
        mgr.start_slot(5, "run-5".into(), None, registry, bc_dyn, res_dyn);
        // Drive the slot into Working so the post-tool-use trigger
        // doesn't hit the quiet-state guard.
        mgr.notify(5, Trigger::ActivityChanged(WorkerActivity::Working));
        for _ in 0..(POST_TOOL_USE_K - 1) {
            mgr.notify(5, Trigger::PostToolUse);
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        // K-1 sub-modulo notifies all coalesce — no broadcast.
        assert_eq!(bc.calls.load(Ordering::Relaxed), 0);
        mgr.stop_slot(5);
    }

    /// Phase-4 acceptance test: an increment via the legacy `inc_*`
    /// wrapper and a direct increment via the framework handle both
    /// show up in `snapshot()` and in `registry.counter_value()`.
    #[test]
    fn dispatcher_stats_dual_surface_consistency() {
        let registry = Arc::new(Registry::new());
        register_metrics(&registry);
        let stats = DispatcherStats::new(registry.clone());

        // Increment via the legacy shim.
        stats.inc_hook_events_total();

        let snap = stats.snapshot();
        assert_eq!(snap.hook_events_total, 1, "snapshot must reflect inc_* increment");
        assert_eq!(
            registry.counter_value(DISPATCHER_HOOK_EVENTS_TOTAL.name()),
            Some(1),
            "registry.counter_value must reflect inc_* increment"
        );

        // Increment via the framework handle directly.
        DISPATCHER_HOOK_EVENTS_TOTAL.inc(&registry);

        let snap2 = stats.snapshot();
        assert_eq!(
            snap2.hook_events_total, 2,
            "snapshot must reflect direct handle increment"
        );
        assert_eq!(
            registry.counter_value(DISPATCHER_HOOK_EVENTS_TOTAL.name()),
            Some(2),
            "registry.counter_value must reflect direct handle increment"
        );

        // Spot-check a second counter via both surfaces.
        stats.inc_persist_updated();
        stats.inc_persist_updated();
        let snap3 = stats.snapshot();
        assert_eq!(snap3.transcript_path_persist_updated, 2);
        assert_eq!(
            registry.counter_value(DISPATCHER_TRANSCRIPT_PATH_PERSIST_UPDATED.name()),
            Some(2),
        );
    }
}
