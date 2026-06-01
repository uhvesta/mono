//! Engine-owned reconciler that auto-recovers workers wedged by a
//! *transient* Claude API error.
//!
//! ## The failure this closes
//!
//! Boss launches each worker as an **interactive** `claude` session in
//! a libghostty pane (`runner.rs`: `claude … "$(cat initial-prompt.txt)"`
//! with no `--print`). When claude exhausts its own internal retries on
//! a transient API error — "API Error: The socket connection was closed
//! unexpectedly", `overloaded_error`, a 5xx, a 429, a request timeout —
//! it prints the error, ends the turn, and returns to its REPL. The
//! events socket reports the turn-ending `Stop` as `Idle`, so the
//! worker *looks done* while actually being wedged mid-chore. The
//! dead-PID sweep can't see it (the process is alive); the completion
//! path can't see it (no PR, no clean finish). Before this module a
//! human had to notice and restart the run (the Yar/T678 incident).
//!
//! ## Design: nudge-first, orphan+respawn as fallback
//!
//! On a transient API error the worker's `claude` process stays alive
//! at its REPL — it printed the error, ended the turn, and returned to
//! its prompt. For this alive-but-idle case the cheap first recovery is
//! a runtime nudge ("your previous turn ended on a transient API error;
//! please retry the last step") injected via the same channel as
//! `bossctl agents send`. Full orphan+respawn (spawn a fresh `claude`
//! process on the same workspace) is reserved for cases where the
//! worker is actually dead, the nudge did not clear the error by the
//! next sweep, or the error is permanent.
//!
//! Each pass, for every non-actively-working worker slot whose backing
//! execution is old enough ([`RECOVERY_GRACE_SECS`]):
//!
//!   1. Read the worker's transcript tail — the authoritative signal.
//!      [`crate::transient_error::extract_worker_error`] returns the
//!      halting API-error text **only if it is the last meaningful
//!      entry** (if the worker did any work after the error it
//!      recovered on its own and we leave it alone). We never trust the
//!      `Idle` hook alone — it can't distinguish "finished cleanly" from
//!      "wedged on an error."
//!   2. Classify the error
//!      ([`crate::transient_error::classify_claude_error`]) and apply
//!      the bounded-retry policy
//!      ([`crate::transient_error::RecoveryPolicy`]).
//!   3. **Nudge** (transient, under cap, worker alive and idle, not
//!      already nudged this session): send a runtime message into the
//!      existing `claude` REPL asking it to retry. If the nudge fails
//!      (send error, unknown slot, etc.) fall through to orphan+respawn.
//!      On the next sweep if the error is still the last entry,
//!      increment the attempt counter and proceed to orphan+respawn.
//!   4. **Resume** (transient, under the cap, not nudgeable): orphan the
//!      dead execution and insert a fresh `ready` one that prefers the
//!      same cube workspace (so `--prefer` re-leases it and the
//!      in-progress jj branch is not lost), carrying an incremented
//!      `transient_failure_count` and a `dispatch_not_before` backoff.
//!      The runner's existing startup-recovery prompt then directs the
//!      new worker to resume the prior branch.
//!   5. **Escalate** (permanent / unrecognised / retry cap reached):
//!      raise a `WorkAttentionItem` and stop. The orphan-active sweep
//!      excludes work items with an open recovery attention item
//!      (`list_orphan_active_candidates`), so a non-retryable failure is
//!      not blindly re-dispatched.
//!
//! ## No infinite loop
//!
//! The retry cap and exponential backoff live in
//! [`crate::transient_error::RecoveryPolicy`]; after the cap the sweep
//! escalates instead of resuming. The incremented
//! `transient_failure_count` is carried across resume executions, so the
//! cap holds across the whole chain, not per-execution. A nudge does NOT
//! consume a retry slot — the orphan+respawn cap stays at 3 — but each
//! execution only gets one nudge attempt per engine session before the
//! sweep falls back to orphan+respawn.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;

