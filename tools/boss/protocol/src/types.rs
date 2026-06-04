use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Field-ordering convention (applies to all structs in this file):
//
//   1. Identity fields first: `id`, `short_id`, primary FK identifiers.
//
//   2. Required (non-`Option`) fields, alphabetical within this group.
//
//   3. Optional (`Option<T>`) fields, alphabetical within this group.
//
// Struct *definitions* are sorted alphabetically by type name.
// Both orderings reduce merge conflicts when adding new structs or fields.
// Serde JSON and Swift Codable are both name-keyed, so field order does not
// affect wire format.
// ---------------------------------------------------------------------------



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddDependencyInput {
    /// Selector or id of the work item that becomes gated.
    pub dependent: String,

    /// Selector or id of the work item that gates it.
    pub prerequisite: String,

    /// Defaults to `"blocks"` if omitted.
    #[serde(default)]
    pub relation: Option<String>,
}



/// One member of an [`AttentionGroup`]. Id prefix `atn`.
///
/// Question groups carry the `question_type` / `prompt_text` /
/// `choice_options` / `answer` fields. Followup groups carry the
/// `proposed_*` / `rationale` fields. Both share `source_anchor`,
/// `answer_state`, and `confidence_source`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Attention {
    pub id: String,
    pub group_id: String,
    /// Display order within the group (1-based).
    pub ordinal: i64,
    /// Doc section / heading slug (questions) or transcript offset hint.
    /// Drives inline placement in the design-doc viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_anchor: Option<String>,
    /// `"open"` | `"answered"` | `"skipped"` | `"dismissed"`.
    #[builder(default = "open".to_string())]
    pub answer_state: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    // --- question fields (populated when group.kind = "question") ---
    /// `"yes_no"` | `"multiple_choice"` | `"prompt"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_type: Option<String>,
    /// The question shown to the human.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    /// JSON array of strings (`multiple_choice` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_options: Option<String>,
    /// Captured answer: `"yes"`/`"no"`, chosen index/value, or free text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    // --- followup fields (populated when group.kind = "followup") ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_description: Option<String>,
    /// Effort hint (`"trivial"` … `"max"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_effort: Option<String>,
    /// `"task"` | `"chore"` | `"project"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_work_kind: Option<String>,
    /// Why the agent suggested this followup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// `"structured"` (from a manifest/sentinel) or `"extracted"`
    /// (from a model pass over a transcript or doc).
    #[builder(default = "structured".to_string())]
    pub confidence_source: String,
}



/// One attention group — the human-actionable unit of the Attentions
/// feature. Id prefix `atg`. Related attentions (questions or followups)
/// collect into a group keyed by a stable `grouping_key`; the group is
/// what the human reads and acts on, producing a single downstream
/// artifact.
///
/// Design: `tools/boss/docs/designs/attentions.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct AttentionGroup {
    pub id: String,
    /// Per-product `A<n>` friendly id. `None` until the engine assigns
    /// one at creation time. Partial-unique index enforces uniqueness
    /// per product when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    /// Bumped each time the same source re-runs after the prior group
    /// was actioned/dismissed, keeping "one group ⇒ one revision" true.
    #[builder(default = 0)]
    pub generation: i64,

    /// Stable derived key — the upsert dedup target for reconciliation.
    /// Shape: `"question|{project_id}|doc:{path}"` or
    /// `"followup|{task_id}"`.
    pub grouping_key: String,

    /// `"question"` or `"followup"`.
    pub kind: String,

    /// `"design_doc"` | `"task_transcript"` | `"manual"`.
    pub source_kind: String,

    /// `"open"` | `"partially_answered"` | `"actioned"` | `"dismissed"`.
    #[builder(default = "open".to_string())]
    pub state: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actioned_at: Option<String>,

    /// Exactly one of `association_project_id` / `association_task_id`
    /// is set — the XOR constraint mirrors `work_attention_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_project_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,

    /// Set when the group has been actioned: `"revision"` |
    /// `"design_task"` | `"tasks"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_artifact_kind: Option<String>,

    /// JSON: revision task id / new task ids / PR url.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_artifact_ref: Option<String>,

    /// Head branch for in-review viewing of the source doc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_branch: Option<String>,

    /// Repo-relative design-doc path (populated for `design_doc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_path: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_repo_remote_url: Option<String>,

    /// Transcript pointer (`runs.id`); pairs with `runs.transcript_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,

    /// Originating design/impl task (jump-back target for the UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
}



/// A standing, triggered instruction that periodically asks whether a
/// concrete maintenance task exists right now, and if so spawns one via
/// a two-phase triage → execute flow. Automations live outside the normal
/// backlog; the tasks they produce carry `source_automation_id` so they
/// can be excluded from the kanban and accounted against the per-automation
/// open-task cap.
///
/// See `tools/boss/docs/designs/maintenance-tasks.md` for the full design.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Automation {
    pub id: String,
    /// Per-product A-namespace short id (e.g. A1, A2 …). `None` only on rows
    /// that predate the column (in practice always `Some` after schema
    /// migration runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    /// Surface that created this automation (`cli`, `mac_app`, `unknown`, …).
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    /// `true` → the scheduler considers this automation for firing. `false` →
    /// the automation is paused; no fires are recorded.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub enabled: bool,

    pub name: String,
    /// Maximum number of produced tasks that may be open simultaneously. The
    /// scheduler skips a fire and records `suppressed_at_limit` when the live
    /// count reaches this cap. Default 1.
    #[serde(default = "default_open_task_limit")]
    #[builder(default = default_open_task_limit())]
    pub open_task_limit: i64,

    /// The standing instruction passed verbatim to the triage agent.
    pub standing_instruction: String,

    /// Deserialized trigger — schedule cron+tz for the `schedule` variant.
    /// Stored in the DB as two columns (`trigger_kind` + `trigger_config`).
    pub trigger: AutomationTrigger,

    pub updated_at: String,
    /// Per-automation override of the catch-up window (seconds). `None` → use
    /// the engine constant (15 min). See scheduling semantics in the design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    /// RFC 3339 timestamp of the most recent scheduler fire (whether it
    /// produced a task, was skipped, or failed). `None` until the first fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,

    /// Outcome of the most recent `automation_runs` row for this automation.
    /// Mirrors `AutomationRun::outcome`; denormalised here for cheap list
    /// display. `None` until the first fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<String>,

    /// UTC RFC 3339 timestamp of the next scheduled fire, computed from the
    /// cron expression + timezone. `None` for disabled automations or before
    /// the first `next_due_at` computation runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<String>,

    /// Explicit target repo for the triage worker lease. `None` → default to
    /// the product's primary `repo_remote_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

fn default_open_task_limit() -> i64 {
    1
}



/// Input to `UpdateAutomation`. All fields are `Option`; `None` means
/// "leave unchanged."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_task_limit: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_instruction: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<AutomationTrigger>,
}



/// One recorded fire of an automation — the wire shape of an
/// `automation_runs` row. Created for every occurrence, including
/// no-ops (`skipped`) and failures (`failed_will_retry`,
/// `failed_gave_up`), so the Automations tab can show a complete
/// history. `outcome` values:
///
/// - `produced_task` — triage agent created a task; `produced_task_id` is set.
/// - `skipped` — triage agent decided nothing actionable exists right now.
/// - `suppressed_at_limit` — fire was due but open-task count was already at
///   the cap; no triage agent ran.
/// - `pool_throttled` — the automation pool was full; execution is queued
///   (`ready`) and will self-dispatch when a slot frees. Not a failure.
/// - `triage_running` — a pool slot was claimed and the triage agent is active.
///   Replaced by the terminal outcome on completion.
/// - `failed_will_retry` — genuine pre-start failure (VPN down, cube lease
///   error); same `scheduled_for` will be retried with backoff.
/// - `failed_gave_up` — retries exhausted; occurrence abandoned, schedule
///   advances.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationRun {
    pub id: String,
    pub automation_id: String,
    pub outcome: String,
    /// UTC RFC 3339 timestamp of the cron occurrence this run satisfies.
    /// Used as the dedup key (at most one run per occurrence per automation).
    pub scheduled_for: String,

    pub started_at: String,
    /// Human-readable reason for `skipped` or failure detail for
    /// `failed_*` outcomes. `None` for `produced_task` /
    /// `suppressed_at_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    /// FK to the `tasks.id` produced by triage. Set iff `outcome =
    /// 'produced_task'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_task_id: Option<String>,

    /// The `work_executions.id` of the phase-1 triage execution. `None`
    /// when no triage execution was created (e.g. `suppressed_at_limit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_execution_id: Option<String>,
}

/// `automation_runs.outcome` discriminators. The scheduler writes
/// `suppressed_at_limit`, `skipped` (stale catch-up skip), and the
/// pessimistic `failed_will_retry` default at fire time; the triage
/// outcome detector flips a fired run to `produced_task` / `skipped` /
/// `failed_gave_up` once the worker reaches a decision (Maint task 6).
///
/// Progressive in-flight states (not terminal):
/// - `pool_throttled` — the automation pool was full when dispatch was
///   attempted; the triage execution is queued (`ready`) and will be
///   dispatched automatically once a slot frees up. Not a failure.
/// - `triage_running` — a pool slot was claimed and the triage agent is
///   actively running. Replaced by the terminal outcome on completion.
pub const AUTOMATION_OUTCOME_PRODUCED_TASK: &str = "produced_task";
pub const AUTOMATION_OUTCOME_SKIPPED: &str = "skipped";
pub const AUTOMATION_OUTCOME_SUPPRESSED_AT_LIMIT: &str = "suppressed_at_limit";
pub const AUTOMATION_OUTCOME_FAILED_WILL_RETRY: &str = "failed_will_retry";
pub const AUTOMATION_OUTCOME_FAILED_GAVE_UP: &str = "failed_gave_up";
pub const AUTOMATION_OUTCOME_POOL_THROTTLED: &str = "pool_throttled";
pub const AUTOMATION_OUTCOME_TRIAGE_RUNNING: &str = "triage_running";

/// Discriminator for the `work_executions.kind` column.  Exhaustive
/// match enforces that every callsite handles new variants explicitly —
/// adding a new kind here produces a compile error at every kind-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    AutomationTriage,
    ChoreImplementation,
    CiRemediation,
    ConflictResolution,
    InvestigationImplementation,
    PrReview,
    ProductDesign,
    ProjectDesign,
    RevisionImplementation,
    TaskImplementation,
}

impl ExecutionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutomationTriage => "automation_triage",
            Self::ChoreImplementation => "chore_implementation",
            Self::CiRemediation => "ci_remediation",
            Self::ConflictResolution => "conflict_resolution",
            Self::InvestigationImplementation => "investigation_implementation",
            Self::PrReview => "pr_review",
            Self::ProductDesign => "product_design",
            Self::ProjectDesign => "project_design",
            Self::RevisionImplementation => "revision_implementation",
            Self::TaskImplementation => "task_implementation",
        }
    }
}

impl std::fmt::Display for ExecutionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExecutionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "automation_triage" => Ok(Self::AutomationTriage),
            "chore_implementation" => Ok(Self::ChoreImplementation),
            "ci_remediation" => Ok(Self::CiRemediation),
            "conflict_resolution" => Ok(Self::ConflictResolution),
            "investigation_implementation" => Ok(Self::InvestigationImplementation),
            "pr_review" => Ok(Self::PrReview),
            "product_design" => Ok(Self::ProductDesign),
            "project_design" => Ok(Self::ProjectDesign),
            "revision_implementation" => Ok(Self::RevisionImplementation),
            "task_implementation" => Ok(Self::TaskImplementation),
            other => Err(format!(
                "unknown execution kind: `{other}`; expected one of: \
                 automation_triage, chore_implementation, ci_remediation, \
                 conflict_resolution, investigation_implementation, pr_review, \
                 product_design, project_design, revision_implementation, task_implementation"
            )),
        }
    }
}

/// Discriminator for the `work_executions.status` column. Exhaustive
/// match enforces that every callsite handles new variants explicitly —
/// adding a new status here produces a compile error at every status-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    #[default]
    Queued,
    Ready,
    WaitingDependency,
    Running,
    WaitingHuman,
    WaitingReview,
    WaitingMerge,
    Completed,
    Failed,
    Abandoned,
    Cancelled,
    Orphaned,
}

impl ExecutionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Ready => "ready",
            Self::WaitingDependency => "waiting_dependency",
            Self::Running => "running",
            Self::WaitingHuman => "waiting_human",
            Self::WaitingReview => "waiting_review",
            Self::WaitingMerge => "waiting_merge",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Abandoned | Self::Cancelled | Self::Orphaned
        )
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Running | Self::WaitingHuman)
    }

    pub fn can_reconcile(&self) -> bool {
        matches!(self, Self::Queued | Self::Ready | Self::WaitingDependency)
    }
}

impl std::fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExecutionStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "ready" => Ok(Self::Ready),
            "waiting_dependency" => Ok(Self::WaitingDependency),
            "running" => Ok(Self::Running),
            "waiting_human" => Ok(Self::WaitingHuman),
            "waiting_review" => Ok(Self::WaitingReview),
            "waiting_merge" => Ok(Self::WaitingMerge),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "abandoned" => Ok(Self::Abandoned),
            "cancelled" => Ok(Self::Cancelled),
            "orphaned" => Ok(Self::Orphaned),
            other => Err(format!(
                "unknown execution status: `{other}`; expected one of: \
                 queued, ready, waiting_dependency, running, waiting_human, \
                 waiting_review, waiting_merge, completed, failed, abandoned, \
                 cancelled, orphaned"
            )),
        }
    }
}

/// `work_executions.kind` discriminator for an automation triage execution
/// (Maint task 6). Kept as a constant alias; prefer `ExecutionKind::AutomationTriage`
/// in new code.
pub const EXECUTION_KIND_AUTOMATION_TRIAGE: &str = "automation_triage";

/// Execution kind for an independent reviewer agent. Kept as a constant
/// alias; prefer `ExecutionKind::PrReview` in new code.
pub const EXECUTION_KIND_PR_REVIEW: &str = "pr_review";

/// `task_blocked_signals.reason` literal stamped when a CI-remediation worker
/// classifies a failure as flaky/infra and re-triggers the failing job rather
/// than pushing a code change (`boss engine ci mark-retriggered`). Unlike the
/// `ci_failure` reasons this signal does NOT move the parent to
/// `status='blocked'`: the verdict is "the agent attributed the failure to
/// infra, re-ran CI, and there is nothing to push." It surfaces a flake tag on
/// the task card and tells the completion path to park the worker (awaiting the
/// CI retry / a human decision) instead of probing it for a diff that will
/// never exist. Cleared when the PR's CI resolves or a fresh remediation
/// attempt supersedes the verdict.
pub const BLOCKED_REASON_CI_FLAKY_RETRIGGERED: &str = "ci_flaky_retriggered";



/// Trigger specification for an automation. The `schedule` variant is
/// the only implemented trigger in v1; the enum is open to future
/// variants (`Event`, `Manual`, etc.) without a schema migration because
/// the DB stores the tagged JSON representation across two columns
/// (`trigger_kind` discriminator + `trigger_config` body).
///
/// IANA timezone names (e.g. `"America/Los_Angeles"`) are stored alongside
/// the cron expression so "every weekday at 2pm" means 2pm *local* across
/// DST transitions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTrigger {
    Schedule {
        /// Standard 5-field cron expression (`min hour dom month dow`).
        cron: String,
        /// IANA timezone name (e.g. `"America/Los_Angeles"`).
        timezone: String,
    },
}



/// One active or historical blocked-reason for a work item — the
/// wire shape of a `task_blocked_signals` row. The set of rows for
/// one `work_item_id` is the parent's multi-signal block state; the
/// scalar `Task::blocked_reason` is a denormalised "primary reason"
/// cache derived from this set per the design's §Q2 priority order.
///
/// `reason` is one of the documented signals (`'dependency'`,
/// `'merge_conflict'`, `'review_feedback'`, `'ci_failure'`,
/// `'ci_failure_exhausted'`); the engine treats the set as open so
/// new reasons can ship without bumping this type. `attempt_id` is a
/// soft FK into the attempt table for the matching reason
/// (`conflict_resolutions` for `'merge_conflict'`, `ci_remediations`
/// for the CI signals, etc.) and is `None` for `'dependency'` (the
/// prereqs are queried via `work_item_dependencies` instead).
///
/// `cleared_at` is `None` while the signal is active and is stamped
/// when the signal clears; rows are retained as history alongside
/// `conflict_resolutions` and `ci_remediations`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockedSignal {
    pub work_item_id: String,
    pub created_at: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<String>,
}



/// Which naming strategy to use for worker branches pushed to this
/// product's repo. The execution-id suffix is always appended for
/// uniqueness; only the leading prefix component varies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BranchNaming {
    /// Engine default: `boss/exec_<id>`. Clearly identifies Boss-authored
    /// branches; unwanted in repos with strict per-developer prefix rules.
    #[default]
    BossExecPrefix,
    /// Replace the leading prefix with a short opaque hash so the branch
    /// name gives no hint of its Boss origin while remaining unique by
    /// construction.
    OpaqueHash,
    /// Use `<prefix>exec_<id>` instead of `boss/exec_<id>`. Satisfies
    /// orgs that enforce per-developer branch prefixes (e.g. `bduff/`).
    CustomPrefix { prefix: String },
}



