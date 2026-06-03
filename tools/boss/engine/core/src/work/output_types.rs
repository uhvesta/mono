use super::*;

/// Where the chore should land after [`WorkDb::record_worker_pr_completion`].
/// `InReview` is the typical case (open PR, ready for human review);
/// `Done` is used when the PR was already merged at the time the
/// worker's Stop event fired, so we skip the review column entirely.
/// `PendingReview` (P992) is used when an independent reviewer pass
/// is enqueued: the task's `pr_url` is stamped but its status is *not*
/// advanced — the task stays in the Doing column until the reviewer resolves
/// (or the fallback timeout fires).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPrCompletionTarget {
    InReview,
    Done,
    /// Task `pr_url` is stamped; task `status` is unchanged. The independent
    /// reviewer pass (P992) drives the subsequent `active → in_review`
    /// transition once the review pass resolves (or the timeout fires).
    PendingReview,
}

/// Outcome of [`WorkDb::set_run_transcript_path_if_unset`]. The third
/// variant exists to keep "the latest run for this execution already
/// has a transcript_path" (legitimate no-op) distinguishable from
/// "no `work_runs` row exists for this execution yet" (real problem,
/// either a startup race or a wrong-namespace identifier). Returning
/// a flat `bool` from this call is what hid the 2026-05-12 bug:
/// every hook delivery silently looked like an already-set no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetRunTranscriptPathOutcome {
    Updated,
    AlreadySet,
    RowMissing,
}

/// One detached remote run returned by
/// [`WorkDb::list_reattachable_remote_runs`]: an `active` `work_runs`
/// row on a non-local host whose execution is still non-terminal. The
/// engine's startup reattach pass (see [`crate::remote_reattach`])
/// re-establishes the reverse events-socket forward for each of these
/// so the still-running worker's hook stream reaches the new engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRunHandle {
    /// `work_runs.id` (`run_*`).
    pub run_id: String,
    /// `work_runs.execution_id` (`exec_*`) — also the worker's
    /// `BOSS_RUN_ID` and the key for the remote events socket path.
    pub execution_id: String,
    /// The host the run was dispatched to (never `'local'`).
    pub host_id: String,
    /// Remote worker pid captured at spawn, if the wrapper handshake
    /// reported one. Informational for signal addressing; reattach of
    /// the events forward does not depend on it.
    pub remote_pid: Option<i64>,
}

/// Result of a successful [`WorkDb::record_worker_pr_completion`] call.
/// Carries the cube lease/workspace ids that were attached to the
/// execution so the caller can drive cube release out-of-band.
#[derive(Debug, Clone)]
pub struct WorkerPrCompletion {
    pub execution: WorkExecution,
    pub work_item: WorkItem,
    pub released_lease_id: Option<String>,
    pub released_workspace_id: Option<String>,
}

/// One row from [`WorkDb::list_chores_pending_merge_check`]: a chore
/// or project_task the merge poller still needs to ask GitHub about.
#[derive(Debug, Clone)]
pub struct PendingMergeCheck {
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
}

/// One row from [`WorkDb::list_recently_terminal_executions_pending_pr_detection`]:
/// a terminal execution whose task is still `active` with no `pr_url`. The merge
/// poller's late-PR sweep uses this to recover chores that were orphan-swept
/// while their worker pane was still running (double-spawn race — Bug B).
#[derive(Debug, Clone)]
pub struct LatePrCandidate {
    pub execution_id: String,
    pub work_item_id: String,
    pub repo_remote_url: String,
    /// Branch-naming strategy snapshotted from the product's
    /// `editorial_rules.branch_naming` at execution spawn time. Carried so
    /// the late-PR sweep reconstructs the correct expected branch name via
    /// [`crate::completion::expected_branch_name`]. Defaults to
    /// [`BranchNaming::BossExecPrefix`] for rows created before this column
    /// existed (i.e. `NULL` in the DB).
    pub branch_naming: BranchNaming,
    /// Worker branch-name prefix snapshotted from the product's
    /// `worker_branch_prefix` column at execution spawn time. Carried
    /// alongside `branch_naming` so the late-PR sweep reconstructs the
    /// exact branch name via [`crate::completion::expected_branch_name`]
    /// — under the default `BossExecPrefix` strategy this is what turns
    /// `boss/exec_<id>` into the product's configured `<prefix>exec_<id>`.
    /// `None` → the engine default `boss/`.
    pub worker_branch_prefix: Option<String>,
}

/// Raw external-ref data as stored in the `tasks` table. Returned by
/// [`WorkDb::list_external_refs_for_product`]. The `web_url` field present
/// on [`WorkItemExternalRef`] is tracker-specific and is derived by the
/// reconciler layer; the DB layer does not compute it.
#[derive(Debug, Clone)]
pub struct StoredExternalRef {
    pub kind: String,
    pub canonical_id: String,
    pub raw: serde_json::Value,
    pub synced_at: Option<String>,
    pub unbound_at: Option<String>,
}

/// A `ci_remediations` row that is `pending` but has no live execution
/// (`kind='ci_remediation'` with status in `'ready'`, `'running'`, or
/// `'waiting_human'`). This arises when two merge-queue dequeue events
/// arrive in the same sweep: the first flips the task to
/// `blocked: ci_failure` (consuming the `status='in_review'` WHERE
/// guard) and the second inserts its own `ci_remediations` row but
/// cannot flip the task again — leaving the row orphaned with no
/// executor. The merge poller's stranded-attempt sweep rescues these
/// by re-emitting a fresh execution request so a worker is dispatched
/// without waiting for the task to return to `in_review`.
#[derive(Debug, Clone)]
pub struct StrandedCiRemediationAttempt {
    pub attempt_id: String,
    pub work_item_id: String,
    pub product_id: String,
    pub pr_url: String,
}
