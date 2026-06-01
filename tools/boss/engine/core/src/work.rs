use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

/// How long sqlite will internally retry on `SQLITE_BUSY` before
/// surfacing the error to the caller. We funnel concurrent CLI writes
/// against the same `state.db` (multiple `boss chore bind-pr` etc.
/// landing in the engine in parallel) — without this the second writer
/// would fail with "database is locked" even though the first writer
/// finishes in microseconds. Five seconds is overkill for the in-engine
/// case (writes are tiny) but cheap when uncontended.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Sliding window for the merge-conflict churn-guard heuristic
/// (`merge-conflict-handling-in-review.md` Q6 / Phase 6 #16): the
/// 4th `conflict_resolutions` row for a given work item inside one
/// hour is created as `abandoned` instead of `pending`.
pub const CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window count at which the next attempt is pre-abandoned.
/// The first 3 attempts inside `CHURN_GUARD_WINDOW_SECS` go live; the
/// 4th trips the guard.
pub const CHURN_GUARD_THRESHOLD: i64 = 3;
/// `failure_reason` stamped on the pre-abandoned row.
pub const CHURN_GUARD_REASON: &str = "churn_threshold_exceeded";

/// Sliding window for the orphan-active redispatch churn guard: the
/// 4th orphan-redispatch for a given work item inside one hour is
/// skipped and a warning is logged instead.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_WINDOW_SECS: i64 = 60 * 60;
/// Trailing-window terminal-execution count at which the next
/// orphan-redispatch is skipped. The first 3 cycles inside the window
/// go live; the 4th trips the guard.
pub const ORPHAN_REDISPATCH_CHURN_GUARD_THRESHOLD: i64 = 3;

/// Attention-item `kind` raised when the engine stops auto-resuming a
/// worker because its Claude API error is non-retryable (permanent or
/// unrecognised). See [`crate::transient_recovery`].
pub const ATTENTION_KIND_RECOVERY_PERMANENT: &str = "worker_recovery_permanent_error";
/// Attention-item `kind` raised when the engine stops auto-resuming a
/// worker because the transient-error retry cap was reached.
pub const ATTENTION_KIND_RECOVERY_EXHAUSTED: &str = "worker_recovery_exhausted";

/// Sliding window for the duplicate-create guard: a non-deleted task/chore
/// in the same product with the same name created within this many seconds
/// of the attempted insert causes a `DuplicateTaskError` unless
/// `force_duplicate` is set on the input.
pub const DUPLICATE_GUARD_WINDOW_SECS: i64 = 60;

/// Phase 12 #40 — sliding window for the CI-retry churn guard. The
/// engine counts `ci_remediations` created in the last
/// `CI_CHURN_WINDOW_SECS` for the work item; once the count crosses
/// [`CI_CHURN_LIMIT`], the next manual `boss engine ci retry`
/// invocation is rate-limited and requires `--force` to proceed.
pub const CI_CHURN_WINDOW_SECS: i64 = 60 * 60;
/// Threshold for [`CI_CHURN_WINDOW_SECS`]. Set well above the default
/// 3-attempt budget so the natural failure → fix → green cycle never
/// triggers; the only realistic path to 5 within an hour is repeated
/// manual retries against a flaky / fundamentally-broken PR.
pub const CI_CHURN_LIMIT: i64 = 5;

pub use boss_protocol::{
    AddDependencyInput, Attention, AttentionGroup, Automation, AutomationPatch, AutomationRun,
    AutomationTrigger, BlockedSignal, BranchNaming, COMMENT_STATUS_ACTIVE,
    COMMENT_STATUS_DISMISSED, COMMENT_STATUS_DISPATCHED, COMMENT_STATUS_ORPHANED,
    COMMENT_STATUS_RESOLVED, CREATED_VIA_ATTENTION, CREATED_VIA_CI_FIX_PREFIX,
    CREATED_VIA_ENGINE_AUTO, CREATED_VIA_MERGE_CONFLICT_PREFIX, CREATED_VIA_UNKNOWN,
    CiBudgetSnapshot, CiRemediation, CommentAnchor, CommentResolution,
    ConflictResolution, CreateAttentionInput, CreateAttentionItemInput, CreateAutomationInput,
    CreateChoreInput, CreateCommentInput, CreateExecutionInput, CreateManyChoresInput,
    CreateManyTasksInput, CreateProductInput, CreateProjectInput, CreateRevisionInput,
    CreateRunInput, CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter,
    EditorialRules, EffortLevel, EngineAttemptListEntry, ExecutionReconcileResult,
    ListDependenciesInput, MagicWandDispatch, MAGIC_WAND_STATUS_APPLIED,
    MAGIC_WAND_STATUS_CHORE_CREATED, MAGIC_WAND_STATUS_CONFLICT, MAGIC_WAND_STATUS_DISCARDED,
    MAGIC_WAND_STATUS_FAILED, MAGIC_WAND_STATUS_IN_FLIGHT, MAGIC_WAND_STATUS_RETURNED,
    PrWorkItemMatch, Product, Project,
    ProjectDesignDocState, RESOLVED_WITH_EXACT, RESOLVED_WITH_FUZZY, RESOLVED_WITH_ORPHAN,
    RemoveDependencyInput, RequestExecutionInput, ResolveProjectDesignDocOutput, ResolvedComment,
    ResolvedDesignDoc, ResolvedDesignDocKind, SetProjectDesignDocInput, Task, TaskRuntime,
    WorkAttentionItem, WorkComment, WorkExecution, WorkItem, WorkItemDependency,
    WorkItemDependencyDetail, WorkItemDependencyView, WorkItemExternalRef, WorkItemPatch, WorkRun,
    WorkTree, is_known_created_via,
};