/// Snapshot of a per-PR CI attempt budget — the wire shape behind the
/// `boss engine ci budget show <work-item-id>` verb (design Phase 11
/// #35). `per_pr_override` is the value of `tasks.ci_attempt_budget`
/// when it has been explicitly set on the PR (otherwise `None`).
/// `product_default` is the product's `ci_attempt_budget` (defaults to
/// `3` when the column is unset). `effective` is what the engine
/// actually uses for budget checks (`per_pr_override` when present,
/// else `product_default`, clamped to `0..=10`). `used` is the live
/// `tasks.ci_attempts_used` counter.
///
/// `blocked_reason` carries the parent's current `tasks.blocked_reason`
/// when the task is `status='blocked'`, so the CLI can surface "now
/// exhausted" vs "now in-flight". `None` when the parent is not blocked
/// (e.g. `in_review` / `done`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiBudgetSnapshot {
    pub work_item_id: String,
    pub effective: i64,
    pub product_default: i64,
    pub used: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_pr_override: Option<i64>,
}



/// One engine attempt to clear a CI failure on an `in_review` PR —
/// the wire shape of a `ci_remediations` row. Sibling of
/// [`ConflictResolution`]; the side-table-not-tasks-row rationale is
/// the same (`merge-conflict-handling-in-review.md` §Q3). Stored as
/// a sibling to `WorkExecution` rather than as a `Task` because the
/// attempt is not itself a kanban work item; it's an engine-managed
/// remediation tied to its parent via `work_item_id`.
///
/// `status` values: `pending` (row created, worker not yet
/// dispatched), `running` (worker holds a lease and is editing),
/// `succeeded` (push landed, CI green again), `superseded` (a newer
/// attempt — or a human push — replaced this one), `failed` (worker
/// gave up / errored), `abandoned` (engine declined to spawn, e.g.
/// budget exhausted or product opt-out).
///
/// `attempt_kind` distinguishes `'fix'` (the worker reads logs and
/// pushes a code change) from `'retrigger'` (the engine just re-runs
/// the failing job — cheap, doesn't consume budget). Re-triggers are
/// chosen pre-spawn for unambiguous infra signals (`STARTUP_FAILURE`);
/// the worker may also pivot from `'fix'` to a re-trigger if its
/// triage classifies the failure as `'flaky_or_infra'`.
///
/// `consumes_budget` is the engine's post-hoc answer to "did this
/// count against `tasks.ci_attempts_used`?" — `1` for a fix attempt
/// that actually pushed, `0` for re-triggers and triage-bailouts.
/// `triage_class` is the worker's classification of the failure
/// after reading the log (`'tractable'` / `'flaky_or_infra'` /
/// `'unfixable'`); `None` until the worker fills it.
///
/// `failed_checks` is a JSON-encoded list of `{name, conclusion,
/// provider, target_url, provider_job_id}` snapshots captured at
/// trigger time; `log_excerpt` is the failing-job log tail the
/// engine fetched pre-spawn and seeded into the worker prompt
/// (typically the last 200 lines).
///
/// `pr_url` / `pr_number` / `head_branch` are snapshots of the
/// parent's PR state at trigger time so the row stays interpretable
/// after the parent's branch is recycled. `head_sha_at_trigger` is
/// the discriminator that the UNIQUE key
/// (`(work_item_id, head_sha_at_trigger, attempt_kind)`) uses to
/// keep two probes on the same failure from creating two rows.
/// `head_sha_after` brackets the worker's push (`None` on failure
/// or for re-trigger-only attempts).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CiRemediation {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub attempt_kind: String,
    pub consumes_budget: i64,
    pub created_at: String,
    /// JSON-encoded list of failing-check snapshots, one entry per
    /// failed required check at trigger time. Wire-encoded as a
    /// string so the engine can roll the schema forward without
    /// bumping this type; consumers parse on demand.
    pub failed_checks: String,

    pub head_branch: String,
    pub head_sha_at_trigger: String,
    pub pr_number: i64,
    pub pr_url: String,
    pub status: String,
    /// For `failure_kind='merge_queue_rebounce'`: the `beforeCommit.oid`
    /// from the `RemovedFromMergeQueueEvent` — the synthetic merge SHA
    /// that failed CI. Workers must fetch CI logs from this SHA, not from
    /// the PR head (whose checks are green). `None` for `'pr_branch_ci'`
    /// attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_commit_sha: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,

    /// Discriminates the origin of this attempt:
    /// `'pr_branch_ci'` — the PR's own required checks failed on the PR's
    /// head SHA (the normal path). `'merge_queue_rebounce'` — the PR was
    /// dequeued from GitHub's merge queue with `reason=FAILED_CHECKS` on a
    /// synthetic merge commit; the PR's own CI is green.
    /// `None` on rows written before this field existed (treated as
    /// `'pr_branch_ci'`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,

    /// Soft FK to the `tasks.id` of the `kind=revision` task this attempt
    /// spawned, or `None` until the producer creates the revision. Set when
    /// the CI-failure producer calls the revision-create path for `fix` kind
    /// attempts (Phase 2+ of `unify-pr-remediation-on-revisions.md`);
    /// `None` for all pre-unification rows and for `retrigger` attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_class: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}



// ===========================================================================
// Comments in the markdown viewer (design:
// tools/boss/docs/designs/comments-in-markdown-viewer.md). Phase 2 adds the
// engine-backed persistence + W3C-Web-Annotation-style resilient anchoring.
// ===========================================================================

/// W3C Web Annotation Data Model [`TextQuoteSelector`][wadm], serialised
/// inline on each comment row. The three fields are taken from the
/// rendered *plain-text projection* of the markdown (not the raw source)
/// because the user selects on rendered text.
///
/// `prefix`/`suffix` default to 64 chars each at the authoring path; they
/// disambiguate the `exact` quote when it recurs in the doc, and let the
/// fuzzy resolver re-anchor through edits that touch the surrounding text.
///
/// [wadm]: https://www.w3.org/TR/annotation-model/#text-quote-selector
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CommentAnchor {
    /// The verbatim selected text.
    pub exact: String,

    /// Up to ~64 chars of plain text immediately preceding `exact`.
    /// Empty when the selection begins at the start of the doc.
    #[serde(default)]
    pub prefix: String,

    /// Up to ~64 chars of plain text immediately following `exact`.
    /// Empty when the selection ends at the end of the doc.
    #[serde(default)]
    pub suffix: String,
}

impl CommentAnchor {
    /// The full context string the resolver matches against:
    /// `prefix + exact + suffix`.
    pub fn context(&self) -> String {
        format!("{}{}{}", self.prefix, self.exact, self.suffix)
    }
}

/// Comment status values. `active` is the authored state; `resolved` is the
/// soft-dismiss outcome (hidden from the active sidebar but kept for the
/// history surface); `orphaned` is derived — the renderer reports that an
/// anchor could no longer resolve, and the engine records the flip so the
/// sidebar can group it. `dismissed` is reserved for a future hard-dismiss.
pub const COMMENT_STATUS_ACTIVE: &str = "active";
pub const COMMENT_STATUS_RESOLVED: &str = "resolved";
pub const COMMENT_STATUS_ORPHANED: &str = "orphaned";
pub const COMMENT_STATUS_DISMISSED: &str = "dismissed";
/// Phase 4: comment against a PR-backed doc whose magic-wand button dispatched
/// a Boss chore worker. Transitions `active` → `dispatched` at chore creation,
/// then `dispatched` → `resolved` when the chore's PR merges.
pub const COMMENT_STATUS_DISPATCHED: &str = "dispatched";

/// How the comment's anchor last resolved against the doc's plain-text
/// projection: `exact`, `fuzzy` (drives the ⚠ sidebar glyph), or `orphan`.
pub const RESOLVED_WITH_EXACT: &str = "exact";
pub const RESOLVED_WITH_FUZZY: &str = "fuzzy";
pub const RESOLVED_WITH_ORPHAN: &str = "orphan";

pub fn default_comment_status() -> String {
    COMMENT_STATUS_ACTIVE.to_owned()
}



/// The outcome of resolving one comment's anchor against a doc's current
/// plain-text projection. `start`/`length` are character offsets (Unicode
/// scalar count) of the `exact` span within the plain text; both are `None`
/// for an orphan. `score` is the fuzzy match score (only set for `fuzzy`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommentResolution {
    /// `exact` | `fuzzy` | `orphan`.
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
}



/// One engine attempt to clear a merge conflict on an `in_review`
/// PR — the wire shape of a `conflict_resolutions` row. Stored as a
/// sibling to `WorkExecution` rather than as a `Task` because the
/// attempt is not itself a kanban work item; it's an engine-managed
/// remediation tied to its parent via `work_item_id`. See
/// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`
/// (Q3) for the side-table-not-tasks-row rationale.
///
/// `status` values: `pending` (row created, worker not yet
/// dispatched), `running` (worker holds a lease and is editing),
/// `succeeded` (push landed, PR back to mergeable), `superseded`
/// (a newer attempt — or a human push — replaced this one),
/// `failed` (worker gave up / errored), `abandoned` (engine
/// declined to spawn, e.g. churn-threshold or product opt-out).
///
/// `pr_url` / `pr_number` / `head_branch` / `base_branch` are
/// snapshots of the parent's PR state at trigger time so the row
/// stays interpretable after the parent's branch is recycled.
/// `base_sha_at_trigger` is the conflict-event discriminator that
/// the UNIQUE key (`(work_item_id, base_sha_at_trigger)`) uses to
/// keep two probes on the same conflict from creating two rows.
/// `head_sha_before` / `head_sha_after` bracket the worker's push.
/// `conflict_diagnosis` is structured JSON produced by the
/// pre-spawn diagnosis collector — null until the engine fills it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictResolution {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub base_branch: String,
    pub created_at: String,
    pub head_branch: String,
    pub pr_number: i64,
    pub pr_url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha_at_trigger: Option<String>,

    /// Structured JSON output of the pre-spawn diagnosis collector.
    /// Wire-encoded as a string so the engine can roll the schema
    /// forward without bumping this type; consumers parse on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_diagnosis: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_before: Option<String>,

    /// Soft FK to the `tasks.id` of the `kind=revision` task this attempt
    /// spawned, or `None` until the producer creates the revision. Set when
    /// the merge-conflict producer calls the revision-create path
    /// (Phase 2+ of `unify-pr-remediation-on-revisions.md`); `None` for
    /// all pre-unification rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_task_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}



/// Input for creating a new attention (question or followup member).
/// The engine resolves or creates the appropriate group based on the
/// association and source fields; callers may pass an explicit
/// `group_id` to join an already-open group.
#[derive(bon::Builder, Debug, Clone, Default, Serialize, Deserialize)]
#[builder(on(String, into))]
pub struct CreateAttentionInput {
    /// `"question"` or `"followup"`.
    pub kind: String,
    /// Explicit group to join. When `None` the engine derives or creates
    /// the group from `(kind, association, source_*)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Caller-supplied grouping key override. Ignored when `group_id` is
    /// set; the engine computes the key from association + source when
    /// both are `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_task_id: Option<String>,
    /// `"design_doc"` | `"task_transcript"` | `"manual"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_repo_remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_anchor: Option<String>,
    // question content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_options: Option<String>,
    // followup content
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_work_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// `"structured"` or `"extracted"`. Defaults to `"structured"` when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence_source: Option<String>,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateAttentionItemInput {
    pub body_markdown: String,
    pub kind: String,
    pub title: String,
    /// The execution this item attaches to. `Some` for the common
    /// execution-scoped case; `None` together with `work_item_id =
    /// Some(...)` for sticky pre-dispatch items like `repo_unresolved`.
    #[serde(default)]
    pub execution_id: Option<String>,

    pub resolved_at: Option<String>,
    pub status: Option<String>,
    /// The work item this item attaches to when no execution row
    /// exists. Mutually exclusive with `execution_id` — the engine
    /// rejects inputs where both are set or both are missing.
    #[serde(default)]
    pub work_item_id: Option<String>,
}



/// Input to `CreateAutomation`. Carries only the caller-supplied fields;
/// the engine stamps `id`, `short_id`, `created_at`, `updated_at`, and the
/// initial scheduler bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateAutomationInput {
    pub product_id: String,
    /// When `false`, the automation is created disabled. Defaults to `true`.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub enabled: bool,

    pub name: String,
    #[serde(default = "default_open_task_limit")]
    #[builder(default = default_open_task_limit())]
    pub open_task_limit: i64,

    pub standing_instruction: String,
    pub trigger: AutomationTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_window_secs: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateChoreInput {
    pub product_id: String,
    /// When `false`, the engine creates the chore in `todo` but does
    /// NOT spin up a `ready` execution for the auto-dispatcher to pick
    /// up. The chore stays parked until something explicitly schedules
    /// it (`bossctl work start <id>` or a kanban drag-to-Doing). Older
    /// clients that omit this field get the historical behavior
    /// (`autostart = true`).
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// See [`CreateTaskInput::force_duplicate`].
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    pub name: String,
    /// See `CreateTaskInput::created_via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    pub description: Option<String>,
    /// See [`CreateTaskInput::effort_level`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// See [`CreateTaskInput::model_override`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Per-work-item repo override. `None` → the chore inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time). A bare registered cube repo
    /// slug (e.g. `bduff`) is also accepted and resolved to its origin
    /// URL at write time so the stored row is always dispatchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}



/// `comments_create` request body. The renderer supplies `doc_version` (it
/// hashes the plain-text projection) so the engine and renderer agree on the
/// authoring input without the engine having to render markdown itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct CreateCommentInput {
    pub artifact_id: String,
    pub anchor: CommentAnchor,
    pub artifact_kind: String,
    pub author: String,
    pub body: String,
    pub doc_version: String,
    #[serde(default)]
    #[builder(default)]
    pub plain_text_projection_version: i64,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateExecutionInput {
    pub work_item_id: String,
    /// When `true`, cube will be invoked with `--allow-dirty` so it
    /// reclaims the preferred workspace with uncommitted work intact.
    /// Only set on the orphan recovery re-dispatch path.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    pub kind: ExecutionKind,
    /// When true, the cube lease fallback degrades silently to any free
    /// workspace if the preferred workspace is gone or leased. Used for
    /// `revision_implementation` executions where warmth is a hint only.
    #[serde(default)]
    #[builder(default)]
    pub prefer_is_soft: bool,

    pub cube_lease_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub finished_at: Option<String>,
    /// PR URL to bind to this execution row at creation time. For
    /// `revision_implementation` this is the chain root's `pr_url` so
    /// the SHA-delta gate can snapshot and verify the parent PR HEAD.
    #[serde(default)]
    pub pr_url: Option<String>,

    pub preferred_workspace_id: Option<String>,
    pub priority: Option<i64>,
    pub repo_remote_url: Option<String>,
    pub started_at: Option<String>,
    pub status: Option<ExecutionStatus>,
    pub workspace_path: Option<String>,
}



/// Input for `boss task create --kind investigation`. Parallel to
/// [`CreateChoreInput`] but adds `project_id` (investigation tasks
/// are product-level work items optionally scoped to a project) and
/// uses `kind = 'investigation'` on insert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateInvestigationInput {
    pub product_id: String,
    /// See [`CreateChoreInput::autostart`].
    #[serde(default = "default_true")]
    pub autostart: bool,

    #[serde(default)]
    pub force_duplicate: bool,

    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Optional project scope. When set, the investigation appears
    /// under the project on the kanban. `None` → product-level only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,

    /// Per-task repo override for the investigation deliverable. `None`
    /// → resolve from product `docs_repo`, then `BOSS_USER_DOCS_REPO`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}



/// Batch counterpart of [`CreateChoreInput`]. See
/// [`CreateManyTasksInput`] for atomicity / event semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManyChoresInput {
    pub items: Vec<CreateChoreInput>,
}



/// Batch counterpart of [`CreateTaskInput`]. Items are fully resolved
/// inputs — the CLI merges any top-level `--product` / `--project` /
/// `--no-autostart` defaults into each entry before sending. The
/// engine inserts every item in one sqlite transaction and emits one
/// `WorkItemsCreated` response carrying the full list. On any
/// per-item validation failure the entire transaction is rolled back
/// — there is no partial state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManyTasksInput {
    pub items: Vec<CreateTaskInput>,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateProductInput {
    pub name: String,
    pub description: Option<String>,
    /// See [`Product::design_repo`]. `None` → no override; design
    /// tasks resolve through the standard chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// See [`Product::docs_repo`]. `None` → fall through to
    /// `BOSS_USER_DOCS_REPO` for investigation deliverables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    pub repo_remote_url: Option<String>,
    /// See [`Product::worker_branch_prefix`]. `None` → the engine
    /// default `boss/`. Stored canonicalised with a trailing `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateProjectInput {
    pub product_id: String,
    /// Project creation auto-creates a `kind = 'design'` task as the
    /// first row under the project so the design phase shows up on
    /// the kanban like any other task. With `autostart = false` that
    /// design task is created in `todo` but the engine will NOT
    /// dispatch a worker against it until something explicitly
    /// schedules it (CLI `work start`, kanban drag-to-Doing, etc.).
    /// Mirrors the chore/task `autostart` semantics — same gate,
    /// applied at the moment the design task is born.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    pub name: String,
    /// When `true`, skip creation of the auto-generated `kind=design`
    /// seed task entirely. The project is filed alone with zero child
    /// tasks. Useful for non-design-shaped projects (postmortems,
    /// milestone aggregators, checklists) where the seed task is dead
    /// weight. Defaults to `false` to preserve existing behaviour.
    #[serde(default)]
    #[builder(default)]
    pub no_design_task: bool,

    pub description: Option<String>,
    pub goal: Option<String>,
}



