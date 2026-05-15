use serde::{Deserialize, Serialize};

use crate::engine_app::{EngineToAppRequest, EngineToAppResponse};
use crate::live_worker_state::LiveWorkerState;
use crate::types::{
    AddDependencyInput, ConflictResolution, CreateAttentionItemInput, CreateChoreInput,
    CreateExecutionInput, CreateManyChoresInput, CreateManyTasksInput, CreateProductInput,
    CreateProjectInput, CreateRunInput, CreateTaskInput, DependencyFilter, ListDependenciesInput,
    Product, Project, RemoveDependencyInput, RequestExecutionInput, ResolveProjectDesignDocOutput,
    SetProjectDesignDocInput, Task, TaskRuntime, WorkAttentionItem, WorkExecution, WorkItem,
    WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView, WorkItemPatch, WorkRun,
};

pub const TOPIC_WORK_PRODUCTS: &str = "work.products";

pub fn work_product_topic(product_id: &str) -> String {
    format!("work.product.{product_id}")
}

pub fn execution_topic(execution_id: &str) -> String {
    format!("executions.{execution_id}")
}

/// Per-run topic that carries probe lifecycle pushes for `run_id`.
/// Subscribers (e.g. a `bossctl probe` invocation that wants to wait
/// for the worker's reply) join this topic on the run they care about
/// and observe [`FrontendEvent::ProbeReplied`] when the engine pops a
/// queued probe and watches the next Stop boundary land.
pub fn probe_topic(run_id: &str) -> String {
    format!("probes.{run_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRequestEnvelope {
    pub request_id: String,
    pub payload: FrontendRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendEventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub payload: FrontendEvent,
}

impl FrontendEventEnvelope {
    pub fn response(request_id: impl Into<String>, payload: FrontendEvent) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: None,
            payload,
        }
    }

    pub fn push(payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: None,
            payload,
        }
    }

    pub fn response_with_revision(
        request_id: impl Into<String>,
        revision: u64,
        payload: FrontendEvent,
    ) -> Self {
        Self {
            request_id: Some(request_id.into()),
            revision: Some(revision),
            payload,
        }
    }

    pub fn push_with_revision(revision: u64, payload: FrontendEvent) -> Self {
        Self {
            request_id: None,
            revision: Some(revision),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendRequest {
    Subscribe {
        topics: Vec<String>,
    },
    Unsubscribe {
        topics: Vec<String>,
    },
    CreateProduct {
        #[serde(flatten)]
        input: CreateProductInput,
    },
    ListProducts,
    ListProjects {
        product_id: String,
        /// Phase 3 dep filter (Q6). Restricts the returned list to
        /// rows that match a dependency-graph predicate before any
        /// CLI-side filters (status / match / id). Backwards-
        /// compatible: pre-Phase-3 callers omit the field and get the
        /// historical behaviour.
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
    },
    ListTasks {
        product_id: String,
        project_id: Option<String>,
        /// Phase 3 dep filter (Q6). See [`Self::ListProjects`].
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
    },
    ListChores {
        product_id: String,
        /// Phase 3 dep filter (Q6). See [`Self::ListProjects`].
        #[serde(default)]
        dep_filter: Option<DependencyFilter>,
    },
    GetWorkItem {
        id: String,
    },
    /// Look up a work item by its per-product short_id (the friendly
    /// numeric id, e.g. 42 for `#42`). Searches both `tasks` and
    /// `projects` tables. Replies with `WorkItemResult` on success or
    /// `WorkError` when no match exists.
    GetWorkItemByShortId {
        product_id: String,
        short_id: i64,
    },
    CreateProject {
        #[serde(flatten)]
        input: CreateProjectInput,
    },
    CreateTask {
        #[serde(flatten)]
        input: CreateTaskInput,
    },
    CreateChore {
        #[serde(flatten)]
        input: CreateChoreInput,
    },
    /// Batch create N tasks in one engine round-trip. Atomic: the
    /// whole batch is wrapped in a single sqlite transaction and
    /// rolled back on the first per-item failure. Replies with
    /// `WorkItemsCreated` carrying the full list of inserted rows.
    CreateManyTasks {
        #[serde(flatten)]
        input: CreateManyTasksInput,
    },
    /// Batch create N chores in one engine round-trip. See
    /// `CreateManyTasks` for atomicity semantics.
    CreateManyChores {
        #[serde(flatten)]
        input: CreateManyChoresInput,
    },
    UpdateWorkItem {
        id: String,
        patch: WorkItemPatch,
    },
    DeleteWorkItem {
        id: String,
    },
    GetWorkTree {
        product_id: String,
    },
    ReorderProjectTasks {
        project_id: String,
        task_ids: Vec<String>,
    },
    CreateExecution {
        #[serde(flatten)]
        input: CreateExecutionInput,
    },
    RequestExecution {
        #[serde(flatten)]
        input: RequestExecutionInput,
    },
    ListExecutions {
        work_item_id: Option<String>,
    },
    GetExecution {
        id: String,
    },
    CreateRun {
        #[serde(flatten)]
        input: CreateRunInput,
    },
    ListRuns {
        execution_id: String,
    },
    GetRun {
        id: String,
    },
    CreateAttentionItem {
        #[serde(flatten)]
        input: CreateAttentionItemInput,
    },
    ListAttentionItems {
        execution_id: String,
    },
    GetAttentionItem {
        id: String,
    },
    /// App self-identifies as the singleton app session. The engine
    /// rejects this unless `LOCAL_PEERPID` matches the app's pid (the
    /// engine's parent). After registration, `EngineRequest` events
    /// flow to this session only.
    RegisterAppSession,
    /// App tells the engine which pid is the Boss session's shell.
    /// Used to populate the second trust root for Boss-only RPCs.
    /// Only the registered app session may call this.
    RegisterBossSession {
        shell_pid: i32,
    },
    /// App's reply to a previous `FrontendEvent::EngineRequest`.
    /// `request_id` echoes the value the engine sent.
    EngineResponse {
        request_id: String,
        response: EngineToAppResponse,
    },
    /// Boss-tier RPC: queue a probe prompt for `run_id`. By default
    /// the engine holds the text until the next `Stop` hook event for
    /// that run, then writes it into the worker's pty as if it were
    /// typed by the user. When `urgent` is `true`, the engine delivers
    /// the probe at the next `PostToolUse` boundary instead — after the
    /// current tool call finishes (so no in-flight Bash is cancelled)
    /// but before the worker starts its next tool call. Urgent probes
    /// are pushed to the front of the per-run queue so they always
    /// land before any queued non-urgent probes. Returns immediately
    /// with a `ProbeQueued` event carrying the engine-minted `probe_id`;
    /// the worker's reply is surfaced asynchronously via
    /// [`FrontendEvent::ProbeReplied`] on the [`probe_topic`] for
    /// `run_id`. Urgent probes are prefixed with `[coordinator-nudge]`
    /// in the transcript so the worker and human readers can identify
    /// coordinator-injected text.
    ProbeRun {
        run_id: String,
        text: String,
        /// When `true`, deliver at the next tool-call boundary
        /// (PostToolUse) rather than the next Stop boundary. The
        /// engine waits for any in-flight tool call to return before
        /// injecting, so no work is discarded. Omit or set to `false`
        /// for the original queue-for-Stop behaviour.
        #[serde(default)]
        urgent: bool,
    },
    /// Boss-tier RPC: tear down the libghostty pane hosting `run_id`
    /// and release the cube workspace its execution still holds.
    /// Used by `bossctl agents stop`. Idempotent — duplicate requests
    /// (or one racing with completion-detection) collapse to a no-op
    /// on the second pass.
    StopRun {
        run_id: String,
    },
    /// Boss-tier RPC: bring the worker pane hosting `run_id` to the
    /// front in the macOS app. Resolves `run_id → slot_id` via the
    /// engine's worker registry and forwards a `FocusWorkerPane`
    /// engine→app request. Used by `bossctl agents focus`. Returns a
    /// `WorkError` if the run is unknown or has no allocated pane.
    FocusWorkerPane {
        run_id: String,
    },
    /// Boss-tier RPC: write `text` into the worker pane hosting
    /// `run_id` as if the user typed it. Resolves `run_id → slot_id`
    /// via the worker registry and forwards a `SendToPane` engine→app
    /// request, which the app routes through the same libghostty
    /// surface a real keystroke takes. Used by `bossctl agents send`.
    /// Returns `WorkError` if the run is unknown, has no allocated
    /// pane, or the app rejects the injection.
    SendInputToWorker {
        run_id: String,
        text: String,
    },
    /// Boss-tier RPC: interrupt the worker pane hosting `run_id` —
    /// equivalent to the human pressing Esc inside that pane.
    /// Resolves `run_id → slot_id` and forwards an
    /// `InterruptWorkerPane` engine→app request. Cancels the worker's
    /// in-flight turn without killing the run. Used by `bossctl
    /// agents interrupt`. Returns a `WorkError` if the run is unknown
    /// or has no allocated pane.
    InterruptWorkerPane {
        run_id: String,
    },
    /// Snapshot of every allocated worker slot's live state — what
    /// model it's running, what activity (working / waiting / idle /
    /// errored / terminated), most recent tool, etc. Source of truth
    /// for the kanban Doing-icon and the per-pane titlebar pill.
    /// Subscribers can also listen on the `worker.live_states` topic
    /// for push updates whenever any slot's state changes.
    ListWorkerLiveStates,
    /// Cancel a queued or running execution. Marks the execution row
    /// `cancelled`, releases any cube workspace lease it still holds,
    /// and tears down the libghostty pane (if one was allocated).
    /// Idempotent on already-terminal rows (returns `WorkError`).
    CancelExecution {
        execution_id: String,
    },
    /// Boss-only RPC: mark the execution backing `run_id` as the
    /// terminal `orphaned` status and preserve its cube workspace
    /// lease so a fresh execution can resume against the same branch.
    /// Used by `bossctl agents reap` for orphans that the engine
    /// startup heuristics missed — e.g. when the cube lease is still
    /// within its TTL because the previous app crash was recent.
    /// Returns `WorkError` if the run id is unknown or already
    /// terminal.
    ReapRun {
        run_id: String,
    },
    /// Tail the most recent transcript chunk for `run_id`. `run_id`
    /// may be either an `exec_*` (execution) or `run_*` (work_runs)
    /// id — `bossctl agents transcript` passes the execution id (the
    /// alias the live registry uses); programmatic callers may pass
    /// the work_runs id.
    ///
    /// The engine resolves the transcript path via the dispatcher's
    /// in-memory cache, falling back to either DB namespace, and
    /// returns the trailing `lines` lines (raw JSONL — the caller
    /// decides how to render).
    ///
    /// Error shapes (all `WorkError`, distinguishable by message
    /// prefix so callers can branch):
    /// - `transcript not yet available for run <id>: …` — the run is
    ///   known and live, but no hook has carried a `transcript_path`
    ///   yet. Transient; retry in a few seconds. (Use this prefix to
    ///   distinguish a still-buffering live worker from a genuinely
    ///   unknown id — pre-fix the engine reported both as `unknown
    ///   run`, which masked the live-vs-stale distinction.)
    /// - `run <id> has no transcript path recorded` — the run/execution
    ///   is known but terminal and never persisted a transcript path.
    /// - `unknown run: <id>` — no live entry, no DB row matches.
    TailRunTranscript {
        run_id: String,
        lines: usize,
    },
    /// Snapshot the cube workspace pool. Proxies to
    /// `cube --json workspace list`; the engine adds no editorial — the
    /// returned vector mirrors cube's view, optionally annotated with
    /// the engine's own knowledge of which leases back which executions.
    WorkspacePoolSummary,
    /// Declare a `blocks` edge from `dependent` to `prerequisite`.
    /// Idempotent: re-adding an existing edge is a no-op. Cycles are
    /// rejected at the engine before insert.
    AddDependency {
        #[serde(flatten)]
        input: AddDependencyInput,
    },
    /// Drop the `(dependent, prerequisite, relation)` edge. No-op if
    /// the edge does not exist (mirrors `boss <kind> delete` on an
    /// already-archived row).
    RemoveDependency {
        #[serde(flatten)]
        input: RemoveDependencyInput,
    },
    /// Return the prerequisite and/or dependent edges for one work
    /// item. `direction` defaults to `both`.
    ListDependencies {
        #[serde(flatten)]
        input: ListDependenciesInput,
    },
    /// Resolved counterpart of [`Self::ListDependencies`]: returns
    /// the same incoming / outgoing split, but each entry carries the
    /// peer's status and name already joined in. Used by `boss
    /// <kind> show` so the human / JSON renderer needs one round-trip
    /// instead of N+1.
    ListDependenciesDetailed {
        #[serde(flatten)]
        input: ListDependenciesInput,
    },
    /// Per-slot toggle for the live-status summarizer. When
    /// `enabled = false`, the engine stops calling the summarizer for
    /// `slot_id` and clears any existing `live_status`; the UI falls
    /// back to the static pane_summary. Persisted in the engine
    /// metadata table so the choice survives engine restarts.
    /// Idempotent — toggling to the current state is a benign no-op.
    SetLiveStatusEnabled {
        slot_id: u8,
        enabled: bool,
    },
    /// Snapshot of which slots currently have the live-status
    /// summarizer disabled. The UI uses this to render the toggle
    /// state on the Agents-tab worker row.
    ListLiveStatusDisabledSlots,
    /// One-shot diagnostic snapshot of the live-status pipeline.
    /// Returns the engine build SHA, ANTHROPIC_API_KEY presence, and
    /// per-slot detail covering trigger / outcome / transcript path —
    /// see [`crate::LiveStatusDebugReport`]. Wired through to
    /// `bossctl live-status debug`. Read-only; no side effects.
    DebugLiveStatusPipeline,
    /// Set (or clear) a project's design-doc pointer. Persists the
    /// three `projects.design_doc_*` columns per
    /// [`SetProjectDesignDocInput`]'s semantics and replies with the
    /// updated `Project` row wrapped in a `WorkItemUpdated` event —
    /// same shape `UpdateWorkItem` returns for any other property
    /// edit, so existing kanban subscribers refresh without special
    /// casing. Publishes a `work_invalidated` topic event on the
    /// project's product so other connected clients see the change.
    SetProjectDesignDoc {
        #[serde(flatten)]
        input: SetProjectDesignDocInput,
    },
    /// Read-only: resolve a project's design-doc pointer into the
    /// structured [`ResolveProjectDesignDocOutput`] the UI consumes.
    /// Engine-side this is `WorkDb::resolve_project_design_doc`
    /// composed with a cheap check against the engine's in-flight
    /// execution list to populate
    /// [`ProjectDesignDocState::Resolved::workspace_path`].
    /// No DB writes; no topic events.
    ResolveProjectDesignDoc { project_id: String },
    /// Worker-facing escape hatch for the merge-conflict resolution
    /// flow: flip a `conflict_resolutions` attempt to `failed` with a
    /// reason. The CLI surface is `boss engine conflicts mark-failed
    /// <attempt-id> --reason <r>` — workers call it when they hit one
    /// of the stop conditions (semantic obsolescence, product decision
    /// required, architectural mismatch) and decide not to push. See
    /// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`
    /// Q4 / Q11.
    MarkConflictResolutionFailed {
        attempt_id: String,
        reason: String,
    },
    /// Read-only: list `conflict_resolutions` rows. The CLI surface is
    /// `boss engine conflicts list` (design Phase 5 / #13). Filters are
    /// AND-ed; an empty `status` list matches every status. Ordering is
    /// `created_at DESC, id DESC` so the freshest attempt is first.
    ListConflictResolutions {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        product_id: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        status: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        work_item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// Read-only: fetch a single attempt row by id. Returns
    /// [`FrontendEvent::ConflictResolution`] on success and
    /// [`FrontendEvent::WorkError`] when the id is unknown.
    GetConflictResolution { attempt_id: String },
    /// Reset a terminal-failure attempt back to `pending` so the
    /// dispatcher re-spawns a worker. Only valid for rows whose status
    /// is `failed` or `abandoned`; calling on a non-terminal row
    /// (`pending` / `running`) is rejected. The parent work item is
    /// re-flipped to `blocked: merge_conflict` and the new
    /// `blocked_attempt_id` points at the reset row. See Phase 5 #13.
    RetryConflictResolution { attempt_id: String },
    /// Engine-side abandon: flip a non-terminal attempt to `abandoned`
    /// with the supplied reason. Distinct from `mark-failed` in that the
    /// caller is explicitly stepping away (PR closed, parent merged
    /// externally, manual override) rather than declaring the worker
    /// gave up. Idempotent; rows already terminal yield a WorkError.
    AbandonConflictResolution {
        attempt_id: String,
        reason: String,
    },
    /// Set (or clear) a product's `default_model` per the
    /// effort-and-model-estimation design (PR #370). `model` is a
    /// claude model slug stored verbatim; `None` clears the column.
    /// The engine does NOT validate the slug — claude is the source
    /// of truth on what `--model` accepts (design §Q3). Returns the
    /// updated product wrapped in `WorkItemUpdated`.
    SetProductDefaultModel {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Heuristic feedback-loop audit (design §Q4 follow-up, PR #370).
    /// Aggregates recorded escalation events for `product_id`
    /// against the §Q4 marker corpus and returns a snapshot report
    /// of under-classification rates per marker. Read-only; backs
    /// the `boss product audit-effort` CLI verb. `window_days`
    /// trims the event set to a rolling window (events older than
    /// `now - window` are excluded); `None` means "all recorded
    /// events." Replies with [`FrontendEvent::EffortAuditReport`].
    AuditProductEffort {
        product_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window_days: Option<u32>,
    },
    /// Append an effort-level escalation event (design §Q5, PR
    /// #370 follow-up). Wire surface used by the sibling
    /// escalation-handler task; this task ships the row format and
    /// the read path. Engine assigns `id` and `created_at`; the
    /// caller passes the row's original / new level and the §Q4
    /// markers the heuristic recorded against the row at creation.
    /// Replies with [`FrontendEvent::EffortEscalationRecorded`].
    RecordEffortEscalation {
        work_item_id: String,
        original_level: crate::EffortLevel,
        new_level: crate::EffortLevel,
        markers: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
    },
    /// Snapshot of every registered engine feature flag and its current
    /// value. Backs the macOS app's Feature Flags debug pane (incident
    /// 001 AI #5). Replies with [`FrontendEvent::FeatureFlagsList`].
    /// Read-only; no side effects.
    ListFeatureFlags,
    /// Toggle one feature flag on or off. The engine updates the
    /// in-memory map and rewrites the on-disk file atomically; the
    /// new value is visible to consumer-side `is_enabled` calls the
    /// moment this request returns. The reply
    /// ([`FrontendEvent::FeatureFlagSet`]) confirms the persisted
    /// state and is the round-trip "the engine has reloaded" signal
    /// the debug pane uses to render the toggle as committed.
    SetFeatureFlag { name: String, enabled: bool },
    /// Ask the engine to identify itself. Replies with
    /// [`FrontendEvent::EngineVersionResult`] carrying the build SHA,
    /// build time, and binary-content fingerprint of the running
    /// engine binary. Used by the macOS app on attach to detect
    /// whether the running engine matches the app's bundled engine; if
    /// they differ the app stops the old engine and spawns the new one
    /// from the bundle, ensuring the user always gets the version that
    /// shipped with the app they launched.
    GetEngineVersion,
    /// Snapshot of every registered per-installation setting and its
    /// current value. Used by the macOS Settings window to render the
    /// current state on open. Replies with
    /// [`FrontendEvent::SettingsList`]. Read-only; no side effects.
    GetSettings,
    /// Set one per-installation setting. The engine persists to
    /// `settings.toml` atomically; consumer-side reads see the new
    /// value the moment this returns. The reply
    /// ([`FrontendEvent::SettingSet`]) confirms the persisted value.
    SetSetting { key: String, enabled: bool },
    /// Read a single metric's current in-memory value, bypassing the
    /// 30s flush-staleness window. Used by `bossctl metrics show
    /// --live`. The engine replies with
    /// [`FrontendEvent::MetricsShowLiveResult`]; `entry` is `None`
    /// when no counter or gauge with `name` is registered.
    MetricsShowLive { name: String },
    /// Reset one or all counter / gauge values to zero — both
    /// in-memory and in `state.db` — in a single atomic step.
    /// `name = None` means "reset everything". Routes through engine
    /// RPC so the in-memory atomic and the database row are cleared in
    /// lockstep; a direct SQLite write would leave the atomic stale
    /// until the next flush. Replies with
    /// [`FrontendEvent::MetricsResetDone`].
    MetricsReset { name: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FrontendEvent {
    Hello {
        session_id: String,
    },
    Subscribed {
        topics: Vec<String>,
        current_revision: u64,
    },
    Unsubscribed {
        topics: Vec<String>,
    },
    TopicEvent {
        topic: String,
        revision: u64,
        origin_session_id: String,
        origin_request_id: Option<String>,
        event: TopicEventPayload,
    },
    ProductsList {
        products: Vec<Product>,
    },
    ProjectsList {
        product_id: String,
        projects: Vec<Project>,
    },
    TasksList {
        product_id: String,
        project_id: Option<String>,
        tasks: Vec<Task>,
    },
    ChoresList {
        product_id: String,
        chores: Vec<Task>,
    },
    WorkTree {
        product: Product,
        projects: Vec<Project>,
        tasks: Vec<Task>,
        chores: Vec<Task>,
        #[serde(default)]
        task_runtimes: Vec<TaskRuntime>,
        #[serde(default)]
        dependencies: Vec<WorkItemDependency>,
    },
    WorkItemResult {
        item: WorkItem,
    },
    WorkItemCreated {
        item: WorkItem,
    },
    /// Response to a batch create (`CreateManyTasks` /
    /// `CreateManyChores`). Carries every row inserted by the batch in
    /// the order the caller submitted them. Per-item subscribers can
    /// keep treating each entry as if it had arrived via a regular
    /// `WorkItemCreated` event — the engine also publishes the usual
    /// `work_invalidated` topic event covering the full id list, so
    /// kanban consumers reload once.
    WorkItemsCreated {
        items: Vec<WorkItem>,
    },
    WorkItemUpdated {
        item: WorkItem,
    },
    ProjectTasksReordered {
        project_id: String,
        task_ids: Vec<String>,
    },
    ExecutionsList {
        work_item_id: Option<String>,
        executions: Vec<WorkExecution>,
    },
    ExecutionResult {
        execution: WorkExecution,
    },
    ExecutionCreated {
        execution: WorkExecution,
    },
    ExecutionRequested {
        execution: WorkExecution,
    },
    RunsList {
        execution_id: String,
        runs: Vec<WorkRun>,
    },
    RunResult {
        run: WorkRun,
    },
    RunCreated {
        run: WorkRun,
    },
    AttentionItemsList {
        execution_id: String,
        items: Vec<WorkAttentionItem>,
    },
    AttentionItemResult {
        item: WorkAttentionItem,
    },
    AttentionItemCreated {
        item: WorkAttentionItem,
    },
    WorkItemDeleted {
        id: String,
    },
    WorkError {
        message: String,
    },
    /// Returned instead of `WorkError` when a create is rejected because
    /// a non-deleted task/chore in the same product has an identical name
    /// and was created within the last 60 seconds. Carries enough info for
    /// the CLI to display a helpful message and for `--json` consumers to
    /// act on the existing row. Pass `force_duplicate: true` in the input
    /// to bypass the guard and insert unconditionally.
    WorkItemDuplicateBlocked {
        /// Primary id of the existing row that triggered the guard.
        existing_id: String,
        /// Friendly short id of the existing row (e.g. `439`).
        existing_short_id: i64,
        /// The name that triggered the match.
        name: String,
        /// Seconds elapsed since the existing row was created.
        age_secs: i64,
    },
    Error {
        message: String,
    },
    /// Engine confirms the calling session is now the registered app
    /// session, and any prior registration was invalidated.
    AppSessionRegistered,
    /// Engine confirms the Boss session pid was registered.
    BossSessionRegistered,
    /// Engine confirms a probe was queued for the given run. The
    /// engine-minted `probe_id` lets callers correlate a queued probe
    /// with the eventual [`FrontendEvent::ProbeReplied`] push, which
    /// arrives on the [`probe_topic`] for `run_id` once the worker's
    /// follow-up Stop boundary lands. `urgent` echoes the flag from
    /// the originating [`FrontendRequest::ProbeRun`] call so the
    /// caller can confirm the delivery semantics that were accepted.
    ProbeQueued {
        run_id: String,
        probe_id: String,
        /// Echoes the `urgent` flag from the originating `ProbeRun`
        /// request. When `true`, the probe will be delivered at the
        /// next `PostToolUse` boundary rather than the next `Stop`.
        #[serde(default)]
        urgent: bool,
    },
    /// Push: the worker for `run_id` has replied to a previously
    /// dispatched probe. Emitted on the Stop boundary that follows
    /// the dispatch (so callers can correlate "probe goes in" with
    /// "next assistant turn comes out"). `text` is the assistant
    /// turn the engine extracted from the worker's transcript;
    /// `probe_id` matches the value [`FrontendEvent::ProbeQueued`]
    /// returned for the originating [`FrontendRequest::ProbeRun`]
    /// call. Pushed on the [`probe_topic`] for `run_id`.
    ProbeReplied {
        run_id: String,
        probe_id: String,
        text: String,
    },
    /// Engine acknowledges a stop request — the pane release has
    /// been kicked off and (if applicable) the cube workspace lease
    /// released. The reply does not wait for the libghostty pane to
    /// fully drain; teardown is asynchronous.
    RunStopped {
        run_id: String,
    },
    /// Engine acknowledges a focus request — the worker pane has
    /// been raised in the macOS app. Carries the resolved `slot_id`
    /// so the caller (e.g. `bossctl agents focus`) can confirm which
    /// slot was raised when the agent reference was a crew name or
    /// run id.
    WorkerPaneFocused {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges a `SendInputToWorker` request — the text
    /// has been written into the worker pane via the same surface a
    /// user-typed keystroke takes. Carries the resolved `slot_id` so
    /// the caller (e.g. `bossctl agents send`) can confirm which
    /// pane was targeted when the agent reference was a crew name
    /// or run id.
    WorkerInputSent {
        run_id: String,
        slot_id: u8,
    },
    /// Engine acknowledges an interrupt request — an Esc keystroke
    /// has been delivered to the worker pane's pty. Carries the
    /// resolved `slot_id` so the caller can confirm which slot was
    /// interrupted when the agent reference was a crew name or run
    /// id.
    WorkerPaneInterrupted {
        run_id: String,
        slot_id: u8,
    },
    /// Engine asks the registered app session to perform a pane
    /// operation. The app must reply with a
    /// [`FrontendRequest::EngineResponse`] carrying the same
    /// `request_id`.
    EngineRequest {
        request_id: String,
        request: EngineToAppRequest,
    },
    /// Snapshot of every allocated worker slot's live state. Used as
    /// both the response to [`FrontendRequest::ListWorkerLiveStates`]
    /// and the body of pushes on the `worker.live_states` topic. The
    /// list is the entire snapshot, not a delta — receivers can
    /// blindly replace their local map.
    WorkerLiveStatesList {
        states: Vec<LiveWorkerState>,
    },
    /// Engine confirms an execution has been cancelled. The cancelled
    /// row's status is now `cancelled`; resource teardown (pane
    /// release, cube workspace release) is asynchronous.
    ExecutionCancelled {
        execution: WorkExecution,
    },
    /// Engine confirms a manual orphan reap. The execution row is now
    /// in the terminal `orphaned` status; its cube workspace lease has
    /// intentionally been left intact so a fresh execution can resume
    /// against the same branch.
    RunReaped {
        run_id: String,
        execution: WorkExecution,
    },
    /// Trailing transcript chunk for a run. `lines` are the raw JSONL
    /// lines the engine read off the recorded transcript path
    /// (newest-last). `truncated` is set when the file had more lines
    /// than were returned.
    RunTranscriptTail {
        run_id: String,
        transcript_path: String,
        lines: Vec<String>,
        truncated: bool,
    },
    /// Snapshot of the cube workspace pool. The engine proxies
    /// `cube --json workspace list`; each entry corresponds to one
    /// workspace cube knows about, annotated (when the engine has
    /// matching state) with the execution id currently leasing it.
    WorkspacePoolSummaryResult {
        workspaces: Vec<WorkspacePoolEntry>,
    },
    /// Engine confirms a dependency edge has been added. Returns the
    /// row that was inserted (or the existing row if the call was an
    /// idempotent re-add).
    DependencyAdded {
        edge: WorkItemDependency,
    },
    /// Engine confirms a dependency edge has been removed (or that no
    /// matching edge existed to begin with — also a success).
    DependencyRemoved {
        dependent_id: String,
        prerequisite_id: String,
        relation: String,
        removed: bool,
    },
    /// Edge listing for a single work item, with prerequisites and
    /// dependents in two parallel lists.
    DependencyList {
        view: WorkItemDependencyView,
    },
    /// Resolved edge listing — same shape as
    /// [`Self::DependencyList`] but each side carries the peer's
    /// status and name already joined in.
    DependencyDetail {
        detail: WorkItemDependencyDetail,
    },
    /// Response to [`FrontendRequest::SetLiveStatusEnabled`]. Carries
    /// the resulting enabled flag for the slot so the caller can
    /// distinguish "applied" from "already in that state" if it
    /// wants.
    LiveStatusEnabledSet {
        slot_id: u8,
        enabled: bool,
    },
    /// Snapshot of which slots currently have the live-status
    /// summarizer disabled. The UI uses this to render the toggle
    /// state on the Agents-tab worker row.
    LiveStatusDisabledSlotsList {
        slot_ids: Vec<u8>,
    },
    /// One-shot diagnostic snapshot of the live-status pipeline, in
    /// response to [`FrontendRequest::DebugLiveStatusPipeline`]. The
    /// full shape is documented on [`crate::LiveStatusDebugReport`].
    LiveStatusDebugReportEvent {
        report: crate::LiveStatusDebugReport,
    },
    /// Response to [`FrontendRequest::ResolveProjectDesignDoc`]: the
    /// resolved pointer state for a single project. Carried inline
    /// (not flattened) so the kanban can deserialise straight into a
    /// `ResolveProjectDesignDocOutput` without going through the
    /// envelope.
    ProjectDesignDocResolved {
        output: ResolveProjectDesignDocOutput,
    },
    /// Response to
    /// [`FrontendRequest::MarkConflictResolutionFailed`]: the
    /// post-update `conflict_resolutions` row. Carries the full row
    /// so the CLI can pretty-print "attempt foo flipped to failed,
    /// reason bar" without a follow-up `get`.
    ConflictResolutionMarkedFailed {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::ListConflictResolutions`]: the
    /// filtered set of rows, ordered freshest-first.
    ConflictResolutionsList {
        attempts: Vec<ConflictResolution>,
    },
    /// Response to [`FrontendRequest::GetConflictResolution`]: a single
    /// row by id.
    ConflictResolution {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::RetryConflictResolution`]: the
    /// row after the reset to `pending`. The engine has already
    /// re-flipped the parent work item back to `blocked:
    /// merge_conflict` so the dispatcher can pick up the new attempt.
    ConflictResolutionRetried {
        attempt: ConflictResolution,
    },
    /// Response to [`FrontendRequest::AbandonConflictResolution`]: the
    /// row after the flip to `abandoned`.
    ConflictResolutionMarkedAbandoned {
        attempt: ConflictResolution,
    },
    /// Activity-feed push: a fresh conflict-resolution attempt has been
    /// created for an in-review PR and a worker is about to take over
    /// (Phase 4 / design Q8). Broadcast on the parent product's
    /// work-tree topic so the macOS app can render an activity-feed
    /// entry without having to poll.
    ConflictResolutionStarted {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: the engine observed the parent PR back at
    /// mergeable and retired the conflict-resolution attempt. The
    /// parent has been flipped from `blocked: merge_conflict` back to
    /// `in_review`; the attempt row is `succeeded`; the worker's cube
    /// workspace lease has been released (if not already).
    ConflictResolutionSucceeded {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: a conflict-resolution attempt terminated in
    /// `failed`. Emitted when the worker calls
    /// `boss engine conflicts mark-failed`, when the completion path's
    /// catch-all (`no_push_no_stop_condition`) fires, or any other
    /// terminal-failure transition. The parent remains `blocked:
    /// merge_conflict`; the user is the next actor.
    ConflictResolutionFailed {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: a conflict-resolution attempt terminated in
    /// `abandoned`. Distinct from `failed` in that the engine stepped
    /// away on purpose (PR closed, parent merged externally, manual
    /// override). The parent has typically already moved out of
    /// `blocked: merge_conflict` by some other path.
    ConflictResolutionAbandoned {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: a fresh CI-remediation attempt has been
    /// created for an in-review PR (design §"CI worker spawn",
    /// Phase 8 #22). `attempt_kind` is `"fix"` or `"retrigger"` —
    /// the engine's pre-spawn triage decision.
    CiRemediationStarted {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        attempt_kind: String,
    },
    /// Activity-feed push: the engine observed the parent PR back at
    /// CI clean and retired the remediation attempt. The parent has
    /// been flipped from `blocked: ci_failure` back to `in_review`;
    /// the attempt row is `succeeded`.
    CiRemediationSucceeded {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
    },
    /// Activity-feed push: a CI-remediation attempt terminated in
    /// `failed`. Emitted when the worker calls
    /// `boss engine ci mark-failed` or when the completion path's
    /// catch-all fires. The parent remains `blocked: ci_failure`.
    CiRemediationFailed {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: a CI-remediation attempt was abandoned
    /// (engine declined to spawn — opt-out, suppression, or
    /// budget-related path).
    CiRemediationAbandoned {
        product_id: String,
        work_item_id: String,
        attempt_id: String,
        pr_url: String,
        failure_reason: String,
    },
    /// Activity-feed push: the engine has given up auto-fixing this
    /// PR's CI. The parent is now `blocked: ci_failure_exhausted` and
    /// the user is the next actor — typically via
    /// `boss engine ci retry <work-item-id>`.
    CiRemediationExhausted {
        product_id: String,
        work_item_id: String,
        pr_url: String,
        attempts_used: i64,
        budget: i64,
    },
    /// Response to [`FrontendRequest::AuditProductEffort`]. Carries
    /// the per-marker under-classification analysis for one
    /// product. Read-only snapshot; the engine recomputes from
    /// scratch each call.
    EffortAuditReport {
        report: crate::EffortAuditReport,
    },
    /// Response to [`FrontendRequest::RecordEffortEscalation`].
    /// Carries the inserted row with engine-assigned `id` and
    /// `created_at`.
    EffortEscalationRecorded {
        event: crate::EffortEscalation,
    },
    /// Response to [`FrontendRequest::ListFeatureFlags`]: a snapshot
    /// of every registered engine feature flag plus its current
    /// effective value. Order is registry order so the debug pane
    /// renders flags in a stable, predictable sequence.
    FeatureFlagsList { flags: Vec<FeatureFlagSnapshot> },
    /// Response to [`FrontendRequest::SetFeatureFlag`]: the engine has
    /// updated and persisted the named flag to `enabled`. Receiving
    /// this event is the debug pane's "reload confirmed" signal — the
    /// new value is in effect immediately for all subsequent
    /// consumer-side checks.
    FeatureFlagSet { name: String, enabled: bool },
    /// Response to [`FrontendRequest::GetEngineVersion`]: identifies
    /// the running engine binary. `binary_fingerprint` is the most
    /// reliable signal — it is a truncated SHA-256 of the engine
    /// binary's on-disk bytes (same algorithm as
    /// `boss_engine::build_info::binary_fingerprint`). The macOS app
    /// computes the same hash for its bundled engine file and compares;
    /// a mismatch means the running engine pre-dates the current app
    /// bundle and should be replaced. `git_sha` and `build_time` are
    /// included for human-readable logging only; they may be "unknown"
    /// in dev builds.
    EngineVersionResult {
        git_sha: String,
        build_time: String,
        binary_fingerprint: String,
    },
    /// Response to [`FrontendRequest::GetSettings`]: a snapshot of every
    /// registered per-installation setting and its current value.
    SettingsList { settings: Vec<SettingSnapshot> },
    /// Response to [`FrontendRequest::SetSetting`]: the engine has
    /// persisted the new value. The macOS Settings window uses this as
    /// the "saved" signal to commit the toggle state.
    SettingSet { key: String, enabled: bool },
    /// Response to [`FrontendRequest::MetricsShowLive`]: the
    /// in-memory snapshot for `name`. `entry` is `None` when no
    /// counter or gauge with that name is registered in the current
    /// engine binary.
    MetricsShowLiveResult { entry: Option<MetricLiveEntry> },
    /// Response to [`FrontendRequest::MetricsReset`]. Reports how
    /// many counters and gauges were zeroed so the caller can print a
    /// meaningful confirmation. `name = None` means "all" was
    /// requested.
    MetricsResetDone {
        name: Option<String>,
        counters_reset: u64,
        gauges_reset: u64,
    },
}

/// Snapshot of one feature flag's static metadata + current value.
/// Mirrors `boss_engine::feature_flags::FeatureFlagSnapshot` so the
/// engine can ship its in-memory snapshot over the wire without
/// translating field-by-field at the call site.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeatureFlagSnapshot {
    /// Stable flag identifier (lowercase snake_case). Also the key
    /// the consumer passes to `is_enabled`.
    pub name: String,
    /// Human-readable one-sentence description rendered in the
    /// debug pane.
    pub description: String,
    /// Free-form grouping label used as a section header in the
    /// debug pane (e.g. `"completion"`).
    pub category: String,
    /// What the flag is when the on-disk file has no entry for it.
    /// Rendered as a "default: ON / OFF" hint next to the toggle so
    /// the human can tell what they would revert to.
    pub default_enabled: bool,
    /// Current effective value — what `is_enabled(name)` returns
    /// right now. Equals `default_enabled` when no override exists.
    pub enabled: bool,
}

/// Snapshot of one per-installation setting's static metadata + current
/// value. Wire type for [`FrontendEvent::SettingsList`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingSnapshot {
    /// Stable key (lowercase snake_case). The toggle send path uses
    /// this verbatim as the `key` in `SetSetting`.
    pub key: String,
    /// One-sentence description rendered in the Settings window.
    pub description: String,
    /// Registry default — rendered as a "default: ON/OFF" hint.
    pub default_enabled: bool,
    /// Current effective value.
    pub enabled: bool,
}

/// One row of the cube workspace pool, as exposed via
/// [`FrontendEvent::WorkspacePoolSummaryResult`]. Mirrors
/// `CubeWorkspaceStatus` plus an optional engine-side annotation
/// that maps a workspace's current lease to the execution holding it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspacePoolEntry {
    pub workspace_id: String,
    pub workspace_path: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at_epoch_s: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at_epoch_s: Option<i64>,
    /// The execution id whose row currently records this lease, if
    /// the engine knows about one. Null when cube reports the lease
    /// but the engine has no matching execution row (drift) or the
    /// workspace is idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TopicEventPayload {
    WorkInvalidated {
        reason: String,
        product_id: Option<String>,
        item_ids: Vec<String>,
    },
    ExecutionInvalidated {
        reason: String,
        execution_id: String,
        work_item_id: String,
        status: String,
    },
}

/// In-memory snapshot of one metric (counter or gauge), returned by
/// [`FrontendEvent::MetricsShowLiveResult`]. Values are read directly
/// from the engine's atomics so they are not subject to the 30s
/// flush-staleness window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricLiveEntry {
    pub name: String,
    pub description: String,
    /// `"counter"` or `"gauge"`.
    pub kind: String,
    /// Counter value cast to `i64` (same bit pattern — values above
    /// `i64::MAX` are theoretical). Gauge value is a signed `i64`.
    pub value: i64,
    /// Milliseconds since Unix epoch of the last increment (counter)
    /// or set (gauge). 0 means "never updated since registration".
    pub timestamp_ms: i64,
    /// True when this entry was rehydrated from `state.db` but no
    /// handle in the current binary matches its name.
    pub stale: bool,
}

#[cfg(test)]
mod feature_flags_wire_tests {
    use super::*;

    #[test]
    fn list_feature_flags_request_round_trips() {
        let original = FrontendRequest::ListFeatureFlags;
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("list_feature_flags"));
        let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, FrontendRequest::ListFeatureFlags));
    }

    #[test]
    fn set_feature_flag_request_round_trips() {
        let original = FrontendRequest::SetFeatureFlag {
            name: "detect_pr_cold_fallback".into(),
            enabled: false,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("set_feature_flag"));
        assert!(json.contains("detect_pr_cold_fallback"));
        let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendRequest::SetFeatureFlag { name, enabled } => {
                assert_eq!(name, "detect_pr_cold_fallback");
                assert!(!enabled);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn feature_flags_list_event_round_trips() {
        let snap = FeatureFlagSnapshot {
            name: "detect_pr_cold_fallback".into(),
            description: "test description".into(),
            category: "completion".into(),
            default_enabled: true,
            enabled: false,
        };
        let original = FrontendEvent::FeatureFlagsList {
            flags: vec![snap.clone()],
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("feature_flags_list"));
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendEvent::FeatureFlagsList { flags } => {
                assert_eq!(flags, vec![snap]);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn feature_flag_set_event_round_trips() {
        let original = FrontendEvent::FeatureFlagSet {
            name: "detect_pr_cold_fallback".into(),
            enabled: true,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("feature_flag_set"));
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendEvent::FeatureFlagSet { name, enabled } => {
                assert_eq!(name, "detect_pr_cold_fallback");
                assert!(enabled);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn metrics_show_live_request_round_trips() {
        let original = FrontendRequest::MetricsShowLive {
            name: "pr_url_capture.primary_path.hit".into(),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("metrics_show_live"), "serialized: {json}");
        assert!(json.contains("pr_url_capture.primary_path.hit"), "serialized: {json}");
        let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendRequest::MetricsShowLive { name } => {
                assert_eq!(name, "pr_url_capture.primary_path.hit");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn metrics_reset_request_round_trips_with_name() {
        let original = FrontendRequest::MetricsReset {
            name: Some("pr_url_capture.primary_path.hit".into()),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("metrics_reset"), "serialized: {json}");
        let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendRequest::MetricsReset { name } => {
                assert_eq!(name.as_deref(), Some("pr_url_capture.primary_path.hit"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn metrics_reset_request_round_trips_with_all() {
        let original = FrontendRequest::MetricsReset { name: None };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("metrics_reset"), "serialized: {json}");
        let parsed: FrontendRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendRequest::MetricsReset { name } => assert!(name.is_none()),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn metrics_show_live_result_event_round_trips() {
        let entry = MetricLiveEntry {
            name: "pr_url_capture.primary_path.hit".into(),
            description: "test desc".into(),
            kind: "counter".into(),
            value: 42,
            timestamp_ms: 1_700_000_000_000,
            stale: false,
        };
        let original = FrontendEvent::MetricsShowLiveResult { entry: Some(entry.clone()) };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("metrics_show_live_result"), "serialized: {json}");
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendEvent::MetricsShowLiveResult { entry: Some(e) } => {
                assert_eq!(e, entry);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn metrics_reset_done_event_round_trips() {
        let original = FrontendEvent::MetricsResetDone {
            name: Some("pr_url_capture.primary_path.hit".into()),
            counters_reset: 1,
            gauges_reset: 0,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("metrics_reset_done"), "serialized: {json}");
        let parsed: FrontendEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            FrontendEvent::MetricsResetDone { name, counters_reset, gauges_reset } => {
                assert_eq!(name.as_deref(), Some("pr_url_capture.primary_path.hit"));
                assert_eq!(counters_reset, 1);
                assert_eq!(gauges_reset, 0);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
