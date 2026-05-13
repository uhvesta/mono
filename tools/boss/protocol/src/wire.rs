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