/// Input for `boss task create-revision`. Creates a `kind = 'revision'`
/// task bound to an existing parent task whose PR is open and unmerged.
/// The worker's deliverable is a new commit on the *parent's* PR branch —
/// no new PR is opened. The `parent_task_id` field is required; the engine
/// enforces "kind = revision ⇒ parent_task_id IS NOT NULL" in
/// `insert_revision_in_tx` (Phase 2). `product_id` and `project_id` are
/// inherited from the parent at create time; `repo_remote_url` is likewise
/// inherited so the revision always targets the parent's repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateRevisionInput {
    /// The task whose PR this revision will commit to. Must refer to a task
    /// (or chain of revisions) with an open, unmerged PR. May itself be a
    /// `revision` — the gate is evaluated against the chain root's PR.
    pub parent_task_id: String,

    /// The operator's verbatim ask. Stored as the task's `description` and
    /// shown in the Review-lane rollup affordance so reviewers can see what
    /// each new commit was for.
    pub description: String,

    /// Bypass the recent-duplicate guard. See
    /// [`CreateTaskInput::force_duplicate`].
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    /// Surface that filed this revision — `"operator"` for Source A
    /// (direct boss-operator feedback); `"pr-comment:<repo>#<pr>:<cid>"`
    /// for Source B (deferred comment-triage UI). Stored in
    /// `tasks.created_via`; the `(repo, pr#, comment-id)` pointer is
    /// carried verbatim here rather than mirrored into separate columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    /// Effort estimate. Omitted → defaults to `small` (revisions are
    /// typically narrow; the heuristic can escalate on signal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Explicit model slug override. `None` → resolve per design §Q3
    /// precedence (same as other task kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// Short summary title for the revision card (1–10 words). When the
    /// coordinator supplies this, it is used verbatim as `tasks.name`;
    /// when absent the engine falls back to deriving a name from the first
    /// line of `description` (legacy behaviour, preserved for callers that
    /// pre-date this field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → inherits from the
    /// parent task's priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// When `false`, the revision is created in `todo` but the engine
    /// does NOT auto-dispatch a worker. The caller must explicitly start
    /// it (via `bossctl work start <id>` or a kanban drag-to-Doing).
    /// Defaults to `true` (auto-dispatch immediately), which is the
    /// existing behaviour and the right default for revision serialisation
    /// on a parent PR.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateRunInput {
    pub agent_id: String,
    pub execution_id: String,
    pub artifacts_path: Option<String>,
    pub error_text: Option<String>,
    pub finished_at: Option<String>,
    pub result_summary: Option<String>,
    pub started_at: Option<String>,
    pub status: Option<String>,
    pub transcript_path: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct CreateTaskInput {
    pub product_id: String,
    pub project_id: String,
    /// See `CreateChoreInput::autostart`. Project tasks honour the
    /// same flag, but the kanban already serialises them via
    /// `waiting_dependency` so only the first incomplete task is ever
    /// `ready`. Defaults to `true`.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// Bypass the recent-duplicate guard. When `true`, the engine skips
    /// the 60-second same-name/same-product duplicate check and inserts
    /// a second row unconditionally. Intended as a CLI escape hatch for
    /// operators who explicitly want a second task with the same name.
    #[serde(default)]
    #[builder(default)]
    pub force_duplicate: bool,

    pub name: String,
    /// Surface that filed this task — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`. Documented callers always set it explicitly;
    /// when omitted, the engine falls back to a transport-layer hint
    /// so the row is never silently labeled `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,

    pub description: Option<String>,
    /// Effort estimate. `None` → leave NULL on the row; dispatcher
    /// falls through to product / engine default per design §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Explicit model slug override. `None` → no override; dispatcher
    /// resolves per design §Q3 precedence. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`), which is the right answer for the vast majority
    /// of tasks; only callers who care should set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,

    /// Per-work-item repo override. `None` → the task inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time). A bare registered cube repo
    /// slug (e.g. `bduff`) is also accepted and resolved to its origin
    /// URL at write time so the stored row is always dispatchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

/// Direction of a dependency listing — incoming (prereqs of the
/// named row), outgoing (dependents), or both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDirection {
    Prereqs,
    Dependents,
    #[default]
    Both,
}


/// One enriched dependency edge as displayed by `boss <kind> show`.
/// Unlike [`WorkItemDependency`] (a raw storage row with both
/// endpoints), this struct collapses the edge into "the peer + the
/// fact that this is a `relation` edge." `id` / `kind` / `name` /
/// `status` describe the peer (the prerequisite when this edge sits
/// in `prerequisites`, the dependent when it sits in `dependents`),
/// so the human / JSON renderer doesn't need a second lookup.
///
/// `kind` is `task`, `chore`, or `project` — derived from the id
/// prefix and the row's `tasks.kind`. UI surfaces use it to choose
/// the right icon / link.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyEdge {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub relation: String,
    pub status: String,
}



/// Predicate applied to `boss <kind> list` requests to surface only
/// the rows that match a dependency-graph question. Q6 spells out
/// four flags; this enum is the one-flag-per-variant projection.
/// CLI parsing rejects combinations (the four flags are mutually
/// exclusive at the surface) so the engine never sees an
/// over-constrained request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DependencyFilter {
    /// Only items that the named row depends on (its incoming edges).
    PrerequisitesOf { id: String },
    /// Only items that depend on the named row (its outgoing edges).
    DependentsOf { id: String },
    /// Only items in `todo` with no gating prerequisite — i.e. the
    /// rows the dispatcher could pick up next.
    Unblocked,
    /// Only items currently gated by at least one incomplete prereq.
    BlockedByDeps,
}



/// One recorded enforcement action taken by the editorial-rules hook
/// against a `gh` command invocation. Stored in `editorial_actions`
/// for audit and debugging; surfaced via `ListEditorialActions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct EditorialAction {
    pub id: String,
    pub execution_id: String,
    pub product_id: String,
    /// What the hook did: `"redact"` (body was rewritten in place),
    /// `"block"` (invocation was rejected), or `"advise"` (warning
    /// prepended to the prompt but the call was allowed through).
    pub action: String,

    pub created_at: String,
    /// Human-readable explanation produced by the hook (the matched
    /// pattern name or the template section that was missing).
    pub reason: String,

    /// Verbatim command the worker attempted (e.g. `gh pr create
    /// --title "…" --body "…"`), truncated to 4 KiB for storage.
    pub tool_command: String,

    /// The PR URL the action was taken on, when known at hook time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}



/// Per-product editorial rules constraining what workers write into
/// GitHub-visible surfaces.
///
/// All fields carry `#[serde(default)]` so an absent or `null` JSON
/// object deserialises to the identity value that preserves today's
/// behaviour. The `Default` impl is therefore the "no rules configured"
/// state and matches what unconfigured products experience.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EditorialRules {
    /// Branch-naming strategy for worker branches on this product.
    #[serde(default)]
    pub branch_naming: BranchNaming,

    /// Whether AI co-author trailers should be stripped from commit
    /// messages authored by workers on this product.
    #[serde(default)]
    pub commit_trailer_policy: TrailerPolicy,

    /// Ordered redaction rules applied to every `gh pr|issue
    /// {create,edit,comment}` body before the call goes through.
    /// Empty → no redaction pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redactions: Vec<RedactionRule>,

    /// How strictly `.github/PULL_REQUEST_TEMPLATE.md` conformance is
    /// enforced on PR bodies.
    #[serde(default)]
    pub template_policy: TemplatePolicy,

    /// Free-text instructions injected verbatim into the worker's
    /// initial prompt beneath a `[editorial-rules]` header. `None` →
    /// no free-text block injected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}



/// Suggested action a human reviewer might take, encoded so JSON
/// consumers can branch on it without parsing free text. Mirrors
/// the annotation strings in [`EffortAuditMarkerRow::annotation`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffortAuditAnnotation {
    /// Rate exceeds the configured under-classification threshold:
    /// the marker maps the row to a level workers commonly judge
    /// too low. Surface as "consider promoting to <higher level>."
    ConsiderPromoting,
    /// Rate is below the well-classified ceiling AND match volume
    /// is above the well-classified floor: the marker is doing its
    /// job. Surface as "marker holds; level correct."
    MarkerHolds,
    /// Either threshold-eligible but on the over-class side, or
    /// volume too low to call. No callout.
    None,
}



/// Per-marker analysis row in the effort-audit report. One entry
/// per marker in the §Q4 corpus that matched at least one chore in
/// the product (markers with zero matches are filtered out so the
/// table stays scannable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EffortAuditMarkerRow {
    /// Of those, the count that subsequently raised an
    /// `[effort-escalation]` marker promoting the row to a higher
    /// level (per [`EffortLevel`]'s natural ordering trivial < small
    /// < medium < large < max).
    pub escalations: u32,

    /// Marker string from the §Q4 corpus, e.g. `"rename"`,
    /// `"investigate"`, `"engine"`, lowercased.
    pub marker: String,

    /// Total chores (kind = `chore`) on the product whose title or
    /// description matched this marker, regardless of whether they
    /// escalated.
    pub matches: u32,

    /// Heuristic level the marker maps to per §Q4 (`trivial` for
    /// mechanical-edit markers, `medium` for multi-subsystem hints,
    /// `large` for investigate-family markers).
    pub original_level: EffortLevel,

    /// Human-readable callout produced when the rate / volume cross
    /// the thresholds named in `engine/src/effort.rs`. Empty when
    /// the marker is neither "consider promoting" nor "marker
    /// holds."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,

    /// `escalations / matches` as a 0.0-1.0 fraction. `0.0` when
    /// `matches > 0 && escalations == 0`; absent (per
    /// [`Option`]'s `None`) when `matches == 0` so callers don't
    /// have to special-case divide-by-zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under_class_rate: Option<f64>,
}



/// Output shape for `boss product audit-effort <product>`. One
/// snapshot of the marker corpus's under-classification rates
/// against the recorded escalation events for a single product.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EffortAuditReport {
    pub product_id: String,
    /// Epoch seconds when the audit was generated, for the
    /// human-readable header.
    pub generated_at: String,

    pub product_slug: String,
    /// Per-marker analysis, sorted by `under_class_rate`
    /// descending so the noisy markers are visible first. Markers
    /// with zero matches are filtered out.
    pub rows: Vec<EffortAuditMarkerRow>,

    /// Total chores (kind = `chore`, `deleted_at IS NULL`) on the
    /// product that the audit scanned for marker matches.
    pub total_chores: u32,

    /// Total escalation events the audit considered (after window
    /// filter). Equal to the sum of per-marker `escalations` only
    /// when every event carried exactly one marker — events can
    /// match multiple markers and double-count by design.
    pub total_escalations: u32,

    /// Under-classification threshold (0.0-1.0) at which the audit
    /// produces a "consider promoting" callout. Echoed back so
    /// JSON consumers don't have to re-import the constant.
    pub under_class_threshold: f64,

    /// Window cap in days applied to escalation events
    /// (`created_at` after now - window). `None` means "no window;
    /// include all recorded escalations."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
}



/// One recorded effort-level escalation event — the wire shape of
/// an `effort_escalations` row. Written by the coordinator's
/// escalation handler (design §Q5) when a worker raises an
/// `[effort-escalation]` Stop-boundary marker; read by the
/// heuristic feedback-loop audit report (`boss product
/// audit-effort`).
///
/// Carries the row's `original_level` (what the heuristic chose at
/// creation time), the `new_level` the worker requested, and the
/// list of `markers` the heuristic recorded as having matched the
/// row when it picked the original level. The audit report
/// aggregates these by marker to surface "marker X under-classified
/// Y% of the time" without changing the heuristic itself.
///
/// `markers` is the §Q4 marker corpus the heuristic uses; entries
/// are the literal marker strings ("rename", "investigate", etc.)
/// stored as a JSON array in SQLite. `rule_id` is optional and
/// names the §Q4 rule that fired (`"rule-2"`, `"rule-5"`, etc.) for
/// the heuristic's own bookkeeping; the audit report does not
/// depend on it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffortEscalation {
    pub id: String,
    pub product_id: String,
    pub work_item_id: String,
    pub created_at: String,
    pub markers: Vec<String>,
    pub new_level: EffortLevel,
    pub original_level: EffortLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
}



/// Allowed values for `tasks.effort_level`. Per design §"Naming" /
/// §Q1: `trivial | small | medium | large | max`. Stored as TEXT
/// in SQLite (no `CHECK` constraint), validated in code by
/// [`EffortLevel::from_str`].
///
/// `max` is the human-only escape hatch: the coordinator's
/// heuristic never emits it; humans set it via `--effort max` when
/// they want Claude's maximum reasoning depth regardless of what
/// the scope markers suggest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Trivial,
    Small,
    Medium,
    Large,
    Max,
}

impl EffortLevel {
    pub const ALL: &'static [EffortLevel] = &[
        EffortLevel::Trivial,
        EffortLevel::Small,
        EffortLevel::Medium,
        EffortLevel::Large,
        EffortLevel::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EffortLevel::Trivial => "trivial",
            EffortLevel::Small => "small",
            EffortLevel::Medium => "medium",
            EffortLevel::Large => "large",
            EffortLevel::Max => "max",
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for EffortLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trivial" => Ok(EffortLevel::Trivial),
            "small" => Ok(EffortLevel::Small),
            "medium" => Ok(EffortLevel::Medium),
            "large" => Ok(EffortLevel::Large),
            "max" => Ok(EffortLevel::Max),
            other => Err(format!(
                "unknown effort level `{other}`; expected one of: trivial, small, medium, large, max"
            )),
        }
    }
}



/// One row in the unified `boss engine attempts list` v2 result —
/// design Phase 11 #36. A small projection across three attempt
/// subsystems (`conflict_resolutions`, `rebase_attempts`,
/// `ci_remediations`) with a `kind` discriminator. The full per-row
/// state still lives on its origin table; this view is the columns the
/// shared list view needs (id, kind, status, work item, PR, reason,
/// timestamps) — callers fetching deeper detail switch to the
/// kind-specific `show` verb.
///
/// `kind` is one of:
/// - `"conflict"`  — `conflict_resolutions` row (merge-conflict flow)
/// - `"rebase"`    — `rebase_attempts` row (auto-rebase flow)
/// - `"ci"`        — `ci_remediations` row (CI-failure flow)
///
/// `work_item_id` is the parent's id where the kind has one;
/// `rebase_attempts` is keyed on `dependent_pr_url`, so its
/// `work_item_id` may be `None` (depending on schema as it lands).
///
/// `extra` carries kind-specific scalar values that are useful in the
/// shared list view but don't justify a column — currently
/// `attempt_kind` for `ci` rows. The contract is "stringly typed
/// extras"; consumers index by key and tolerate absence.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EngineAttemptListEntry {
    pub id: String,
    pub product_id: String,
    pub created_at: String,
    /// Kind-specific scalar columns the consumer may want to render
    /// (e.g. `attempt_kind` for `ci`). Stringly-typed; consumers
    /// fall back to the kind-specific `show` verb for deep detail.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub extra: std::collections::BTreeMap<String, String>,

    pub kind: String,
    pub pr_url: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_item_id: Option<String>,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionReconcileResult {
    pub created: Vec<WorkExecution>,
    pub updated: Vec<WorkExecution>,
}



/// Display-safe GitHub OAuth auth state pushed from the engine to the UI.
/// The token itself is never included — only fields safe to render.
///
/// Matches the state machine in the OAuth device-flow design (§3):
/// `Disconnected → RequestingCode → PendingUserAuth → Authorized/Expired/Denied/Error`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GitHubAuthStateDto {
    /// No stored token; no flow in progress.
    Disconnected,
    /// Device code is being requested from GitHub's `/login/device/code`.
    RequestingCode,
    /// Device code obtained. The user must type `user_code` at
    /// `verification_uri` (or `verification_uri_complete` if present) to
    /// authorize. The engine is polling.
    PendingUserAuth {
        user_code: String,
        verification_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_uri_complete: Option<String>,
        /// Unix epoch seconds when the device code expires.
        expires_at: i64,
        interval_seconds: u32,
    },
    /// Token obtained, validated, and stored. `granted_scopes` is what
    /// GitHub actually granted (may differ from what was requested).
    Authorized {
        login: String,
        granted_scopes: Vec<String>,
        org_state: OrgAuthState,
    },
    /// The device code expired before the user completed authorization.
    /// The user must restart the flow.
    Expired,
    /// The user denied the authorization request in the browser.
    Denied,
    /// A non-recoverable error occurred during the flow.
    Error {
        message: String,
    },
}



/// Input to `LinkWorkItemExternalRef`: manually bind a work item to a
/// specific upstream issue. The engine stores `kind`/`canonical_id` in
/// the `tasks.external_ref_*` columns so the reconciler can start
/// mirroring state for the row on its next tick. The `raw` blob and
/// `web_url` are populated by the engine from the tracker's
/// `fetch_item` response; the caller does not supply them here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkExternalRefInput {
    /// Stable tracker-specific id (`"spinyfin/mono#560"` for GitHub).
    pub canonical_id: String,

    pub work_item_id: String,
    /// Tracker discriminator matching `products.external_tracker_kind`
    /// for the work item's product.
    pub kind: String,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListDependenciesInput {
    /// Selector or id of the work item to list edges for.
    pub work_item: String,

    #[serde(default)]
    pub direction: Option<DependencyDirection>,
}