use boss_protocol::{WorkItemPatch, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::transient_error::{
    EscalateReason, RecoveryDecision, RecoveryPolicy, classify_claude_error, extract_worker_error,
};
use crate::work::{
    ATTENTION_KIND_RECOVERY_EXHAUSTED, ATTENTION_KIND_RECOVERY_PERMANENT, WorkDb,
};

/// How often the sweep runs.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Skip executions whose `started_at` is within this many seconds (or
/// not yet recorded). Guards against acting on a worker that just
/// spawned or just hit a blip claude may still be retrying internally —
/// the transcript only carries the *final* API error after claude gives
/// up, but the grace keeps us from racing a fresh dispatch.
pub const RECOVERY_GRACE_SECS: i64 = 60;

/// Only the tail of the transcript matters (we want the last entry).
/// Reading a bounded suffix keeps the sweep cheap even for multi-MB
/// transcripts.
const TRANSCRIPT_TAIL_MAX_BYTES: u64 = 256 * 1024;

/// Clip error strings to this many bytes before putting them on a
/// dispatch event or attention item.
const ERROR_CLIP_BYTES: usize = 240;

/// Inject text into a live worker's REPL without tearing it down.
/// The recovery sweep uses this to nudge an idle-but-wedged worker
/// before falling back to the heavier orphan+respawn path.
#[async_trait]
pub trait WorkerNudger: Send + Sync {
    async fn nudge_worker(&self, run_id: &str, text: String) -> Result<(), String>;
}

/// No-op nudger used in tests and contexts without an app session.
/// Always returns `Err`, which causes the sweep to fall through to
/// the orphan+respawn path — preserving pre-nudge test behaviour.
pub struct NoopWorkerNudger;

#[async_trait]
impl WorkerNudger for NoopWorkerNudger {
    async fn nudge_worker(&self, _run_id: &str, _text: String) -> Result<(), String> {
        Err("no nudger configured".into())
    }
}

/// Counts from one pass; logged at `info` when anything happened.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TransientRecoveryOutcome {
    /// Workers sent a runtime nudge (alive-idle path).
    pub nudged: usize,
    /// Workers orphaned and re-queued via the full orphan+respawn path.
    pub resumed: usize,
    pub escalated: usize,
    pub grace_skipped: usize,
    pub no_error_skipped: usize,
}