/// Outcome of `WorkDb::record_pre_start_failure`. The coordinator uses
/// this to decide whether to schedule a delayed kick (retry) or surface
/// a permanent failure to the operator.
#[derive(Debug, Clone, PartialEq)]
pub enum PreStartFailureOutcome {
    /// The execution has been reset to `ready` with a `dispatch_not_before`
    /// delay. The coordinator should kick the scheduler after `delay`.
    Retry { delay: Duration },
    /// All retry attempts exhausted. The execution is now `failed`.
    /// The coordinator should surface an attention item.
    PermanentFail,
}

/// Returned by `insert_task_in_tx` / `insert_chore_in_tx` when the
/// duplicate guard fires. Carried as an `anyhow::Error` so `app.rs` can
/// downcast and send a structured `WorkItemDuplicateBlocked` event.
#[derive(Debug)]
pub struct DuplicateTaskError {
    pub existing_id: String,
    pub existing_short_id: i64,
    pub name: String,
    pub age_secs: i64,
}

impl std::fmt::Display for DuplicateTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "A task/chore named {:?} was created {} seconds ago (id: {}, short_id: T{}); \
             pass --force-duplicate to create another",
            self.name, self.age_secs, self.existing_id, self.existing_short_id,
        )
    }
}

impl std::error::Error for DuplicateTaskError {}

/// One row demoted by [`WorkDb::heal_ghost_active_chores`]. Carries the
/// owning `product_id` so the caller can publish a `work_item_changed`
/// invalidation on the product's topic — the kanban view subscribes on
/// product topics, not task ids, so the topic must be the product to
/// reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealedGhostActive {
    pub work_item_id: String,
    pub product_id: String,
}

use crate::work_dependencies::{self as deps, EdgeInsertOutcome, RELATION_BLOCKS};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MEM_DB_ID: AtomicU64 = AtomicU64::new(1);

/// Keeps a named shared-cache in-memory SQLite database alive. `Connection`
/// is `Send` but not `Sync`; wrapping in `Mutex` makes the anchor `Sync`.
/// `Arc` lets `WorkDb::clone` share the anchor across copies of the same
/// in-memory database (needed by the concurrent-insert test).
#[derive(Clone)]
struct InMemoryAnchor {
    uri: String,
    _conn: Arc<Mutex<Connection>>,
}

pub struct WorkDb {
    path: PathBuf,
    /// Present only when the database is in-memory (path == ":memory:").
    memory: Option<InMemoryAnchor>,
}

impl Clone for WorkDb {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            memory: self.memory.clone(),
        }
    }
}

// ---- module tree (see PR description for the split rationale) ----
mod attentions;
mod audit_misc;
mod automations;
mod blocking;
mod chain_helpers;
mod comments;
mod conflict_res;
mod create_entities;
mod dep_helpers;
mod dispatch;
mod dispatch_helpers;
mod exec_status_helpers;
mod exec_tail;
mod executions_runs;
mod insert_helpers;
mod mappers;
mod metrics_db;
mod metrics_types;
mod migrations_a;
mod migrations_b;
mod output_types;
mod pr_flow;
mod pr_state;
mod products_design;
mod query_ensure;
mod revision_helpers;
mod schema_init;
#[cfg(test)]
mod tests;
mod updates;
mod workitems;

pub(crate) use audit_misc::*;
pub(crate) use chain_helpers::*;
pub(crate) use dep_helpers::*;
pub(crate) use dispatch_helpers::*;
pub(crate) use exec_status_helpers::*;
pub(crate) use insert_helpers::*;
pub(crate) use mappers::*;
pub(crate) use migrations_a::*;
pub(crate) use migrations_b::*;
pub(crate) use query_ensure::*;
pub(crate) use revision_helpers::*;

pub use attentions::ActionedAttentionGroup;
pub use audit_misc::AUDIT_ACTOR_DESIGN_DETECTOR;
pub use audit_misc::AUDIT_ACTOR_HUMAN;
pub use automations::AutomationFireRecord;
pub use audit_misc::ProjectPropertyAuditEntry;
pub use audit_misc::canonicalize_repo_remote_url;
pub use audit_misc::canonicalize_worker_branch_prefix;
pub use audit_misc::short_name_for;
pub use mappers::CiInFlightObservation;
pub use mappers::CiRemediationInsertInput;
pub use mappers::ConflictResolutionInsertInput;
pub use metrics_types::MetricsCounterRow;
pub use metrics_types::MetricsGaugeRow;
pub use output_types::LatePrCandidate;
pub use output_types::PendingMergeCheck;
pub use output_types::SetRunTranscriptPathOutcome;
pub use output_types::StoredExternalRef;
pub use output_types::StrandedCiRemediationAttempt;
pub use output_types::WorkerPrCompletion;
pub use output_types::WorkerPrCompletionTarget;
#[cfg(test)]
pub use pr_state::FakePrStateChecker;
pub use pr_state::GhPrStateChecker;
pub use pr_state::PrOpenState;
pub use pr_state::PrStateChecker;
pub use pr_state::RevisionGateError;
pub use pr_state::StaticPrStateChecker;
pub use revision_helpers::normalize_priority;