/// An engine-persisted magic-wand dispatch row (`magic_wand_dispatches` table).
/// Records the one-shot specialised Claude call dispatched when the user clicks
/// the magic-wand button on a comment (Phase 3: engine-owned doc; Phase 4:
/// PR-backed doc → Boss chore worker). 13+ fields → builder pattern per project
/// convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct MagicWandDispatch {
    pub id: String,
    pub artifact_id: String,
    pub comment_id: String,
    /// True when the model returned a result but the highlighted anchor text is
    /// absent from the result AND wholesale changes occurred elsewhere. Surfaced
    /// as a warning in the preview sheet; the user decides whether to apply.
    #[serde(default)]
    #[builder(default)]
    pub anchor_warning: bool,

    pub artifact_kind: String,
    pub created_at: String,
    /// The `doc_version` from the comment — the plain-text-projection SHA the
    /// dispatch ran against. Used as the CAS value on Apply.
    pub doc_version: String,

    /// `in_flight` | `returned` | `applied` | `discarded` | `conflict` |
    /// `failed` | `chore_created`
    pub status: String,

    /// Phase 4 only: the id of the Boss chore worker spawned to address this
    /// comment. Set when `status = 'chore_created'`; `None` for all Phase 3
    /// dispatches. Links the dispatch row to the chore for audit traceability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chore_id: Option<String>,

    /// Short error classification (`length_sanity` | `diff_sanity` | `api_error`
    /// | `empty_response`). `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,

    /// The proposed updated markdown. `None` until the dispatch completes
    /// successfully (Phase 3 only; always `None` for `chore_created`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_md: Option<String>,
}



/// Sub-state of `GitHubAuthStateDto::Authorized` that reflects whether the
/// stored token can actually reach private org resources. A valid user token
/// may still be blocked by org approval or SAML SSO requirements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrgAuthState {
    /// Token can read the org's private resources. Sync should work.
    Ok,
    /// The OAuth App has not yet been approved by an org owner. Sync
    /// against private org resources will fail. `request_url` is the
    /// org-owner approval / request page.
    NeedsOrgApproval {
        request_url: String,
    },
    /// The token requires SAML SSO authorization for the org. `sso_url`
    /// is the SSO authorization URL from GitHub's `X-GitHub-SSO` header.
    NeedsSso {
        sso_url: String,
    },
    /// Org auth state could not be determined (probe failed for an
    /// unexpected reason). Sync may or may not work.
    Unknown,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Product {
    pub id: String,
    pub created_at: String,
    pub description: String,
    pub name: String,
    pub slug: String,
    pub status: String,
    pub updated_at: String,
    /// Per-product default model slug used when a task/chore on this
    /// product has no `model_override` set. `None` → fall through to
    /// the effort-level default / engine default (per the design's Q3
    /// precedence). Stored verbatim — the engine does not validate the
    /// slug, so a future Claude release can ship without a Boss
    /// migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    /// Optional override repo for `kind = 'design'` tasks on this
    /// product. When set, design tasks resolve to this repo (the docs
    /// site) instead of `repo_remote_url`. Implementation-kind tasks
    /// (`task`, `chore`, `project_task`) are unaffected. Per-task
    /// `--repo` overrides still win — this is a new middle layer in
    /// the existing precedence chain. Stored canonicalised, same as
    /// `repo_remote_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// Optional preamble prepended to every worker's initial context
    /// at spawn time, visibly bracketed so humans reading transcripts
    /// know what was injected and by whom. `None` / empty → today's
    /// behaviour (no injection). Intended for per-product runtime
    /// guidance such as test-runner preferences that workers should
    /// see on every spawn rather than only when they read AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,

    /// Optional repo where `kind = 'investigation'` task deliverables
    /// (markdown docs) are filed. When set, investigation workers open
    /// PRs against this repo instead of the user-level fallback
    /// (`BOSS_USER_DOCS_REPO`). Stored canonicalised, same as
    /// `repo_remote_url`. `None` → fall through to user-level default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    /// Per-product editorial rules that constrain what workers write
    /// into GitHub-visible surfaces (PR bodies, comments, branch name,
    /// commit messages). `None` means no rules are configured; the
    /// engine uses its built-in defaults (strip known Boss identifier
    /// patterns). See `editorial-controls-for-agent-authored-prs-and-github-comments.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editorial_rules: Option<EditorialRules>,

    /// Kind-specific config blob for the bound external tracker.
    /// JSON shape is validated by the tracker impl's `validate_config`
    /// at write time; the protocol type carries it opaquely so new
    /// tracker kinds can ship without a protocol version bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_config: Option<serde_json::Value>,

    /// Discriminator for the external tracker bound to this product.
    /// `None` means no tracker is bound and the reconciler skips this
    /// product. When set (e.g. `"github"`), `external_tracker_config`
    /// carries the kind-specific JSON config. See the external-tracker
    /// sync design (`external-issue-tracker-sync-github-projects.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_tracker_kind: Option<String>,

    pub repo_remote_url: Option<String>,
    /// Leading prefix for worker branch names on this product. The
    /// engine names a worker's branch `<worker_branch_prefix>exec_<id>`;
    /// the `exec_<id>` suffix is the stable identifier every subsystem
    /// keys off (PR-to-execution linking, the kanban Review lane, lease
    /// lookups), so only this leading literal is configurable. `None`
    /// (or empty) → the engine default `boss/`. Set it to satisfy orgs
    /// that enforce per-developer branch prefixes via local hooks (e.g.
    /// `bduff/`). Stored canonicalised with a guaranteed trailing `/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Project {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    pub created_at: String,
    pub description: String,
    pub goal: String,
    /// Who made the most recent status change. Three values:
    /// - `'human'` (default) — a CLI / app caller with no registered
    ///   Boss-session ancestry, or a drag-drop gesture in the macOS app.
    /// - `'boss'` — the caller's process ancestry traces back to the
    ///   registered Boss-coordinator session pid (the libghostty pane
    ///   where Claude Code runs as coordinator).
    /// - `'engine'` — the engine wrote the status itself (dependency
    ///   auto-block/unblock, merge poller, CI watch, etc.).
    ///
    /// The auto-unblock path only flips a `blocked` row back to `todo`
    /// when this is `'engine'` — manual and Boss-driven blocks stick.
    #[serde(default = "default_human_actor")]
    #[builder(default = default_human_actor())]
    pub last_status_actor: String,

    pub name: String,
    #[builder(default = default_priority())]
    pub priority: String,

    pub slug: String,
    pub status: String,
    pub updated_at: String,
    /// Branch the design doc lives on. `None` → inherit from the
    /// product's docs branch (or `"main"` if no per-product default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,

    /// Repo-relative path to the design doc (e.g.
    /// `"tools/boss/docs/designs/foo.md"`). `None` → no pointer set;
    /// UI affordance is hidden. This is the load-bearing field — when
    /// `None` the other two are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,

    /// Repo URL the project's design doc lives in. `None` → inherit
    /// from the project's product (`products.repo_remote_url`). Set
    /// explicitly when the doc lives in a different repo (the
    /// separate-doc-repo case at work).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
}



/// Wire-level state returned by `ResolveProjectDesignDoc`. The UI
/// uses this directly to pick the right affordance (hidden, plain
/// icon, warning glyph) without re-implementing the resolution
/// logic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProjectDesignDocState {
    /// The project has no design-doc pointer set. UI hides the
    /// affordance entirely.
    NotSet,
    /// The pointer resolved cleanly. Carries the structured triple,
    /// the absolute path of a leased cube workspace for the resolved
    /// repo (so the open dispatcher can pick the filesystem fast
    /// path), and a pre-rendered GitHub web URL for the kanban
    /// tooltip / right-click "copy link."
    Resolved {
        resolved: ResolvedDesignDoc,
        /// Absolute path to a cube workspace leased for
        /// `resolved.repo_remote_url`, if any. `Some(path)` means the
        /// open dispatcher can hand `<workspace_path>/<resolved.path>`
        /// to `$EDITOR` / the in-app renderer; `None` means no
        /// workspace is currently leased so the affordance falls back
        /// to `raw_content_url` or the GitHub web URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_path: Option<String>,
        /// `https://github.com/<owner>/<repo>/blob/<branch>/<path>`,
        /// pre-built so consumers don't have to re-parse the repo
        /// URL.
        web_url: String,
        /// `https://raw.githubusercontent.com/<owner>/<repo>/<branch>/<path>`,
        /// present when `resolved.repo_remote_url` is a github.com URL.
        /// Used by the macOS app to fetch and render the doc inline when
        /// no workspace fast-path is available — in particular when the
        /// design task is `in_review` and the file lives on the PR head
        /// branch rather than `main`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_content_url: Option<String>,
    },
    /// The pointer is set but cannot be resolved (e.g. path with no
    /// repo to resolve against). The UI surfaces a warning glyph
    /// linking to the set-design-doc form.
    Broken { reason: String },
}



/// One work item bound to a given PR number, together with the
/// revisions in that PR's chain. Returned by
/// [`crate::wire::FrontendRequest::FindWorkItemsByPr`].
///
/// `owner` is the row that owns the `pr_url` — the chain root, which
/// may be any kind (`project_task`, `chore`, `design`,
/// `investigation`). `revisions` are the `kind = 'revision'`
/// descendants that committed to the same PR branch without owning a
/// `pr_url` of their own; they carry `revision_seq` /
/// `revision_parent_pr_url` projections and are ordered by sequence
/// (R1, R2, …). Empty when the owner has no revisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrWorkItemMatch {
    pub owner: Task,
    #[serde(default)]
    pub revisions: Vec<Task>,
}



// ---------------------------------------------------------------------------
// Editorial controls (editorial-controls-for-agent-authored-prs-and-github-comments.md)
// ---------------------------------------------------------------------------

/// What the editorial hook does when a redaction pattern matches: rewrite
/// the matched substring in place, or block the `gh` invocation entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedactionKind {
    /// Replace the match with `RedactionRule::replacement`.
    #[default]
    Rewrite,
    /// Reject the `gh` invocation outright with an actionable message.
    Block,
}



/// One user-configured redaction rule applied to `gh pr|issue` bodies.
/// `pattern` is a regex (Rust `regex` crate syntax); `replacement` is
/// substituted for every match when `kind = Rewrite` and ignored when
/// `kind = Block`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionRule {
    #[serde(default)]
    pub kind: RedactionKind,

    pub pattern: String,
    pub replacement: String,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveDependencyInput {
    pub dependent: String,
    pub prerequisite: String,
    #[serde(default)]
    pub relation: Option<String>,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct RequestExecutionInput {
    pub work_item_id: String,
    /// Request cube to reclaim the preferred workspace with its dirty
    /// working copy intact (passed through to `CreateExecutionInput`
    /// and stored on the `work_executions` row). Set `true` on the
    /// orphan recovery re-dispatch path; `false` everywhere else.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    /// Skip the dispatcher's pool-cap deferral. With `force = false`
    /// (the default), `RequestExecution` is the soft "queue this and
    /// dispatch when a slot frees up" verb. With `force = true`
    /// (`bossctl agents launch`), the engine grows the worker pool by
    /// one slot — bounded by the hard cap `MAX_WORKER_POOL_SIZE` — so
    /// the work item starts immediately even when every configured
    /// slot is busy.
    #[serde(default)]
    #[builder(default)]
    pub force: bool,

    pub preferred_workspace_id: Option<String>,
    pub priority: Option<i64>,
}



/// A comment paired with its resolution against the supplied plain text.
/// Returned by `comments_resolve`. The comment carries any side-effects the
/// resolve persisted (a fuzzy re-anchor, or a flip to `orphaned`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedComment {
    pub comment: WorkComment,
    pub resolution: CommentResolution,
}

// --- Magic-wand dispatch (Phase 3: engine-owned docs) ---

/// `magic_wand_dispatches.status` values.
pub const MAGIC_WAND_STATUS_IN_FLIGHT: &str = "in_flight";
pub const MAGIC_WAND_STATUS_RETURNED: &str = "returned";
pub const MAGIC_WAND_STATUS_APPLIED: &str = "applied";
pub const MAGIC_WAND_STATUS_DISCARDED: &str = "discarded";
pub const MAGIC_WAND_STATUS_CONFLICT: &str = "conflict";
pub const MAGIC_WAND_STATUS_FAILED: &str = "failed";
/// Phase 4 terminal status: a Boss chore worker was created to address the
/// comment on a PR-backed doc. The `chore_id` column carries the spawned
/// chore's id for audit linkage. There is no further engine-side Claude call.
pub const MAGIC_WAND_STATUS_CHORE_CREATED: &str = "chore_created";



/// Result of resolving a project's design-doc pointer. Carries the
/// concrete `(repo, branch, path)` triple plus a discriminator that
/// tells the open affordance which fast path it can take.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedDesignDoc {
    pub branch: String,
    pub kind: ResolvedDesignDocKind,
    pub path: String,
    pub repo_remote_url: String,
}



/// Where the resolved design doc actually lives relative to the
/// project's product. Drives the open affordance: `SameProduct` and
/// `OtherProduct` can open in the leased workspace's filesystem when
/// cube has a workspace for the repo; `External` always falls back
/// to the GitHub web URL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResolvedDesignDocKind {
    /// Doc lives in the project's own product's repo. Fast path: read
    /// the file straight from a leased workspace.
    SameProduct { product_id: String },
    /// Doc lives in a Boss-tracked product different from the
    /// project's. If cube has a workspace for that repo, the same
    /// fast path applies; otherwise fall through to web.
    OtherProduct { product_id: String },
    /// Doc lives in a repo Boss does not track as a Product. Open
    /// always goes through the GitHub web URL.
    External,
}



/// Output of the `ResolveProjectDesignDoc` RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveProjectDesignDocOutput {
    pub project_id: String,
    pub state: ProjectDesignDocState,
}



/// Role/origin of a rendered transcript segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentRole {
    User,
    Assistant,
    Thinking,
    Tool,
    System,
}



/// Input for `SetProductEditorialRules`: replace or clear a product's
/// editorial-rules blob. `rules = None` clears the column and reverts
/// the product to the engine defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetProductEditorialRulesInput {
    pub product_id: String,
    /// The new rules to store. `None` clears the column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<EditorialRules>,
}

fn default_true() -> bool {
    true
}

pub fn default_priority() -> String {
    "medium".to_owned()
}

pub fn default_human_actor() -> String {
    "human".to_owned()
}

/// A status change made by a human operator through the CLI or macOS app,
/// or by any peer whose process ancestry doesn't match the Boss-session pid.
pub const LAST_STATUS_ACTOR_HUMAN: &str = "human";
/// A status change whose caller's process ancestry traces back to the
/// registered Boss-coordinator session (the libghostty pane where the
/// Boss Claude Code instance runs as coordinator).
pub const LAST_STATUS_ACTOR_BOSS: &str = "boss";
/// A status change made directly by the engine (auto-block, dep-unblock,
/// merge poller, CI watch, etc.) — never comes from a peer RPC call.
pub const LAST_STATUS_ACTOR_ENGINE: &str = "engine";

/// Canonical "I don't know where this came from" stamp. Applied by
/// the migration to existing rows and by the engine's last-resort
/// fallback when a caller omits the field. Fresh writes from any
/// documented surface (`cli`, `bossctl`, `mac_app`, `engine_auto`)
/// must carry their own value.
pub fn default_unknown_created_via() -> String {
    CREATED_VIA_UNKNOWN.to_owned()
}

pub const CREATED_VIA_CLI: &str = "cli";
pub const CREATED_VIA_BOSSCTL: &str = "bossctl";
pub const CREATED_VIA_MAC_APP: &str = "mac_app";
pub const CREATED_VIA_ENGINE_AUTO: &str = "engine_auto";
pub const CREATED_VIA_EXTERNAL_TRACKER_SYNC: &str = "external_tracker_sync";
pub const CREATED_VIA_UNKNOWN: &str = "unknown";
/// Prefix for engine-triggered revisions spawned by the merge-conflict
/// watcher: `merge-conflict:<conflict_resolutions.id>`. The attempt id is
/// the back-pointer; `(repo, pr#)` is recoverable from the chain root.
/// Design: `tools/boss/docs/designs/unify-pr-remediation-on-revisions.md` Q2.
pub const CREATED_VIA_MERGE_CONFLICT_PREFIX: &str = "merge-conflict:";
/// Prefix for engine-triggered revisions spawned by the CI-failure watcher:
/// `ci-fix:<ci_remediations.id>`. Mirrors `CREATED_VIA_MERGE_CONFLICT_PREFIX`.
pub const CREATED_VIA_CI_FIX_PREFIX: &str = "ci-fix:";
/// Prefix for engine-triggered revisions spawned by the automated PR reviewer
/// (P992): `pr_review:<pr_review_execution_id>`.
pub const CREATED_VIA_PR_REVIEW_PREFIX: &str = "pr_review:";
/// Engine-triggered work spawned by actioning an attention group
/// (`ActionAttentionGroup`): the revision / design task produced from a
/// question group, or the batch of tasks/chores produced from a followup
/// group. Design: `tools/boss/docs/designs/attentions.md`.
pub const CREATED_VIA_ATTENTION: &str = "attention";

/// Documented `created_via` values. The engine canonicalises caller-
/// supplied strings against this set; values outside it are stored
/// as-is but logged so we can spot undocumented sources sneaking in.
pub const KNOWN_CREATED_VIA: &[&str] = &[
    CREATED_VIA_CLI,
    CREATED_VIA_BOSSCTL,
    CREATED_VIA_MAC_APP,
    CREATED_VIA_ENGINE_AUTO,
    CREATED_VIA_EXTERNAL_TRACKER_SYNC,
    CREATED_VIA_ATTENTION,
    CREATED_VIA_UNKNOWN,
];