impl TransientRecoveryOutcome {
    fn has_activity(&self) -> bool {
        self.nudged > 0 || self.resumed > 0 || self.escalated > 0
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// firing immediately on spawn.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    nudger: Arc<dyn WorkerNudger>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let policy = RecoveryPolicy::default();
    tokio::spawn(async move {
        // Execution IDs that received a runtime nudge this engine session.
        // On the next sweep pass, if the error is still present, we skip
        // the nudge and fall through to orphan+respawn. Keyed by the
        // original execution ID (not the replacement), so stale entries
        // from completed/orphaned executions are harmless.
        let mut nudged_executions: HashSet<String> = HashSet::new();
        loop {
            let now = current_epoch_s();
            let outcome = run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                &policy,
                nudger.as_ref(),
                &mut nudged_executions,
                now,
            )
            .await;
            if outcome.has_activity() {
                tracing::info!(
                    nudged = outcome.nudged,
                    resumed = outcome.resumed,
                    escalated = outcome.escalated,
                    "transient-recovery sweep: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single recovery pass. `now_epoch_secs` is injected so tests
/// can pin the clock for the grace guard.
///
/// `nudger` is used to send a runtime message into a live idle worker's
/// REPL instead of tearing it down. `nudged_executions` persists across
/// calls (owned by the spawn loop) so the sweep knows which executions
/// have already been nudged and should proceed to orphan+respawn.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    policy: &RecoveryPolicy,
    nudger: &dyn WorkerNudger,
    nudged_executions: &mut HashSet<String>,
    now_epoch_secs: i64,
) -> TransientRecoveryOutcome {
    let mut outcome = TransientRecoveryOutcome::default();
    let grace_cutoff = now_epoch_secs - RECOVERY_GRACE_SECS;

    for state in live_states.snapshot() {
        // Skip slots that are actively progressing or not yet up — only
        // a wedged-idle / errored / terminated slot can be stalled on an
        // API error. (A working slot's last transcript entry is a tool
        // call, never the trailing error, so it would be filtered out
        // anyway; this just avoids the file read.)
        if !should_inspect(state.activity) {
            continue;
        }

        let execution_id = state.run_id.clone();
        let execution = match work_db.get_execution(&execution_id) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(execution_id, ?err, "transient-recovery: execution lookup failed");
                continue;
            }
        };
        // Terminal executions are settled (completion path / dead-PID
        // sweep / a prior recovery pass handled them).
        if execution_status_is_terminal(&execution.status) {
            continue;
        }

        // Grace guard: don't act on a worker that only just started.
        let started_epoch = execution
            .started_at
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok());
        match started_epoch {
            Some(t) if t < grace_cutoff => {}
            _ => {
                outcome.grace_skipped += 1;
                continue;
            }
        }

        // Ground truth: the transcript. No path → no signal → leave it
        // for the other reconcilers.
        let Some(transcript_path) = work_db
            .latest_transcript_path(&execution_id)
            .ok()
            .flatten()
        else {
            outcome.no_error_skipped += 1;
            continue;
        };
        let lines = read_transcript_tail(&transcript_path, TRANSCRIPT_TAIL_MAX_BYTES).await;
        let Some(error_text) = extract_worker_error(&lines) else {
            // No trailing API error: worker either finished cleanly or
            // recovered on its own. Not ours to touch.
            outcome.no_error_skipped += 1;
            continue;
        };

        let class = classify_claude_error(&error_text);
        let prior_attempts = execution.transient_failure_count.max(0) as u32;
        let decision = policy.decide(class, prior_attempts);
        let clipped = clip(&error_text, ERROR_CLIP_BYTES);
        let work_item_id = state
            .work_item_id
            .clone()
            .unwrap_or_else(|| execution.work_item_id.clone());

        match decision {
            RecoveryDecision::Resume { attempt, backoff } => {
                // Prefer a cheap runtime nudge when the worker is alive
                // and idle. Only nudge once per execution per engine
                // session: if it didn't clear the error by the next
                // sweep, fall through to orphan+respawn.
                let already_nudged = nudged_executions.remove(&execution_id);
                let try_nudge =
                    !already_nudged && state.activity == WorkerActivity::Idle;

                if try_nudge {
                    let msg = format!(
                        "Your previous turn ended on a transient Claude API error. \
                         Please retry the last step.\n\nError: {clipped}\n"
                    );
                    match nudger.nudge_worker(&execution_id, msg).await {
                        Ok(()) => {
                            nudged_executions.insert(execution_id.clone());
                            tracing::info!(
                                execution_id,
                                work_item_id = %work_item_id,
                                error = %clipped,
                                "transient-recovery: nudged live idle worker; will re-check next sweep",
                            );
                            dispatch_events
                                .emit(
                                    DispatchEvent::new(
                                        Stage::TransientRecoveryNudge,
                                        Outcome::Ok,
                                        &execution_id,
                                    )
                                    .with_work_item(&work_item_id)
                                    .with_details(serde_json::json!({
                                        "error": clipped,
                                        "class": "transient",
                                    })),
                                )
                                .await;
                            outcome.nudged += 1;
                            continue; // leave slot and execution intact
                        }
                        Err(nudge_err) => {
                            tracing::info!(
                                execution_id,
                                work_item_id = %work_item_id,
                                nudge_err,
                                "transient-recovery: nudge not available; falling back to orphan+respawn",
                            );
                            // Fall through to the orphan+respawn path below.
                        }
                    }
                }

                // --- Orphan+respawn path ---
                let dispatch_not_before = now_epoch_secs + backoff.as_secs() as i64;
                let reason = format!(
                    "transient Claude API error (auto-resume attempt {attempt}/{max}): {clipped}",
                    max = policy.max_attempts(),
                );
                if let Err(err) = work_db.request_resume_execution(
                    &execution_id,
                    attempt as i64,
                    dispatch_not_before,
                    &reason,
                ) {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "transient-recovery: failed to create resume execution; skipping",
                    );
                    continue;
                }
                tracing::info!(
                    execution_id,
                    work_item_id = %work_item_id,
                    attempt,
                    max_attempts = policy.max_attempts(),
                    backoff_secs = backoff.as_secs(),
                    error = %clipped,
                    "transient-recovery: worker stalled on transient API error; auto-resuming on same workspace",
                );
                release_slot(&coordinator, state.slot_id).await;
                append_recovery_audit(
                    work_db,
                    &work_item_id,
                    &format!(
                        "transient API error; auto-resuming attempt {attempt}/{max} after {secs}s backoff",
                        max = policy.max_attempts(),
                        secs = backoff.as_secs(),
                    ),
                    now_epoch_secs,
                );
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::TransientRecovery, Outcome::Ok, &execution_id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "attempt": attempt,
                                "max_attempts": policy.max_attempts(),
                                "backoff_secs": backoff.as_secs(),
                                "class": "transient",
                                "error": clipped,
                            })),
                    )
                    .await;

