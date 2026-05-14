use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub repo_remote_url: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// Per-product default model slug used when a task/chore on this
    /// product has no `model_override` set. `None` → fall through to
    /// the effort-level default / engine default (per the design's Q3
    /// precedence). Stored verbatim — the engine does not validate the
    /// slug, so a future Claude release can ship without a Boss
    /// migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Optional preamble prepended to every worker's initial context
    /// at spawn time, visibly bracketed so humans reading transcripts
    /// know what was injected and by whom. `None` / empty → today's
    /// behaviour (no injection). Intended for per-product runtime
    /// guidance such as test-runner preferences that workers should
    /// see on every spawn rather than only when they read AGENTS.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub priority: String,
    pub created_at: String,
    pub updated_at: String,
    /// Who made the most recent status change. Three values:
    /// - `'human'` (default) — a CLI / app caller with no registered
    ///   Boss-session ancestry, or a drag-drop gesture in the macOS app.
    /// - `'boss'` — the caller's process ancestry traces back to the
    ///   registered Boss-coordinator session pid (the libghostty pane
    ///   where Claude Code runs as coordinator).
    /// - `'engine'` — the engine wrote the status itself (dependency
    ///   auto-block/unblock, merge poller, CI watch, etc.).
    /// The auto-unblock path only flips a `blocked` row back to `todo`
    /// when this is `'engine'` — manual and Boss-driven blocks stick.
    #[serde(default = "default_human_actor")]
    pub last_status_actor: String,
    /// Repo URL the project's design doc lives in. `None` → inherit
    /// from the project's product (`products.repo_remote_url`). Set
    /// explicitly when the doc lives in a different repo (the
    /// separate-doc-repo case at work).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// Per-product short id allocated at insert time. Always `Some` after the
    /// schema migration runs; `None` only on rows predating it (which the
    /// migration backfills, so in practice this is never `None` at runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<i64>,
    pub product_id: String,
    pub project_id: Option<String>,
    pub kind: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub ordinal: Option<i64>,
    pub pr_url: Option<String>,
    pub deleted_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// When `false`, the engine's auto-dispatcher will not turn this
    /// work item into a `ready` execution while it sits in `todo`.
    /// Existing rows from before this column was introduced default
    /// to `true` so legacy callers keep their old auto-start behavior.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Who made the most recent status change — `'human'`, `'boss'`,
    /// or `'engine'`. See `Project.last_status_actor` for full semantics.
    #[serde(default = "default_human_actor")]
    pub last_status_actor: String,
    /// One of `low` / `medium` / `high`. Mirrors `Project.priority`
    /// exactly so kanban surfaces can render every work-item kind with
    /// the same vocabulary. Existing rows from before this column was
    /// introduced default to `medium`.
    #[serde(default = "default_priority")]
    pub priority: String,
    /// The surface that filed this row — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`, or `unknown`. Stamped at insert time and never
    /// rewritten. `unknown` only appears on rows that predate this
    /// column (the migration default); fresh writes always carry one
    /// of the other values.
    #[serde(default = "default_unknown_created_via")]
    pub created_via: String,
    /// Per-work-item repo override. `None` → inherit from the parent
    /// `Product.repo_remote_url`. Stored as a canonical remote URL
    /// (e.g. `git@github.com:myorg/repo.git` or
    /// `https://github.com/myorg/repo.git`); short-name display is
    /// derived on the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
    /// When `status = 'blocked'`, an open-ended discriminator
    /// explaining *why*. Documented values: `'dependency'` (gated by a
    /// `work_item_dependencies` prereq), `'merge_conflict'` (an
    /// `in_review` PR's branch conflicts with `main`), `'review_feedback'`
    /// (a reviewer requested changes), `'ci_failure'` / `'ci_failure_exhausted'`
    /// (CI on the PR went red). `None` for non-`blocked` rows and for
    /// legacy `blocked` rows whose reason wasn't tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// Soft FK to the attempt row currently trying to clear the block —
    /// `conflict_resolutions.id` when `blocked_reason = 'merge_conflict'`,
    /// the review-iteration table's id when `blocked_reason = 'review_feedback'`,
    /// etc. `None` for `'dependency'` (the prereqs are queried via
    /// `work_item_dependencies` instead) and for any block without an
    /// engine-managed attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_attempt_id: Option<String>,
    /// Effort estimate for the work item. `None` means "no level set;
    /// dispatcher falls through to product / engine default per design
    /// §Q3." Set by the coordinator's heuristic at creation, or by an
    /// explicit `--effort` flag on `boss task/chore create|edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,
    /// Explicit model slug override. `None` → resolve via the design's
    /// Q3 precedence (effort default → product default → engine default).
    /// Stored verbatim — the engine does not validate the slug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Per-PR override of the CI auto-fix attempt budget. `None` →
    /// inherit the product default (`products.ci_attempt_budget`,
    /// default 3). `Some(0)` means "notify only" (no auto-fix on this
    /// PR). See `merge-conflict-handling-in-review.md` §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_attempt_budget: Option<i64>,
    /// Number of CI fix attempts the engine has already consumed for
    /// the current cycle. Reset to 0 when the parent transitions back
    /// to `in_review` after a successful auto-fix (or when the user
    /// runs `boss engine ci retry`). Only `attempt_kind = 'fix'`
    /// attempts that progressed past the worker's go/no-go decision
    /// count. Existing rows from before this column was introduced
    /// default to 0.
    #[serde(default)]
    pub ci_attempts_used: i64,
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
    pub blocked_signals: Vec<BlockedSignal>,
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
pub const CREATED_VIA_UNKNOWN: &str = "unknown";