/// `true` when `value` is one of the documented `created_via` strings
/// or matches a documented prefix pattern (`merge-conflict:*`,
/// `ci-fix:*`, `pr-comment:*`). Engine writes for unknown values still
/// go through, but a warning is logged at the insert site.
pub fn is_known_created_via(value: &str) -> bool {
    KNOWN_CREATED_VIA.contains(&value)
        || value.starts_with(CREATED_VIA_MERGE_CONFLICT_PREFIX)
        || value.starts_with(CREATED_VIA_CI_FIX_PREFIX)
        || value.starts_with(CREATED_VIA_PR_REVIEW_PREFIX)
        || value.starts_with("pr-comment:")
}



/// Input to `SetProductExternalTracker`: bind (or unbind) an external
/// tracker on a product. When `unset` is `true`, the engine clears both
/// `external_tracker_kind` and `external_tracker_config` regardless of the
/// other fields. When `unset` is `false`, both `kind` and `config` must be
/// `Some`; the engine passes `config` through the tracker's
/// `validate_config` before persisting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetProductExternalTrackerInput {
    pub product_id: String,
    /// When `true`, clear the tracker binding. All other fields are
    /// ignored.
    #[serde(default)]
    pub unset: bool,

    /// Kind-specific JSON config. `None` only when `unset = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,

    /// Tracker discriminator (`"github"`, etc.). `None` only when
    /// `unset = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}



/// Input to the `SetProjectDesignDoc` RPC: point a project at its
/// design doc. Three optional fields (mirroring the three columns on
/// `projects`), plus an `unset` switch that clears the pointer.
///
/// Resolution semantics (also enforced engine-side):
/// - `design_doc_path = Some(p)` with non-empty `p` → set the
///   pointer; `repo_remote_url` / `branch` are best-effort overrides
///   (any `None` falls back to the product's defaults).
/// - `design_doc_path = None` with `unset = false` → only the
///   non-path fields are updated; the existing path stays put.
/// - `unset = true` → clear all three columns. Any explicit field
///   values supplied alongside are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SetProjectDesignDocInput {
    pub project_id: String,
    /// When `true`, clear the pointer entirely (all three columns set
    /// to NULL). Takes precedence over any explicit field values.
    #[serde(default)]
    pub unset: bool,

    /// `None` means "inherit from `product.docs_branch`, falling back
    /// to `"main"`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,

    /// Repo-relative path. Setting `Some("")` is rejected by the
    /// engine (use `unset = true` to clear). `None` leaves the
    /// existing path unchanged unless `unset` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,

    /// `None` means "inherit from `product.repo_remote_url`" (the
    /// in-repo case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
}



/// Discriminator for the `tasks.kind` column.  Exhaustive match
/// enforces that every callsite handles new variants explicitly —
/// adding a new kind here produces a compile error at every kind-keyed
/// branch that must reason about it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Chore,
    Design,
    Investigation,
    ProjectTask,
    Revision,
    Task,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chore => "chore",
            Self::Design => "design",
            Self::Investigation => "investigation",
            Self::ProjectTask => "project_task",
            Self::Revision => "revision",
            Self::Task => "task",
        }
    }
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "chore" => Ok(Self::Chore),
            "design" => Ok(Self::Design),
            "investigation" => Ok(Self::Investigation),
            "project_task" => Ok(Self::ProjectTask),
            "revision" => Ok(Self::Revision),
            "task" => Ok(Self::Task),
            other => Err(format!(
                "unknown task kind: `{other}`; expected one of: \
                 chore, design, investigation, project_task, revision, task"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct Task {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,

    pub product_id: String,
    /// When `false`, the engine's auto-dispatcher will not turn this
    /// work item into a `ready` execution while it sits in `todo`.
    /// Existing rows from before this column was introduced default
    /// to `true` so legacy callers keep their old auto-start behavior.
    #[serde(default = "default_true")]
    #[builder(default = true)]
    pub autostart: bool,

    /// Every active block reason currently in flight on this work
    /// item — the multi-signal companion to the scalar
    /// `blocked_reason` cache. Mirrors the `task_blocked_signals`
    /// side table. Empty when the row is not blocked. The scalar
    /// `blocked_reason` / `blocked_attempt_id` fields above remain the
    /// denormalised "primary reason" cache for UI rendering and resolve
    /// to the highest-priority entry in this list per the design's
    /// §Q2 priority order. Existing rows from before this column was
    /// introduced default to an empty list.
    #[serde(default)]
    #[builder(default)]
    pub blocked_signals: Vec<BlockedSignal>,

    /// Number of CI fix attempts the engine has already consumed for
    /// the current cycle. Reset to 0 when the parent transitions back
    /// to `in_review` after a successful auto-fix (or when the user
    /// runs `boss engine ci retry`). Only `attempt_kind = 'fix'`
    /// attempts that progressed past the worker's go/no-go decision
    /// count. Existing rows from before this column was introduced
    /// default to 0.
    #[serde(default)]
    #[builder(default)]
    pub ci_attempts_used: i64,

    pub created_at: String,
    /// The surface that filed this row — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`, or `unknown`. Stamped at insert time and never
    /// rewritten. `unknown` only appears on rows that predate this
    /// column (the migration default); fresh writes always carry one
    /// of the other values.
    #[serde(default = "default_unknown_created_via")]
    #[builder(default = default_unknown_created_via())]
    pub created_via: String,

    pub description: String,
    /// `true` when any descendant revision task in the chain has status
    /// `todo` or `active` — new commits are still incoming, so the PR is
    /// not safe to merge yet. Derived projection, not stored. Only
    /// meaningful on chain-root tasks that carry a `pr_url`.
    #[serde(default, skip_serializing_if = "is_false")]
    #[builder(default)]
    pub has_in_progress_revision: bool,

    pub kind: TaskKind,
    /// Who made the most recent status change — `'human'`, `'boss'`,
    /// or `'engine'`. See `Project.last_status_actor` for full semantics.
    #[serde(default = "default_human_actor")]
    #[builder(default = default_human_actor())]
    pub last_status_actor: String,

    pub name: String,
    /// One of `low` / `medium` / `high`. Mirrors `Project.priority`
    /// exactly so kanban surfaces can render every work-item kind with
    /// the same vocabulary. Existing rows from before this column was
    /// introduced default to `medium`.
    #[serde(default = "default_priority")]
    #[builder(default = default_priority())]
    pub priority: String,

    pub status: String,
    pub updated_at: String,
    /// Soft FK to the attempt row currently trying to clear the block —
    /// `conflict_resolutions.id` when `blocked_reason = 'merge_conflict'`,
    /// the review-iteration table's id when `blocked_reason = 'review_feedback'`,
    /// etc. `None` for `'dependency'` (the prereqs are queried via
    /// `work_item_dependencies` instead) and for any block without an
    /// engine-managed attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_attempt_id: Option<String>,

    /// When `status = 'blocked'`, an open-ended discriminator
    /// explaining *why*. Documented values: `'dependency'` (gated by a
    /// `work_item_dependencies` prereq), `'merge_conflict'` (an
    /// `in_review` PR's branch conflicts with `main`), `'review_feedback'`
    /// (a reviewer requested changes), `'ci_failure'` / `'ci_failure_exhausted'`
    /// (CI on the PR went red). `None` for non-`blocked` rows and for
    /// legacy `blocked` rows whose reason wasn't tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    /// Per-PR override of the CI auto-fix attempt budget. `None` →
    /// inherit the product default (`products.ci_attempt_budget`,
    /// default 3). `Some(0)` means "notify only" (no auto-fix on this
    /// PR). See `merge-conflict-handling-in-review.md` §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_attempt_budget: Option<i64>,

    /// Structured detail for the CI indicator tooltip. JSON-encoded list of
    /// objects with `name` and `conclusion` keys, one per failing required
    /// check. `None` when `ci_required_state` is not `"fail"` or when no
    /// detail is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_required_detail: Option<String>,

    /// Aggregate state of required CI checks at last poll. Three terminal
    /// values: `"in_progress"` (at least one required check is still
    /// running), `"success"` (all required checks passed), `"fail"` (at
    /// least one required check failed). `"unknown"` means the repo has no
    /// branch protection or the first poll hasn't run yet. `None` until the
    /// merge poller has performed at least one successful probe. Only
    /// meaningful when `status = "in_review"` and `pr_url` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_required_state: Option<String>,

    pub deleted_at: Option<String>,
    /// Effort estimate for the work item. `None` means "no level set;
    /// dispatcher falls through to product / engine default per design
    /// §Q3." Set by the coordinator's heuristic at creation, or by an
    /// explicit `--effort` flag on `boss task/chore create|edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,

    /// Stable pointer to the upstream tracker issue linked to this work item.
    /// `None` when no external tracker binding exists. Populated by the
    /// reconciler on import or manual link; cleared (with `unbound_at` set)
    /// when the upstream item leaves the product's configured scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<WorkItemExternalRef>,

    /// Merge-queue state at last poll. `Some("queued")` when the PR is
    /// currently in GitHub's merge queue; `None` when it is not queued or the
    /// repo does not have a merge queue configured. Replaces the CI indicator
    /// on Review-lane cards while the PR is actively merging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_queue_state: Option<String>,

    /// Explicit model slug override. `None` → resolve via the design's
    /// Q3 precedence (effort default → product default → engine default).
    /// Stored verbatim — the engine does not validate the slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    pub ordinal: Option<i64>,
    /// Soft FK to the `tasks.id` whose PR this revision targets. `None`
    /// for every non-`revision` row. Required (app-enforced) when
    /// `kind = 'revision'`; never set by `ALTER TABLE … ADD COLUMN`
    /// backfill, so pre-revision rows carry `NULL` as expected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,

    /// RFC 3339 timestamp of the most recent successful poll that wrote
    /// `ci_required_state` / `review_required_state`. `None` until the first
    /// probe completes. The UI uses this to render "last checked: N ago".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_state_polled_at: Option<String>,

    pub pr_url: Option<String>,
    pub project_id: Option<String>,
    /// Per-work-item repo override. `None` → inherit from the parent
    /// `Product.repo_remote_url`. Stored as a canonical remote URL
    /// (e.g. `git@github.com:myorg/repo.git` or
    /// `https://github.com/myorg/repo.git`); short-name display is
    /// derived on the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,

    /// Reviewer names for the review indicator tooltip. JSON-encoded list of
    /// login strings. For `"approved"`: the approving reviewers. For
    /// `"changes_requested"`: the requesting reviewers. `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_required_detail: Option<String>,

    /// State of required-review gating at last poll. Values:
    /// `"required"` (awaiting at least one required review),
    /// `"approved"` (all required reviews approved),
    /// `"changes_requested"` (at least one reviewer requested changes),
    /// `"unknown"` (review state could not be determined). `None` until the
    /// merge poller has performed at least one successful probe. Only
    /// meaningful when `status = "in_review"` and `pr_url` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_required_state: Option<String>,

    /// How many automated reviewer passes have completed for this PR.
    /// Starts at 0; incremented after each `pr_review` execution finishes.
    /// The engine skips enqueueing a new reviewer pass when this value
    /// reaches `max_review_cycles` (config knob; default 3). Only
    /// meaningful on tasks that carry a `pr_url`. P992 design §7.
    #[serde(default)]
    #[builder(default)]
    pub review_cycle: i64,

    /// HEAD SHA of the PR at the time the most recent reviewer pass
    /// completed. Used by the no-op skip gate (P992 design §8, task 10)
    /// to detect pure rebases and skip re-review when nothing meaningful
    /// changed since the last pass. `None` until at least one reviewer
    /// pass has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_sha: Option<String>,

    /// Denormalised PR URL of the chain-root task for fast revision-card
    /// rendering. `None` for non-revision rows and for revisions whose chain
    /// root has no PR yet (rare — the create gate normally blocks that).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_parent_pr_url: Option<String>,

    /// Engine-computed R-number for revision tasks. 1-based, chain-root-scoped,
    /// creation-ordered: the N-th revision filed against a given chain root
    /// gets `revision_seq = N`. `None` for every non-`revision` row. This is
    /// a derived projection — not a stored column — computed fresh on every
    /// `get_work_tree` call so deletions and soft-deletes stay consistent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_seq: Option<i64>,

    /// FK to the `automations.id` that produced this task via the triage
    /// phase. `None` for every task not produced by an automation. When set:
    /// (1) links the task back to its automation for per-automation task
    /// listing, (2) drives backlog/kanban exclusion, (3) routes the
    /// execution to the automations pool, (4) is the denominator for the
    /// automation's open-task limit. `None` on all pre-automation rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_automation_id: Option<String>,

    /// `true` while an independent `pr_review` reviewer execution is
    /// running for this task. The task is held in the Doing column
    /// (`status = "active"`) until the reviewer finalises (or a timeout
    /// forces the advance). Surfaces as a "Reviewing (AI)" badge on the
    /// kanban card so the user can tell the hold is intentional.
    ///
    /// This is a derived projection set by the engine's `get_work_tree`
    /// path (not a stored DB column). It is always `false` for tasks
    /// not currently undergoing an AI review pass.
    #[serde(default, skip_serializing_if = "is_false")]
    #[builder(default)]
    pub ai_reviewing: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}



/// Live runtime status for a single task/chore — the current execution
/// and most recent run, summarized for the kanban view. `None` fields
/// mean no execution (or no run) exists yet for the work item.
///
/// `execution_id` is the active or most recent execution row; the
/// engine uses the same value as `run_id` when registering live
/// worker state, so UI consumers can join `task → execution_id →
/// LiveWorkerState`. `current_run_id` is the latest `work_runs` row
/// attached to that execution (`None` until the dispatch loop has
/// progressed past the cube-workspace-lease stage and called
/// `start_execution_run`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRuntime {
    pub work_item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,

    pub execution_status: Option<ExecutionStatus>,
    pub run_status: Option<String>,
}



/// How strictly the product's `.github/PULL_REQUEST_TEMPLATE.md`
/// conformance is enforced on PR bodies.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TemplatePolicy {
    /// No template enforcement — worker writes whatever it likes (today's
    /// default behaviour).
    #[default]
    Off,
    /// Inject the template as guidance in the worker prompt, but do not
    /// block a non-conforming PR body.
    Advise,
    /// Block `gh pr create` / `gh pr edit` calls whose body does not
    /// contain the mandatory template sections.
    Enforce,
}



/// Whether the worker should append an AI co-author trailer to commit
/// messages.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrailerPolicy {
    /// Engine default: the worker follows whatever its CLAUDE.md says
    /// (today that means appending `Co-Authored-By: Claude …`).
    #[default]
    Default,
    /// Strip any AI co-author trailer before the worker calls `git
    /// commit`. The worker is also instructed not to add it.
    NoAiTrailer,
}



/// One rendered transcript segment, as returned by `executions.transcript`.
///
/// Structured for lazy per-segment rendering in the UI: each segment maps to
/// one JSONL event (user turn, assistant turn, tool call, tool result, …) and
/// carries its own markdown body so the renderer builds ASTs one at a time.
#[derive(Debug, Clone, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct TranscriptSegment {
    #[builder(default)]
    pub collapsible: bool,

    #[builder(default)]
    pub default_collapsed: bool,

    /// Short human-readable label (e.g. `"User"`, `"⚙ Bash"`, `"↳ result"`).
    pub label: String,

    /// Rendered markdown body for this segment.
    pub markdown: String,

    pub role: SegmentRole,
    pub seq: u64,
    pub model: Option<String>,
    pub timestamp: Option<String>,
    pub truncated: Option<TruncationInfo>,
}



/// Set when a tool result was truncated by the renderer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationInfo {
    pub shown_bytes: usize,
    pub total_bytes: usize,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAttentionItem {
    pub id: String,
    pub body_markdown: String,
    pub created_at: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    /// The execution this item attaches to, when the failure has a
    /// concrete execution row (e.g. a worker run failed mid-flight).
    /// `None` when the item attaches to a work item directly because
    /// no execution row exists yet — the `repo_unresolved` flow per
    /// `multi-repo-work-modeling.md` Q5 is the load-bearing case.
    #[serde(default)]
    pub execution_id: Option<String>,

    pub resolved_at: Option<String>,
    /// The work item this item attaches to when there is no execution
    /// row (sticky, pre-dispatch failures). Mutually exclusive with
    /// `execution_id` — exactly one of the two is `Some`.
    #[serde(default)]
    pub work_item_id: Option<String>,
}