                // Defer a kick until the backoff window expires so the
                // resume dispatches promptly, plus an immediate kick so
                // the coordinator notices the freed slot.
                coordinator.kick();
                let coordinator = coordinator.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(backoff).await;
                    coordinator.kick();
                });
                outcome.resumed += 1;
            }
            RecoveryDecision::Escalate { reason } => {
                let (kind, class_label) = match reason {
                    EscalateReason::Permanent => (ATTENTION_KIND_RECOVERY_PERMANENT, "permanent"),
                    EscalateReason::Indeterminate => {
                        (ATTENTION_KIND_RECOVERY_PERMANENT, "indeterminate")
                    }
                    EscalateReason::RetriesExhausted => {
                        (ATTENTION_KIND_RECOVERY_EXHAUSTED, "transient")
                    }
                };
                // Settle the execution so it isn't re-inspected; ignore a
                // race where another reconciler already marked it terminal.
                if let Err(err) = work_db.mark_execution_orphaned(
                    &execution_id,
                    &format!("transient-recovery escalation ({}): {clipped}", reason.as_str()),
                ) {
                    tracing::debug!(
                        execution_id,
                        ?err,
                        "transient-recovery: execution already terminal at escalation (benign)",
                    );
                }
                let title = match reason {
                    EscalateReason::RetriesExhausted => {
                        "Worker auto-recovery exhausted retries".to_owned()
                    }
                    _ => "Worker hit a non-retryable Claude API error".to_owned(),
                };
                let body = format!(
                    "The engine stopped auto-resuming this work item.\n\n\
                     **Reason:** {reason}\n\n\
                     **Error class:** {class_label}\n\n\
                     **Last worker error:** {clipped}\n\n\
                     **Transient resume attempts already made:** {prior_attempts} / {max}\n\n\
                     Resolve this attention item once the underlying problem is fixed to \
                     allow the work item to be re-dispatched.",
                    reason = reason.as_str(),
                    max = policy.max_attempts(),
                );
                if let Err(err) =
                    work_db.upsert_work_item_attention(&work_item_id, kind, &title, &body)
                {
                    tracing::warn!(
                        execution_id,
                        work_item_id = %work_item_id,
                        ?err,
                        "transient-recovery: failed to raise attention item",
                    );
                }
                tracing::warn!(
                    execution_id,
                    work_item_id = %work_item_id,
                    reason = reason.as_str(),
                    class = class_label,
                    error = %clipped,
                    "transient-recovery: escalating worker for human attention (not auto-retried)",
                );
                release_slot(&coordinator, state.slot_id).await;
                dispatch_events
                    .emit(
                        DispatchEvent::new(
                            Stage::TransientRecoveryExhausted,
                            Outcome::Error,
                            &execution_id,
                        )
                        .with_work_item(&work_item_id)
                        .with_details(serde_json::json!({
                            "reason": reason.as_str(),
                            "class": class_label,
                            "prior_attempts": prior_attempts,
                            "max_attempts": policy.max_attempts(),
                            "error": clipped,
                        })),
                    )
                    .await;
                coordinator.kick();
                outcome.escalated += 1;
            }
        }
    }

    outcome
}

/// True for slot states a stalled-on-error worker can be in. A
/// `Working`/`Spawning` slot is actively progressing (or not yet up).
fn should_inspect(activity: WorkerActivity) -> bool {
    matches!(
        activity,
        WorkerActivity::Idle
            | WorkerActivity::WaitingForInput
            | WorkerActivity::Errored
            | WorkerActivity::Terminated
    )
}

async fn release_slot(coordinator: &Arc<ExecutionCoordinator>, slot_id: u8) {
    // Use worker_id_for_slot (not WorkerPool::worker_id_for_slot) so
    // automation-pool slots (> MAX_WORKER_POOL_SIZE) produce the
    // "auto-worker-N" prefix and release_worker_and_kick routes to
    // the correct pool via pool_for_worker_id.
    let worker_id = worker_id_for_slot(slot_id);
    coordinator
        .release_worker_and_kick(&worker_id, None)
        .await;
}