/// Documented `created_via` values. The engine canonicalises caller-
/// supplied strings against this set; values outside it are stored
/// as-is but logged so we can spot undocumented sources sneaking in.
pub const KNOWN_CREATED_VIA: &[&str] = &[
    CREATED_VIA_CLI,
    CREATED_VIA_BOSSCTL,
    CREATED_VIA_MAC_APP,
    CREATED_VIA_ENGINE_AUTO,
    CREATED_VIA_UNKNOWN,
];

/// `true` when `value` is one of the documented `created_via` strings.
/// Engine writes for unknown values still go through, but a warning is
/// logged at the insert site.
pub fn is_known_created_via(value: &str) -> bool {
    KNOWN_CREATED_VIA.contains(&value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkExecution {
    pub id: String,
    pub work_item_id: String,
    pub kind: String,
    pub status: String,
    pub repo_remote_url: String,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub workspace_path: Option<String>,
    #[serde(default)]
    pub priority: i64,
    pub preferred_workspace_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkRun {
    pub id: String,
    pub execution_id: String,
    pub agent_id: String,
    pub status: String,
    pub error_text: Option<String>,
    pub result_summary: Option<String>,
    pub transcript_path: Option<String>,
    pub artifacts_path: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkAttentionItem {
    pub id: String,
    /// The execution this item attaches to, when the failure has a
    /// concrete execution row (e.g. a worker run failed mid-flight).
    /// `None` when the item attaches to a work item directly because
    /// no execution row exists yet — the `repo_unresolved` flow per
    /// `multi-repo-work-modeling.md` Q5 is the load-bearing case.
    #[serde(default)]
    pub execution_id: Option<String>,
    /// The work item this item attaches to when there is no execution
    /// row (sticky, pre-dispatch failures). Mutually exclusive with
    /// `execution_id` — exactly one of the two is `Some`.
    #[serde(default)]
    pub work_item_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub body_markdown: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
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
    pub original_level: EffortLevel,
    pub new_level: EffortLevel,
    pub markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub created_at: String,
}

/// Per-marker analysis row in the effort-audit report. One entry
/// per marker in the §Q4 corpus that matched at least one chore in
/// the product (markers with zero matches are filtered out so the
/// table stays scannable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EffortAuditMarkerRow {
    /// Marker string from the §Q4 corpus, e.g. `"rename"`,
    /// `"investigate"`, `"engine"`, lowercased.
    pub marker: String,
    /// Heuristic level the marker maps to per §Q4 (`trivial` for
    /// mechanical-edit markers, `medium` for multi-subsystem hints,
    /// `large` for investigate-family markers).
    pub original_level: EffortLevel,
    /// Total chores (kind = `chore`) on the product whose title or
    /// description matched this marker, regardless of whether they
    /// escalated.
    pub matches: u32,
    /// Of those, the count that subsequently raised an
    /// `[effort-escalation]` marker promoting the row to a higher
    /// level (per [`EffortLevel`]'s natural ordering trivial < small
    /// < medium < large < max).
    pub escalations: u32,
    /// `escalations / matches` as a 0.0-1.0 fraction. `0.0` when
    /// `matches > 0 && escalations == 0`; absent (per
    /// [`Option`]'s `None`) when `matches == 0` so callers don't
    /// have to special-case divide-by-zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub under_class_rate: Option<f64>,
    /// Human-readable callout produced when the rate / volume cross
    /// the thresholds named in `engine/src/effort.rs`. Empty when
    /// the marker is neither "consider promoting" nor "marker
    /// holds."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
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

/// Output shape for `boss product audit-effort <product>`. One
/// snapshot of the marker corpus's under-classification rates
/// against the recorded escalation events for a single product.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EffortAuditReport {
    pub product_id: String,
    pub product_slug: String,
    /// Window cap in days applied to escalation events
    /// (`created_at` after now - window). `None` means "no window;
    /// include all recorded escalations."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_days: Option<u32>,
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
    /// Per-marker analysis, sorted by `under_class_rate`
    /// descending so the noisy markers are visible first. Markers
    /// with zero matches are filtered out.
    pub rows: Vec<EffortAuditMarkerRow>,
    /// Epoch seconds when the audit was generated, for the
    /// human-readable header.
    pub generated_at: String,
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
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha_at_trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Structured JSON output of the pre-spawn diagnosis collector.
    /// Wire-encoded as a string so the engine can roll the schema
    /// forward without bumping this type; consumers parse on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_diagnosis: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
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
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_at: Option<String>,
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
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub head_sha_at_trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha_after: Option<String>,
    pub attempt_kind: String,
    pub consumes_budget: i64,
    /// JSON-encoded list of failing-check snapshots, one entry per
    /// failed required check at trigger time. Wire-encoded as a
    /// string so the engine can roll the schema forward without
    /// bumping this type; consumers parse on demand.
    pub failed_checks: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_lease_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cube_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionReconcileResult {
    pub created: Vec<WorkExecution>,
    pub updated: Vec<WorkExecution>,
}

/// Live runtime status for a single task/chore — the current execution
/// and most recent run, summarized for the kanban view. `None` fields
/// mean no execution (or no run) exists yet for the work item.
///
/// `execution_id` is the active or most recent execution row; the
/// engine uses the same value as `run_id` when registering live
/// worker state, so UI consumers can join `task → execution_id →
/// LiveWorkerState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRuntime {
    pub work_item_id: String,
    pub execution_status: Option<String>,
    pub run_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTree {
    pub product: Product,
    pub projects: Vec<Project>,
    pub tasks: Vec<Task>,
    pub chores: Vec<Task>,
    #[serde(default)]
    pub task_runtimes: Vec<TaskRuntime>,
    /// Every `work_item_dependencies` edge whose dependent belongs to
    /// this product. Lets the kanban resolve "blocked by <prereq>"
    /// labels (and any future dep affordance) without an N+1 round
    /// trip — clients already have every task/chore/project name.
    #[serde(default)]
    pub dependencies: Vec<WorkItemDependency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "item_type", rename_all = "snake_case")]
pub enum WorkItem {
    Product(Product),
    Project(Project),
    Task(Task),
    Chore(Task),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProductInput {
    pub name: String,
    pub description: Option<String>,
    pub repo_remote_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
    pub goal: Option<String>,
    /// Project creation auto-creates a `kind = 'design'` task as the
    /// first row under the project so the design phase shows up on
    /// the kanban like any other task. With `autostart = false` that
    /// design task is created in `todo` but the engine will NOT
    /// dispatch a worker against it until something explicitly
    /// schedules it (CLI `work start`, kanban drag-to-Doing, etc.).
    /// Mirrors the chore/task `autostart` semantics — same gate,
    /// applied at the moment the design task is born.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// When `true`, skip creation of the auto-generated `kind=design`
    /// seed task entirely. The project is filed alone with zero child
    /// tasks. Useful for non-design-shaped projects (postmortems,
    /// milestone aggregators, checklists) where the seed task is dead
    /// weight. Defaults to `false` to preserve existing behaviour.
    #[serde(default)]
    pub no_design_task: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskInput {
    pub product_id: String,
    pub project_id: String,
    pub name: String,
    pub description: Option<String>,
    /// See `CreateChoreInput::autostart`. Project tasks honour the
    /// same flag, but the kanban already serialises them via
    /// `waiting_dependency` so only the first incomplete task is ever
    /// `ready`. Defaults to `true`.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`), which is the right answer for the vast majority
    /// of tasks; only callers who care should set this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Surface that filed this task — `cli`, `bossctl`, `mac_app`,
    /// `engine_auto`. Documented callers always set it explicitly;
    /// when omitted, the engine falls back to a transport-layer hint
    /// so the row is never silently labeled `unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,
    /// Per-work-item repo override. `None` → the task inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
    /// Effort estimate. `None` → leave NULL on the row; dispatcher
    /// falls through to product / engine default per design §Q3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,
    /// Explicit model slug override. `None` → no override; dispatcher
    /// resolves per design §Q3 precedence. Stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Bypass the recent-duplicate guard. When `true`, the engine skips
    /// the 60-second same-name/same-product duplicate check and inserts
    /// a second row unconditionally. Intended as a CLI escape hatch for
    /// operators who explicitly want a second task with the same name.
    #[serde(default)]
    pub force_duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateChoreInput {
    pub product_id: String,
    pub name: String,
    pub description: Option<String>,
    /// When `false`, the engine creates the chore in `todo` but does
    /// NOT spin up a `ready` execution for the auto-dispatcher to pick
    /// up. The chore stays parked until something explicitly schedules
    /// it (`bossctl work start <id>` or a kanban drag-to-Doing). Older
    /// clients that omit this field get the historical behavior
    /// (`autostart = true`).
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// One of `low` / `medium` / `high`. Omitted → engine default
    /// (`medium`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// See `CreateTaskInput::created_via`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_via: Option<String>,
    /// Per-work-item repo override. `None` → the chore inherits from
    /// its product. Canonical remote URL form (engine canonicalises
    /// caller-supplied URLs at write time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
    /// See [`CreateTaskInput::effort_level`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<EffortLevel>,
    /// See [`CreateTaskInput::model_override`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// See [`CreateTaskInput::force_duplicate`].
    #[serde(default)]
    pub force_duplicate: bool,
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

/// Batch counterpart of [`CreateChoreInput`]. See
/// [`CreateManyTasksInput`] for atomicity / event semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateManyChoresInput {
    pub items: Vec<CreateChoreInput>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateExecutionInput {
    pub work_item_id: String,
    pub kind: String,
    pub status: Option<String>,
    pub repo_remote_url: Option<String>,
    pub cube_repo_id: Option<String>,
    pub cube_lease_id: Option<String>,
    pub cube_workspace_id: Option<String>,
    pub workspace_path: Option<String>,
    pub priority: Option<i64>,
    pub preferred_workspace_id: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestExecutionInput {
    pub work_item_id: String,
    pub priority: Option<i64>,
    pub preferred_workspace_id: Option<String>,
    /// Skip the dispatcher's pool-cap deferral. With `force = false`
    /// (the default), `RequestExecution` is the soft "queue this and
    /// dispatch when a slot frees up" verb. With `force = true`
    /// (`bossctl agents launch`), the engine grows the worker pool by
    /// one slot — bounded by the hard cap `MAX_WORKER_POOL_SIZE` — so
    /// the work item starts immediately even when every configured
    /// slot is busy.
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateRunInput {
    pub execution_id: String,
    pub agent_id: String,
    pub status: Option<String>,
    pub error_text: Option<String>,
    pub result_summary: Option<String>,
    pub transcript_path: Option<String>,
    pub artifacts_path: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateAttentionItemInput {
    /// The execution this item attaches to. `Some` for the common
    /// execution-scoped case; `None` together with `work_item_id =
    /// Some(...)` for sticky pre-dispatch items like `repo_unresolved`.
    #[serde(default)]
    pub execution_id: Option<String>,
    /// The work item this item attaches to when no execution row
    /// exists. Mutually exclusive with `execution_id` — the engine
    /// rejects inputs where both are set or both are missing.
    #[serde(default)]
    pub work_item_id: Option<String>,
    pub kind: String,
    pub status: Option<String>,
    pub title: String,
    pub body_markdown: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub goal: Option<String>,
    pub priority: Option<String>,
    pub repo_remote_url: Option<String>,
    pub pr_url: Option<String>,
    pub ordinal: Option<i64>,
    /// Effort estimate to apply on this update. `None` → leave the
    /// existing column value alone. `Some("")` → clear the column
    /// (write NULL). Any other string is validated against the
    /// [`EffortLevel`] enum at the engine boundary; invalid values
    /// reject the entire patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<String>,
    /// Model slug override. `None` → leave unchanged. `Some("")` →
    /// clear the column. Any other string is stored verbatim (no
    /// validation — `claude` is the source of truth on slugs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    /// Product-level default model. Only honoured on
    /// product-targeted updates; ignored when patching a task/chore/
    /// project. `None` → leave unchanged. `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Product-level dispatch preamble. Only honoured on
    /// product-targeted updates. `None` → leave unchanged.
    /// `Some("")` → clear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_preamble: Option<String>,
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
}

/// One row of the `work_item_dependencies` table — an edge from a
/// dependent to a prerequisite. `relation` is `"blocks"` for v1; the
/// column exists so future relation types (`"relates-to"`,
/// `"duplicates"`, …) can ship without a re-migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkItemDependency {
    pub dependent_id: String,
    pub prerequisite_id: String,
    #[serde(default = "default_relation")]
    pub relation: String,
    pub created_at: String,
}

pub fn default_relation() -> String {
    "blocks".to_owned()
}

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveDependencyInput {
    pub dependent: String,
    pub prerequisite: String,
    #[serde(default)]
    pub relation: Option<String>,
}

/// Direction of a dependency listing — incoming (prereqs of the
/// named row), outgoing (dependents), or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDirection {
    Prereqs,
    Dependents,
    Both,
}

impl Default for DependencyDirection {
    fn default() -> Self {
        Self::Both
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListDependenciesInput {
    /// Selector or id of the work item to list edges for.
    pub work_item: String,
    #[serde(default)]
    pub direction: Option<DependencyDirection>,
}

/// Two parallel edge lists for one work item — incoming (rows that
/// gate me) and outgoing (rows that I gate). Returned by
/// `ListDependencies` and embedded in `boss <kind> show`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyView {
    pub work_item_id: String,
    pub prerequisites: Vec<WorkItemDependency>,
    pub dependents: Vec<WorkItemDependency>,
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
    pub relation: String,
    pub kind: String,
    pub name: String,
    pub status: String,
}

/// Resolved dependency listing for a single work item. Each side
/// carries [`DependencyEdge`] entries with the peer's status and
/// name already joined in. Used by `boss <kind> show` and (in time)
/// the macOS dep section. Distinct from [`WorkItemDependencyView`]
/// because that one returns raw edge rows for the depend-list verb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkItemDependencyDetail {
    pub work_item_id: String,
    pub prerequisites: Vec<DependencyEdge>,
    pub dependents: Vec<DependencyEdge>,
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
    /// `None` means "inherit from `product.repo_remote_url`" (the
    /// in-repo case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_repo_remote_url: Option<String>,
    /// `None` means "inherit from `product.docs_branch`, falling back
    /// to `"main"`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_branch: Option<String>,
    /// Repo-relative path. Setting `Some("")` is rejected by the
    /// engine (use `unset = true` to clear). `None` leaves the
    /// existing path unchanged unless `unset` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_doc_path: Option<String>,
    /// When `true`, clear the pointer entirely (all three columns set
    /// to NULL). Takes precedence over any explicit field values.
    #[serde(default)]
    pub unset: bool,
}

/// Result of resolving a project's design-doc pointer. Carries the
/// concrete `(repo, branch, path)` triple plus a discriminator that
/// tells the open affordance which fast path it can take.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedDesignDoc {
    pub repo_remote_url: String,
    pub branch: String,
    pub path: String,
    pub kind: ResolvedDesignDocKind,
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
        /// to the GitHub web URL. Boolean form is `workspace_path.is_some()`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_path: Option<String>,
        /// `https://github.com/<owner>/<repo>/blob/<branch>/<path>`,
        /// pre-built so consumers don't have to re-parse the repo
        /// URL.
        web_url: String,
    },
    /// The pointer is set but cannot be resolved (e.g. path with no
    /// repo to resolve against). The UI surfaces a warning glyph
    /// linking to the set-design-doc form.
    Broken { reason: String },
}

/// Output of the `ResolveProjectDesignDoc` RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveProjectDesignDocOutput {
    pub project_id: String,
    pub state: ProjectDesignDocState,
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

    #[test]
    fn product_decodes_without_default_model() {
        let raw = json!({
            "id": "prod_1",
            "name": "Boss",
            "slug": "boss",
            "description": "",
            "repo_remote_url": Value::Null,
            "status": "active",
            "created_at": "1747000000",
            "updated_at": "1747000000",
        });
        let product: Product = serde_json::from_value(raw).unwrap();
        assert!(product.default_model.is_none());
    }

    #[test]
    fn product_roundtrips_with_default_model() {
        let raw = json!({
            "id": "prod_1",
            "name": "Boss",
            "slug": "boss",
            "description": "",
            "repo_remote_url": Value::Null,
            "status": "active",
            "created_at": "1747000000",
            "updated_at": "1747000000",
            "default_model": "sonnet",
        });
        let product: Product = serde_json::from_value(raw).unwrap();
        assert_eq!(product.default_model.as_deref(), Some("sonnet"));
        let encoded = serde_json::to_value(&product).unwrap();
        assert_eq!(encoded["default_model"], Value::String("sonnet".into()));
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
        };
        let raw = serde_json::to_value(&attempt).unwrap();
        let back: CiRemediation = serde_json::from_value(raw).unwrap();
        assert_eq!(attempt, back);
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
        ] {
            assert!(
                !obj.contains_key(absent),
                "expected {absent} omitted on encode",
            );
        }
        let back: CiRemediation = serde_json::from_value(encoded).unwrap();
        assert_eq!(attempt, back);
    }
}