/// An engine-persisted comment row (`work_comments` table). Anchored to an
/// artifact (`work_item:<id>` or `pr_doc:<repo>:<branch>:<path>`) via a
/// [`CommentAnchor`]. 13 fields → uses the builder pattern per the project's
/// `boss-protocol` convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct WorkComment {
    pub id: String,
    /// The work-item id, or the synthetic `pr_doc:<repo>:<branch>:<path>`
    /// composite key.
    pub artifact_id: String,

    /// The W3C `TextQuoteSelector` anchor. Stored as `anchor_json` in the DB.
    pub anchor: CommentAnchor,

    /// `work_item` (engine-owned description) or `pr_doc` (markdown on a
    /// PR branch).
    pub artifact_kind: String,

    /// `user:<email>` for human-authored comments; `magic_wand:<id>` reserved.
    pub author: String,

    pub body: String,
    pub created_at: String,
    /// SHA-256 (or other opaque digest) of the plain-text projection the
    /// comment was authored against. Used only for equality (magic-wand
    /// CAS in a later phase); never parsed.
    pub doc_version: String,

    /// Version of the renderer's plain-text-projection algorithm the anchor
    /// was authored against. A future projection upgrade can mass re-anchor
    /// every comment whose value is stale (design § Risks mitigation).
    #[serde(default)]
    #[builder(default)]
    pub plain_text_projection_version: i64,

    /// `active` | `resolved` | `orphaned` | `dismissed`.
    #[serde(default = "default_comment_status")]
    #[builder(default = default_comment_status())]
    pub status: String,

    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,

    /// `exact` | `fuzzy` | `orphan` — how the anchor last resolved. `None`
    /// until the renderer reports a resolution. Drives the ⚠ glyph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resolved_with: Option<String>,

    /// Who flipped status last (`user:<email>`, `engine_design_detector`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_actor: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct WorkExecution {
    pub id: String,
    pub work_item_id: String,
    /// When `true`, the cube lease call for this execution will include
    /// `--allow-dirty`, causing cube to reclaim the named preferred workspace
    /// with its uncommitted working copy intact rather than resetting it.
    /// Set only on the orphan recovery re-dispatch path (when the predecessor
    /// execution was `orphaned`) so the recovering worker lands on the exact
    /// dirty workspace its predecessor left behind. Normal and transient-retry
    /// dispatches leave this `false`.
    #[serde(default)]
    #[builder(default)]
    pub allow_dirty: bool,

    /// Branch-naming strategy snapshotted from the owning product's
    /// `editorial_rules.branch_naming` at execution spawn time. Frozen
    /// here so that the engine can reconstruct the expected branch name
    /// from `state.db` alone, even after the product's rule changes.
    /// `NULL` in the DB (pre-migration rows) deserialises as the default
    /// [`BranchNaming::BossExecPrefix`], preserving historical behaviour.
    #[serde(default)]
    #[builder(default)]
    pub branch_naming: BranchNaming,

    pub created_at: String,
    pub kind: ExecutionKind,
    /// Number of pre-start failures (cube_repo_ensure, workspace_lease,
    /// change_create, run_start) accumulated on this execution row. The
    /// engine retries up to N times before marking the execution `failed`
    /// permanently. Reset to 0 on a fresh `ready` execution.
    #[serde(default)]
    #[builder(default)]
    pub pre_start_failure_count: i64,

    /// When `true`, the cube workspace preference (`preferred_workspace_id`)
    /// is treated as a warmth hint only: if the preferred workspace is
    /// unavailable or busy, the coordinator falls back silently to any free
    /// workspace rather than failing terminally. Set `true` for
    /// `revision_implementation` executions (warmth ≠ correctness; the
    /// branch is always recoverable via `jj git fetch`). Pre-revision rows
    /// default to `false`, preserving the existing hard-prefer semantics
    /// used by orphan-resume.
    #[serde(default)]
    #[builder(default)]
    pub prefer_is_soft: bool,

    #[serde(default)]
    #[builder(default)]
    pub priority: i64,

    pub repo_remote_url: String,
    pub status: ExecutionStatus,
    /// Number of times the engine has auto-resumed this work item's
    /// chain of executions because a worker stalled or died on a
    /// *transient* Claude API error (socket closed, connection reset,
    /// 5xx, `overloaded_error`, `rate_limit`/429, request timeout).
    /// Carried forward onto each fresh resume execution by
    /// [`crate::WorkExecution`]'s recovery path so the engine can cap
    /// retries and back off — distinct from
    /// [`Self::pre_start_failure_count`], which counts failures that
    /// happen *before* a worker ever runs. Reset to 0 on a human-
    /// initiated or first dispatch.
    #[serde(default)]
    #[builder(default)]
    pub transient_failure_count: i64,

    pub cube_lease_id: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    /// Unix epoch seconds (as a string) before which this `ready`
    /// execution must not be dispatched. `None` means dispatchable
    /// immediately. Set during pre-start retry backoff windows.
    #[serde(default)]
    pub dispatch_not_before: Option<String>,

    pub finished_at: Option<String>,
    /// SHA of the bound chore PR's head ref at the moment this
    /// execution started running. Captured once at run start when
    /// `Task.pr_url` is already populated (i.e. this is a resume /
    /// bounce-back of an already-bound chore). Used by the Stop
    /// boundary's SHA-delta gate to decide whether the run actually
    /// contributed to the bound PR before falling through to the
    /// `PROBE_NO_PR` nudge — fixes the runtime-nudge-loop bug where
    /// resume runs that pushed a fix commit got re-nudged forever.
    /// `None` when `Task.pr_url` was empty at run start (new-PR
    /// flow), when the snapshot fetch failed, or on rows that
    /// predate this column.
    #[serde(default)]
    pub pr_head_before: Option<String>,

    /// The PR URL captured at the end of this execution's run, if any.
    /// Set when the worker successfully opens a PR and the engine
    /// records the `completed` transition for this execution.
    #[serde(default)]
    pub pr_url: Option<String>,

    pub preferred_workspace_id: Option<String>,
    pub started_at: Option<String>,
    /// Worker branch-name prefix frozen onto this execution at creation
    /// time, denormalised from the owning product's
    /// `worker_branch_prefix` (same pattern as `repo_remote_url`).
    /// Freezing it here keeps the engine-supplied branch name
    /// reconstructible from `state.db` alone and immune to a product
    /// prefix change between spawn and PR detection. `None` → the
    /// engine default `boss/`. The branch name is
    /// `<worker_branch_prefix>exec_<id>`; only the prefix varies.
    #[serde(default)]
    pub worker_branch_prefix: Option<String>,

    pub workspace_path: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "item_type", rename_all = "snake_case")]
pub enum WorkItem {
    Product(Product),
    Project(Project),
    Task(Task),
    Chore(Task),
}



/// One row of the `work_item_dependencies` table — an edge from a
/// dependent to a prerequisite. `relation` is `"blocks"` for v1; the
/// column exists so future relation types (`"relates-to"`,
/// `"duplicates"`, …) can ship without a re-migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemDependency {
    pub dependent_id: String,
    pub prerequisite_id: String,
    pub created_at: String,
    #[serde(default = "default_relation")]
    pub relation: String,
}

pub fn default_relation() -> String {
    "blocks".to_owned()
}



/// Resolved dependency listing for a single work item. Each side
/// carries [`DependencyEdge`] entries with the peer's status and
/// name already joined in. Used by `boss <kind> show` and (in time)
/// the macOS dep section. Distinct from [`WorkItemDependencyView`]
/// because that one returns raw edge rows for the depend-list verb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyDetail {
    pub work_item_id: String,
    pub dependents: Vec<DependencyEdge>,
    pub prerequisites: Vec<DependencyEdge>,
}



/// Two parallel edge lists for one work item — incoming (rows that
/// gate me) and outgoing (rows that I gate). Returned by
/// `ListDependencies` and embedded in `boss <kind> show`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyView {
    pub work_item_id: String,
    pub dependents: Vec<WorkItemDependency>,
    pub prerequisites: Vec<WorkItemDependency>,
}



/// Stable upstream pointer stored on a work item that has been linked to
/// an external tracker issue. All three `kind`/`canonical_id`/`raw` fields
/// mirror the corresponding `tasks.external_ref_*` columns; `web_url` is
/// the canonical browser URL for the upstream issue (derived by the engine
/// at read time, not stored). See the external-tracker sync design.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkItemExternalRef {
    /// Stable opaque id used as the reconciler's lookup key.
    /// For GitHub: `"spinyfin/mono#560"`.
    pub canonical_id: String,

    /// Tracker discriminator (`"github"`, eventually `"jira"`, etc.).
    pub kind: String,

    /// Tracker-specific extras opaque to the engine. For GitHub: the
    /// `project_item_id` needed for status-field reads/writes.
    pub raw: serde_json::Value,

    /// Canonical browser URL for the upstream issue. Derived at read
    /// time by the engine; not stored in the DB.
    pub web_url: String,

    /// Unix-seconds string of the last successful upstream→Boss
    /// reconcile. `None` until the first reconcile completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<String>,

    /// Unix-seconds string when the binding was cleared because the
    /// upstream item disappeared from the product's configured scope.
    /// `None` while the binding is active. Retained so the reconciler
    /// can re-bind automatically if the item reappears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unbound_at: Option<String>,
}