/// Append an `[engine-reconcile]` audit line to the work item's
/// description so the recovery is visible in the chore detail. Best
/// effort — a failure here never blocks recovery.
fn append_recovery_audit(work_db: &WorkDb, work_item_id: &str, note: &str, now_epoch_secs: i64) {
    let item = match work_db.get_work_item(work_item_id) {
        Ok(i) => i,
        Err(err) => {
            tracing::warn!(work_item_id, ?err, "transient-recovery: audit lookup failed");
            return;
        }
    };
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => {
            t.description.as_str()
        }
    };
    let new_desc =
        format!("{current_desc}\n[engine-reconcile] epoch {now_epoch_secs}: {note}.");
    if let Err(err) = work_db.update_work_item(
        work_item_id,
        WorkItemPatch {
            description: Some(new_desc),
            ..WorkItemPatch::default()
        },
    ) {
        tracing::warn!(work_item_id, ?err, "transient-recovery: audit append failed (non-fatal)");
    }
}

/// Read the last `max_bytes` of a transcript file and parse the
/// complete JSONL lines within. Tolerant: a missing file, an unreadable
/// file, or malformed lines yield an empty/partial vec rather than an
/// error — recovery should never crash on a bad transcript.
async fn read_transcript_tail(path: &str, max_bytes: u64) -> Vec<Value> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let (seek_to, drop_first_partial) = if len > max_bytes {
        (len - max_bytes, true)
    } else {
        (0, false)
    };
    if file.seek(SeekFrom::Start(seek_to)).await.is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).await.is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut iter = text.lines();
    // If we seeked into the middle of the file the first line is likely
    // a partial JSON fragment — drop it.
    if drop_first_partial {
        iter.next();
    }
    iter.filter_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            None
        } else {
            serde_json::from_str::<Value>(trimmed).ok()
        }
    })
    .collect()
}

fn clip(s: &str, max_bytes: usize) -> String {
    let one_line = s.trim().replace('\n', " ");
    if one_line.len() <= max_bytes {
        one_line
    } else {
        let mut end = max_bytes;
        while !one_line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &one_line[..end])
    }
}

fn execution_status_is_terminal(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "abandoned" | "cancelled" | "orphaned"
    )
}