#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemPatch {
    /// Flip the `autostart` flag. `None` → leave unchanged.
    /// `Some(true)` → enable auto-dispatch; `Some(false)` → disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,

    /// Set or clear the `blocked_reason` field. `None` → leave unchanged.
    /// `Some("")` → clear (write NULL). Any non-empty string is stored verbatim
    /// (e.g. `"merge_conflict"`, `"ci_failure"`). Manual escape hatch for
    /// clearing stale reasons the automated sweepers missed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,

    /// Product-level default model. Only honoured on
    /// product-targeted updates; ignored when patching a task/chore/
    /// project. `None` → leave unchanged. `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,

    pub description: Option<String>,
    /// Product-level design-task repo override. Only honoured on
    /// product-targeted updates; ignored when patching a task /
    /// chore / project. `None` → leave unchanged. `Some("")` →
    /// clear (write NULL). Stored canonicalised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_repo: Option<String>,

    /// Product-level dispatch preamble. Only honoured on
    /// product-targeted updates. `None` → leave unchanged.
    /// `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,

    /// Product-level investigation-task ("docs") repo override. Only
    /// honoured on product-targeted updates; ignored when patching a
    /// task / chore / project. `None` → leave unchanged. `Some("")` →
    /// clear (write NULL → fall through to `BOSS_USER_DOCS_REPO`).
    /// Stored canonicalised. See [`Product::docs_repo`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_repo: Option<String>,

    /// Effort estimate to apply on this update. `None` → leave the
    /// existing column value alone. `Some("")` → clear the column
    /// (write NULL). Any other string is validated against the
    /// [`EffortLevel`] enum at the engine boundary; invalid values
    /// reject the entire patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,

    pub goal: Option<String>,
    /// Model slug override. `None` → leave unchanged. `Some("")` →
    /// clear the column. Any other string is stored verbatim (no
    /// validation — `claude` is the source of truth on slugs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,

    pub name: Option<String>,
    pub ordinal: Option<i64>,
    pub pr_url: Option<String>,
    pub priority: Option<String>,
    pub repo_remote_url: Option<String>,
    pub status: Option<String>,
    /// Product-level worker branch-name prefix. Only honoured on
    /// product-targeted updates; ignored when patching a task / chore /
    /// project. `None` → leave unchanged. `Some("")` → clear (write
    /// NULL → engine default `boss/`). Stored canonicalised with a
    /// trailing `/`. See [`Product::worker_branch_prefix`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_branch_prefix: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRun {
    pub id: String,
    pub agent_id: String,
    pub execution_id: String,
    pub created_at: String,
    pub status: String,
    pub artifacts_path: Option<String>,
    pub error_text: Option<String>,
    pub finished_at: Option<String>,
    pub result_summary: Option<String>,
    pub started_at: Option<String>,
    pub transcript_path: Option<String>,
}



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTree {
    pub chores: Vec<Task>,
    /// Every `work_item_dependencies` edge whose dependent belongs to
    /// this product. Lets the kanban resolve "blocked by <prereq>"
    /// labels (and any future dep affordance) without an N+1 round
    /// trip — clients already have every task/chore/project name.
    #[serde(default)]
    pub dependencies: Vec<WorkItemDependency>,

    pub product: Product,
    pub projects: Vec<Project>,
    #[serde(default)]
    pub task_runtimes: Vec<TaskRuntime>,

    pub tasks: Vec<Task>,
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn sample_project_json(extra: Value) -> Value {
        let mut base = json!({
            "id": "proj_1",
            "product_id": "prod_1",
            "name": "Demo",
            "slug": "demo",
            "description": "",
            "goal": "",
            "status": "todo",
            "priority": "medium",
            "created_at": "2026-05-11T00:00:00Z",
            "updated_at": "2026-05-11T00:00:00Z",
        });
        if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
            for (k, v) in extra {
                target.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn project_decodes_without_short_id() {
        let raw = sample_project_json(json!({}));
        let project: Project = serde_json::from_value(raw).unwrap();
        assert!(project.short_id.is_none());
    }

    #[test]
    fn project_skips_none_short_id_on_encode() {
        let project: Project = serde_json::from_value(sample_project_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&project).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("short_id"));
    }

    #[test]
    fn project_roundtrips_with_short_id() {
        let raw = sample_project_json(json!({"short_id": 42}));
        let project: Project = serde_json::from_value(raw).unwrap();
        assert_eq!(project.short_id, Some(42));
        let reencoded = serde_json::to_value(&project).unwrap();
        assert_eq!(reencoded["short_id"], Value::from(42_i64));
        let project2: Project = serde_json::from_value(reencoded).unwrap();
        assert_eq!(project.short_id, project2.short_id);
    }

    #[test]
    fn project_decodes_without_design_doc_fields() {
        let raw = sample_project_json(json!({}));
        let project: Project = serde_json::from_value(raw).unwrap();
        assert!(project.design_doc_repo_remote_url.is_none());
        assert!(project.design_doc_branch.is_none());
        assert!(project.design_doc_path.is_none());
        assert_eq!(project.last_status_actor, "human");
    }

    #[test]
    fn project_skips_none_design_doc_fields_on_encode() {
        let project: Project = serde_json::from_value(sample_project_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&project).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("design_doc_repo_remote_url"));
        assert!(!obj.contains_key("design_doc_branch"));
        assert!(!obj.contains_key("design_doc_path"));
    }

    #[test]
    fn project_roundtrips_with_design_doc_fields() {
        let raw = sample_project_json(json!({
            "design_doc_repo_remote_url": "https://github.com/foo/bar.git",
            "design_doc_branch": "main",
            "design_doc_path": "tools/boss/docs/designs/demo.md",
        }));
        let project: Project = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            project.design_doc_repo_remote_url.as_deref(),
            Some("https://github.com/foo/bar.git"),
        );
        assert_eq!(project.design_doc_branch.as_deref(), Some("main"));
        assert_eq!(
            project.design_doc_path.as_deref(),
            Some("tools/boss/docs/designs/demo.md"),
        );

        let reencoded = serde_json::to_value(&project).unwrap();
        let project2: Project = serde_json::from_value(reencoded).unwrap();
        assert_eq!(
            project.design_doc_repo_remote_url,
            project2.design_doc_repo_remote_url,
        );
        assert_eq!(project.design_doc_branch, project2.design_doc_branch);
        assert_eq!(project.design_doc_path, project2.design_doc_path);
    }

    #[test]
    fn set_project_design_doc_input_roundtrips() {
        let input = SetProjectDesignDocInput {
            project_id: "proj_1".into(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: Some("tools/boss/docs/designs/demo.md".into()),
            unset: false,
        };
        let raw = serde_json::to_value(&input).unwrap();
        let obj = raw.as_object().unwrap();
        assert!(!obj.contains_key("design_doc_repo_remote_url"));
        assert!(!obj.contains_key("design_doc_branch"));
        assert_eq!(obj.get("unset"), Some(&Value::Bool(false)));
        let back: SetProjectDesignDocInput = serde_json::from_value(raw).unwrap();
        assert_eq!(back.project_id, input.project_id);
        assert_eq!(back.design_doc_path, input.design_doc_path);
        assert_eq!(back.unset, input.unset);
    }

    #[test]
    fn set_project_design_doc_input_unset_decodes_without_optional_fields() {
        let raw = json!({
            "project_id": "proj_1",
            "unset": true,
        });
        let parsed: SetProjectDesignDocInput = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.project_id, "proj_1");
        assert!(parsed.unset);
        assert!(parsed.design_doc_path.is_none());
    }

    #[test]
    fn resolved_design_doc_kind_serializes_as_internally_tagged() {
        let same = ResolvedDesignDocKind::SameProduct {
            product_id: "prod_1".into(),
        };
        let raw = serde_json::to_value(&same).unwrap();
        assert_eq!(
            raw,
            json!({"type": "same_product", "product_id": "prod_1"})
        );

        let external = ResolvedDesignDocKind::External;
        let raw = serde_json::to_value(&external).unwrap();
        assert_eq!(raw, json!({"type": "external"}));

        let back: ResolvedDesignDocKind =
            serde_json::from_value(json!({"type": "other_product", "product_id": "prod_2"}))
                .unwrap();
        assert_eq!(
            back,
            ResolvedDesignDocKind::OtherProduct {
                product_id: "prod_2".into(),
            }
        );
    }

    #[test]
    fn project_design_doc_state_roundtrips_all_variants() {
        let not_set = ProjectDesignDocState::NotSet;
        let raw = serde_json::to_value(&not_set).unwrap();
        assert_eq!(raw, json!({"type": "not_set"}));
        assert_eq!(
            serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(),
            not_set,
        );

        let resolved = ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "https://github.com/foo/bar.git".into(),
                branch: "main".into(),
                path: "docs/x.md".into(),
                kind: ResolvedDesignDocKind::SameProduct {
                    product_id: "prod_1".into(),
                },
            },
            workspace_path: Some("/Users/me/Documents/dev/workspaces/mono-agent-001".into()),
            web_url: "https://github.com/foo/bar/blob/main/docs/x.md".into(),
            raw_content_url: Some(
                "https://raw.githubusercontent.com/foo/bar/main/docs/x.md".into(),
            ),
        };
        let raw = serde_json::to_value(&resolved).unwrap();
        assert_eq!(raw["type"], "resolved");
        assert_eq!(
            serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(),
            resolved,
        );

        let broken = ProjectDesignDocState::Broken {
            reason: "no repo".into(),
        };
        let raw = serde_json::to_value(&broken).unwrap();
        assert_eq!(raw, json!({"type": "broken", "reason": "no repo"}));
        assert_eq!(
            serde_json::from_value::<ProjectDesignDocState>(raw).unwrap(),
            broken,
        );
    }

    fn sample_task_json(extra: Value) -> Value {
        let mut base = json!({
            "id": "task_1",
            "product_id": "prod_1",
            "project_id": Value::Null,
            "kind": "chore",
            "name": "Demo",
            "description": "",
            "status": "todo",
            "ordinal": Value::Null,
            "pr_url": Value::Null,
            "deleted_at": Value::Null,
            "created_at": "2026-05-11T00:00:00Z",
            "updated_at": "2026-05-11T00:00:00Z",
        });
        if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
            for (k, v) in extra {
                target.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn task_decodes_without_short_id() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.short_id.is_none());
    }

    #[test]
    fn task_skips_none_short_id_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("short_id"));
    }

    #[test]
    fn task_roundtrips_with_short_id() {
        let raw = sample_task_json(json!({"short_id": 99}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(task.short_id, Some(99));
        let reencoded = serde_json::to_value(&task).unwrap();
        assert_eq!(reencoded["short_id"], Value::from(99_i64));
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.short_id, task2.short_id);
    }

    #[test]
    fn task_decodes_without_repo_remote_url() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.repo_remote_url.is_none());
        assert_eq!(task.created_via, CREATED_VIA_UNKNOWN);
    }

    #[test]
    fn task_skips_none_repo_remote_url_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("repo_remote_url"));
    }

    #[test]
    fn task_roundtrips_with_repo_remote_url() {
        let raw = sample_task_json(json!({
            "repo_remote_url": "https://github.com/foo/bar.git",
        }));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(
            task.repo_remote_url.as_deref(),
            Some("https://github.com/foo/bar.git"),
        );
        let reencoded = serde_json::to_value(&task).unwrap();
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.repo_remote_url, task2.repo_remote_url);
    }

    #[test]
    fn create_task_input_repo_remote_url_roundtrips() {
        let raw = json!({
            "product_id": "prod_1",
            "project_id": "proj_1",
            "name": "Demo",
            "description": null,
            "repo_remote_url": "git@github.com:foo/bar.git",
        });
        let parsed: CreateTaskInput = serde_json::from_value(raw).unwrap();
        assert_eq!(
            parsed.repo_remote_url.as_deref(),
            Some("git@github.com:foo/bar.git"),
        );
        let encoded = serde_json::to_value(&parsed).unwrap();
        assert_eq!(
            encoded["repo_remote_url"],
            Value::String("git@github.com:foo/bar.git".into()),
        );

        let without_field = json!({
            "product_id": "prod_1",
            "project_id": "proj_1",
            "name": "Demo",
            "description": null,
        });
        let parsed_none: CreateTaskInput = serde_json::from_value(without_field).unwrap();
        assert!(parsed_none.repo_remote_url.is_none());
        let encoded_none = serde_json::to_value(&parsed_none).unwrap();
        assert!(!encoded_none.as_object().unwrap().contains_key("repo_remote_url"));
    }

    #[test]
    fn create_chore_input_repo_remote_url_roundtrips() {
        let raw = json!({
            "product_id": "prod_1",
            "name": "Demo",
            "description": null,
            "repo_remote_url": "",
        });
        let parsed: CreateChoreInput = serde_json::from_value(raw).unwrap();
        // Empty string is preserved here; the engine interprets it as
        // "clear" on update verbs but for create it just resolves as
        // not-set / inherit.
        assert_eq!(parsed.repo_remote_url.as_deref(), Some(""));

        let without_field = json!({
            "product_id": "prod_1",
            "name": "Demo",
            "description": null,
        });
        let parsed_none: CreateChoreInput = serde_json::from_value(without_field).unwrap();
        assert!(parsed_none.repo_remote_url.is_none());
        let encoded_none = serde_json::to_value(&parsed_none).unwrap();
        assert!(!encoded_none.as_object().unwrap().contains_key("repo_remote_url"));
    }

    #[test]
    fn resolve_project_design_doc_output_roundtrips() {
        let output = ResolveProjectDesignDocOutput {
            project_id: "proj_1".into(),
            state: ProjectDesignDocState::Resolved {
                resolved: ResolvedDesignDoc {
                    repo_remote_url: "https://github.com/foo/bar.git".into(),
                    branch: "main".into(),
                    path: "docs/x.md".into(),
                    kind: ResolvedDesignDocKind::External,
                },
                workspace_path: None,
                web_url: "https://github.com/foo/bar/blob/main/docs/x.md".into(),
                raw_content_url: Some(
                    "https://raw.githubusercontent.com/foo/bar/main/docs/x.md".into(),
                ),
            },
        };
        let raw = serde_json::to_value(&output).unwrap();
        let back: ResolveProjectDesignDocOutput = serde_json::from_value(raw).unwrap();
        assert_eq!(output, back);
    }

    // Note: `sample_task_json` is defined earlier in this test module;
    // the duplicate that previously sat here was a merge-resolution
    // leftover that broke `cargo test -p boss-protocol`. The helper
    // above carries the same field set; the timestamp shape change is
    // harmless because Task's serde fields accept any string for the
    // ISO-8601 columns. See the diagnostics PR description for why
    // this one-line cleanup is bundled with the live_status work.

    #[test]
    fn task_decodes_without_blocked_fields() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.blocked_reason.is_none());
        assert!(task.blocked_attempt_id.is_none());
    }

    #[test]
    fn task_skips_none_blocked_fields_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("blocked_reason"));
        assert!(!obj.contains_key("blocked_attempt_id"));
    }

    #[test]
    fn task_roundtrips_with_blocked_fields() {
        let raw = sample_task_json(json!({
            "status": "blocked",
            "blocked_reason": "merge_conflict",
            "blocked_attempt_id": "conflict_18ab_1",
        }));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));
        assert_eq!(task.blocked_attempt_id.as_deref(), Some("conflict_18ab_1"));

        let reencoded = serde_json::to_value(&task).unwrap();
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.blocked_reason, task2.blocked_reason);
        assert_eq!(task.blocked_attempt_id, task2.blocked_attempt_id);
    }

    #[test]
    fn conflict_resolution_roundtrips_with_all_fields() {
        let attempt = ConflictResolution {
            id: "conflict_18ab_1".into(),
            product_id: "prod_1".into(),
            work_item_id: "task_77".into(),
            pr_url: "https://github.com/foo/bar/pull/243".into(),
            pr_number: 243,
            head_branch: "feat/banana".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("def456".into()),
            head_sha_after: Some("ghi789".into()),
            status: "succeeded".into(),
            failure_reason: None,
            cube_lease_id: Some("lease_1".into()),
            cube_workspace_id: Some("ws_1".into()),
            worker_id: Some("worker_1".into()),
            conflict_diagnosis: Some("{\"hunks\":1}".into()),
            created_at: "1747000000".into(),
            started_at: Some("1747000010".into()),
            finished_at: Some("1747000100".into()),
            revision_task_id: None,
        };
        let raw = serde_json::to_value(&attempt).unwrap();
        let back: ConflictResolution = serde_json::from_value(raw).unwrap();
        assert_eq!(attempt, back);
    }

    #[test]
    fn conflict_resolution_pending_skips_optional_fields_on_encode() {
        let attempt = ConflictResolution {
            id: "conflict_18ab_2".into(),
            product_id: "prod_1".into(),
            work_item_id: "task_77".into(),
            pr_url: "https://github.com/foo/bar/pull/243".into(),
            pr_number: 243,
            head_branch: "feat/banana".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: None,
            head_sha_before: None,
            head_sha_after: None,
            status: "pending".into(),
            failure_reason: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            worker_id: None,
            conflict_diagnosis: None,
            created_at: "1747000000".into(),
            started_at: None,
            finished_at: None,
            revision_task_id: None,
        };
        let encoded = serde_json::to_value(&attempt).unwrap();
        let obj = encoded.as_object().unwrap();
        for absent in [
            "base_sha_at_trigger",
            "head_sha_before",
            "head_sha_after",
            "failure_reason",
            "cube_lease_id",
            "cube_workspace_id",
            "worker_id",
            "conflict_diagnosis",
            "started_at",
            "finished_at",
        ] {
            assert!(
                !obj.contains_key(absent),
                "expected {absent} omitted on encode",
            );
        }
        let back: ConflictResolution = serde_json::from_value(encoded).unwrap();
        assert_eq!(attempt, back);
    }

    #[test]
    fn effort_level_parses_all_five_values() {
        use std::str::FromStr;
        assert_eq!(EffortLevel::from_str("trivial").unwrap(), EffortLevel::Trivial);
        assert_eq!(EffortLevel::from_str("small").unwrap(), EffortLevel::Small);
        assert_eq!(EffortLevel::from_str("medium").unwrap(), EffortLevel::Medium);
        assert_eq!(EffortLevel::from_str("large").unwrap(), EffortLevel::Large);
        assert_eq!(EffortLevel::from_str("max").unwrap(), EffortLevel::Max);
    }

    #[test]
    fn effort_level_rejects_unknown_values() {
        use std::str::FromStr;
        let err = EffortLevel::from_str("galaxybrain").unwrap_err();
        assert!(err.contains("galaxybrain"));
        assert!(err.contains("trivial"));
        assert!(err.contains("max"));
    }

    #[test]
    fn effort_level_serializes_as_lowercase_string() {
        let encoded = serde_json::to_value(EffortLevel::Large).unwrap();
        assert_eq!(encoded, Value::String("large".into()));
        let back: EffortLevel = serde_json::from_value(Value::String("trivial".into())).unwrap();
        assert_eq!(back, EffortLevel::Trivial);
    }

    #[test]
    fn task_decodes_without_effort_or_model_fields() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.effort_level.is_none());
        assert!(task.model_override.is_none());
    }

    #[test]
    fn task_skips_none_effort_and_model_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("effort_level"));
        assert!(!obj.contains_key("model_override"));
    }

    #[test]
    fn task_roundtrips_with_effort_and_model_set() {
        let raw = sample_task_json(json!({
            "effort_level": "large",
            "model_override": "claude-opus-4-7",
        }));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(task.effort_level, Some(EffortLevel::Large));
        assert_eq!(task.model_override.as_deref(), Some("claude-opus-4-7"));

        let reencoded = serde_json::to_value(&task).unwrap();
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.effort_level, task2.effort_level);
        assert_eq!(task.model_override, task2.model_override);
    }

    fn sample_product_json(extra: Value) -> Value {
        let mut base = json!({
            "id": "prod_1",
            "name": "Boss",
            "slug": "boss",
            "description": "",
            "repo_remote_url": Value::Null,
            "status": "active",
            "created_at": "1747000000",
            "updated_at": "1747000000",
        });
        if let (Value::Object(target), Value::Object(extra)) = (&mut base, extra) {
            for (k, v) in extra {
                target.insert(k, v);
            }
        }
        base
    }

    #[test]
    fn product_decodes_without_default_model() {
        let raw = sample_product_json(json!({}));
        let product: Product = serde_json::from_value(raw).unwrap();
        assert!(product.default_model.is_none());
    }

    #[test]
    fn product_roundtrips_with_default_model() {
        let raw = sample_product_json(json!({"default_model": "sonnet"}));
        let product: Product = serde_json::from_value(raw).unwrap();
        assert_eq!(product.default_model.as_deref(), Some("sonnet"));
        let encoded = serde_json::to_value(&product).unwrap();
        assert_eq!(encoded["default_model"], Value::String("sonnet".into()));
    }

    #[test]
    fn product_decodes_without_design_repo() {
        let raw = sample_product_json(json!({}));
        let product: Product = serde_json::from_value(raw).unwrap();
        assert!(product.design_repo.is_none());
    }

    #[test]
    fn product_roundtrips_with_design_repo() {
        let raw = sample_product_json(
            json!({"design_repo": "https://github.com/linkedin-sandbox/bduff.git"}),
        );
        let product: Product = serde_json::from_value(raw).unwrap();
        assert_eq!(
            product.design_repo.as_deref(),
            Some("https://github.com/linkedin-sandbox/bduff.git"),
        );
        let encoded = serde_json::to_value(&product).unwrap();
        assert_eq!(
            encoded["design_repo"],
            Value::String("https://github.com/linkedin-sandbox/bduff.git".into()),
        );
    }

    #[test]
    fn product_skips_none_design_repo_on_encode() {
        let product: Product = serde_json::from_value(sample_product_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&product).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("design_repo"));
    }

    #[test]
    fn product_decodes_without_external_tracker_fields() {
        let raw = sample_product_json(json!({}));
        let product: Product = serde_json::from_value(raw).unwrap();
        assert!(product.external_tracker_kind.is_none());
        assert!(product.external_tracker_config.is_none());
    }

    #[test]
    fn product_skips_none_external_tracker_fields_on_encode() {
        let product: Product = serde_json::from_value(sample_product_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&product).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("external_tracker_kind"));
        assert!(!obj.contains_key("external_tracker_config"));
    }

    #[test]
    fn product_roundtrips_with_external_tracker_fields() {
        let config = json!({"org": "spinyfin", "repo": "mono", "project_number": 1});
        let raw = sample_product_json(json!({
            "external_tracker_kind": "github",
            "external_tracker_config": config.clone(),
        }));
        let product: Product = serde_json::from_value(raw).unwrap();
        assert_eq!(product.external_tracker_kind.as_deref(), Some("github"));
        assert_eq!(product.external_tracker_config.as_ref().unwrap()["org"], "spinyfin");

        let reencoded = serde_json::to_value(&product).unwrap();
        let product2: Product = serde_json::from_value(reencoded).unwrap();
        assert_eq!(product.external_tracker_kind, product2.external_tracker_kind);
        assert_eq!(product.external_tracker_config, product2.external_tracker_config);
    }

    #[test]
    fn task_decodes_without_ci_attempt_fields() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.ci_attempt_budget.is_none());
        assert_eq!(task.ci_attempts_used, 0);
        assert!(task.blocked_signals.is_empty());
    }

    #[test]
    fn task_skips_default_ci_attempt_fields_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("ci_attempt_budget"));
        // `ci_attempts_used` and `blocked_signals` carry zero/empty
        // defaults rather than `Option::None`, so they round-trip
        // through the wire as concrete values. `serde(default)` on the
        // decode side is what makes the omitted-from-payload shape
        // legal.
        assert_eq!(obj.get("ci_attempts_used"), Some(&Value::from(0_i64)));
        assert_eq!(
            obj.get("blocked_signals"),
            Some(&Value::Array(Vec::new())),
        );
    }

    #[test]
    fn task_roundtrips_with_ci_attempt_fields_set() {
        let raw = sample_task_json(json!({
            "ci_attempt_budget": 5,
            "ci_attempts_used": 2,
            "blocked_signals": [
                {
                    "work_item_id": "task_1",
                    "reason": "ci_failure",
                    "attempt_id": "ci_18ab_1",
                    "created_at": "1747000000",
                    "cleared_at": Value::Null,
                },
                {
                    "work_item_id": "task_1",
                    "reason": "merge_conflict",
                    "attempt_id": "conflict_18ab_1",
                    "created_at": "1747000010",
                },
            ],
        }));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(task.ci_attempt_budget, Some(5));
        assert_eq!(task.ci_attempts_used, 2);
        assert_eq!(task.blocked_signals.len(), 2);
        assert_eq!(task.blocked_signals[0].reason, "ci_failure");
        assert_eq!(
            task.blocked_signals[0].attempt_id.as_deref(),
            Some("ci_18ab_1"),
        );

        let reencoded = serde_json::to_value(&task).unwrap();
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.ci_attempt_budget, task2.ci_attempt_budget);
        assert_eq!(task.ci_attempts_used, task2.ci_attempts_used);
        assert_eq!(task.blocked_signals, task2.blocked_signals);
    }

    #[test]
    fn blocked_signal_skips_optional_fields_on_encode() {
        let signal = BlockedSignal {
            work_item_id: "task_1".into(),
            reason: "dependency".into(),
            attempt_id: None,
            created_at: "1747000000".into(),
            cleared_at: None,
        };
        let encoded = serde_json::to_value(&signal).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("attempt_id"));
        assert!(!obj.contains_key("cleared_at"));
        let back: BlockedSignal = serde_json::from_value(encoded).unwrap();
        assert_eq!(signal, back);
    }

    #[test]
    fn ci_remediation_roundtrips_with_all_fields() {
        let attempt = CiRemediation {
            id: "ci_18ab_1".into(),
            product_id: "prod_1".into(),
            work_item_id: "task_77".into(),
            pr_url: "https://github.com/foo/bar/pull/647".into(),
            pr_number: 647,
            head_branch: "feat/banana".into(),
            head_sha_at_trigger: "abc123".into(),
            head_sha_after: Some("def456".into()),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[{\"name\":\"test\"}]".into(),
            triage_class: Some("tractable".into()),
            log_excerpt: Some("error: ...".into()),
            status: "succeeded".into(),
            failure_reason: None,
            cube_lease_id: Some("lease_1".into()),
            cube_workspace_id: Some("ws_1".into()),
            worker_id: Some("worker_1".into()),
            created_at: "1747000000".into(),
            started_at: Some("1747000010".into()),
            finished_at: Some("1747000100".into()),
            failure_kind: Some("pr_branch_ci".into()),
            before_commit_sha: None,
            revision_task_id: None,
        };
        let raw = serde_json::to_value(&attempt).unwrap();
        let back: CiRemediation = serde_json::from_value(raw).unwrap();
        assert_eq!(attempt, back);
    }

    #[test]
    fn task_decodes_without_external_ref() {
        let raw = sample_task_json(json!({}));
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.external_ref.is_none());
    }

    #[test]
    fn task_skips_none_external_ref_on_encode() {
        let task: Task = serde_json::from_value(sample_task_json(json!({}))).unwrap();
        let encoded = serde_json::to_value(&task).unwrap();
        assert!(!encoded.as_object().unwrap().contains_key("external_ref"));
    }

    #[test]
    fn task_roundtrips_with_external_ref() {
        let raw = sample_task_json(json!({
            "external_ref": {
                "kind": "github",
                "canonical_id": "spinyfin/mono#560",
                "raw": {"issue_number": 560, "project_item_id": "PVTI_abc"},
                "web_url": "https://github.com/spinyfin/mono/issues/560",
                "synced_at": "1747000100",
            },
        }));
        let task: Task = serde_json::from_value(raw).unwrap();
        let ext = task.external_ref.as_ref().unwrap();
        assert_eq!(ext.kind, "github");
        assert_eq!(ext.canonical_id, "spinyfin/mono#560");
        assert_eq!(ext.web_url, "https://github.com/spinyfin/mono/issues/560");
        assert_eq!(ext.synced_at.as_deref(), Some("1747000100"));
        assert!(ext.unbound_at.is_none());

        let reencoded = serde_json::to_value(&task).unwrap();
        let task2: Task = serde_json::from_value(reencoded).unwrap();
        assert_eq!(task.external_ref, task2.external_ref);
    }

    #[test]
    fn work_item_external_ref_skips_optional_fields_on_encode() {
        let ext = WorkItemExternalRef {
            kind: "github".into(),
            canonical_id: "spinyfin/mono#560".into(),
            raw: json!({"issue_number": 560}),
            web_url: "https://github.com/spinyfin/mono/issues/560".into(),
            synced_at: None,
            unbound_at: None,
        };
        let encoded = serde_json::to_value(&ext).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("synced_at"));
        assert!(!obj.contains_key("unbound_at"));
        let back: WorkItemExternalRef = serde_json::from_value(encoded).unwrap();
        assert_eq!(ext, back);
    }

    #[test]
    fn set_product_external_tracker_input_roundtrips() {
        let input = SetProductExternalTrackerInput {
            product_id: "prod_1".into(),
            kind: Some("github".into()),
            config: Some(json!({"org": "spinyfin", "repo": "mono", "project_number": 1})),
            unset: false,
        };
        let raw = serde_json::to_value(&input).unwrap();
        let back: SetProductExternalTrackerInput = serde_json::from_value(raw).unwrap();
        assert_eq!(back.product_id, "prod_1");
        assert_eq!(back.kind.as_deref(), Some("github"));
        assert!(!back.unset);
    }

    #[test]
    fn set_product_external_tracker_input_unset_skips_kind_and_config() {
        let input = SetProductExternalTrackerInput {
            product_id: "prod_1".into(),
            kind: None,
            config: None,
            unset: true,
        };
        let encoded = serde_json::to_value(&input).unwrap();
        let obj = encoded.as_object().unwrap();
        assert!(!obj.contains_key("kind"));
        assert!(!obj.contains_key("config"));
        assert_eq!(obj["unset"], Value::Bool(true));
    }

    #[test]
    fn link_external_ref_input_roundtrips() {
        let input = LinkExternalRefInput {
            work_item_id: "task_1".into(),
            kind: "github".into(),
            canonical_id: "spinyfin/mono#560".into(),
        };
        let raw = serde_json::to_value(&input).unwrap();
        assert_eq!(raw["work_item_id"], Value::String("task_1".into()));
        assert_eq!(raw["kind"], Value::String("github".into()));
        assert_eq!(raw["canonical_id"], Value::String("spinyfin/mono#560".into()));
        let back: LinkExternalRefInput = serde_json::from_value(raw).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn ci_remediation_pending_skips_optional_fields_on_encode() {
        let attempt = CiRemediation {
            id: "ci_18ab_2".into(),
            product_id: "prod_1".into(),
            work_item_id: "task_77".into(),
            pr_url: "https://github.com/foo/bar/pull/648".into(),
            pr_number: 648,
            head_branch: "feat/coconut".into(),
            head_sha_at_trigger: "abc123".into(),
            head_sha_after: None,
            attempt_kind: "retrigger".into(),
            consumes_budget: 0,
            failed_checks: "[]".into(),
            triage_class: None,
            log_excerpt: None,
            status: "pending".into(),
            failure_reason: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            worker_id: None,
            created_at: "1747000000".into(),
            started_at: None,
            finished_at: None,
            failure_kind: None,
            before_commit_sha: None,
            revision_task_id: None,
        };
        let encoded = serde_json::to_value(&attempt).unwrap();
        let obj = encoded.as_object().unwrap();
        for absent in [
            "head_sha_after",
            "triage_class",
            "log_excerpt",
            "failure_reason",
            "cube_lease_id",
            "cube_workspace_id",
            "worker_id",
            "started_at",
            "finished_at",
            "failure_kind",
            "before_commit_sha",
            "revision_task_id",
        ] {
            assert!(
                !obj.contains_key(absent),
                "expected {absent} omitted on encode",
            );
        }
        let back: CiRemediation = serde_json::from_value(encoded).unwrap();
        assert_eq!(attempt, back);
    }

    #[test]
    fn github_auth_state_dto_disconnected_roundtrips() {
        let state = GitHubAuthStateDto::Disconnected;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "disconnected");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_requesting_code_roundtrips() {
        let state = GitHubAuthStateDto::RequestingCode;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "requesting_code");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_pending_user_auth_roundtrips() {
        let state = GitHubAuthStateDto::PendingUserAuth {
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://github.com/login/device".into(),
            verification_uri_complete: Some(
                "https://github.com/login/device?user_code=WDJB-MJHT".into(),
            ),
            expires_at: 1_748_000_000,
            interval_seconds: 5,
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "pending_user_auth");
        assert_eq!(raw["user_code"], "WDJB-MJHT");
        assert_eq!(raw["interval_seconds"], 5);
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_pending_user_auth_skips_none_complete_uri() {
        let state = GitHubAuthStateDto::PendingUserAuth {
            user_code: "WDJB-MJHT".into(),
            verification_uri: "https://github.com/login/device".into(),
            verification_uri_complete: None,
            expires_at: 1_748_000_000,
            interval_seconds: 5,
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert!(!raw.as_object().unwrap().contains_key("verification_uri_complete"));
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_authorized_roundtrips() {
        let state = GitHubAuthStateDto::Authorized {
            login: "octocat".into(),
            granted_scopes: vec!["repo".into(), "project".into()],
            org_state: OrgAuthState::Ok,
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "authorized");
        assert_eq!(raw["login"], "octocat");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_expired_roundtrips() {
        let state = GitHubAuthStateDto::Expired;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "expired");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_denied_roundtrips() {
        let state = GitHubAuthStateDto::Denied;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "denied");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_error_roundtrips() {
        let state = GitHubAuthStateDto::Error {
            message: "network error fetching device code".into(),
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "error");
        assert_eq!(raw["message"], "network error fetching device code");
        let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn org_auth_state_ok_roundtrips() {
        let state = OrgAuthState::Ok;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "ok");
        let back: OrgAuthState = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn org_auth_state_needs_org_approval_roundtrips() {
        let state = OrgAuthState::NeedsOrgApproval {
            request_url: "https://github.com/orgs/spinyfin/policies/applications".into(),
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "needs_org_approval");
        assert_eq!(
            raw["request_url"],
            "https://github.com/orgs/spinyfin/policies/applications"
        );
        let back: OrgAuthState = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn org_auth_state_needs_sso_roundtrips() {
        let state = OrgAuthState::NeedsSso {
            sso_url: "https://github.com/orgs/spinyfin/sso".into(),
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "needs_sso");
        assert_eq!(raw["sso_url"], "https://github.com/orgs/spinyfin/sso");
        let back: OrgAuthState = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn org_auth_state_unknown_roundtrips() {
        let state = OrgAuthState::Unknown;
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "unknown");
        let back: OrgAuthState = serde_json::from_value(raw).unwrap();
        assert_eq!(state, back);
    }

    #[test]
    fn github_auth_state_dto_authorized_with_org_states_roundtrips() {
        let states = vec![
            OrgAuthState::Ok,
            OrgAuthState::NeedsOrgApproval {
                request_url: "https://example.com/approve".into(),
            },
            OrgAuthState::NeedsSso {
                sso_url: "https://example.com/sso".into(),
            },
            OrgAuthState::Unknown,
        ];
        for org_state in states {
            let auth = GitHubAuthStateDto::Authorized {
                login: "user".into(),
                granted_scopes: vec!["repo".into()],
                org_state: org_state.clone(),
            };
            let raw = serde_json::to_value(&auth).unwrap();
            let back: GitHubAuthStateDto = serde_json::from_value(raw).unwrap();
            assert_eq!(auth, back);
        }
    }

    #[test]
    fn automation_trigger_schedule_roundtrips() {
        let trigger = AutomationTrigger::Schedule {
            cron: "0 14 * * 1-5".to_owned(),
            timezone: "America/Los_Angeles".to_owned(),
        };
        let encoded = serde_json::to_value(&trigger).unwrap();
        assert_eq!(encoded["kind"], "schedule");
        assert_eq!(encoded["cron"], "0 14 * * 1-5");
        assert_eq!(encoded["timezone"], "America/Los_Angeles");
        let back: AutomationTrigger = serde_json::from_value(encoded).unwrap();
        assert_eq!(trigger, back);
    }

    #[test]
    fn automation_roundtrips() {
        let trigger = AutomationTrigger::Schedule {
            cron: "0 2 * * *".to_owned(),
            timezone: "UTC".to_owned(),
        };
        let automation = Automation::builder()
            .id("auto_1")
            .product_id("prod_1")
            .name("Nightly lint")
            .trigger(trigger)
            .standing_instruction("Fix clippy warnings if any exist")
            .created_at("1700000000")
            .updated_at("1700000000")
            .build();
        assert_eq!(automation.open_task_limit, 1);
        assert!(automation.enabled);
        assert_eq!(automation.created_via, CREATED_VIA_UNKNOWN);
        let encoded = serde_json::to_value(&automation).unwrap();
        let back: Automation = serde_json::from_value(encoded).unwrap();
        assert_eq!(automation.id, back.id);
        assert_eq!(automation.open_task_limit, back.open_task_limit);
    }

    #[test]
    fn automation_run_roundtrips() {
        let run = AutomationRun::builder()
            .id("run_1")
            .automation_id("auto_1")
            .scheduled_for("1700000000")
            .started_at("1700000001")
            .outcome("skipped")
            .detail("no clippy warnings found")
            .build();
        let encoded = serde_json::to_value(&run).unwrap();
        let back: AutomationRun = serde_json::from_value(encoded).unwrap();
        assert_eq!(run.id, back.id);
        assert_eq!(run.outcome, back.outcome);
        assert_eq!(run.detail, back.detail);
        assert!(back.produced_task_id.is_none());
    }

    #[test]
    fn task_source_automation_id_defaults_to_none() {
        let raw = json!({
            "id": "task_1",
            "product_id": "prod_1",
            "project_id": null,
            "kind": "chore",
            "name": "Fix lint",
            "description": "",
            "status": "todo",
            "ordinal": null,
            "pr_url": null,
            "deleted_at": null,
            "created_at": "1700000000",
            "updated_at": "1700000000",
        });
        let task: Task = serde_json::from_value(raw).unwrap();
        assert!(task.source_automation_id.is_none());
    }

    #[test]
    fn task_source_automation_id_roundtrips() {
        let raw = json!({
            "id": "task_1",
            "product_id": "prod_1",
            "project_id": null,
            "kind": "chore",
            "name": "Fix lint",
            "description": "",
            "status": "todo",
            "ordinal": null,
            "pr_url": null,
            "deleted_at": null,
            "created_at": "1700000000",
            "updated_at": "1700000000",
            "source_automation_id": "auto_1",
        });
        let task: Task = serde_json::from_value(raw).unwrap();
        assert_eq!(task.source_automation_id.as_deref(), Some("auto_1"));
        let encoded = serde_json::to_value(&task).unwrap();
        assert_eq!(encoded["source_automation_id"], "auto_1");
    }

    #[test]
    fn is_known_created_via_recognises_engine_trigger_prefixes() {
        // Exact-match values
        assert!(is_known_created_via(CREATED_VIA_CLI));
        assert!(is_known_created_via(CREATED_VIA_ENGINE_AUTO));
        assert!(is_known_created_via(CREATED_VIA_UNKNOWN));

        // Prefix-based values — engine-triggered revisions
        assert!(is_known_created_via("merge-conflict:crz_abc123"));
        assert!(is_known_created_via("ci-fix:crm_def456"));
        // Pre-existing prefix used by Source B
        assert!(is_known_created_via("pr-comment:owner/repo#42:comment_id"));

        // Unknown values still return false
        assert!(!is_known_created_via("something_undocumented"));
        assert!(!is_known_created_via(""));
    }

    // ── TaskKind / ExecutionKind round-trip tests ────────────────────────────

    #[test]
    fn task_kind_display_and_parse_are_inverses() {
        let all = [
            TaskKind::Chore,
            TaskKind::Design,
            TaskKind::Investigation,
            TaskKind::ProjectTask,
            TaskKind::Revision,
            TaskKind::Task,
        ];
        for kind in &all {
            let s = kind.to_string();
            let back: TaskKind = s.parse().unwrap_or_else(|e| {
                panic!("TaskKind::from_str({s:?}) failed: {e}")
            });
            assert_eq!(*kind, back, "round-trip failed for {kind:?}");
        }
    }

    #[test]
    fn task_kind_serde_uses_wire_strings() {
        assert_eq!(serde_json::to_string(&TaskKind::Chore).unwrap(), r#""chore""#);
        assert_eq!(serde_json::to_string(&TaskKind::Design).unwrap(), r#""design""#);
        assert_eq!(serde_json::to_string(&TaskKind::Investigation).unwrap(), r#""investigation""#);
        assert_eq!(serde_json::to_string(&TaskKind::ProjectTask).unwrap(), r#""project_task""#);
        assert_eq!(serde_json::to_string(&TaskKind::Revision).unwrap(), r#""revision""#);
        assert_eq!(serde_json::to_string(&TaskKind::Task).unwrap(), r#""task""#);

        let chore: TaskKind = serde_json::from_str(r#""chore""#).unwrap();
        assert_eq!(chore, TaskKind::Chore);
        let project_task: TaskKind = serde_json::from_str(r#""project_task""#).unwrap();
        assert_eq!(project_task, TaskKind::ProjectTask);
    }

    #[test]
    fn execution_kind_display_and_parse_are_inverses() {
        let all = [
            ExecutionKind::AutomationTriage,
            ExecutionKind::ChoreImplementation,
            ExecutionKind::CiRemediation,
            ExecutionKind::ConflictResolution,
            ExecutionKind::InvestigationImplementation,
            ExecutionKind::PrReview,
            ExecutionKind::ProductDesign,
            ExecutionKind::ProjectDesign,
            ExecutionKind::RevisionImplementation,
            ExecutionKind::TaskImplementation,
        ];
        for kind in &all {
            let s = kind.to_string();
            let back: ExecutionKind = s.parse().unwrap_or_else(|e| {
                panic!("ExecutionKind::from_str({s:?}) failed: {e}")
            });
            assert_eq!(*kind, back, "round-trip failed for {kind:?}");
        }
    }

    #[test]
    fn execution_kind_serde_uses_wire_strings() {
        assert_eq!(
            serde_json::to_string(&ExecutionKind::ChoreImplementation).unwrap(),
            r#""chore_implementation""#
        );
        assert_eq!(
            serde_json::to_string(&ExecutionKind::RevisionImplementation).unwrap(),
            r#""revision_implementation""#
        );
        assert_eq!(
            serde_json::to_string(&ExecutionKind::InvestigationImplementation).unwrap(),
            r#""investigation_implementation""#
        );
        assert_eq!(
            serde_json::to_string(&ExecutionKind::ProjectDesign).unwrap(),
            r#""project_design""#
        );
        assert_eq!(
            serde_json::to_string(&ExecutionKind::PrReview).unwrap(),
            r#""pr_review""#
        );

        let task_impl: ExecutionKind = serde_json::from_str(r#""task_implementation""#).unwrap();
        assert_eq!(task_impl, ExecutionKind::TaskImplementation);
        let chore_impl: ExecutionKind = serde_json::from_str(r#""chore_implementation""#).unwrap();
        assert_eq!(chore_impl, ExecutionKind::ChoreImplementation);
    }

    #[test]
    fn execution_kind_constants_match_enum() {
        assert_eq!(
            EXECUTION_KIND_AUTOMATION_TRIAGE,
            ExecutionKind::AutomationTriage.as_str()
        );
        assert_eq!(
            EXECUTION_KIND_PR_REVIEW,
            ExecutionKind::PrReview.as_str()
        );
    }
}