pub fn current_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Write;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::WorkItemBinding;
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease,
        CubeWorkspaceStatus, ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::transient_error::RecoveryPolicy;
    use crate::work::{CreateChoreInput, CreateProductInput, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    // ─── stubs ────────────────────────────────────────────────────────

    struct NoopCube;

    #[async_trait]
    impl CubeClient for NoopCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::PathBuf, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    struct NoopRunner;

    #[async_trait]
    impl ExecutionRunner for NoopRunner {
        async fn run_execution(
            &self,
            _worker_id: &str,
            _execution: &WorkExecution,
            _work_item: &crate::work::WorkItem,
            _workspace_path: &std::path::Path,
            _cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!()
        }
    }

    /// Records which run_ids were nudged. Used to assert nudge behaviour
    /// without needing a real app session.
    struct RecordingNudger {
        nudged: tokio::sync::Mutex<Vec<String>>,
    }

    impl RecordingNudger {
        fn new() -> Self {
            Self {
                nudged: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        async fn nudged_ids(&self) -> Vec<String> {
            self.nudged.lock().await.clone()
        }
    }

    #[async_trait]
    impl WorkerNudger for RecordingNudger {
        async fn nudge_worker(&self, run_id: &str, _text: String) -> Result<(), String> {
            self.nudged.lock().await.push(run_id.to_owned());
            Ok(())
        }
    }

    // ─── helpers ──────────────────────────────────────────────────────

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn create_product(db: &WorkDb) -> String {
        db.create_product(CreateProductInput {
            name: "test-product".to_owned(),
            description: None,
            repo_remote_url: Some("https://github.com/test/repo".to_owned()),
            design_repo: None,
            docs_repo: None,
            worker_branch_prefix: None,
        })
        .unwrap()
        .id
    }

    fn create_active_chore(db: &WorkDb, product_id: &str) -> String {
        let chore = db
            .create_chore(CreateChoreInput {
                product_id: product_id.to_owned(),
                name: "test chore".to_owned(),
                description: None,
                repo_remote_url: None,
                priority: None,
                effort_level: None,
                model_override: None,
                created_via: None,
                autostart: true,
                force_duplicate: false,
            })
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        chore.id
    }

    /// Create a `running` execution with a backdated `started_at` (past
    /// the grace window) and a run whose transcript is `transcript_path`.
    fn create_running_execution(
        db: &WorkDb,
        work_item_id: &str,
        transcript_path: &str,
        prior_transient_failures: i64,
    ) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder()
                .work_item_id(work_item_id)
                .preferred_workspace_id("mono-agent-007")
                .build())
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-007",
            "/tmp/mono-agent-007",
        )
        .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, transcript_path)
            .unwrap();
        if prior_transient_failures > 0 {
            db.force_transient_failure_count_for_test(&execution.id, prior_transient_failures)
                .unwrap();
        }
        let old_started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(600) as i64;
        db.force_started_at_for_test(&execution.id, old_started)
            .unwrap();
        execution.id
    }

    fn write_transcript(dir: &TempDir, name: &str, lines: &[&str]) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    fn make_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(pool_size),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

    fn register_idle_slot(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
        // Drive Spawning → Idle via a Stop event (no pending notification).
        live_states.apply_event(
            slot_id,
            &boss_protocol::WorkerEvent::Stop {
                session_id: "s".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
        );
    }

    const SOCKET_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}}"#;
    const AUTH_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 401 authentication_error: invalid x-api-key"}]}}"#;
    const NORMAL_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on the task"}]}}"#;

    fn now() -> i64 {
        super::current_epoch_s()
    }

    // ─── tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transient_error_nudges_live_idle_worker() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&exec_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let nudger = RecordingNudger::new();
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let mut nudged = HashSet::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &nudger,
            &mut nudged,
            now(),
        )
        .await;

        // First pass: should nudge, not orphan+respawn.
        assert_eq!(outcome.nudged, 1, "alive idle worker should be nudged first");
        assert_eq!(outcome.resumed, 0, "should not orphan+respawn on first nudge");
        assert_eq!(outcome.escalated, 0);
        assert!(nudged.contains(&exec_id), "execution should be in nudged set");
        assert_eq!(nudger.nudged_ids().await, vec![exec_id.clone()]);

        // Execution is still running (not orphaned).
        assert_eq!(db.get_execution(&exec_id).unwrap().status, "running");

        // One transient_recovery_nudge dispatch event.
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery_nudge");
        assert_eq!(events[0].outcome, "ok");
    }

    #[tokio::test]
    async fn nudged_worker_still_stalled_falls_back_to_orphan_respawn() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&exec_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        // Pre-populate nudged set to simulate a prior-pass nudge.
        let mut nudged = HashSet::new();
        nudged.insert(exec_id.clone());

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut nudged,
            now(),
        )
        .await;

        // Second pass: nudge already tried, error still present → orphan+respawn.
        assert_eq!(outcome.resumed, 1, "second pass should orphan+respawn");
        assert_eq!(outcome.nudged, 0);
        assert!(!nudged.contains(&exec_id), "id removed from nudged set on orphan+respawn");

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        let dead = execs.iter().find(|e| e.id == exec_id).unwrap();
        assert_eq!(dead.status, "orphaned");
        let fresh = execs
            .iter()
            .find(|e| e.id != exec_id && e.status == "ready")
            .expect("expected a fresh ready execution");
        assert_eq!(fresh.preferred_workspace_id.as_deref(), Some("mono-agent-007"));
        assert_eq!(fresh.transient_failure_count, 1);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery");
    }

    #[tokio::test]
    async fn transient_error_resumes_on_same_workspace() {
        // NoopWorkerNudger always fails → falls through to orphan+respawn.
        // Exercises the pre-nudge behaviour for contexts where nudge is unavailable.
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&dead_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome.resumed, 1, "noop nudger falls through to orphan+respawn");
        assert_eq!(outcome.escalated, 0);

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        let dead = execs.iter().find(|e| e.id == dead_id).unwrap();
        assert_eq!(dead.status, "orphaned");
        let fresh = execs
            .iter()
            .find(|e| e.id != dead_id && e.status == "ready")
            .expect("expected a fresh ready execution");
        assert_eq!(fresh.preferred_workspace_id.as_deref(), Some("mono-agent-007"));
        assert_eq!(fresh.transient_failure_count, 1);
        assert!(
            fresh.dispatch_not_before.is_some(),
            "resume must be deferred by a backoff window",
        );

        let claimed = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(!claimed.contains(&dead_id));

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery");
        assert_eq!(events[0].outcome, "ok");
    }

    #[tokio::test]
    async fn permanent_error_escalates_and_does_not_resume() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, AUTH_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&dead_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome.escalated, 1, "permanent error should escalate");
        assert_eq!(outcome.resumed, 0, "permanent error must NOT resume");

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            !execs.iter().any(|e| e.id != dead_id && e.status == "ready"),
            "permanent error must not create a resume execution",
        );
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn.len(), 1);
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_PERMANENT);
        assert_eq!(attn[0].status, "open");

        let candidates = db.list_orphan_active_candidates(0).unwrap();
        assert!(
            !candidates.contains(&work_item_id),
            "escalated item must be excluded from orphan-active redispatch",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery_exhausted");
    }

    #[tokio::test]
    async fn transient_error_at_cap_escalates_exhausted() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        // Already at the cap (3 prior transient resumes).
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 3);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&dead_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome.escalated, 1, "at cap, must escalate not resume");
        assert_eq!(outcome.resumed, 0);
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_EXHAUSTED);
    }

    #[tokio::test]
    async fn worker_that_recovered_on_its_own_is_left_alone() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        // Error, then more work → recovered. extract_worker_error → None.
        let transcript =
            write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE, NORMAL_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator
            .worker_pool()
            .claim_worker(&dead_id, None)
            .await
            .unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.escalated, 0);
        assert_eq!(outcome.no_error_skipped, 1);
        assert_eq!(db.get_execution(&dead_id).unwrap().status, "running");
        assert!(sink.events().await.is_empty());
    }

    #[tokio::test]
    async fn fresh_execution_within_grace_is_skipped() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder()
                .work_item_id(work_item_id.clone())
                .preferred_workspace_id("mono-agent-007")
                .build())
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-007",
            "/tmp/mono-agent-007",
        )
        .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, &transcript)
            .unwrap();
        // started_at = NOW (within grace).
        db.force_started_at_for_test(&execution.id, now()).unwrap();

        let live = Arc::new(LiveWorkerStateRegistry::new());
        register_idle_slot(&live, 1, &execution.id, &work_item_id);
        let coordinator = make_coordinator(db.clone(), 2);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.escalated, 0);
        assert_eq!(outcome.grace_skipped, 1);
    }

    #[tokio::test]
    async fn actively_working_slot_is_not_inspected() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        live.register_spawn(
            1,
            &dead_id,
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "c".to_owned(),
                execution_id: dead_id.clone(),
            }),
        );
        // Drive to Working via PreToolUse — must be skipped.
        live.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        let coordinator = make_coordinator(db.clone(), 2);

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live,
            coordinator.clone(),
            sink.as_ref(),
            &RecoveryPolicy::default(),
            &NoopWorkerNudger,
            &mut HashSet::new(),
            now(),
        )
        .await;

        assert_eq!(outcome, TransientRecoveryOutcome::default());
    }

    #[tokio::test]
    async fn read_transcript_tail_handles_missing_file() {
        let lines = read_transcript_tail("/nonexistent/transcript.jsonl", 1024).await;
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn read_transcript_tail_bounds_and_drops_partial_first_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // A long padding line, then a couple of valid JSON lines.
        writeln!(f, "{}", "x".repeat(5000)).unwrap();
        writeln!(f, r#"{{"i":1}}"#).unwrap();
        writeln!(f, r#"{{"i":2}}"#).unwrap();
        drop(f);
        // max_bytes smaller than the file → seek into the padding line,
        // which gets dropped as a partial; the two JSON lines survive.
        let lines = read_transcript_tail(&path.to_string_lossy(), 64).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["i"], 1);
        assert_eq!(lines[1]["i"], 2);
    }
}
