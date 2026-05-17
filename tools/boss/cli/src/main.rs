use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use boss_client::{
    BossClient, Discovery, engine_socket_reachable, ensure_engine_running, running_engine_pid,
    stop_engine,
};
use boss_protocol::{
    AddDependencyInput, CREATED_VIA_CLI, ConflictResolution, CreateChoreInput,
    CreateManyChoresInput, CreateManyTasksInput, CreateProductInput, CreateProjectInput,
    CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter, EffortAuditReport,
    EffortLevel, FrontendEvent, FrontendRequest, LinkExternalRefInput, ListDependenciesInput,
    Product, Project, ProjectDesignDocState, RemoveDependencyInput, ResolveProjectDesignDocOutput,
    ResolvedDesignDocKind, SetProductExternalTrackerInput, SetProjectDesignDocInput, Task,
    TaskRuntime, WorkExecution, WorkItem, WorkItemDependency, WorkItemDependencyDetail,
    WorkItemDependencyView, WorkItemPatch,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use serde::Serialize;

mod repo_resolution;

#[derive(Debug, Parser)]
#[command(name = "boss", about = "Boss work CLI")]
struct Cli {
    #[command(flatten)]
    global: GlobalFlags,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Args)]
struct GlobalFlags {
    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true)]
    quiet: bool,

    #[arg(long, global = true)]
    no_input: bool,

    /// Suppress autostart side effects.
    ///
    /// Three effects, all off-by-default:
    ///   1. The CLI will not transparently start the engine when
    ///      its socket is unreachable.
    ///   2. `boss task create` / `boss chore create` create the work
    ///      item but the engine will NOT auto-dispatch a worker for
    ///      it. The new chore/task stays in the `todo` column until
    ///      something explicitly schedules it (`bossctl work start
    ///      <id>` or a kanban drag-to-Doing).
    ///   3. `boss project create` still files the project AND its
    ///      auto-spawned `kind=design` seed task, but the seed task
    ///      is born with `autostart=false` so the engine does not
    ///      dispatch a worker against it. Use this to author the
    ///      design brief on the seed task (via `boss task update
    ///      <design-task-id> --description ...`) before releasing it
    ///      with `bossctl work start <design-task-id>`.
    #[arg(long, global = true)]
    no_autostart: bool,

    #[arg(long, global = true)]
    socket_path: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print authoritative Boss CLI reference documentation.
    Reference,
    Product {
        #[command(subcommand)]
        command: ProductCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Chore {
        #[command(subcommand)]
        command: ChoreCommand,
    },
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
    /// Remove an installed Boss.app bundle.
    ///
    /// By default removes ~/Applications/Boss.app and leaves the state
    /// directory (~/Library/Application Support/Boss/) intact. Pass
    /// --purge-state to also remove state (requires confirmation unless
    /// --yes is also provided).
    ///
    /// When BOSS_INSTALL_ROOT is set the uninstall operates on that
    /// install root instead of ~/Applications. In that case the engine
    /// stop is skipped — the caller is responsible for their own engine
    /// lifecycle (stopping the default pid file would kill the host
    /// engine instead of any sandbox engine).
    Uninstall(UninstallArgs),
}

#[derive(Debug, Subcommand)]
enum ProductCommand {
    Create(ProductCreateArgs),
    List,
    Show(ProductSelectorArg),
    Update(ProductUpdateArgs),
    /// Archive a product. Products are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProductSelectorArg),
    /// Move a product into a different lifecycle status (active/paused/archived).
    Move(ProductMoveArgs),
    /// Set (or clear) the product's default claude model slug. Used by
    /// the dispatcher (per the effort-and-model design's Q3
    /// precedence) when a task/chore on this product has no
    /// `model_override` set. Slug is stored verbatim — claude is the
    /// source of truth on which slugs resolve.
    #[command(name = "set-default-model")]
    SetDefaultModel(ProductSetDefaultModelArgs),
    /// Heuristic feedback-loop audit (design §Q4 follow-up, PR
    /// #370). Aggregates recorded effort-escalation events against
    /// the §Q4 marker corpus and prints a per-marker
    /// under-classification report. Read-only diagnostic — does
    /// not retune anything. Use to spot markers that workers
    /// commonly escalate past (candidates for promoting to a
    /// higher level in the §Q4 rules).
    #[command(name = "audit-effort")]
    AuditEffort(ProductAuditEffortArgs),
    /// Bind (or unbind) an external issue tracker on a product.
    ///
    /// Use `--kind github --org ORG --repo REPO --project N` to bind the
    /// product to a GitHub Projects board. The engine validates the config
    /// and stores the binding; the reconciler (once running) will begin
    /// syncing upstream issues as Boss chores.
    ///
    /// `--reverse-close` enables opt-in writeback: when a Boss work item
    /// under this product is marked done without a merged PR, Boss will
    /// explicitly close the upstream GitHub issue. Off by default.
    ///
    /// `--unset` removes any existing binding. All other flags are ignored.
    #[command(name = "set-external-tracker")]
    SetExternalTracker(ProductSetExternalTrackerArgs),
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Create(ProjectCreateArgs),
    List(ProjectListArgs),
    Show(ProjectShowArgs),
    Update(ProjectUpdateArgs),
    /// Archive a project. Projects are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProjectSelectorArgs),
    /// Move a project into a different lifecycle status
    /// (planned/active/blocked/done/archived).
    Move(ProjectMoveArgs),
    /// Set or clear a project's design-doc pointer. `--path` sets the
    /// repo-relative doc path; `--repo` and `--branch` are optional
    /// overrides that fall back to the product's defaults. `--unset`
    /// clears all three pointer columns.
    #[command(name = "set-design-doc")]
    SetDesignDoc(ProjectSetDesignDocArgs),
    /// Resolve a project's design-doc pointer and open it. Default
    /// behaviour: if the doc lives in the project's own product and a
    /// workspace is leased, open the file in `$EDITOR`; otherwise open
    /// the GitHub web URL. `--web` forces the web URL; `--print` just
    /// emits the resolved target without opening it.
    #[command(name = "open-design")]
    OpenDesign(ProjectOpenDesignArgs),
    /// Manage dependency edges (`A depends on B` ⇒ B gates A).
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
}

/// Subcommands under `boss task ...`.
///
/// The kind-agnostic verbs (`show`, `update`, `move`, `delete`,
/// `depend`, `bind-pr`) operate on any leaf work item by id. A chore
/// *is* a kind of task — the engine already knows the kind from the
/// id, so the noun is permissive. The same verbs are mirrored under
/// `boss chore ...` for back-compat and for callers who prefer to
/// name the kind explicitly.
///
/// Kind-specific verbs (`create`, `create-many`, `list`, `reorder`)
/// stay split because their inputs / filters genuinely differ by
/// kind (e.g. tasks have a project, chores don't; reorder is only
/// meaningful for project tasks).
#[derive(Debug, Subcommand)]
enum TaskCommand {
    Create(TaskCreateArgs),
    /// Bulk-create N tasks from a JSON array. Sidesteps the per-call
    /// CLI startup overhead of running `task create` N times — one
    /// invocation, one engine round-trip, atomic transaction. See
    /// `--help` for the input schema.
    #[command(name = "create-many")]
    CreateMany(TaskCreateManyArgs),
    List(TaskListArgs),
    /// Show any leaf work item (task or chore) by id.
    Show(TaskIdArg),
    /// Update any leaf work item (task or chore) by id.
    Update(TaskUpdateArgs),
    /// Move any leaf work item (task or chore) into a different status.
    Move(TaskMoveArgs),
    /// Delete any leaf work item (task or chore) by id.
    Delete(TaskDeleteArgs),
    Reorder(TaskReorderArgs),
    /// Manage dependency edges (`A depends on B` ⇒ B gates A).
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
    /// Attach a GitHub PR URL to an existing leaf work item (task or chore).
    ///
    /// Use this when the engine's auto-detection (worker stop hook
    /// or merge poller) didn't pick up a PR — for example, if the
    /// PR was opened before its work item existed, the work was
    /// started outside the worker spawn path, or a multi-phase task
    /// was split into per-phase tasks after the original PR was open.
    /// Idempotent: re-binding the same URL is a no-op. Re-binding to
    /// a different URL overwrites with a stderr warning. Status is
    /// not changed; move the item explicitly with `boss task move`
    /// if needed.
    #[command(name = "bind-pr")]
    BindPr(BindPrArgs),
    /// Manually link a work item to a specific upstream tracker issue.
    ///
    /// The engine stores `kind`/`id` on the row. The `raw` blob and
    /// `web_url` fields are populated on the next reconcile tick when
    /// the engine fetches the upstream item. Replaces an existing
    /// binding silently.
    #[command(name = "link-external")]
    LinkExternal(LinkExternalArgs),
    /// Remove the external-tracker binding from a work item.
    ///
    /// Clears the active binding flag (`external_ref_unbound_at` is
    /// set to now). The `kind`/`canonical_id` columns are retained so
    /// the reconciler can re-bind automatically if the upstream item
    /// reappears. Other fields are unaffected.
    #[command(name = "unlink-external")]
    UnlinkExternal(TaskIdArg),
}

/// Subcommands under `boss chore ...`. Kind-agnostic verbs here are
/// thin aliases for `boss task <verb>` — they accept any leaf work
/// item id and route through the same handlers. Kept for back-compat
/// and for callers who prefer to name the kind explicitly.
#[derive(Debug, Subcommand)]
enum ChoreCommand {
    Create(ChoreCreateArgs),
    /// Bulk-create N chores from a JSON array. See `boss task
    /// create-many --help` for the schema; chores omit `project_id`.
    #[command(name = "create-many")]
    CreateMany(ChoreCreateManyArgs),
    List(ChoreListArgs),
    /// Alias for `boss task show`. Accepts any leaf work item id.
    Show(TaskIdArg),
    /// Alias for `boss task update`. Accepts any leaf work item id.
    Update(TaskUpdateArgs),
    /// Alias for `boss task move`. Accepts any leaf work item id.
    Move(TaskMoveArgs),
    /// Alias for `boss task delete`. Accepts any leaf work item id.
    Delete(TaskDeleteArgs),
    /// Alias for `boss task depend`. The engine doesn't care about kind.
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
    /// Alias for `boss task bind-pr`. Accepts any leaf work item id.
    #[command(name = "bind-pr")]
    BindPr(BindPrArgs),
    /// Alias for `boss task link-external`. Accepts any leaf work item id.
    #[command(name = "link-external")]
    LinkExternal(LinkExternalArgs),
    /// Alias for `boss task unlink-external`. Accepts any leaf work item id.
    #[command(name = "unlink-external")]
    UnlinkExternal(TaskIdArg),
}

/// Shared subcommands for dependency CRUD. The engine doesn't care
/// about the parent kind — same verbs live under task / chore /
/// project so the CLI grammar stays consistent (`boss task ...`,
/// `boss chore ...`, `boss project ...`).
#[derive(Debug, Subcommand)]
enum DependCommand {
    /// Declare an edge: `dependent` becomes gated until `prerequisite`
    /// reaches a satisfied status.
    Add(DependAddArgs),
    /// Drop the named edge. No-op if the edge does not exist.
    Rm(DependRmArgs),
    /// List the prerequisites and/or dependents of a single work item.
    List(DependListArgs),
}

#[derive(Debug, Clone, Args)]
struct DependAddArgs {
    /// Id of the work item that becomes gated.
    dependent: String,
    /// Id of the work item that gates it.
    prerequisite: String,
    /// Edge type. Only `blocks` is supported in v1.
    #[arg(long, default_value = "blocks")]
    relation: String,
}

#[derive(Debug, Clone, Args)]
struct DependRmArgs {
    dependent: String,
    prerequisite: String,
    #[arg(long, default_value = "blocks")]
    relation: String,
}

#[derive(Debug, Clone, Args)]
struct DependListArgs {
    /// Id of the work item to inspect.
    selector: String,
    /// Which side(s) of the edge to return. Defaults to `both`.
    #[arg(long, value_enum, default_value_t = DependDirectionArg::Both)]
    direction: DependDirectionArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DependDirectionArg {
    Prereqs,
    Dependents,
    Both,
}

impl From<DependDirectionArg> for DependencyDirection {
    fn from(value: DependDirectionArg) -> Self {
        match value {
            DependDirectionArg::Prereqs => DependencyDirection::Prereqs,
            DependDirectionArg::Dependents => DependencyDirection::Dependents,
            DependDirectionArg::Both => DependencyDirection::Both,
        }
    }
}

#[derive(Debug, Args)]
struct UninstallArgs {
    /// Also remove ~/Library/Application Support/Boss/ (state.db,
    /// executions/, audit log). Requires interactive confirmation
    /// unless --yes is also passed.
    #[arg(long)]
    purge_state: bool,

    /// Skip interactive confirmation prompts.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    Status,
    Start,
    Stop,
    /// Inspect and manage the merge-conflict resolution attempt table
    /// (`conflict_resolutions`). Worker-facing surface for the in-review
    /// merge-conflict handling flow.
    Conflicts {
        #[command(subcommand)]
        command: EngineConflictsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum EngineConflictsCommand {
    /// List `conflict_resolutions` rows, freshest first. Filters are
    /// AND-ed; omit them all to see every attempt. Human output is a
    /// table; `--json` emits the full row vector.
    List(EngineConflictsListArgs),
    /// Show a single `conflict_resolutions` row by id. Carries every
    /// column the engine has for the attempt, including the structured
    /// `conflict_diagnosis` blob (verbatim) — useful when debugging
    /// what the worker was handed.
    Show(EngineConflictsShowArgs),
    /// Reset a `failed` or `abandoned` attempt back to `pending` so the
    /// engine re-dispatches a worker. Rejected for non-terminal rows
    /// (`pending` / `running`). The parent work item is re-flipped to
    /// `blocked: merge_conflict` as part of the reset.
    Retry(EngineConflictsRetryArgs),
    /// Mark a non-terminal attempt `abandoned`. Distinct from
    /// `mark-failed`: the caller is explicitly stepping away (PR closed,
    /// parent merged externally, manual override) rather than declaring
    /// the worker gave up.
    Abandon(EngineConflictsAbandonArgs),
    /// Flip a `conflict_resolutions` attempt to `failed` with a
    /// reason. Worker-facing escape hatch: the resolution worker calls
    /// this when it hits a stop condition (semantic obsolescence,
    /// product decision required, architectural mismatch) and chooses
    /// not to push.
    MarkFailed(EngineConflictsMarkFailedArgs),
}

#[derive(Debug, Clone, Args)]
struct EngineConflictsListArgs {
    /// Filter to a single product (id or slug). Omit for all products.
    #[arg(long)]
    product: Option<String>,

    /// Filter by status. Repeatable / comma-separated. Documented
    /// values: pending, running, succeeded, failed, abandoned,
    /// superseded.
    #[arg(long, value_delimiter = ',')]
    status: Vec<String>,

    /// Filter to a single parent work item id.
    #[arg(long = "work-item")]
    work_item: Option<String>,

    /// Cap the number of returned rows. Engine returns every match
    /// when omitted; the CLI default is 50 to keep human output
    /// readable.
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct EngineConflictsShowArgs {
    /// Attempt id from the `conflict_resolutions` table (e.g. `crz_…`).
    attempt_id: String,
}

#[derive(Debug, Clone, Args)]
struct EngineConflictsRetryArgs {
    /// Attempt id from the `conflict_resolutions` table.
    attempt_id: String,
}

#[derive(Debug, Clone, Args)]
struct EngineConflictsAbandonArgs {
    /// Attempt id from the `conflict_resolutions` table.
    attempt_id: String,

    /// Free-form reason stored verbatim in `failure_reason`.
    /// Default: `manual_abandon`.
    #[arg(long, default_value = "manual_abandon")]
    reason: String,
}

#[derive(Debug, Clone, Args)]
struct EngineConflictsMarkFailedArgs {
    /// Attempt id from the `conflict_resolutions` table (e.g.
    /// `crz_…`). The current attempt id is part of the worker's
    /// spawn prompt.
    attempt_id: String,

    /// Free-form failure reason. The design canonicalises three:
    /// `obsolescence_suspected`, `product_decision_required`,
    /// `architectural_mismatch`. Any string is accepted; the engine
    /// stores it verbatim on the attempt row.
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Clone, Args)]
struct ProductSelectorArg {
    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProductSetDefaultModelArgs {
    selector: String,

    /// Claude model slug to store as the product default (e.g.
    /// `opus`, `sonnet`, `haiku`, `claude-opus-4-7`). Stored verbatim
    /// — no validation against the engine. Mutually exclusive with
    /// `--unset`; one of the two is required.
    #[arg(long, value_name = "SLUG", conflicts_with = "unset")]
    model: Option<String>,

    /// Clear the product's `default_model` so the dispatcher falls
    /// through to the effort-level default (per design §Q3).
    /// Mutually exclusive with `--model`.
    #[arg(long)]
    unset: bool,
}

#[derive(Debug, Clone, Args)]
struct ProductAuditEffortArgs {
    /// Product id or slug to audit.
    selector: String,

    /// Restrict the report to escalation events recorded within
    /// the last N days. Default: all recorded events.
    #[arg(long, value_name = "DAYS")]
    window_days: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct ProductSetExternalTrackerArgs {
    /// Product id or slug to bind.
    selector: String,

    /// Tracker kind. Currently only `github` is supported.
    #[arg(long, value_name = "KIND", conflicts_with = "unset")]
    kind: Option<String>,

    /// GitHub organisation name (required when `--kind github`).
    #[arg(long, value_name = "ORG", conflicts_with = "unset")]
    org: Option<String>,

    /// GitHub repository name (required when `--kind github`).
    #[arg(long, value_name = "REPO", conflicts_with = "unset")]
    repo: Option<String>,

    /// GitHub project number (required when `--kind github`).
    #[arg(long, value_name = "N", conflicts_with = "unset")]
    project: Option<u64>,

    /// Opt in to reverse-close: when a Boss work item is marked done
    /// without a merged PR, Boss closes the upstream issue. Off by
    /// default. Only meaningful for `--kind github`.
    #[arg(long, conflicts_with = "unset")]
    reverse_close: bool,

    /// Remove the external-tracker binding from this product.
    /// Mutually exclusive with all other tracker flags.
    #[arg(long)]
    unset: bool,
}

#[derive(Debug, Clone, Args)]
struct ProjectSelectorArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProductScopedArgs {
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductCreateArgs {
    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductUpdateArgs {
    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    #[arg(long)]
    status: Option<ProductStatus>,

    /// Text prepended to every worker's initial context at spawn time,
    /// wrapped in visible `[product-preamble]…[/product-preamble]`
    /// markers. Pass `""` to clear an existing preamble.
    #[arg(long)]
    dispatch_preamble: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProjectCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,

    /// Skip the auto-generated `kind=design` seed task. Pass this for
    /// non-design-shaped projects (postmortems, checklists, milestone
    /// aggregators) where the seed task would be dead weight.
    /// Defaults to false (preserves existing behaviour).
    #[arg(long = "no-design-task", default_value_t = false)]
    no_design_task: bool,
}

#[derive(Debug, Clone, Args)]
struct ProjectListArgs {
    #[arg(long)]
    product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    status: Vec<ProjectStatus>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    id: Vec<String>,

    /// Filter by resolved repo. Accepts a full URL or a short
    /// name (basename of the URL minus `.git`). Short-name match is
    /// case-insensitive prefix; selectors shorter than 2 chars are
    /// rejected to keep false-positive density low.
    ///
    /// Projects don't carry a repo column today; the filter matches
    /// against the parent product's `repo_remote_url`.
    #[arg(long = "repo")]
    repo: Option<String>,

    #[command(flatten)]
    dep: DependencyFilterArgs,
}

#[derive(Debug, Clone, Args)]
struct ProjectShowArgs {
    #[arg(long)]
    product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    with_primary_id: bool,

    /// Project id, short id (#42 or 42), or slug.
    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProjectUpdateArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,

    #[arg(long)]
    status: Option<ProjectStatus>,

    #[arg(long)]
    priority: Option<ProjectPriority>,
}

/// Args for `boss project set-design-doc`. Either `--path` (with
/// optional `--repo` / `--branch`) or `--unset` must be supplied;
/// clap enforces mutual exclusion for the conflict cases and the
/// handler rejects the empty case at runtime.
#[derive(Debug, Clone, Args)]
struct ProjectSetDesignDocArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    /// Repo-relative path to the design doc (e.g.
    /// `tools/boss/docs/designs/foo.md`). Must end in `.md` /
    /// `.markdown`; absolute paths and `..` segments are rejected
    /// engine-side.
    #[arg(long, conflicts_with = "unset")]
    path: Option<String>,

    /// Override the repo URL the doc lives in. Omit to inherit from
    /// the project's product (the same-repo case).
    #[arg(long, requires = "path", conflicts_with = "unset")]
    repo: Option<String>,

    /// Override the branch the doc lives on. Omit to inherit from
    /// the product's docs branch (or `main`).
    #[arg(long, requires = "path", conflicts_with = "unset")]
    branch: Option<String>,

    /// Clear all three pointer columns. Mutually exclusive with
    /// `--path` / `--repo` / `--branch`.
    #[arg(long)]
    unset: bool,
}

/// Args for `boss project open-design`. `--web` forces the GitHub
/// web URL; `--print` emits the resolved target without launching
/// anything. Both flags can combine — `--web --print` prints the
/// web URL.
#[derive(Debug, Clone, Args)]
struct ProjectOpenDesignArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    /// Skip the same-product / workspace fast path and always emit
    /// the GitHub web URL.
    #[arg(long)]
    web: bool,

    /// Don't launch anything; print the resolved target to stdout
    /// instead. Combine with `--web` to print the web URL.
    #[arg(long)]
    print: bool,
}

#[derive(Debug, Clone, Args)]
struct TaskCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    /// Priority of the new task. Omitted → engine default (`medium`).
    #[arg(long)]
    priority: Option<TaskPriority>,

    /// Repo URL override for this task. Omit to inherit from the
    /// product default; pass `""` later via `task update --repo ""`
    /// to clear an override.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    /// Effort estimate (`trivial`/`small`/`medium`/`large`/`max`).
    /// Omitted → no level set; the dispatcher falls through to
    /// product / engine default per the design's Q3 precedence.
    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    /// Claude model slug override (e.g. `opus`, `sonnet`, `haiku`,
    /// or a fully-qualified id). Stored verbatim — claude is the
    /// source of truth on slugs.
    #[arg(long, value_name = "SLUG")]
    model: Option<String>,

    /// Bypass the duplicate guard. When a task with the same name
    /// already exists in this product and was created within the last
    /// 60 seconds, the engine rejects the create to catch fat-finger
    /// retries. Pass this flag to override and insert a second row
    /// unconditionally.
    #[arg(long = "force-duplicate", default_value_t = false)]
    force_duplicate: bool,
}

#[derive(Debug, Clone, Args)]
struct TaskListArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    status: Vec<TaskStatus>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    /// e.g. `--priority high` shows only high-priority work.
    #[arg(long, value_delimiter = ',')]
    priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    id: Vec<String>,

    /// Filter by resolved repo. Accepts a full URL or a short
    /// name (basename of the URL minus `.git`). Resolution falls
    /// back to the parent product's `repo_remote_url` when the
    /// task carries no override, so `--repo nimbus` finds inherited
    /// matches too. Short-name match is case-insensitive prefix;
    /// selectors shorter than 2 chars are rejected.
    #[arg(long = "repo")]
    repo: Option<String>,

    #[command(flatten)]
    dep: DependencyFilterArgs,
}

#[derive(Debug, Clone, Args)]
struct ChoreCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    /// Priority of the new chore. Omitted → engine default (`medium`).
    #[arg(long)]
    priority: Option<TaskPriority>,

    /// Repo URL override for this chore. Omit to inherit from the
    /// product default; pass `""` later via `chore update --repo ""`
    /// to clear an override.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    /// Effort estimate (`trivial`/`small`/`medium`/`large`/`max`).
    /// Omitted → no level set; the dispatcher falls through per
    /// design §Q3 precedence.
    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    /// Claude model slug override. Stored verbatim — claude is the
    /// source of truth on slugs.
    #[arg(long, value_name = "SLUG")]
    model: Option<String>,

    /// Bypass the duplicate guard. See `boss task create --help` for
    /// the full description.
    #[arg(long = "force-duplicate", default_value_t = false)]
    force_duplicate: bool,
}

/// Args for `boss task create-many`. The CLI reads a JSON array of
/// item objects from `--from-file <path>` (use `-` for stdin) and
/// fans them out into a single batched engine request. Top-level
/// `--product` / `--project` plus the global `--no-autostart` act as
/// defaults applied to every item; per-item fields override.
///
/// Item schema (per array entry):
/// ```json
/// {
///   "name": "...",                 // required, non-empty
///   "description": "...",          // required (may be empty string)
///   "autostart": true,             // optional, defaults to top-level
///   "project_id": "proj_..."       // optional override of --project
/// }
/// ```
#[derive(Debug, Clone, Args)]
struct TaskCreateManyArgs {
    /// Path to a JSON file containing the array of items. Use `-` to
    /// read from stdin.
    #[arg(long = "from-file")]
    from_file: String,

    /// Default product for items that don't specify one. Required
    /// unless every item carries a fully-resolved engine `product_id`.
    #[arg(long)]
    product: Option<String>,

    /// Default project for items that don't specify one. Items may
    /// override via per-item `project_id`.
    #[arg(long)]
    project: Option<String>,
}

/// Args for `boss chore create-many`. Identical to
/// [`TaskCreateManyArgs`] but with no project axis.
#[derive(Debug, Clone, Args)]
struct ChoreCreateManyArgs {
    #[arg(long = "from-file")]
    from_file: String,

    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ChoreListArgs {
    #[arg(long)]
    product: Option<String>,

    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    with_primary_id: bool,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    status: Vec<TaskStatus>,

    /// Filter by priority. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    priority: Vec<TaskPriority>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    id: Vec<String>,

    /// Filter by resolved repo. See `boss task list --help`.
    #[arg(long = "repo")]
    repo: Option<String>,

    #[command(flatten)]
    dep: DependencyFilterArgs,
}

/// The four dependency-graph filter flags from design Q6. They are
/// mutually exclusive — clap enforces this so the engine never sees
/// an over-constrained request. Flattened into each
/// `*ListArgs` so every list verb gets the same surface.
#[derive(Debug, Clone, Args)]
#[group(multiple = false)]
struct DependencyFilterArgs {
    /// Items that the named work item depends on (its incoming edges).
    #[arg(long = "prerequisites-of", value_name = "ID")]
    prerequisites_of: Option<String>,

    /// Items that depend on the named work item (its outgoing edges).
    #[arg(long = "dependents-of", value_name = "ID")]
    dependents_of: Option<String>,

    /// Items in `todo` with no gating prerequisite — i.e. what the
    /// dispatcher could pick up next.
    #[arg(long = "unblocked")]
    unblocked: bool,

    /// Items currently gated by at least one incomplete prereq.
    #[arg(long = "blocked-by-deps")]
    blocked_by_deps: bool,
}

impl DependencyFilterArgs {
    fn into_filter(self) -> Option<DependencyFilter> {
        if let Some(id) = self.prerequisites_of {
            return Some(DependencyFilter::PrerequisitesOf { id });
        }
        if let Some(id) = self.dependents_of {
            return Some(DependencyFilter::DependentsOf { id });
        }
        if self.unblocked {
            return Some(DependencyFilter::Unblocked);
        }
        if self.blocked_by_deps {
            return Some(DependencyFilter::BlockedByDeps);
        }
        None
    }
}

#[derive(Debug, Clone, Args)]
struct TaskIdArg {
    /// Task/chore id. Accepts: primary id (`task_…`), friendly short id
    /// (`T441`, `t441`, `42`, or `#42`), or cross-product form
    /// (`boss/42` or `boss/#42`).
    id: String,
    /// Resolve a friendly short id (`42` or `#42`) against this product
    /// (slug or id). Ignored when the selector already embeds a product
    /// slug (`boss/42`) or when the selector is a primary id.
    #[arg(long)]
    product: Option<String>,
    /// Also display the primary id alongside the friendly id.
    #[arg(long = "with-primary-id")]
    with_primary_id: bool,
}

#[derive(Debug, Clone, Args)]
struct TaskUpdateArgs {
    id: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    status: Option<TaskStatus>,

    #[arg(long)]
    priority: Option<TaskPriority>,

    #[arg(long)]
    ordinal: Option<i64>,

    /// Escape hatch for backfilling `pr_url` when the engine's
    /// auto-detection couldn't pick it up. With the on-Stop +
    /// merge-poller pair installed in the engine you should rarely
    /// need this; hidden from `-h` short help to keep the common
    /// path clean while still surfacing it in `--help` and via
    /// `boss chore update --help`.
    #[arg(long = "pr-url", hide_short_help = true)]
    pr_url: Option<String>,

    /// Set or clear this item's repo override. `--repo <url>` sets
    /// the override; `--repo ""` clears it so the item inherits
    /// from the product default. Same shape as `--pr-url ""`.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    /// Set the effort level (`trivial`/`small`/`medium`/`large`/`max`).
    /// Mutually exclusive with `--unset-effort`.
    #[arg(long, value_enum, conflicts_with = "unset_effort")]
    effort: Option<EffortLevelArg>,

    /// Clear the effort level so the row falls through to the
    /// dispatcher's product / engine default again (design §Q3).
    #[arg(long = "unset-effort")]
    unset_effort: bool,

    /// Claude model slug override. Stored verbatim. Mutually
    /// exclusive with `--unset-model`.
    #[arg(long, value_name = "SLUG", conflicts_with = "unset_model")]
    model: Option<String>,

    /// Clear the per-row model override so the dispatcher falls
    /// through per design §Q3 precedence.
    #[arg(long = "unset-model")]
    unset_model: bool,

    /// Enable or disable auto-dispatch for this item. `--autostart true`
    /// lets the engine auto-dispatch the item when a worker slot is free;
    /// `--autostart false` parks it in the backlog until you re-enable it.
    #[arg(long, value_name = "BOOL")]
    autostart: Option<bool>,

    /// Set or clear the blocked reason on this item. Accepts any engine
    /// reason value (`merge_conflict`, `ci_failure`, `ci_failure_exhausted`,
    /// `dependency`, `review_feedback`) or an empty string to clear.
    /// Pass `--blocked-reason ""` to wipe a stale reason the automated
    /// sweepers left behind. This is the manual escape hatch; automated
    /// clearing happens when the engine transitions a row away from `blocked`.
    #[arg(long = "blocked-reason", value_name = "REASON", allow_hyphen_values = true)]
    blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskMoveArgs {
    id: String,

    #[arg(long = "to")]
    target: MoveTarget,
}

#[derive(Debug, Clone, Args)]
struct BindPrArgs {
    /// Task or chore id to attach the PR to.
    id: String,

    /// GitHub PR URL of the form
    /// `https://github.com/<org>/<repo>/pull/<n>`.
    pr_url: String,
}

#[derive(Debug, Clone, Args)]
struct LinkExternalArgs {
    /// Task or chore id to link.
    id: String,

    /// Tracker discriminator matching `products.external_tracker_kind`
    /// for the work item's product (e.g. `github`).
    #[arg(long)]
    kind: String,

    /// Stable tracker-specific id for this upstream issue
    /// (e.g. `spinyfin/mono#560` for GitHub).
    #[arg(long = "id", id = "upstream_id")]
    upstream_id: String,
}

#[derive(Debug, Clone, Args)]
struct ProductMoveArgs {
    selector: String,

    #[arg(long = "to")]
    target: ProductStatus,
}

#[derive(Debug, Clone, Args)]
struct ProjectMoveArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    #[arg(long = "to")]
    target: ProjectStatus,
}

#[derive(Debug, Clone, Args)]
struct TaskDeleteArgs {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskReorderArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long, value_delimiter = ',')]
    ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProductStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectStatus {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectPriority {
    Low,
    Medium,
    High,
}

/// Priority enum for tasks and chores. Mirrors `ProjectPriority`
/// exactly so kanban surfaces and CLI flags speak one vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum TaskPriority {
    Low,
    Medium,
    High,
}

/// CLI surface for `tasks.effort_level` (design §Q1):
/// `trivial | small | medium | large | max`. `max` is the human-only
/// escape hatch — the coordinator's heuristic never emits it, but
/// users can set it via `--effort max` to request Claude's maximum
/// reasoning depth.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum EffortLevelArg {
    Trivial,
    Small,
    Medium,
    Large,
    Max,
}

impl EffortLevelArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Max => "max",
        }
    }
}

impl From<EffortLevelArg> for boss_protocol::EffortLevel {
    fn from(value: EffortLevelArg) -> Self {
        match value {
            EffortLevelArg::Trivial => boss_protocol::EffortLevel::Trivial,
            EffortLevelArg::Small => boss_protocol::EffortLevel::Small,
            EffortLevelArg::Medium => boss_protocol::EffortLevel::Medium,
            EffortLevelArg::Large => boss_protocol::EffortLevel::Large,
            EffortLevelArg::Max => boss_protocol::EffortLevel::Max,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TaskStatus {
    Todo,
    Active,
    Blocked,
    InReview,
    Done,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MoveTarget {
    Backlog,
    Doing,
    Review,
    Done,
    Todo,
    Active,
    Blocked,
    InReview,
}

impl ProductStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl ProjectStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl ProjectPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
            Self::Done => "done",
        }
    }
}

impl MoveTarget {
    fn as_status(self) -> &'static str {
        match self {
            Self::Backlog | Self::Todo => "todo",
            Self::Doing | Self::Active => "active",
            Self::Review | Self::InReview => "in_review",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Human,
    Json,
}

#[derive(Debug, Serialize)]
struct CliReferenceDocument {
    cli: &'static str,
    usage_rules: Vec<&'static str>,
    selector_semantics: Vec<&'static str>,
    status_semantics: Vec<&'static str>,
    workflow_guidance: Vec<&'static str>,
    commands: Vec<CliReferenceSection>,
}

#[derive(Debug, Serialize)]
struct CliReferenceSection {
    path: String,
    help: String,
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    NotFound(String),
    Conflict(String),
    EngineUnavailable(String),
    Application(String),
    Internal(anyhow::Error),
}

impl CliError {
    fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    fn engine_unavailable(message: impl Into<String>) -> Self {
        Self::EngineUnavailable(message.into())
    }

    fn application(message: impl Into<String>) -> Self {
        Self::Application(message.into())
    }

    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Usage(_) => ExitCode::from(2),
            Self::NotFound(_) => ExitCode::from(3),
            Self::Conflict(_) => ExitCode::from(4),
            Self::EngineUnavailable(_) => ExitCode::from(5),
            Self::Application(_) => ExitCode::from(6),
            Self::Internal(_) => ExitCode::from(7),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::EngineUnavailable(message)
            | Self::Application(message) => f.write_str(message),
            Self::Internal(err) => write!(f, "{err:#}"),
        }
    }
}

struct RunContext {
    output_mode: OutputMode,
    quiet: bool,
    allow_input: bool,
    discovery: Discovery,
    /// Mirror of the global `--no-autostart` flag. Today this also
    /// gates per-work-item auto-dispatch (`boss chore create
    /// --no-autostart` → engine creates the chore in `todo` but does
    /// not spin up a worker for it).
    no_autostart: bool,
}

fn boss_version_string() -> String {
    let sha = option_env!("BOSS_GIT_SHA").unwrap_or("unknown");
    let time = option_env!("BOSS_BUILD_TIME").unwrap_or("unknown");
    format!("boss 0+{sha} built {time}")
}

#[tokio::main]
async fn main() -> ExitCode {
    // Intercept --version/-V before Cli::parse() so we print the
    // canonical "boss 0+<sha> built <time>" format (design doc Q7).
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--version")
        || argv.get(1).map(|s| s.as_str()) == Some("-V")
    {
        println!("{}", boss_version_string());
        return ExitCode::SUCCESS;
    }

    let cli = Cli::parse();
    match run_cli(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

async fn run_cli(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Commands::Reference => run_reference_command(&cli.global),
        Commands::Product { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_product_command(command, &ctx).await
        }
        Commands::Project { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_project_command(command, &ctx).await
        }
        Commands::Task { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_task_command(command, &ctx).await
        }
        Commands::Chore { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_chore_command(command, &ctx).await
        }
        Commands::Engine { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_engine_command(command, &ctx).await
        }
        Commands::Uninstall(args) => run_uninstall_command(args, &cli.global),
    }
}

fn run_reference_command(flags: &GlobalFlags) -> Result<(), CliError> {
    let output_mode = if flags.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };
    let reference = build_cli_reference()?;

    match output_mode {
        OutputMode::Human => print_cli_reference_human(&reference).map_err(CliError::internal)?,
        OutputMode::Json => {
            serde_json::to_writer_pretty(io::stdout().lock(), &reference)
                .map_err(CliError::internal)?;
            println!();
        }
    }

    Ok(())
}

fn build_cli_reference() -> Result<CliReferenceDocument, CliError> {
    let command = Cli::command().color(clap::ColorChoice::Never);
    let mut commands = Vec::new();
    collect_cli_reference_sections(command, Vec::new(), &mut commands)?;

    Ok(CliReferenceDocument {
        cli: "boss",
        usage_rules: vec![
            "For agent use, prefer non-interactive commands with --json --no-input.",
            "Treat this reference output as the authoritative current CLI surface for this build.",
            "Do not use boss ... --help for syntax discovery when this reference is available.",
            "Omit --socket-path unless you explicitly need a non-default socket.",
            "Omit --no-autostart unless you explicitly need to forbid engine startup or auto-dispatch on `task create` / `chore create` (also gates the auto-spawned `kind=design` seed task on `project create`).",
            "Kind-agnostic verbs (show, update, move, delete, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        selector_semantics: vec![
            "Product selectors accept a product id, slug, or 1-based interactive index. For agent use, prefer slug or id, not numeric indexes.",
            "Project selectors accept a project id, slug, short id (#42 or 42), or 1-based interactive index within the selected product. For agent use, prefer slug, short id, or primary id; avoid numeric indexes.",
            "Task and chore selectors accept: (1) primary id (task_…); (2) friendly short id — `T441` / `t441` / `42` / `#42` within the context product, or `boss/42` / `boss/#42` for a specific product. Projects accept `P7` / `p7` in the same position. For agent use, prefer the short id form (T-prefix or #42) when talking to a human, and the primary id when calling other engine RPCs.",
            "Kind-agnostic verbs (show, update, move, delete, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        status_semantics: vec![
            "CLI status values use in-review on the command line.",
            "Internally, in-review maps to in_review.",
            "Task and chore move targets map: backlog|todo -> todo, doing|active -> active, review|in-review -> in_review, blocked -> blocked, done -> done.",
            "Product move/delete: --to active|paused|archived. delete is a soft archive (sets status=archived).",
            "Project move/delete: --to planned|active|blocked|done|archived. delete is a soft archive (sets status=archived).",
        ],
        workflow_guidance: vec![
            "Use the current UI or conversational context first when deciding where new work belongs.",
            "If you need to compare against existing projects in a product, use boss project list --product <product-selector> --json --no-input.",
            "If the work fits an existing project, create a task in that project.",
            "If it does not fit an existing project and is small and self-contained, create a chore.",
            "If it does not fit an existing project and is broad, ambiguous, investigative, or multi-stage, create a project.",
            "`boss project create` auto-spawns a `kind=design` seed task under the new project (surfaced as `design_task` in the --json response). Do NOT follow up by filing a parallel \"Design\" task; populate the brief by running `boss task update <design_task.id> --description ...` on the seed task. Use `--no-autostart` on `project create` if you want to author the brief before the engine dispatches a worker against the seed task. Use `--no-design-task` for non-design-shaped projects (postmortems, checklists, milestone aggregators) where no seed task is needed; the project is filed with zero child tasks.",
        ],
        commands,
    })
}

fn collect_cli_reference_sections(
    command: clap::Command,
    path: Vec<String>,
    sections: &mut Vec<CliReferenceSection>,
) -> Result<(), CliError> {
    let mut current_path = path;
    current_path.push(command.get_name().to_owned());

    sections.push(CliReferenceSection {
        path: current_path.join(" "),
        help: render_command_help(command.clone())?,
    });

    for subcommand in command.get_subcommands() {
        collect_cli_reference_sections(subcommand.clone(), current_path.clone(), sections)?;
    }

    Ok(())
}

fn render_command_help(mut command: clap::Command) -> Result<String, CliError> {
    command = command.color(clap::ColorChoice::Never);
    let mut buffer = Vec::new();
    command
        .write_long_help(&mut buffer)
        .map_err(CliError::internal)?;
    let help = String::from_utf8(buffer).map_err(CliError::internal)?;
    Ok(help.trim().to_owned())
}

fn print_cli_reference_human(reference: &CliReferenceDocument) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Boss CLI reference:")?;
    writeln!(stdout)?;
    print_reference_list(&mut stdout, "General rules", &reference.usage_rules)?;
    print_reference_list(
        &mut stdout,
        "Selector semantics",
        &reference.selector_semantics,
    )?;
    print_reference_list(&mut stdout, "Status semantics", &reference.status_semantics)?;
    print_reference_list(
        &mut stdout,
        "Workflow guidance",
        &reference.workflow_guidance,
    )?;
    writeln!(stdout, "Command help:")?;
    for section in &reference.commands {
        writeln!(stdout, "[{}]", section.path)?;
        writeln!(stdout, "{}", section.help)?;
        writeln!(stdout)?;
    }
    Ok(())
}

fn print_reference_list(writer: &mut impl Write, title: &str, items: &[&str]) -> io::Result<()> {
    writeln!(writer, "{title}:")?;
    for item in items {
        writeln!(writer, "- {item}")?;
    }
    writeln!(writer)?;
    Ok(())
}

impl RunContext {
    fn from_flags(flags: &GlobalFlags) -> Result<Self, CliError> {
        let allow_input =
            !flags.no_input && io::stdin().is_terminal() && io::stdout().is_terminal();
        let discovery = Discovery::from_env(flags.socket_path.as_deref())
            .map_err(CliError::internal)?
            .with_autostart(!flags.no_autostart);

        Ok(Self {
            output_mode: if flags.json {
                OutputMode::Json
            } else {
                OutputMode::Human
            },
            quiet: flags.quiet,
            allow_input,
            discovery,
            no_autostart: flags.no_autostart,
        })
    }
}

async fn run_product_command(command: ProductCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProductCommand::Create(args) => {
            let name = required_text(args.name, "Product name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let repo_remote_url = optional_text(args.repo_remote_url, "Repo remote URL", ctx)?;

            let product = create_product(
                &mut client,
                CreateProductInput {
                    name,
                    description,
                    repo_remote_url,
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Created product", &product);
            })
        }
        ProductCommand::List => {
            let products = list_products(&mut client).await?;
            print_entity(ctx, &serde_json::json!({ "products": products }), || {
                print_products_table(&products);
            })
        }
        ProductCommand::Show(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Product", &product);
            })
        }
        ProductCommand::Update(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                repo_remote_url: args.repo_remote_url,
                dispatch_preamble: args.dispatch_preamble,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --name or --status",
            )?;
            let item = update_work_item(&mut client, &product.id, patch).await?;
            let product = expect_product(item)?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Updated product", &product);
            })
        }
        ProductCommand::Delete(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProductStatus::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived =
                expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "product": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived product {} ({}) — products are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProductCommand::Move(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "product": moved }), || {
                print_product_details("Moved product", &moved);
            })
        }
        ProductCommand::SetDefaultModel(args) => {
            if !args.unset && args.model.is_none() {
                return Err(CliError::usage(
                    "provide either --model <slug> or --unset",
                ));
            }
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let model = if args.unset { None } else { args.model };
            let updated = set_product_default_model(&mut client, &product.id, model).await?;
            print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                print_product_details("Updated product", &updated);
            })
        }
        ProductCommand::AuditEffort(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AuditProductEffort {
                    product_id: product.id.clone(),
                    window_days: args.window_days,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::EffortAuditReport { report } => print_entity(
                    ctx,
                    &serde_json::json!({ "report": report }),
                    || print_effort_audit_report(&report),
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product audit-effort", &other)),
            }
        }
        ProductCommand::SetExternalTracker(args) => {
            if !args.unset && args.kind.is_none() {
                return Err(CliError::usage(
                    "provide either --kind (with kind-specific flags) or --unset",
                ));
            }
            let selector = args.selector.clone();
            let product = resolve_product(&mut client, Some(selector), ctx).await?;
            let (kind, config) = if args.unset {
                (None, None)
            } else {
                let kind = args.kind.as_deref().unwrap_or("github").to_owned();
                let config = build_external_tracker_config(&kind, &args)?;
                (Some(kind), Some(config))
            };
            let input = SetProductExternalTrackerInput {
                product_id: product.id.clone(),
                kind,
                config,
                unset: args.unset,
            };
            let response = client
                .send_request(&FrontendRequest::SetProductExternalTracker { input })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::WorkItemUpdated { item } => {
                    let updated = expect_product(item)?;
                    print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                        if args.unset {
                            if !ctx.quiet {
                                println!("External tracker binding removed from product {}.", updated.slug);
                            }
                        } else {
                            print_product_details("Updated product", &updated);
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product set-external-tracker", &other)),
            }
        }
    }
}

async fn run_project_command(command: ProjectCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProjectCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Project name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let goal = optional_text(args.goal, "Goal", ctx)?;

            let project = create_project(
                &mut client,
                CreateProjectInput {
                    product_id: product.id.clone(),
                    name,
                    description,
                    goal,
                    // Project creation auto-creates a `kind = 'design'`
                    // task as the project's first row. The global
                    // `--no-autostart` flag, which already gates
                    // chore/task auto-dispatch, now also gates that
                    // design task so a single mental model covers
                    // every work-item kind.
                    autostart: !ctx.no_autostart,
                    no_design_task: args.no_design_task,
                },
            )
            .await?;

            // Surface the auto-spawned `kind=design` seed task so
            // callers (notably the coordinator) can write a design
            // brief onto it without a follow-up `task list` call.
            // `create_project` inserts the design task in the same
            // sqlite transaction, so it's always present by the
            // time we get the project back.
            let design_task = list_tasks(&mut client, &product.id, Some(&project.id), None)
                .await?
                .into_iter()
                .find(|t| t.kind == "design");

            print_entity(
                ctx,
                &serde_json::json!({
                    "project": project,
                    "design_task": design_task,
                }),
                || {
                    print_project_details("Created project", &project, None, false);
                    if let Some(task) = design_task.as_ref() {
                        println!(
                            "Design task: {} (autostart={}, status={})",
                            task.id, task.autostart, task.status
                        );
                    }
                },
            )
        }
        ProjectCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let projects = list_projects(&mut client, &product.id, dep_filter).await?;
            let projects = apply_project_list_filters(
                projects,
                &args.status,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            print_entity(
                ctx,
                &serde_json::json!({ "product": product, "projects": projects }),
                || print_projects_table(&projects, args.with_primary_id),
            )
        }
        ProjectCommand::Show(args) => {
            let with_primary_id = args.with_primary_id;
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let detail = list_dependencies_detailed(
                &mut client,
                ListDependenciesInput {
                    work_item: project.id.clone(),
                    direction: Some(DependencyDirection::Both),
                },
            )
            .await?;
            let design_doc = resolve_project_design_doc(&mut client, &project.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project": project,
                    "dependencies": detail,
                    "design_doc": design_doc,
                }),
                || {
                    print_project_details("Project", &project, Some(&product), with_primary_id);
                    if let Some(line) = format_project_design_doc_line(&design_doc.state) {
                        println!("Design doc: {line}");
                    }
                    print_dependency_section(&detail);
                },
            )
        }
        ProjectCommand::Update(args) => {
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                goal: args.goal,
                status: args.status.map(|status| status.as_str().to_owned()),
                priority: args.priority.map(|priority| priority.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --goal or --priority",
            )?;
            let item = update_work_item(&mut client, &project.id, patch).await?;
            let project = expect_project(item)?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Updated project", &project, None, false);
            })
        }
        ProjectCommand::Delete(args) => {
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProjectStatus::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived =
                expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived project {} ({}) — projects are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProjectCommand::Move(args) => {
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "project": moved }), || {
                print_project_details("Moved project", &moved, None, false);
            })
        }
        ProjectCommand::SetDesignDoc(args) => {
            if !args.unset && args.path.is_none() {
                return Err(CliError::usage(
                    "provide --path <p> (with optional --repo/--branch) or --unset",
                ));
            }
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let input = if args.unset {
                SetProjectDesignDocInput {
                    project_id: project.id.clone(),
                    unset: true,
                    ..SetProjectDesignDocInput::default()
                }
            } else {
                SetProjectDesignDocInput {
                    project_id: project.id.clone(),
                    design_doc_repo_remote_url: args.repo,
                    design_doc_branch: args.branch,
                    design_doc_path: args.path,
                    unset: false,
                }
            };
            let updated = set_project_design_doc(&mut client, input).await?;
            let resolved = resolve_project_design_doc(&mut client, &updated.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project": updated, "design_doc": resolved }),
                || {
                    print_project_details("Updated project", &updated, None, false);
                    if let Some(line) = format_project_design_doc_line(&resolved.state) {
                        println!("Design doc: {line}");
                    }
                },
            )
        }
        ProjectCommand::OpenDesign(args) => {
            let product =
                resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx)
                    .await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let resolved = resolve_project_design_doc(&mut client, &project.id).await?;
            let action = decide_open_design_action(&resolved.state, args.web)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project_id": project.id,
                    "design_doc": resolved,
                    "action": action.as_json(),
                }),
                || {
                    if !ctx.quiet {
                        println!("{}", action.human_summary());
                    }
                },
            )?;
            if !args.print {
                action.launch()?;
            }
            Ok(())
        }
        ProjectCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
    }
}

async fn run_task_command(command: TaskCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        TaskCommand::Create(args) => {
            let product = resolve_product_inferable(
                &mut client,
                args.product,
                args.project.as_deref(),
                ctx,
            )
            .await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            let name = required_text(args.name, "Task name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let prompt_text = compose_prompt_text(&name, description.as_deref());
            let resolved_repo = repo_resolution::resolve_repo_at_create_time(
                &mut client,
                &product,
                args.repo_remote_url.as_deref(),
                &prompt_text,
                ctx.allow_input,
            )
            .await?;
            // Only error on unresolved repo for multi-repo products (no product default).
            // Single-repo products return None intentionally; the engine inherits from the product.
            if product.repo_remote_url.is_none() && resolved_repo.is_none() && !ctx.allow_input {
                return Err(repo_resolution::unresolved_repo_error(&product.slug));
            }
            let task = create_task(
                &mut client,
                CreateTaskInput {
                    product_id: product.id,
                    project_id: project.id,
                    name,
                    description,
                    autostart: !ctx.no_autostart,
                    priority: args.priority.map(|priority| priority.as_str().to_owned()),
                    created_via: Some(CREATED_VIA_CLI.to_owned()),
                    repo_remote_url: resolved_repo,
                    effort_level: args.effort.map(EffortLevel::from),
                    model_override: normalize_non_empty(args.model),
                    force_duplicate: args.force_duplicate,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task, None, false);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product_inferable(
                &mut client,
                args.product,
                args.project.as_deref(),
                ctx,
            )
            .await?;
            let project = match args.project {
                Some(selector) => {
                    Some(resolve_project(&mut client, &product.id, Some(selector), ctx).await?)
                }
                None => None,
            };
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let tasks = list_tasks(
                &mut client,
                &product.id,
                project.as_ref().map(|project| project.id.as_str()),
                dep_filter,
            )
            .await?;
            let tasks = apply_task_list_filters(
                tasks,
                &args.status,
                &args.priority,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                print_tasks_table(&tasks, args.with_primary_id)
            })
        }
        TaskCommand::Show(args) => run_show_leaf(&mut client, ctx, args, false).await,
        TaskCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        TaskCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        TaskCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        TaskCommand::Reorder(args) => {
            let product = resolve_product_inferable(
                &mut client,
                args.product,
                args.project.as_deref(),
                ctx,
            )
            .await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            if args.ids.is_empty() {
                return Err(CliError::usage("provide at least one task id via --ids"));
            }
            reorder_project_tasks(&mut client, &project.id, &args.ids).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project_id": project.id, "task_ids": args.ids }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Reordered {} tasks for project {}",
                            args.ids.len(),
                            project.name
                        );
                    }
                },
            )
        }
        TaskCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        TaskCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        TaskCommand::LinkExternal(args) => run_link_external(&mut client, ctx, args).await,
        TaskCommand::UnlinkExternal(args) => run_unlink_external(&mut client, ctx, args).await,
        TaskCommand::CreateMany(args) => run_task_create_many(&mut client, ctx, args).await,
    }
}

async fn run_chore_command(command: ChoreCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ChoreCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Chore name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let prompt_text = compose_prompt_text(&name, description.as_deref());
            let resolved_repo = repo_resolution::resolve_repo_at_create_time(
                &mut client,
                &product,
                args.repo_remote_url.as_deref(),
                &prompt_text,
                ctx.allow_input,
            )
            .await?;
            // Only error on unresolved repo for multi-repo products (no product default).
            // Single-repo products return None intentionally; the engine inherits from the product.
            if product.repo_remote_url.is_none() && resolved_repo.is_none() && !ctx.allow_input {
                return Err(repo_resolution::unresolved_repo_error(&product.slug));
            }
            let chore = create_chore(
                &mut client,
                CreateChoreInput {
                    product_id: product.id,
                    name,
                    description,
                    autostart: !ctx.no_autostart,
                    priority: args.priority.map(|priority| priority.as_str().to_owned()),
                    created_via: Some(CREATED_VIA_CLI.to_owned()),
                    repo_remote_url: resolved_repo,
                    effort_level: args.effort.map(EffortLevel::from),
                    model_override: normalize_non_empty(args.model),
                    force_duplicate: args.force_duplicate,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore, None, false);
            })
        }
        ChoreCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let chores = list_chores(&mut client, &product.id, dep_filter).await?;
            let chores = apply_task_list_filters(
                chores,
                &args.status,
                &args.priority,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
                repo_selector.as_ref(),
                product.repo_remote_url.as_deref(),
            );
            print_entity(ctx, &serde_json::json!({ "chores": chores }), || {
                print_tasks_table(&chores, args.with_primary_id)
            })
        }
        ChoreCommand::Show(args) => run_show_leaf(&mut client, ctx, args, true).await,
        ChoreCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        ChoreCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        ChoreCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        ChoreCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        ChoreCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        ChoreCommand::LinkExternal(args) => run_link_external(&mut client, ctx, args).await,
        ChoreCommand::UnlinkExternal(args) => run_unlink_external(&mut client, ctx, args).await,
        ChoreCommand::CreateMany(args) => run_chore_create_many(&mut client, ctx, args).await,
    }
}

/// Shared handler for `boss task show <id>` and `boss chore show <id>`.
/// Routes any leaf work item id through the same path; the JSON key
/// and human-mode label match the actual kind of the returned item.
///
/// `chore_only`: when `true` (called from `boss chore show`), resolving
/// a friendly short id to a non-chore task-table row produces a
/// "wrong kind" error naming the correct verb.
async fn run_show_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskIdArg,
    chore_only: bool,
) -> Result<(), CliError> {
    let with_primary_id = args.with_primary_id;
    let work_item = match parse_work_item_selector(&args.id) {
        WorkItemSelector::ShortId(n) => {
            let product = resolve_product(client, args.product, ctx).await?;
            let item = get_work_item_by_short_id_rpc(client, &product.id, n).await?;
            check_task_kind_for_verb(&item, n, chore_only)?;
            item
        }
        WorkItemSelector::ProductShortId { product_slug, n } => {
            let product = resolve_product(client, Some(product_slug), ctx).await?;
            let item = get_work_item_by_short_id_rpc(client, &product.id, n).await?;
            check_task_kind_for_verb(&item, n, chore_only)?;
            item
        }
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => {
            get_work_item(client, &id).await?
        }
    };
    let (item, label) = expect_leaf_work_item(work_item)?;
    let product = expect_product(get_work_item(client, &item.product_id).await?)?;
    let detail = list_dependencies_detailed(
        client,
        ListDependenciesInput {
            work_item: item.id.clone(),
            direction: Some(DependencyDirection::Both),
        },
    )
    .await?;
    let executions = list_executions_for_item(client, &item.id).await?;
    let runtime = get_task_runtime(client, &item.id).await?;
    let task_json = task_json_with_runtime(&item, &runtime)?;
    print_entity(
        ctx,
        &serde_json::json!({
            label: task_json,
            "dependencies": detail,
            "executions": executions,
        }),
        || {
            print_task_details(label_titlecase(label), &item, Some(&product), with_primary_id);
            print_runtime_section(&runtime);
            print_dependency_section(&detail);
            print_executions_section(&executions);
        },
    )
}

/// Serialise `item` and splice the runtime's `current_execution_id`
/// / `current_run_id` onto the resulting JSON object so a downstream
/// `jq .task.current_execution_id` resolves to the engine's view of
/// the dispatched execution. Both fields land as `null` when no
/// execution / run exists yet — the coordinator wants the keys
/// present so it can distinguish "engine returned null" from "this
/// client predates the field." Cloning into a `serde_json::Value`
/// keeps the wire shape of [`Task`] unchanged everywhere else.
fn task_json_with_runtime(item: &Task, runtime: &TaskRuntime) -> Result<serde_json::Value, CliError> {
    let mut value = serde_json::to_value(item).map_err(CliError::internal)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "current_execution_id".to_owned(),
            runtime
                .execution_id
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
        map.insert(
            "current_run_id".to_owned(),
            runtime
                .current_run_id
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
    }
    Ok(value)
}

fn print_runtime_section(runtime: &TaskRuntime) {
    if runtime.execution_id.is_none() && runtime.current_run_id.is_none() {
        return;
    }
    println!();
    println!("Runtime:");
    println!(
        "  current_execution_id: {}",
        runtime.execution_id.as_deref().unwrap_or("-")
    );
    println!(
        "  current_run_id:       {}",
        runtime.current_run_id.as_deref().unwrap_or("-")
    );
    if let Some(status) = &runtime.execution_status {
        println!("  execution_status:     {status}");
    }
    if let Some(status) = &runtime.run_status {
        println!("  run_status:           {status}");
    }
}

/// Check whether a work item resolved from a short id matches the verb
/// context. When `chore_only` is true and the item is a non-chore task,
/// return a user-friendly error naming the right verb.
fn check_task_kind_for_verb(item: &WorkItem, short_id: i64, chore_only: bool) -> Result<(), CliError> {
    if !chore_only {
        return Ok(());
    }
    match item {
        WorkItem::Task(t) => Err(CliError::application(format!(
            "T{short_id} is a {} (kind={}), not a chore — use `boss task show {short_id}`",
            t.kind, t.kind
        ))),
        WorkItem::Project(_) => Err(CliError::application(format!(
            "P{short_id} is a project, not a chore — use `boss project show {short_id}`"
        ))),
        WorkItem::Chore(_) | WorkItem::Product(_) => Ok(()),
    }
}

/// Shared handler for `boss task update` and `boss chore update`.
async fn run_update_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskUpdateArgs,
) -> Result<(), CliError> {
    let effort_level = if args.unset_effort {
        Some(String::new())
    } else {
        args.effort.map(|e| e.as_str().to_owned())
    };
    let model_override = if args.unset_model {
        Some(String::new())
    } else {
        args.model
    };
    let patch = WorkItemPatch {
        name: args.name,
        description: args.description,
        status: args.status.map(|status| status.as_str().to_owned()),
        priority: args.priority.map(|priority| priority.as_str().to_owned()),
        ordinal: args.ordinal,
        pr_url: args.pr_url,
        // Preserve the empty-string "clear" wire form: `--repo ""`
        // means the engine should clear the override (inherit from
        // the product). Don't `normalize_non_empty` here.
        repo_remote_url: args.repo_remote_url,
        effort_level,
        model_override,
        autostart: args.autostart,
        // Preserve the empty-string "clear" wire form: `--blocked-reason ""`
        // maps to NULL in the engine (clears the field).
        blocked_reason: args.blocked_reason,
        ..WorkItemPatch::default()
    };
    ensure_patch_present(
        &patch,
        "provide at least one field to update, such as --status, --priority, --pr-url, --repo, --effort, --model, --autostart, or --blocked-reason",
    )?;
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Updated {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task move` and `boss chore move`.
async fn run_move_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskMoveArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let patch = WorkItemPatch {
        status: Some(args.target.as_status().to_owned()),
        ..WorkItemPatch::default()
    };
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Moved {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task delete` and `boss chore delete`. The
/// engine doesn't need the kind to delete; we read it back from the
/// pre-delete fetch only so the human-mode message names the right
/// noun.
async fn run_delete_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskDeleteArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let label = match get_work_item(client, &resolved_id).await {
        Ok(item) => expect_leaf_work_item(item).map(|(_, l)| l).unwrap_or("item"),
        Err(_) => "item",
    };
    delete_work_item(client, &resolved_id).await?;
    print_entity(
        ctx,
        &serde_json::json!({ "id": resolved_id, "deleted": true }),
        || {
            if !ctx.quiet {
                println!("Deleted {label} {resolved_id}");
            }
        },
    )
}

/// "task" -> "Task". The label set comes from
/// [`expect_leaf_work_item`], so `&'static str` in / out is enough.
fn label_titlecase(label: &str) -> &'static str {
    match label {
        "task" => "Task",
        "chore" => "Chore",
        _ => "Item",
    }
}

async fn run_engine_command(command: EngineCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCommand::Status => {
            let running = engine_socket_reachable(&ctx.discovery.socket_path).await;
            let pid = running_engine_pid(&ctx.discovery.pid_file_path);
            print_entity(
                ctx,
                &serde_json::json!({
                    "running": running,
                    "pid": pid,
                    "socket_path": ctx.discovery.socket_path,
                    "pid_file_path": ctx.discovery.pid_file_path,
                }),
                || {
                    if running {
                        println!("Boss engine is running.");
                    } else {
                        println!("Boss engine is stopped.");
                    }
                    println!("Socket: {}", ctx.discovery.socket_path);
                    println!("PID file: {}", ctx.discovery.pid_file_path);
                    if let Some(pid) = pid {
                        println!("PID: {pid}");
                    }
                },
            )
        }
        EngineCommand::Start => {
            ensure_engine_running(&ctx.discovery)
                .await
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": true, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Boss engine is running.");
                    }
                },
            )
        }
        EngineCommand::Stop => {
            stop_engine(&ctx.discovery.pid_file_path)
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": false, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Stopped Boss engine.");
                    }
                },
            )
        }
        EngineCommand::Conflicts { command } => run_engine_conflicts_command(command, ctx).await,
    }
}

async fn run_engine_conflicts_command(
    command: EngineConflictsCommand,
    ctx: &RunContext,
) -> Result<(), CliError> {
    match command {
        EngineConflictsCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(
                    resolve_product(&mut client, Some(selector), ctx)
                        .await?
                        .id,
                ),
                None => None,
            };
            // CLI-side default cap so human output stays readable; an
            // explicit `--limit 0` is treated as "no cap" so JSON
            // callers can stream everything.
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListConflictResolutions {
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionsList { attempts } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempts": attempts }),
                    || print_conflict_resolutions_table(&attempts),
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts list", &other)),
            }
        }
        EngineConflictsCommand::Show(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::GetConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolution { attempt } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempt": attempt }),
                    || print_conflict_resolution_detail(&attempt),
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts show", &other)),
            }
        }
        EngineConflictsCommand::Retry(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::RetryConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionRetried { attempt } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempt": attempt }),
                    || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} reset to pending; engine will re-dispatch a worker.",
                                attempt.id,
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts retry", &other)),
            }
        }
        EngineConflictsCommand::Abandon(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AbandonConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionMarkedAbandoned { attempt } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempt": attempt }),
                    || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} marked abandoned (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts abandon", &other)),
            }
        }
        EngineConflictsCommand::MarkFailed(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkConflictResolutionFailed {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionMarkedFailed { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} marked failed (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts mark-failed", &other)),
            }
        }
    }
}

fn print_conflict_resolutions_table(attempts: &[ConflictResolution]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "ID", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED",
        ]);
    for attempt in attempts {
        table.add_row(vec![
            attempt.id.as_str(),
            attempt.status.as_str(),
            attempt.pr_url.as_str(),
            attempt.work_item_id.as_str(),
            attempt.failure_reason.as_deref().unwrap_or(""),
            attempt.created_at.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_conflict_resolution_detail(attempt: &ConflictResolution) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["FIELD", "VALUE"]);
    let unset = "<unset>".to_owned();
    let rows: Vec<(&str, String)> = vec![
        ("id", attempt.id.clone()),
        ("status", attempt.status.clone()),
        ("product_id", attempt.product_id.clone()),
        ("work_item_id", attempt.work_item_id.clone()),
        ("pr_url", attempt.pr_url.clone()),
        ("pr_number", attempt.pr_number.to_string()),
        ("head_branch", attempt.head_branch.clone()),
        ("base_branch", attempt.base_branch.clone()),
        (
            "base_sha_at_trigger",
            attempt.base_sha_at_trigger.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "head_sha_before",
            attempt.head_sha_before.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "head_sha_after",
            attempt.head_sha_after.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "failure_reason",
            attempt.failure_reason.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "cube_lease_id",
            attempt.cube_lease_id.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "cube_workspace_id",
            attempt.cube_workspace_id.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "worker_id",
            attempt.worker_id.clone().unwrap_or_else(|| unset.clone()),
        ),
        ("created_at", attempt.created_at.clone()),
        (
            "started_at",
            attempt.started_at.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "finished_at",
            attempt.finished_at.clone().unwrap_or_else(|| unset.clone()),
        ),
    ];
    for (field, value) in &rows {
        table.add_row(vec![*field, value.as_str()]);
    }
    println!("{table}");
    if let Some(diag) = &attempt.conflict_diagnosis {
        println!();
        println!("conflict_diagnosis (raw):");
        println!("{diag}");
    }
}

/// Human-readable rendering for `boss product audit-effort`. The
/// JSON shape (under `--json`) is the `EffortAuditReport` directly;
/// this is the table the report-shape example in design §Q4
/// follow-up shows.
fn print_effort_audit_report(report: &EffortAuditReport) {
    let window = match report.window_days {
        Some(n) => format!("last {n} days"),
        None => "all recorded escalations".to_owned(),
    };
    println!(
        "Marker analysis ({window}, {n_esc} escalations across {n_chores} chores):",
        n_esc = report.total_escalations,
        n_chores = report.total_chores,
    );
    if report.rows.is_empty() {
        println!();
        println!(
            "  No marker matches recorded yet. Either no chores have been filed against this \
             product or no escalation events are recorded.",
        );
        return;
    }
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "MARKER",
            "ORIG LEVEL",
            "MATCHES",
            "ESCALATIONS",
            "UNDER-CLASS RATE",
            "NOTE",
        ]);
    for row in &report.rows {
        let rate = match row.under_class_rate {
            Some(r) => format!("{:.1}%", r * 100.0),
            None => "—".to_owned(),
        };
        let annotation = row.annotation.clone().unwrap_or_default();
        table.add_row(vec![
            row.marker.as_str(),
            row.original_level.as_str(),
            &row.matches.to_string(),
            &row.escalations.to_string(),
            rate.as_str(),
            annotation.as_str(),
        ]);
    }
    println!("{table}");
    println!();
    println!(
        "Threshold for the \"consider promoting\" callout: under-class rate > {:.0}%. \
         Edit the §Q4 marker lists in code based on this report; v1 keeps the \
         heuristic code-defined.",
        report.under_class_threshold * 100.0,
    );
}

async fn connect_for_work(ctx: &RunContext) -> Result<BossClient, CliError> {
    BossClient::connect(&ctx.discovery)
        .await
        .map_err(|err| CliError::engine_unavailable(err.to_string()))
}

async fn list_products(client: &mut BossClient) -> Result<Vec<Product>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProducts)
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProductsList { products } => Ok(products),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("products list", &other)),
    }
}

async fn list_projects(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Project>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
            dep_filter,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectsList { projects, .. } => Ok(projects),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("projects list", &other)),
    }
}

async fn list_tasks(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
            dep_filter,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("tasks list", &other)),
    }
}

async fn list_chores(
    client: &mut BossClient,
    product_id: &str,
    dep_filter: Option<DependencyFilter>,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
            dep_filter,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ChoresList { chores, .. } => Ok(chores),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chores list", &other)),
    }
}

async fn create_product(
    client: &mut BossClient,
    input: CreateProductInput,
) -> Result<Product, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProduct { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_product(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("product create", &other)),
    }
}

async fn set_product_default_model(
    client: &mut BossClient,
    product_id: &str,
    model: Option<String>,
) -> Result<Product, CliError> {
    match client
        .send_request(&FrontendRequest::SetProductDefaultModel {
            product_id: product_id.to_owned(),
            model,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => expect_product(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("set-default-model", &other)),
    }
}

/// Build the kind-specific JSON config for `set-external-tracker` from CLI args.
fn build_external_tracker_config(
    kind: &str,
    args: &ProductSetExternalTrackerArgs,
) -> Result<serde_json::Value, CliError> {
    match kind {
        "github" => {
            let org = args.org.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
                CliError::usage("--org is required for --kind github")
            })?;
            let repo = args.repo.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
                CliError::usage("--repo is required for --kind github")
            })?;
            let project_number = args.project.ok_or_else(|| {
                CliError::usage("--project is required for --kind github")
            })?;
            Ok(serde_json::json!({
                "org": org,
                "repo": repo,
                "project_number": project_number,
                "reverse_close": args.reverse_close,
            }))
        }
        other => Err(CliError::usage(format!(
            "unknown tracker kind '{other}'; supported: github"
        ))),
    }
}

async fn create_project(
    client: &mut BossClient,
    input: CreateProjectInput,
) -> Result<Project, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProject { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_project(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("project create", &other)),
    }
}

async fn set_project_design_doc(
    client: &mut BossClient,
    input: SetProjectDesignDocInput,
) -> Result<Project, CliError> {
    match client
        .send_request(&FrontendRequest::SetProjectDesignDoc { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => expect_project(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("set project design doc", &other)),
    }
}

async fn resolve_project_design_doc(
    client: &mut BossClient,
    project_id: &str,
) -> Result<ResolveProjectDesignDocOutput, CliError> {
    match client
        .send_request(&FrontendRequest::ResolveProjectDesignDoc {
            project_id: project_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectDesignDocResolved { output } => Ok(output),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("resolve project design doc", &other)),
    }
}

async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateTask { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(CliError::conflict(format!(
            "A task named {name:?} was created {age_secs}s ago as T{existing_short_id} \
             ({existing_id}); pass --force-duplicate to create another."
        ))),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task create", &other)),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(CliError::conflict(format!(
            "A chore named {name:?} was created {age_secs}s ago as T{existing_short_id} \
             ({existing_id}); pass --force-duplicate to create another."
        ))),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chore create", &other)),
    }
}

async fn get_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemResult { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item fetch", &other)),
    }
}

async fn update_work_item(
    client: &mut BossClient,
    id: &str,
    patch: WorkItemPatch,
) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: id.to_owned(),
            patch,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item update", &other)),
    }
}

/// Decide what bind-pr should do given the prior `pr_url` value on
/// the work item. Pure function so it can be unit-tested without an
/// engine round-trip.
#[derive(Debug, PartialEq, Eq)]
enum BindPrAction<'a> {
    /// pr_url already matches `new`: skip the engine round-trip and
    /// report no-op without printing a warning.
    Idempotent,
    /// pr_url is unset (or empty): apply the update silently.
    FirstTime,
    /// pr_url is set to a different value: apply the update and emit
    /// a stderr warning naming the old URL.
    Overwrite { previous: &'a str },
}

fn classify_bind_pr<'a>(prior: Option<&'a str>, new: &str) -> BindPrAction<'a> {
    match prior {
        Some(p) if p == new => BindPrAction::Idempotent,
        Some(p) if p.is_empty() => BindPrAction::FirstTime,
        Some(p) => BindPrAction::Overwrite { previous: p },
        None => BindPrAction::FirstTime,
    }
}

/// Shared handler for `boss task bind-pr` and `boss chore bind-pr`.
/// The kind is read from the actual item, not the noun the user
/// typed, so either invocation works against any leaf work item id.
async fn run_bind_pr(
    client: &mut BossClient,
    ctx: &RunContext,
    args: BindPrArgs,
) -> Result<(), CliError> {
    let new_url = validate_github_pr_url(&args.pr_url)?;

    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let (existing, label) = expect_leaf_work_item(get_work_item(client, &resolved_id).await?)?;
    let prior_url = existing.pr_url.clone();

    match classify_bind_pr(prior_url.as_deref(), new_url) {
        BindPrAction::Idempotent => {
            let id_for_print = existing.id.clone();
            return print_entity(
                ctx,
                &serde_json::json!({
                    label: existing,
                    "rebinding": false,
                    "previous_pr_url": prior_url,
                }),
                || {
                    if !ctx.quiet {
                        println!("{} {} already bound to {}", label, id_for_print, new_url);
                    }
                },
            );
        }
        BindPrAction::Overwrite { previous } => {
            eprintln!(
                "warning: replacing existing PR URL on {} {} (was {}, now {})",
                label, existing.id, previous, new_url,
            );
        }
        BindPrAction::FirstTime => {}
    }

    let patch = WorkItemPatch {
        pr_url: Some(new_url.to_owned()),
        ..WorkItemPatch::default()
    };
    let (updated, _) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;

    let title = format!("Bound PR to {label}");
    print_entity(
        ctx,
        &serde_json::json!({
            label: updated,
            "rebinding": prior_url.is_some(),
            "previous_pr_url": prior_url,
        }),
        || print_task_details(&title, &updated, None, false),
    )
}

/// Shared handler for `boss task link-external` and `boss chore link-external`.
async fn run_link_external(
    client: &mut BossClient,
    ctx: &RunContext,
    args: LinkExternalArgs,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let item = match client
        .send_request(&FrontendRequest::LinkWorkItemExternalRef {
            input: LinkExternalRefInput {
                work_item_id: resolved_id,
                kind: args.kind,
                canonical_id: args.upstream_id,
            },
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            return Err(CliError::application(message));
        }
        other => return Err(unexpected_event("link-external", &other)),
    };
    let (updated, label) = expect_leaf_work_item(item)?;
    let title = format!("Linked external ref on {label}");
    print_entity(
        ctx,
        &serde_json::json!({ label: updated }),
        || print_task_details(&title, &updated, None, false),
    )
}

/// Shared handler for `boss task unlink-external` and `boss chore unlink-external`.
async fn run_unlink_external(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskIdArg,
) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let item = match client
        .send_request(&FrontendRequest::UnlinkWorkItemExternalRef {
            work_item_id: resolved_id,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => item,
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            return Err(CliError::application(message));
        }
        other => return Err(unexpected_event("unlink-external", &other)),
    };
    let (updated, label) = expect_leaf_work_item(item)?;
    let title = format!("Unlinked external ref on {label}");
    print_entity(
        ctx,
        &serde_json::json!({ label: updated }),
        || print_task_details(&title, &updated, None, false),
    )
}

/// One entry in a bulk-create input file. Mirrors the documented
/// schema: `name` and `description` are required; `autostart` and
/// `project_id` (tasks only) are optional per-item overrides of the
/// top-level CLI defaults. Unknown fields are rejected so a typo
/// doesn't silently drop data on the floor.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkCreateItem {
    name: String,
    description: String,
    #[serde(default)]
    autostart: Option<bool>,
    #[serde(default)]
    project_id: Option<String>,
    /// Per-item priority override. Omitted → engine default
    /// (`medium`). Accepts the same `low` / `medium` / `high`
    /// vocabulary as the `--priority` flag.
    #[serde(default)]
    priority: Option<String>,
}

fn read_bulk_input(from_file: &str) -> Result<Vec<BulkCreateItem>, CliError> {
    let raw = if from_file == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|err| CliError::usage(format!("failed to read stdin: {err}")))?;
        buf
    } else {
        std::fs::read_to_string(from_file)
            .map_err(|err| CliError::usage(format!("failed to read {from_file}: {err}")))?
    };
    let items: Vec<BulkCreateItem> = serde_json::from_str(&raw).map_err(|err| {
        CliError::usage(format!(
            "failed to parse {} as a JSON array of items (line {}, column {}): {}",
            display_input_source(from_file),
            err.line(),
            err.column(),
            err,
        ))
    })?;
    if items.is_empty() {
        return Err(CliError::usage(format!(
            "{} contained an empty array; nothing to create",
            display_input_source(from_file),
        )));
    }
    for (index, item) in items.iter().enumerate() {
        if item.name.trim().is_empty() {
            return Err(CliError::usage(format!(
                "item {index}: `name` is required and must not be empty"
            )));
        }
    }
    Ok(items)
}

fn display_input_source(from_file: &str) -> String {
    if from_file == "-" {
        "stdin".to_owned()
    } else {
        from_file.to_owned()
    }
}

async fn run_task_create_many(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskCreateManyArgs,
) -> Result<(), CliError> {
    let items = read_bulk_input(&args.from_file)?;

    // Resolve --product / --project once; per-item project_id (if
    // present) is treated as an already-resolved engine id so we
    // don't pay an extra round-trip per row.
    let product =
        resolve_product_inferable(client, args.product, args.project.as_deref(), ctx).await?;
    let default_project = match args.project {
        Some(selector) => Some(resolve_project(client, &product.id, Some(selector), ctx).await?),
        None => None,
    };

    let default_autostart = !ctx.no_autostart;

    let mut inputs = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let project_id = match item.project_id {
            Some(id) => id,
            None => match default_project.as_ref() {
                Some(project) => project.id.clone(),
                None => {
                    return Err(CliError::usage(format!(
                        "item {index}: no project specified — pass --project as a default or set `project_id` on the item"
                    )));
                }
            },
        };
        inputs.push(CreateTaskInput {
            product_id: product.id.clone(),
            project_id,
            name: item.name,
            description: normalize_non_empty(Some(item.description)),
            autostart: item.autostart.unwrap_or(default_autostart),
            priority: item.priority,
            created_via: Some(CREATED_VIA_CLI.to_owned()),
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        });
    }

    let count = inputs.len();
    let created = create_many_tasks(client, CreateManyTasksInput { items: inputs }).await?;

    print_entity(
        ctx,
        &serde_json::json!({ "tasks": created, "count": created.len() }),
        || {
            if !ctx.quiet {
                println!("Created {} tasks:", created.len());
                print_tasks_table(&created, false);
            }
        },
    )?;
    debug_assert_eq!(created.len(), count);
    Ok(())
}

async fn run_chore_create_many(
    client: &mut BossClient,
    ctx: &RunContext,
    args: ChoreCreateManyArgs,
) -> Result<(), CliError> {
    let items = read_bulk_input(&args.from_file)?;
    let product = resolve_product(client, args.product, ctx).await?;
    let default_autostart = !ctx.no_autostart;

    let mut inputs = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if item.project_id.is_some() {
            return Err(CliError::usage(format!(
                "item {index}: chores do not have a project — remove `project_id`"
            )));
        }
        inputs.push(CreateChoreInput {
            product_id: product.id.clone(),
            name: item.name,
            description: normalize_non_empty(Some(item.description)),
            autostart: item.autostart.unwrap_or(default_autostart),
            priority: item.priority,
            created_via: Some(CREATED_VIA_CLI.to_owned()),
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        });
    }

    let created = create_many_chores(client, CreateManyChoresInput { items: inputs }).await?;
    print_entity(
        ctx,
        &serde_json::json!({ "chores": created, "count": created.len() }),
        || {
            if !ctx.quiet {
                println!("Created {} chores:", created.len());
                print_tasks_table(&created, false);
            }
        },
    )
}

async fn create_many_tasks(
    client: &mut BossClient,
    input: CreateManyTasksInput,
) -> Result<Vec<Task>, CliError> {
    handle_create_many_response(
        client
            .send_request(&FrontendRequest::CreateManyTasks { input })
            .await
            .map_err(CliError::internal)?,
        "tasks create-many",
        |item| match item {
            WorkItem::Task(t) => Ok(t),
            other => Err(CliError::conflict(format!(
                "engine returned non-task item in tasks batch: {:?}",
                std::mem::discriminant(&other),
            ))),
        },
    )
}

async fn create_many_chores(
    client: &mut BossClient,
    input: CreateManyChoresInput,
) -> Result<Vec<Task>, CliError> {
    handle_create_many_response(
        client
            .send_request(&FrontendRequest::CreateManyChores { input })
            .await
            .map_err(CliError::internal)?,
        "chores create-many",
        |item| match item {
            WorkItem::Chore(t) => Ok(t),
            other => Err(CliError::conflict(format!(
                "engine returned non-chore item in chores batch: {:?}",
                std::mem::discriminant(&other),
            ))),
        },
    )
}

fn handle_create_many_response<F>(
    event: FrontendEvent,
    context: &str,
    extract: F,
) -> Result<Vec<Task>, CliError>
where
    F: Fn(WorkItem) -> Result<Task, CliError>,
{
    match event {
        FrontendEvent::WorkItemsCreated { items } => items.into_iter().map(extract).collect(),
        FrontendEvent::WorkItemDuplicateBlocked {
            existing_id,
            existing_short_id,
            name,
            age_secs,
        } => Err(CliError::conflict(format!(
            "Batch rejected: an item named {name:?} was created {age_secs}s ago as \
             T{existing_short_id} ({existing_id}); pass --force-duplicate to bypass."
        ))),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event(context, &other)),
    }
}

/// Accept only `https://github.com/<org>/<repo>/pull/<n>`. Returns the
/// trimmed canonical form on success.
fn validate_github_pr_url(raw: &str) -> Result<&str, CliError> {
    let trimmed = raw.trim();
    let rest = trimmed.strip_prefix("https://github.com/").ok_or_else(|| {
        CliError::usage("PR URL must be of the form https://github.com/<org>/<repo>/pull/<n>")
    })?;
    let mut parts = rest.split('/');
    let org = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    let pull = parts.next().unwrap_or("");
    let number = parts.next().unwrap_or("");
    let extra = parts.next();
    let well_formed = !org.is_empty()
        && !repo.is_empty()
        && pull == "pull"
        && !number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && extra.is_none();
    if !well_formed {
        return Err(CliError::usage(format!(
            "PR URL must be of the form https://github.com/<org>/<repo>/pull/<n>, got `{trimmed}`"
        )));
    }
    Ok(trimmed)
}

async fn delete_work_item(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::DeleteWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemDeleted { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item delete", &other)),
    }
}

async fn run_depend_command(
    command: DependCommand,
    client: &mut BossClient,
    ctx: &RunContext,
) -> Result<(), CliError> {
    match command {
        DependCommand::Add(args) => {
            let dependent = resolve_selector_to_primary_id(client, ctx, &args.dependent, None).await?;
            let prerequisite = resolve_selector_to_primary_id(client, ctx, &args.prerequisite, None).await?;
            let edge = add_dependency(
                client,
                AddDependencyInput {
                    dependent,
                    prerequisite,
                    relation: Some(args.relation),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "edge": edge }), || {
                if !ctx.quiet {
                    println!(
                        "Declared dependency: {} → {} ({})",
                        edge.dependent_id, edge.prerequisite_id, edge.relation
                    );
                }
            })
        }
        DependCommand::Rm(args) => {
            let dependent = resolve_selector_to_primary_id(client, ctx, &args.dependent, None).await?;
            let prerequisite = resolve_selector_to_primary_id(client, ctx, &args.prerequisite, None).await?;
            let removed = remove_dependency(
                client,
                RemoveDependencyInput {
                    dependent: dependent.clone(),
                    prerequisite: prerequisite.clone(),
                    relation: Some(args.relation.clone()),
                },
            )
            .await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "dependent_id": dependent,
                    "prerequisite_id": prerequisite,
                    "relation": args.relation,
                    "removed": removed,
                }),
                || {
                    if !ctx.quiet {
                        if removed {
                            println!(
                                "Removed dependency: {} → {}",
                                dependent, prerequisite,
                            );
                        } else {
                            println!(
                                "No dependency {} → {} (no-op)",
                                dependent, prerequisite,
                            );
                        }
                    }
                },
            )
        }
        DependCommand::List(args) => {
            let selector = resolve_selector_to_primary_id(client, ctx, &args.selector, None).await?;
            let view = list_dependencies(
                client,
                ListDependenciesInput {
                    work_item: selector,
                    direction: Some(args.direction.into()),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "dependencies": view }), || {
                print_dependency_view(&view);
            })
        }
    }
}

async fn add_dependency(
    client: &mut BossClient,
    input: AddDependencyInput,
) -> Result<WorkItemDependency, CliError> {
    match client
        .send_request(&FrontendRequest::AddDependency { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::DependencyAdded { edge } => Ok(edge),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("dependency add", &other)),
    }
}

async fn remove_dependency(
    client: &mut BossClient,
    input: RemoveDependencyInput,
) -> Result<bool, CliError> {
    match client
        .send_request(&FrontendRequest::RemoveDependency { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::DependencyRemoved { removed, .. } => Ok(removed),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("dependency remove", &other)),
    }
}

async fn list_dependencies(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyView, CliError> {
    match client
        .send_request(&FrontendRequest::ListDependencies { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::DependencyList { view } => Ok(view),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("dependency list", &other)),
    }
}

async fn list_dependencies_detailed(
    client: &mut BossClient,
    input: ListDependenciesInput,
) -> Result<WorkItemDependencyDetail, CliError> {
    match client
        .send_request(&FrontendRequest::ListDependenciesDetailed { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::DependencyDetail { detail } => Ok(detail),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("dependency detail", &other)),
    }
}

async fn list_executions_for_item(
    client: &mut BossClient,
    work_item_id: &str,
) -> Result<Vec<WorkExecution>, CliError> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ExecutionsList { mut executions, .. } => {
            executions.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
            executions.truncate(20);
            Ok(executions)
        }
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("executions list", &other)),
    }
}

async fn get_task_runtime(
    client: &mut BossClient,
    work_item_id: &str,
) -> Result<TaskRuntime, CliError> {
    match client
        .send_request(&FrontendRequest::GetTaskRuntime {
            work_item_id: work_item_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::TaskRuntimeResult { runtime } => Ok(runtime),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task runtime", &other)),
    }
}

fn print_executions_section(executions: &[WorkExecution]) {
    if executions.is_empty() {
        return;
    }
    println!();
    println!("Executions ({}):", executions.len());
    for exec in executions {
        let started = exec.started_at.as_deref().unwrap_or("-");
        let finished = exec.finished_at.as_deref().unwrap_or("-");
        print!("  {} [{}] started={} finished={}", exec.id, exec.status, started, finished);
        if let Some(pr) = &exec.pr_url {
            print!(" pr={pr}");
        }
        println!();
    }
}

fn print_dependency_view(view: &WorkItemDependencyView) {
    println!("Dependencies for {}:", view.work_item_id);
    if view.prerequisites.is_empty() && view.dependents.is_empty() {
        println!("  (none)");
        return;
    }
    if !view.prerequisites.is_empty() {
        println!("  Prerequisites ({}):", view.prerequisites.len());
        for edge in &view.prerequisites {
            println!("    {} ({})", edge.prerequisite_id, edge.relation);
        }
    }
    if !view.dependents.is_empty() {
        println!("  Dependents ({}):", view.dependents.len());
        for edge in &view.dependents {
            println!("    {} ({})", edge.dependent_id, edge.relation);
        }
    }
}

/// Print the Dependencies section appended by `boss <kind> show`
/// (Q6). Empty input prints nothing — the surrounding `show` already
/// rendered the rest of the row, and a noisy "Dependencies: (none)"
/// every time would drown out the common case. The body is composed
/// via [`format_dependency_section`] so unit tests can assert on the
/// text without capturing stdout.
fn print_dependency_section(detail: &WorkItemDependencyDetail) {
    for line in format_dependency_section(detail) {
        println!("{line}");
    }
}

/// Pure-function renderer for the Dependencies section. Returns the
/// human-mode lines that [`print_dependency_section`] would emit.
/// Empty result when both sides are empty so the caller can detect
/// "nothing to show" without parsing strings.
fn format_dependency_section(detail: &WorkItemDependencyDetail) -> Vec<String> {
    if detail.prerequisites.is_empty() && detail.dependents.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    lines.push("Dependencies:".to_owned());
    if !detail.prerequisites.is_empty() {
        lines.push(format!(
            "  Prerequisites ({}):",
            detail.prerequisites.len()
        ));
        for edge in &detail.prerequisites {
            lines.push(format_dependency_edge_line(edge, true));
        }
    }
    if !detail.dependents.is_empty() {
        lines.push(format!("  Dependents ({}):", detail.dependents.len()));
        for edge in &detail.dependents {
            lines.push(format_dependency_edge_line(edge, false));
        }
    }
    lines
}

fn format_dependency_edge_line(edge: &DependencyEdge, mark_incomplete: bool) -> String {
    let name = if edge.name.is_empty() {
        String::new()
    } else {
        format!(" \"{}\"", edge.name)
    };
    let suffix = if mark_incomplete && !dependency_status_is_satisfied(&edge.id, &edge.status) {
        "  ← INCOMPLETE"
    } else {
        ""
    };
    format!(
        "    {id:<32}  {status:<10}{name}{suffix}",
        id = edge.id,
        status = edge.status,
    )
}

/// Whether `status` counts as "this prereq is no longer gating its
/// dependent." Mirrors the engine-side rule (Q4 / Q10): tasks /
/// chores satisfy on `done`; projects also satisfy on `archived`.
/// The dependent annotator uses the inverse to print
/// `← INCOMPLETE`.
fn dependency_status_is_satisfied(id: &str, status: &str) -> bool {
    if id.starts_with("proj_") {
        matches!(status, "done" | "archived")
    } else {
        status == "done"
    }
}

async fn reorder_project_tasks(
    client: &mut BossClient,
    project_id: &str,
    task_ids: &[String],
) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::ReorderProjectTasks {
            project_id: project_id.to_owned(),
            task_ids: task_ids.to_vec(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectTasksReordered { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task reorder", &other)),
    }
}

async fn resolve_product(
    client: &mut BossClient,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let products = list_products(client).await?;
    if products.is_empty() {
        return Err(CliError::not_found("no products exist"));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if products.len() == 1 => return Ok(products[0].clone()),
        None if ctx.allow_input => choose_product(&products)?,
        None => {
            return Err(CliError::usage(
                "product is required; pass --product or run interactively",
            ));
        }
    };

    match_products(&products, &selector)
}

/// True when `s` looks like a typed engine work-item id. The engine
/// stamps `prod_…` on products, `proj_…` on projects, and `task_…` on
/// both tasks and chores (chores share the task prefix at the row
/// level, so we don't enumerate `chore_` separately). Slugs are short
/// names like `boss` / `mono` and never collide with these prefixes
/// in practice.
fn is_typed_work_item_id(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("prod_") || s.starts_with("proj_") || s.starts_with("task_")
}

/// Parsed form of a task/chore/project selector.
///
/// Priority order per design Q5 (extended with friendly-id prefix forms):
/// 1. `#42` or `42` or `T441`/`t441`/`P7`/`p7` → short id
/// 2. `boss/42` or `boss/#42` → cross-product short id
/// 3. `task_…` / `proj_…` / `prod_…` → primary id (typed)
/// 4. anything else → slug / existing resolution
#[derive(Debug, Clone)]
enum WorkItemSelector {
    /// `42` or `#42` — short id within the context product.
    ShortId(i64),
    /// `boss/42` or `boss/#42` — short id in the named product slug.
    ProductShortId { product_slug: String, n: i64 },
    /// `task_…` / `proj_…` / `prod_…` — primary engine id.
    PrimaryId(String),
    /// Slug or other selector; fall through to existing resolution.
    Other(String),
}

/// Parse `s` into a [`WorkItemSelector`] per design Q5 grammar.
fn parse_work_item_selector(s: &str) -> WorkItemSelector {
    let s = s.trim();
    // Cross-product form: "boss/42" or "boss/#42"
    if let Some(slash) = s.find('/') {
        let product_slug = &s[..slash];
        let rest = s[slash + 1..].trim_start_matches('#');
        if !product_slug.is_empty() {
            if let Ok(n) = rest.parse::<i64>() {
                if n > 0 {
                    return WorkItemSelector::ProductShortId {
                        product_slug: product_slug.to_owned(),
                        n,
                    };
                }
            }
        }
    }
    // `#42` form (explicit friendly-id prefix)
    if let Some(rest) = s.strip_prefix('#') {
        if let Ok(n) = rest.parse::<i64>() {
            if n > 0 {
                return WorkItemSelector::ShortId(n);
            }
        }
    }
    // `T441` / `t441` / `P12` / `p12` — friendly kanban id (T for tasks/chores, P for projects).
    // Case-insensitive; the leading letter is just visual sugar for the short_id number.
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if first == b'T' || first == b't' || first == b'P' || first == b'p' {
            if let Ok(n) = s[1..].parse::<i64>() {
                if n > 0 {
                    return WorkItemSelector::ShortId(n);
                }
            }
        }
    }
    // Plain integer → short id (Q5 step 2: `#` is optional)
    if let Ok(n) = s.parse::<i64>() {
        if n > 0 {
            return WorkItemSelector::ShortId(n);
        }
    }
    // Primary id prefixes
    if is_typed_work_item_id(s) {
        return WorkItemSelector::PrimaryId(s.to_owned());
    }
    WorkItemSelector::Other(s.to_owned())
}

/// Call the engine's `GetWorkItemByShortId` RPC and return the result.
async fn get_work_item_by_short_id_rpc(
    client: &mut BossClient,
    product_id: &str,
    short_id: i64,
) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::GetWorkItemByShortId {
            product_id: product_id.to_owned(),
            short_id,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemResult { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item fetch by short id", &other)),
    }
}

fn work_item_primary_id(item: &WorkItem) -> &str {
    match item {
        WorkItem::Product(p) => &p.id,
        WorkItem::Project(p) => &p.id,
        WorkItem::Task(t) | WorkItem::Chore(t) => &t.id,
    }
}

/// Resolve any selector form (friendly `T441`, `#42`, plain `42`,
/// cross-product `boss/42`, or primary `task_…` id) to a primary engine
/// id. If the selector is already a primary id or an opaque slug, it is
/// returned unchanged so the engine can reject it with its own error.
async fn resolve_selector_to_primary_id(
    client: &mut BossClient,
    ctx: &RunContext,
    id: &str,
    product: Option<String>,
) -> Result<String, CliError> {
    match parse_work_item_selector(id) {
        WorkItemSelector::ShortId(n) => {
            let product = resolve_product(client, product, ctx).await?;
            let item = get_work_item_by_short_id_rpc(client, &product.id, n).await?;
            Ok(work_item_primary_id(&item).to_owned())
        }
        WorkItemSelector::ProductShortId { product_slug, n } => {
            let product = resolve_product(client, Some(product_slug), ctx).await?;
            let item = get_work_item_by_short_id_rpc(client, &product.id, n).await?;
            Ok(work_item_primary_id(&item).to_owned())
        }
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => Ok(id),
    }
}

/// If `selector` is a typed engine work-item id, look it up and return
/// its product id. Returns `Ok(None)` when the selector isn't shaped
/// like a typed id; callers then fall back to slug / interactive
/// resolution against the existing [`resolve_product`] path.
async fn product_id_from_typed_selector(
    client: &mut BossClient,
    selector: &str,
) -> Result<Option<String>, CliError> {
    let trimmed = selector.trim();
    if !is_typed_work_item_id(trimmed) {
        return Ok(None);
    }
    let item = get_work_item(client, trimmed).await?;
    let product_id = match item {
        WorkItem::Product(p) => p.id,
        WorkItem::Project(p) => p.product_id,
        WorkItem::Task(t) | WorkItem::Chore(t) => t.product_id,
    };
    Ok(Some(product_id))
}

/// Pure validator extracted so the mismatch-handling can be unit-tested
/// without an engine. When `explicit` is `Some`, it must resolve to the
/// same product as `inferred_id`; on mismatch we return a usage error
/// that names both sides so the user can drop the redundant flag.
fn ensure_explicit_product_matches(
    products: &[Product],
    explicit: Option<&str>,
    inferred_id: &str,
    id_hint: &str,
) -> Result<(), CliError> {
    let Some(explicit) = explicit else {
        return Ok(());
    };
    let chosen = match_products(products, explicit)?;
    if chosen.id != inferred_id {
        return Err(CliError::usage(format!(
            "--product {explicit} resolves to {chosen} but {id_hint} belongs to {inferred_id} — drop the redundant --product flag",
            chosen = chosen.id,
        )));
    }
    Ok(())
}

/// Variant of [`resolve_product`] that infers the product from a
/// globally-unique typed work-item id (`proj_…` / `task_…` / `prod_…`)
/// already on the command line. When both an explicit `--product` and
/// a typed-id hint are supplied, the resolved products must agree —
/// mismatches surface as a usage error so the caller can drop the
/// redundant flag.
async fn resolve_product_inferable(
    client: &mut BossClient,
    explicit: Option<String>,
    typed_id_hint: Option<&str>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let inferred_id = match typed_id_hint {
        Some(id) => product_id_from_typed_selector(client, id).await?,
        None => None,
    };

    let Some(inferred_id) = inferred_id else {
        return resolve_product(client, explicit, ctx).await;
    };

    let products = list_products(client).await?;
    let inferred = products
        .iter()
        .find(|p| p.id == inferred_id)
        .cloned()
        .ok_or_else(|| {
            CliError::not_found(format!(
                "id {hint} references product {inferred_id}, but no such product exists",
                hint = typed_id_hint.unwrap_or("(typed id)"),
            ))
        })?;

    ensure_explicit_product_matches(
        &products,
        explicit.as_deref(),
        &inferred.id,
        typed_id_hint.unwrap_or("(typed id)"),
    )?;
    Ok(inferred)
}

async fn resolve_project(
    client: &mut BossClient,
    product_id: &str,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Project, CliError> {
    let projects = list_projects(client, product_id, None).await?;
    if projects.is_empty() {
        return Err(CliError::not_found(
            "no projects exist for the selected product",
        ));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if projects.len() == 1 => return Ok(projects[0].clone()),
        None if ctx.allow_input => choose_project(&projects)?,
        None => {
            return Err(CliError::usage(
                "project is required; pass --project or run interactively",
            ));
        }
    };

    match_projects(&projects, &selector)
}

fn match_products(products: &[Product], selector: &str) -> Result<Product, CliError> {
    if let Some(product) = pick_by_index(products, selector)? {
        return Ok(product);
    }

    let matches = products
        .iter()
        .filter(|product| product.id == selector || product.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown product: {selector}"))
}

fn match_projects(projects: &[Project], selector: &str) -> Result<Project, CliError> {
    // Short id form: "42" or "#42" → match by short_id.
    // This takes priority over the 1-based index so that `boss project show 42`
    // consistently means "the project with short_id 42" everywhere.
    if let WorkItemSelector::ShortId(n) = parse_work_item_selector(selector) {
        let matches = projects
            .iter()
            .filter(|p| p.short_id == Some(n))
            .cloned()
            .collect::<Vec<_>>();
        return resolve_single_match(matches, format!("no project with id #{n}"));
    }

    if let Some(project) = pick_by_index(projects, selector)? {
        return Ok(project);
    }

    let matches = projects
        .iter()
        .filter(|project| project.id == selector || project.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown project: {selector}"))
}

fn resolve_single_match<T>(matches: Vec<T>, not_found_message: String) -> Result<T, CliError> {
    match matches.len() {
        0 => Err(CliError::not_found(not_found_message)),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        _ => Err(CliError::conflict(
            "selector resolved to multiple work items",
        )),
    }
}

fn pick_by_index<T: Clone>(items: &[T], selector: &str) -> Result<Option<T>, CliError> {
    let Ok(index) = selector.parse::<usize>() else {
        return Ok(None);
    };
    if !(1..=items.len()).contains(&index) {
        return Err(CliError::usage(format!(
            "selection {index} is out of range; choose a value between 1 and {}",
            items.len()
        )));
    }
    Ok(Some(items[index - 1].clone()))
}

fn choose_product(products: &[Product]) -> Result<String, CliError> {
    println!("Select a product:");
    for (index, product) in products.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, product.name, product.slug);
    }
    prompt_index_or_selector("Product", products.len()).map_err(CliError::internal)
}

fn choose_project(projects: &[Project]) -> Result<String, CliError> {
    println!("Select a project:");
    for (index, project) in projects.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, project.name, project.slug);
    }
    prompt_index_or_selector("Project", projects.len()).map_err(CliError::internal)
}

fn required_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<String, CliError> {
    if let Some(value) = normalize_non_empty(value) {
        return Ok(value);
    }
    if !ctx.allow_input {
        return Err(CliError::usage(format!(
            "{label} is required; pass it explicitly or omit --no-input"
        )));
    }
    loop {
        let input = prompt_text(label, None).map_err(CliError::internal)?;
        if let Some(value) = normalize_non_empty(Some(input)) {
            return Ok(value);
        }
        eprintln!("{label} cannot be empty.");
    }
}

fn optional_text(
    value: Option<String>,
    label: &str,
    ctx: &RunContext,
) -> Result<Option<String>, CliError> {
    if value.is_some() || !ctx.allow_input {
        return Ok(normalize_non_empty(value));
    }
    let input = prompt_text(label, Some("")).map_err(CliError::internal)?;
    Ok(normalize_non_empty(Some(input)))
}

fn prompt_text(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    match default {
        Some(default) if !default.is_empty() => write!(stdout, "{label} [{default}]: ")?,
        _ => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim_end().to_owned();
    if input.is_empty() {
        Ok(default.unwrap_or_default().to_owned())
    } else {
        Ok(input)
    }
}

fn prompt_index_or_selector(label: &str, count: usize) -> Result<String> {
    loop {
        let input = prompt_text(label, None)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} cannot be empty.");
            continue;
        }
        if let Ok(index) = trimmed.parse::<usize>() {
            if (1..=count).contains(&index) {
                return Ok(index.to_string());
            }
            eprintln!("{label} must be between 1 and {count}.");
            continue;
        }
        return Ok(trimmed.to_owned());
    }
}

/// Glue together the name and optional description into the
/// "prompt text" fed to the repo parser. Mirrors what the engine
/// will eventually see as the chore's contents; the parser only does
/// case-insensitive substring search, so the simple `name\n\ndesc`
/// shape is exactly enough.
fn compose_prompt_text(name: &str, description: Option<&str>) -> String {
    match description.and_then(|d| {
        let trimmed = d.trim();
        if trimmed.is_empty() { None } else { Some(d) }
    }) {
        Some(desc) => format!("{name}\n\n{desc}"),
        None => name.to_owned(),
    }
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn ensure_patch_present(patch: &WorkItemPatch, message: &str) -> Result<(), CliError> {
    let has_fields = patch.name.is_some()
        || patch.description.is_some()
        || patch.status.is_some()
        || patch.goal.is_some()
        || patch.priority.is_some()
        || patch.repo_remote_url.is_some()
        || patch.pr_url.is_some()
        || patch.ordinal.is_some()
        || patch.effort_level.is_some()
        || patch.model_override.is_some()
        || patch.default_model.is_some()
        || patch.dispatch_preamble.is_some()
        || patch.autostart.is_some()
        || patch.blocked_reason.is_some();

    if has_fields {
        Ok(())
    } else {
        Err(CliError::usage(message))
    }
}

fn expect_product(item: WorkItem) -> Result<Product, CliError> {
    match item {
        WorkItem::Product(product) => Ok(product),
        _ => Err(CliError::conflict("work item is not a product")),
    }
}

fn expect_project(item: WorkItem) -> Result<Project, CliError> {
    match item {
        WorkItem::Project(project) => Ok(project),
        _ => Err(CliError::conflict("work item is not a project")),
    }
}

fn expect_task(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Task(task) => Ok(task),
        WorkItem::Chore(_) => Err(CliError::conflict("work item is a chore, not a task")),
        _ => Err(CliError::conflict("work item is not a task")),
    }
}

fn expect_chore(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Chore(task) => Ok(task),
        WorkItem::Task(_) => Err(CliError::conflict("work item is a task, not a chore")),
        _ => Err(CliError::conflict("work item is not a chore")),
    }
}

/// Permissive counterpart of [`expect_task`] / [`expect_chore`]: the
/// kind-agnostic verbs (`show`, `update`, `move`, `delete`, `bind-pr`)
/// accept any leaf work item, so they unwrap the inner [`Task`] and
/// return the kind label (`"task"` or `"chore"`) for user-facing
/// labelling. Products and projects still error — those have their
/// own command surface.
fn expect_leaf_work_item(item: WorkItem) -> Result<(Task, &'static str), CliError> {
    match item {
        WorkItem::Task(task) => Ok((task, "task")),
        WorkItem::Chore(task) => Ok((task, "chore")),
        WorkItem::Product(_) | WorkItem::Project(_) => Err(CliError::conflict(
            "work item is not a task or chore (use `boss product`/`boss project` for those kinds)",
        )),
    }
}

fn unexpected_event(context: &str, event: &FrontendEvent) -> CliError {
    CliError::internal(anyhow::anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(event).unwrap_or_else(|_| "<unserializable>".to_owned())
    ))
}

/// Parsed `--repo <selector>` filter. Per design Q3 +
/// `tools/boss/docs/designs/multi-repo-work-modeling.md` R10:
///   - reject selectors shorter than 2 chars,
///   - match against the *resolved* repo on every row (task override
///     ?? parent product default), not just the override column,
///   - selectors that look like a full URL match the canonicalised
///     URL exactly (case-insensitive),
///   - otherwise treat the selector as a short name and match as
///     case-insensitive prefix of `short_name_for(url)`.
struct RepoSelector {
    /// Lowercased selector — used for both comparison branches.
    lc: String,
    /// `true` when the selector looks like a full URL (contains a
    /// scheme separator or a `git@…:` prefix). URL form ⇒ exact
    /// case-insensitive match; otherwise short-name prefix match.
    is_url_form: bool,
}

impl RepoSelector {
    fn parse(raw: &str) -> Result<Self, CliError> {
        let trimmed = raw.trim();
        if trimmed.len() < 2 {
            return Err(CliError::usage(
                "--repo selector must be at least 2 characters (avoids spurious short-name matches)",
            ));
        }
        let is_url_form = trimmed.contains("://") || trimmed.starts_with("git@");
        Ok(Self {
            lc: trimmed.to_ascii_lowercase(),
            is_url_form,
        })
    }

    /// Match against an effective repo URL. `None` (work item has no
    /// resolution) never matches — `--repo` is a positive filter.
    fn matches(&self, resolved_url: Option<&str>) -> bool {
        let Some(url) = resolved_url else { return false };
        let lc_url = url.to_ascii_lowercase();
        if self.is_url_form {
            return lc_url == self.lc;
        }
        let short = short_name_for(&lc_url);
        short.starts_with(&self.lc)
    }
}

/// Match `short_name_for` from the design — basename of the path
/// minus `.git`. Pure string parse, no registry lookup.
fn short_name_for(url: &str) -> &str {
    let after_slash = url.rsplit('/').next().unwrap_or(url);
    let after_colon = after_slash.rsplit(':').next().unwrap_or(after_slash);
    after_colon.trim_end_matches(".git")
}

/// Resolve a task / chore's effective repo: its own override wins;
/// fall back to the product's default. Used by the `--repo` filter
/// so `--repo nimbus` finds inherited matches too (design R10 / Q3).
fn resolved_repo_for_task<'a>(task: &'a Task, product_repo: Option<&'a str>) -> Option<&'a str> {
    task.repo_remote_url.as_deref().or(product_repo)
}

fn apply_task_list_filters(
    items: Vec<Task>,
    statuses: &[TaskStatus],
    priorities: &[TaskPriority],
    match_term: Option<&str>,
    ids: &[String],
    limit: Option<usize>,
    repo: Option<&RepoSelector>,
    product_repo: Option<&str>,
) -> Vec<Task> {
    let allowed_statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let allowed_priorities: Vec<&str> = priorities.iter().map(|p| p.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let lc_term = match_term.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|task| {
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&task.status.as_str()) {
                return false;
            }
            if !allowed_priorities.is_empty()
                && !allowed_priorities.contains(&task.priority.as_str())
            {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(task.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = task.name.to_lowercase();
                let desc = task.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            if let Some(selector) = repo {
                if !selector.matches(resolved_repo_for_task(task, product_repo)) {
                    return false;
                }
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

fn apply_project_list_filters(
    items: Vec<Project>,
    statuses: &[ProjectStatus],
    match_term: Option<&str>,
    ids: &[String],
    limit: Option<usize>,
    repo: Option<&RepoSelector>,
    product_repo: Option<&str>,
) -> Vec<Project> {
    let allowed_statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let lc_term = match_term.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|project| {
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&project.status.as_str())
            {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(project.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = project.name.to_lowercase();
                let desc = project.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            if let Some(selector) = repo {
                // Projects have no repo column today; they resolve
                // through their parent product, so every project under
                // a given product shares the same effective repo.
                if !selector.matches(product_repo) {
                    return false;
                }
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

fn print_entity<T, F>(ctx: &RunContext, json_value: &T, human: F) -> Result<(), CliError>
where
    T: Serialize,
    F: FnOnce(),
{
    match ctx.output_mode {
        OutputMode::Json => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            serde_json::to_writer_pretty(&mut lock, json_value).map_err(CliError::internal)?;
            writeln!(lock).map_err(CliError::internal)?;
        }
        OutputMode::Human => human(),
    }
    Ok(())
}

fn print_products_table(products: &[Product]) {
    let show_default_model = products.iter().any(|p| p.default_model.is_some());
    let mut table = Table::new();
    let mut header = vec!["ID", "SLUG", "NAME", "STATUS", "REPO"];
    if show_default_model {
        header.push("DEFAULT MODEL");
    }
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    for product in products {
        let mut row = vec![
            product.id.as_str(),
            product.slug.as_str(),
            product.name.as_str(),
            product.status.as_str(),
            product.repo_remote_url.as_deref().unwrap_or(""),
        ];
        if show_default_model {
            row.push(product.default_model.as_deref().unwrap_or(""));
        }
        table.add_row(row);
    }
    println!("{table}");
}

fn print_projects_table(projects: &[Project], with_primary_id: bool) {
    let show_short_id = projects.iter().any(|p| p.short_id.is_some());
    let mut table = Table::new();
    let mut header: Vec<&str> = Vec::new();
    if show_short_id {
        header.push("#");
    }
    if !show_short_id || with_primary_id {
        header.push("ID");
    }
    header.extend_from_slice(&["SLUG", "NAME", "STATUS", "PRIORITY", "GOAL"]);
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    for project in projects {
        let mut row: Vec<String> = Vec::new();
        if show_short_id {
            let friendly = project
                .short_id
                .map(|n| format!("P{n}"))
                .unwrap_or_default();
            row.push(friendly);
        }
        if !show_short_id || with_primary_id {
            row.push(project.id.clone());
        }
        row.push(project.slug.clone());
        row.push(project.name.clone());
        row.push(project.status.clone());
        row.push(project.priority.clone());
        row.push(project.goal.clone());
        table.add_row(row);
    }
    println!("{table}");
}

fn print_tasks_table(tasks: &[Task], with_primary_id: bool) {
    // Only render the EFFORT column when at least one row in the
    // view carries a level — keeps the common case (legacy rows)
    // narrow but surfaces the new field as soon as it lands on
    // anything. JSON output always carries the field; this is a
    // human-readability nicety only.
    let show_effort = tasks.iter().any(|t| t.effort_level.is_some());
    let show_short_id = tasks.iter().any(|t| t.short_id.is_some());
    let mut table = Table::new();
    let mut header: Vec<&str> = Vec::new();
    if show_short_id {
        header.push("#");
    }
    if !show_short_id || with_primary_id {
        header.push("ID");
    }
    header.extend_from_slice(&["NAME", "STATUS", "PRIORITY"]);
    if show_effort {
        header.push("EFFORT");
    }
    header.extend_from_slice(&["PROJECT", "ORDINAL", "PR URL"]);
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    for task in tasks {
        let ordinal = task
            .ordinal
            .map(|value| value.to_string())
            .unwrap_or_default();
        let friendly = task.short_id.map(|n| format!("T{n}")).unwrap_or_default();
        let effort_str = task.effort_level.map(|l| l.as_str().to_owned()).unwrap_or_default();
        let mut row: Vec<String> = Vec::new();
        if show_short_id {
            row.push(friendly);
        }
        if !show_short_id || with_primary_id {
            row.push(task.id.clone());
        }
        row.push(task.name.clone());
        row.push(task.status.clone());
        row.push(task.priority.clone());
        if show_effort {
            row.push(effort_str);
        }
        row.push(task.project_id.clone().unwrap_or_default());
        row.push(ordinal);
        row.push(task.pr_url.clone().unwrap_or_default());
        table.add_row(row);
    }
    println!("{table}");
}

fn print_product_details(title: &str, product: &Product) {
    println!("{title}");
    println!("ID: {}", product.id);
    println!("Name: {}", product.name);
    println!("Slug: {}", product.slug);
    println!("Status: {}", product.status);
    println!("Repo: {}", product.repo_remote_url.as_deref().unwrap_or(""));
    if let Some(model) = product.default_model.as_deref() {
        println!("Default model: {model}");
    }
    if let Some(preamble) = product.dispatch_preamble.as_deref() {
        println!("Dispatch preamble: {preamble}");
    }
    if let Some(kind) = product.external_tracker_kind.as_deref() {
        println!("External tracker:");
        println!("  Kind: {kind}");
        if let Some(config) = product.external_tracker_config.as_ref() {
            if kind == "github" {
                if let Some(org) = config["org"].as_str() {
                    println!("  Org: {org}");
                }
                if let Some(repo) = config["repo"].as_str() {
                    println!("  Repo: {repo}");
                }
                if let Some(project_number) = config["project_number"].as_u64() {
                    println!("  Project: {project_number}");
                }
                let reverse_close = config["reverse_close"].as_bool().unwrap_or(false);
                println!("  Reverse-close: {reverse_close}");
            } else {
                println!("  Config: {config}");
            }
        }
    }
    if !product.description.is_empty() {
        println!("Description: {}", product.description);
    }
}

/// Render the trailing portion of the `Repo:` line emitted by `boss
/// <kind> show` — i.e. everything after the `Repo: ` prefix. Mirrors
/// the engine's `resolve_repo_for_work_item`: per-row override wins,
/// otherwise the product default, otherwise "(none — work item cannot
/// dispatch)".
///
/// `override_url` is the work item's own `repo_remote_url` column.
/// Projects always pass `None` since they don't carry their own
/// override column today; the parenthetical "(inherited from product
/// `<slug>`)" is the only non-`none` shape projects can produce.
fn format_repo_line(override_url: Option<&str>, product: &Product) -> String {
    if let Some(url) = override_url.filter(|s| !s.is_empty()) {
        return format!("{url} (override on this work item)");
    }
    if let Some(url) = product
        .repo_remote_url
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        return format!("{url} (inherited from product `{}`)", product.slug);
    }
    "(none — work item cannot dispatch)".to_owned()
}

fn print_project_details(title: &str, project: &Project, parent_product: Option<&Product>, with_primary_id: bool) {
    println!("{title}");
    if let Some(n) = project.short_id {
        if with_primary_id {
            println!("P{n}  \x1b[2m{}\x1b[0m", project.id);
        } else {
            println!("P{n}");
        }
    } else {
        println!("ID: {}", project.id);
    }
    println!("Product ID: {}", project.product_id);
    println!("Name: {}", project.name);
    println!("Slug: {}", project.slug);
    println!("Status: {}", project.status);
    if let Some(product) = parent_product {
        // Projects have no per-row override column today, so the
        // override slot is always `None`; the line reduces to the
        // product-inherited or "none" shape.
        println!("Repo: {}", format_repo_line(None, product));
    }
    println!("Priority: {}", project.priority);
    if !project.goal.is_empty() {
        println!("Goal: {}", project.goal);
    }
    if !project.description.is_empty() {
        println!("Description: {}", project.description);
    }
}

/// Format the "Design doc:" line appended by `boss project show` /
/// `boss project set-design-doc`. `None` means "no line should be
/// emitted" — used by `Show` so the unset case stays silent rather
/// than printing "Design doc: (not set)" on every project that
/// hasn't been pointed yet. The set / broken cases produce a
/// concrete line so the user can see at a glance which path the doc
/// resolves to and whether the pointer is healthy.
fn format_project_design_doc_line(state: &ProjectDesignDocState) -> Option<String> {
    match state {
        ProjectDesignDocState::NotSet => None,
        ProjectDesignDocState::Resolved { resolved, web_url, .. } => {
            Some(format!("{} ({})", resolved.path, web_url))
        }
        ProjectDesignDocState::Broken { reason } => {
            Some(format!("(broken) {reason}"))
        }
    }
}

/// What `boss project open-design` should do once the engine has
/// resolved the pointer. Built by [`decide_open_design_action`] from
/// the engine's `ProjectDesignDocState` + the `--web` flag; consumed
/// by the handler to either print or launch the right target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OpenDesignAction {
    /// Open a local file inside a leased workspace (the
    /// same-product fast path). The path is workspace-relative and
    /// gets joined to whatever cube currently has leased — but the
    /// CLI doesn't talk to cube, so we surface the doc's repo-relative
    /// path and let the editor / opener resolve it from the user's
    /// cwd (i.e. the worker's leased workspace).
    LocalFile { path: PathBuf, web_url: String },
    /// Open the GitHub web URL. Used for `External` pointers, for
    /// `SameProduct`/`OtherProduct` when no workspace is leased, and
    /// whenever `--web` is explicit.
    Web { url: String },
}

impl OpenDesignAction {
    fn human_summary(&self) -> String {
        match self {
            Self::LocalFile { path, .. } => format!("Opening {} in $EDITOR", path.display()),
            Self::Web { url } => format!("Opening {url} in browser"),
        }
    }

    fn as_json(&self) -> serde_json::Value {
        match self {
            Self::LocalFile { path, web_url } => serde_json::json!({
                "kind": "local_file",
                "path": path.to_string_lossy(),
                "web_url": web_url,
            }),
            Self::Web { url } => serde_json::json!({
                "kind": "web",
                "url": url,
            }),
        }
    }

    fn launch(&self) -> Result<(), CliError> {
        match self {
            Self::LocalFile { path, web_url } => match std::env::var_os("EDITOR") {
                Some(editor) => spawn_opener(editor, [path.as_os_str()]),
                None => {
                    eprintln!(
                        "warning: $EDITOR not set; falling back to web URL ({web_url})",
                    );
                    spawn_opener_for_url(web_url)
                }
            },
            Self::Web { url } => spawn_opener_for_url(url),
        }
    }
}

fn spawn_opener<I, A>(program: I, args: A) -> Result<(), CliError>
where
    I: AsRef<std::ffi::OsStr>,
    A: IntoIterator,
    A::Item: AsRef<std::ffi::OsStr>,
{
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|err| CliError::internal(anyhow::anyhow!("failed to launch opener: {err}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliError::internal(anyhow::anyhow!(
            "opener exited with status {status}",
        )))
    }
}

fn spawn_opener_for_url(url: &str) -> Result<(), CliError> {
    #[cfg(target_os = "macos")]
    let program: &str = "open";
    #[cfg(not(target_os = "macos"))]
    let program: &str = "xdg-open";
    spawn_opener(program, [url])
}

/// Decide which open action [`OpenDesignAction`] to take for a
/// resolved pointer. Pure function; unit-tested. Errors when the
/// pointer is `NotSet` (caller error — should not invoke
/// `open-design` on a project without a pointer) or `Broken` (the
/// pointer can't resolve to a target).
fn decide_open_design_action(
    state: &ProjectDesignDocState,
    force_web: bool,
) -> Result<OpenDesignAction, CliError> {
    match state {
        ProjectDesignDocState::NotSet => Err(CliError::not_found(
            "project has no design-doc pointer (set one with `boss project set-design-doc`)",
        )),
        ProjectDesignDocState::Broken { reason } => Err(CliError::conflict(format!(
            "design-doc pointer is broken: {reason}",
        ))),
        ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            ..
        } => {
            if force_web {
                return Ok(OpenDesignAction::Web {
                    url: web_url.clone(),
                });
            }
            let can_use_filesystem = matches!(
                resolved.kind,
                ResolvedDesignDocKind::SameProduct { .. }
                    | ResolvedDesignDocKind::OtherProduct { .. },
            ) && workspace_path.is_some();
            if can_use_filesystem {
                Ok(OpenDesignAction::LocalFile {
                    path: PathBuf::from(&resolved.path),
                    web_url: web_url.clone(),
                })
            } else {
                Ok(OpenDesignAction::Web {
                    url: web_url.clone(),
                })
            }
        }
    }
}

fn print_task_details(title: &str, task: &Task, parent_product: Option<&Product>, with_primary_id: bool) {
    println!("{title}");
    if let Some(n) = task.short_id {
        if with_primary_id {
            println!("T{n}  \x1b[2m{}\x1b[0m", task.id);
        } else {
            println!("T{n}");
        }
    } else {
        println!("ID: {}", task.id);
    }
    println!("Product ID: {}", task.product_id);
    if let Some(project_id) = &task.project_id {
        println!("Project ID: {}", project_id);
    }
    println!("Name: {}", task.name);
    println!("Kind: {}", task.kind);
    println!("Status: {}", task.status);
    if let Some(product) = parent_product {
        println!(
            "Repo: {}",
            format_repo_line(task.repo_remote_url.as_deref(), product),
        );
    }
    println!("Priority: {}", task.priority);
    if let Some(level) = task.effort_level {
        println!("Effort: {level}");
    }
    if let Some(model) = task.model_override.as_deref() {
        println!("Model override: {model}");
    }
    println!("Source: {}", task.created_via);
    if let Some(ordinal) = task.ordinal {
        println!("Ordinal: {}", ordinal);
    }
    if let Some(pr_url) = &task.pr_url {
        println!("PR URL: {}", pr_url);
    }
    if !task.description.is_empty() {
        println!("Description: {}", task.description);
    }
}

fn resolve_install_root() -> Result<PathBuf, CliError> {
    if let Ok(root) = std::env::var("BOSS_INSTALL_ROOT") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        CliError::internal(anyhow::anyhow!("HOME is not set; cannot resolve install root"))
    })?;
    Ok(PathBuf::from(home).join("Applications"))
}

fn resolve_state_root_for_uninstall() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss"))
}

fn confirm_interactive(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "y" | "Y")
}

fn run_uninstall_command(args: UninstallArgs, flags: &GlobalFlags) -> Result<(), CliError> {
    let install_root = resolve_install_root()?;
    // True when no BOSS_INSTALL_ROOT override is in effect, meaning we are
    // operating on the canonical ~/Applications install. Only in that case
    // should we stop the engine — stopping the default pid file when the
    // caller set a sandbox install root would kill the host engine instead.
    let using_default_install_root = std::env::var("BOSS_INSTALL_ROOT").is_err();
    let app_path = install_root.join("Boss.app");

    if !app_path.exists() {
        if flags.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "not_installed",
                    "message": "no installed Boss found",
                    "searched": app_path.display().to_string(),
                })
            );
        } else {
            eprintln!(
                "boss uninstall: no installed Boss found at {}",
                app_path.display()
            );
            eprintln!("If Boss is running from a dev build, uninstall is not applicable.");
        }
        return Err(CliError::internal(anyhow::anyhow!("no installed Boss to uninstall")));
    }

    let state_root = resolve_state_root_for_uninstall();

    if !flags.json {
        println!("This will remove:");
        println!("  {}", app_path.display());
        if args.purge_state {
            if let Some(ref state) = state_root {
                println!("  {} (--purge-state)", state.display());
            }
        }
    }

    if !args.yes {
        let confirmed = if flags.json {
            true
        } else {
            confirm_interactive("Proceed with uninstall?")
        };
        if !confirmed {
            if flags.json {
                println!(
                    "{}",
                    serde_json::json!({"status": "cancelled", "reason": "user declined"})
                );
            } else {
                println!("uninstall cancelled");
            }
            return Ok(());
        }
    }

    if using_default_install_root {
        let pid_path = std::env::var("BOSS_ENGINE_PID_PATH")
            .unwrap_or_else(|_| boss_client::DEFAULT_PID_PATH.to_owned());
        let _ = stop_engine(&pid_path);
    } else {
        eprintln!(
            "note: not stopping engine: BOSS_INSTALL_ROOT is set; \
             assuming the caller manages their own engine lifecycle"
        );
    }

    std::fs::remove_dir_all(&app_path).map_err(|e| {
        CliError::internal(anyhow::anyhow!("failed to remove {}: {e}", app_path.display()))
    })?;

    let mut removed = vec![app_path.display().to_string()];

    if args.purge_state {
        if let Some(state) = state_root {
            if state.exists() {
                std::fs::remove_dir_all(&state).map_err(|e| {
                    CliError::internal(anyhow::anyhow!(
                        "failed to remove {}: {e}",
                        state.display()
                    ))
                })?;
                removed.push(state.display().to_string());
            }
        }
    }

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "uninstalled",
                "removed": removed,
            })
        );
    } else {
        println!("Uninstalled Boss.");
        for path in &removed {
            println!("  removed: {path}");
        }
    }

    Ok(())
}
#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        BindPrAction, BulkCreateItem, ChoreCommand, Cli, Commands, EffortLevelArg, MoveTarget,
        OpenDesignAction, ProductCommand, ProductStatus, ProjectCommand, ProjectStatus,
        RepoSelector, TaskCommand, classify_bind_pr, decide_open_design_action,
        ensure_explicit_product_matches, expect_leaf_work_item, format_project_design_doc_line,
        format_repo_line, is_typed_work_item_id, pick_by_index, short_name_for,
        validate_github_pr_url,
    };
    use boss_protocol::{
        Product, Project, ProjectDesignDocState, ResolvedDesignDoc, ResolvedDesignDocKind, Task,
        WorkItem,
    };

    #[test]
    fn move_target_maps_review_to_in_review() {
        assert_eq!(MoveTarget::Review.as_status(), "in_review");
        assert_eq!(MoveTarget::Doing.as_status(), "active");
        assert_eq!(MoveTarget::Blocked.as_status(), "blocked");
    }

    #[test]
    fn parses_product_create_command() {
        let cli = Cli::parse_from(["boss", "product", "create", "--name", "Boss"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Create(args),
            } => {
                assert_eq!(args.name.as_deref(), Some("Boss"));
            }
            _ => panic!("expected product create command"),
        }
    }

    #[test]
    fn parses_task_move_command() {
        let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "review"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert!(matches!(args.target, MoveTarget::Review));
            }
            _ => panic!("expected task move command"),
        }
    }

    /// `boss task move <chore-id>` is the case from the work item: the
    /// CLI used to error with "work item is a chore, not a task" even
    /// though the engine already knew the kind from the id. After the
    /// consolidation the parser still accepts it (parsing was never
    /// the issue) and the runtime hands it to the same handler as a
    /// task id; this test pins the parser shape.
    #[test]
    fn parses_task_move_command_with_chore_shaped_id() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "move",
            "task_18ad79226b0ca630_1a",
            "--to",
            "blocked",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_18ad79226b0ca630_1a");
                assert!(matches!(args.target, MoveTarget::Blocked));
            }
            _ => panic!("expected task move command"),
        }
    }

    /// `boss chore move` is now a thin alias for the same handler.
    #[test]
    fn parses_chore_move_command() {
        let cli = Cli::parse_from(["boss", "chore", "move", "task_xyz", "--to", "active"]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_xyz");
                assert!(matches!(args.target, MoveTarget::Active));
            }
            _ => panic!("expected chore move command"),
        }
    }

    fn dummy_task(id: &str, kind: &str) -> Task {
        Task {
            id: id.to_owned(),
            product_id: "prod_1".to_owned(),
            project_id: None,
            kind: kind.to_owned(),
            name: "n".to_owned(),
            description: String::new(),
            status: "todo".to_owned(),
            ordinal: None,
            pr_url: None,
            deleted_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            autostart: true,
            last_status_actor: "human".to_owned(),
            priority: "medium".to_owned(),
            created_via: "unknown".to_owned(),
            repo_remote_url: None,
            blocked_reason: None,
            blocked_attempt_id: None,
            effort_level: None,
            model_override: None,
            ci_attempt_budget: None,
            ci_attempts_used: 0,
            blocked_signals: vec![],
            ci_required_state: None,
            ci_required_detail: None,
            review_required_state: None,
            review_required_detail: None,
            pr_state_polled_at: None,
            merge_queue_state: None,
            short_id: None,
            external_ref: None,
        }
    }

    #[test]
    fn expect_leaf_accepts_task_and_chore() {
        let task = dummy_task("task_1", "task");
        let (unwrapped, label) = expect_leaf_work_item(WorkItem::Task(task.clone())).unwrap();
        assert_eq!(unwrapped.id, "task_1");
        assert_eq!(label, "task");

        let chore = dummy_task("task_2", "chore");
        let (unwrapped, label) = expect_leaf_work_item(WorkItem::Chore(chore)).unwrap();
        assert_eq!(unwrapped.id, "task_2");
        assert_eq!(label, "chore");
    }

    #[test]
    fn expect_leaf_rejects_product_and_project() {
        let product = Product {
            id: "prod_1".to_owned(),
            name: "n".to_owned(),
            slug: "n".to_owned(),
            description: String::new(),
            repo_remote_url: None,
            status: "active".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            default_model: None,
            dispatch_preamble: None,
            external_tracker_kind: None,
            external_tracker_config: None,
        };
        assert!(expect_leaf_work_item(WorkItem::Product(product)).is_err());

        let project = Project {
            id: "proj_1".to_owned(),
            product_id: "prod_1".to_owned(),
            name: "n".to_owned(),
            slug: "n".to_owned(),
            description: String::new(),
            goal: String::new(),
            status: "planned".to_owned(),
            priority: "medium".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            last_status_actor: "human".to_owned(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: None,
            short_id: None,
        };
        assert!(expect_leaf_work_item(WorkItem::Project(project)).is_err());
    }

    /// Helper for the `format_repo_line` golden tests: build a Product
    /// with `repo_remote_url` set or unset and a fixed slug so the
    /// inherited-line text is predictable.
    fn dummy_product(slug: &str, repo: Option<&str>) -> Product {
        Product {
            id: "prod_1".to_owned(),
            name: slug.to_owned(),
            slug: slug.to_owned(),
            description: String::new(),
            repo_remote_url: repo.map(str::to_owned),
            status: "active".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            default_model: None,
            dispatch_preamble: None,
            external_tracker_kind: None,
            external_tracker_config: None,
        }
    }

    /// Golden output: a work item with its own non-empty
    /// `repo_remote_url` reports "(override on this work item)" — the
    /// product's value is ignored in this branch even if it's also set.
    #[test]
    fn format_repo_line_override_on_work_item() {
        let product = dummy_product("boss", Some("git@github.com:spinyfin/mono.git"));
        let rendered = format_repo_line(Some("git@github.com:myorg/nimbus.git"), &product);
        assert_eq!(
            rendered,
            "git@github.com:myorg/nimbus.git (override on this work item)",
        );
    }

    /// Golden output: no override (or empty override) falls through to
    /// the product's value, attributing via the product's slug.
    #[test]
    fn format_repo_line_inherits_from_product() {
        let product = dummy_product("boss", Some("git@github.com:spinyfin/mono.git"));
        let rendered = format_repo_line(None, &product);
        assert_eq!(
            rendered,
            "git@github.com:spinyfin/mono.git (inherited from product `boss`)",
        );

        // Empty-string override is treated as "no override" — mirrors
        // the `--repo ""` clear semantics on update.
        let rendered = format_repo_line(Some(""), &product);
        assert_eq!(
            rendered,
            "git@github.com:spinyfin/mono.git (inherited from product `boss`)",
        );
    }

    /// Golden output: neither row supplies a URL → the work item
    /// cannot dispatch. Matches the engine's `resolve_repo_for_work_item`
    /// returning `None`.
    #[test]
    fn format_repo_line_none_when_nothing_resolves() {
        let product = dummy_product("boss", None);
        let rendered = format_repo_line(None, &product);
        assert_eq!(rendered, "(none — work item cannot dispatch)");

        // Empty string on the product is equivalent to unset.
        let product = dummy_product("boss", Some(""));
        let rendered = format_repo_line(None, &product);
        assert_eq!(rendered, "(none — work item cannot dispatch)");

        // Empty override + empty product still falls through to none.
        let rendered = format_repo_line(Some(""), &product);
        assert_eq!(rendered, "(none — work item cannot dispatch)");
    }

    #[test]
    fn parses_product_delete_command() {
        let cli = Cli::parse_from(["boss", "product", "delete", "boss"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Delete(args),
            } => {
                assert_eq!(args.selector, "boss");
            }
            _ => panic!("expected product delete command"),
        }
    }

    #[test]
    fn parses_product_move_command() {
        let cli = Cli::parse_from(["boss", "product", "move", "boss", "--to", "paused"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Move(args),
            } => {
                assert_eq!(args.selector, "boss");
                assert!(matches!(args.target, ProductStatus::Paused));
            }
            _ => panic!("expected product move command"),
        }
    }

    #[test]
    fn parses_project_delete_command() {
        let cli = Cli::parse_from(["boss", "project", "delete", "work-cli", "--product", "boss"]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::Delete(args),
            } => {
                assert_eq!(args.selector, "work-cli");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected project delete command"),
        }
    }

    #[test]
    fn parses_project_move_command() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "move",
            "work-cli",
            "--product",
            "boss",
            "--to",
            "done",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::Move(args),
            } => {
                assert_eq!(args.selector, "work-cli");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert!(matches!(args.target, ProjectStatus::Done));
            }
            _ => panic!("expected project move command"),
        }
    }

    #[test]
    fn product_status_archived_serializes_to_archived() {
        assert_eq!(ProductStatus::Archived.as_str(), "archived");
        assert_eq!(ProductStatus::Active.as_str(), "active");
        assert_eq!(ProductStatus::Paused.as_str(), "paused");
    }

    #[test]
    fn project_status_archived_serializes_to_archived() {
        assert_eq!(ProjectStatus::Archived.as_str(), "archived");
        assert_eq!(ProjectStatus::Done.as_str(), "done");
        assert_eq!(ProjectStatus::Planned.as_str(), "planned");
    }

    #[test]
    fn numeric_selection_is_one_based() {
        let values = vec!["alpha".to_owned(), "beta".to_owned()];
        assert_eq!(
            pick_by_index(&values, "2").unwrap(),
            Some("beta".to_owned())
        );
        assert!(pick_by_index(&values, "0").is_err());
    }

    #[test]
    fn validate_github_pr_url_accepts_canonical_form() {
        let url = "https://github.com/spinyfin/mono/pull/238";
        assert_eq!(validate_github_pr_url(url).unwrap(), url);
        // surrounding whitespace is trimmed
        assert_eq!(
            validate_github_pr_url("  https://github.com/a/b/pull/1\n").unwrap(),
            "https://github.com/a/b/pull/1"
        );
    }

    #[test]
    fn validate_github_pr_url_rejects_malformed() {
        for bad in [
            "",
            "not-a-url",
            "http://github.com/a/b/pull/1",        // wrong scheme
            "https://gitlab.com/a/b/pull/1",       // wrong host
            "https://github.com/a/b/pulls/1",      // typo
            "https://github.com/a/b/issues/1",     // wrong noun
            "https://github.com/a/b/pull/",        // missing number
            "https://github.com/a/b/pull/abc",     // non-numeric
            "https://github.com/a/b/pull/1/files", // trailing path
            "https://github.com//repo/pull/1",     // empty org
            "https://github.com/org//pull/1",      // empty repo
        ] {
            assert!(
                validate_github_pr_url(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn classify_bind_pr_first_time_when_unset() {
        assert_eq!(
            classify_bind_pr(None, "https://github.com/a/b/pull/1"),
            BindPrAction::FirstTime
        );
        // Empty-string prior (engine normalizes empty → None, but defend
        // against the wire-format edge case) is treated as unset.
        assert_eq!(
            classify_bind_pr(Some(""), "https://github.com/a/b/pull/1"),
            BindPrAction::FirstTime
        );
    }

    #[test]
    fn classify_bind_pr_idempotent_on_same_url() {
        let url = "https://github.com/a/b/pull/1";
        assert_eq!(classify_bind_pr(Some(url), url), BindPrAction::Idempotent);
    }

    #[test]
    fn classify_bind_pr_overwrite_on_different_url() {
        let prior = "https://github.com/a/b/pull/1";
        let new = "https://github.com/a/b/pull/2";
        assert_eq!(
            classify_bind_pr(Some(prior), new),
            BindPrAction::Overwrite { previous: prior }
        );
    }

    #[test]
    fn parses_task_bind_pr_command() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "bind-pr",
            "task_1",
            "https://github.com/a/b/pull/9",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::BindPr(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert_eq!(args.pr_url, "https://github.com/a/b/pull/9");
            }
            _ => panic!("expected task bind-pr command"),
        }
    }

    #[test]
    fn parses_chore_bind_pr_command() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "bind-pr",
            "task_2",
            "https://github.com/a/b/pull/10",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::BindPr(args),
            } => {
                assert_eq!(args.id, "task_2");
                assert_eq!(args.pr_url, "https://github.com/a/b/pull/10");
            }
            _ => panic!("expected chore bind-pr command"),
        }
    }

    #[test]
    fn parses_task_link_external_command() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "link-external",
            "task_1",
            "--kind",
            "github",
            "--id",
            "spinyfin/mono#560",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::LinkExternal(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert_eq!(args.kind, "github");
                assert_eq!(args.upstream_id, "spinyfin/mono#560");
            }
            _ => panic!("expected task link-external command"),
        }
    }

    #[test]
    fn parses_chore_link_external_command() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "link-external",
            "task_2",
            "--kind",
            "github",
            "--id",
            "spinyfin/mono#561",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::LinkExternal(args),
            } => {
                assert_eq!(args.id, "task_2");
                assert_eq!(args.kind, "github");
                assert_eq!(args.upstream_id, "spinyfin/mono#561");
            }
            _ => panic!("expected chore link-external command"),
        }
    }

    #[test]
    fn parses_task_unlink_external_command() {
        let cli = Cli::parse_from(["boss", "task", "unlink-external", "task_3"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::UnlinkExternal(args),
            } => {
                assert_eq!(args.id, "task_3");
            }
            _ => panic!("expected task unlink-external command"),
        }
    }

    #[test]
    fn parses_chore_unlink_external_command() {
        let cli = Cli::parse_from(["boss", "chore", "unlink-external", "task_4"]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::UnlinkExternal(args),
            } => {
                assert_eq!(args.id, "task_4");
            }
            _ => panic!("expected chore unlink-external command"),
        }
    }

    #[test]
    fn parses_task_create_many_command() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "create-many",
            "--from-file",
            "tasks.json",
            "--product",
            "boss",
            "--project",
            "plan",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::CreateMany(args),
            } => {
                assert_eq!(args.from_file, "tasks.json");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.project.as_deref(), Some("plan"));
            }
            _ => panic!("expected task create-many command"),
        }
    }

    #[test]
    fn parses_chore_create_many_with_stdin() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "create-many",
            "--from-file",
            "-",
            "--product",
            "boss",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::CreateMany(args),
            } => {
                assert_eq!(args.from_file, "-");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected chore create-many command"),
        }
    }

    #[test]
    fn bulk_create_item_deserializes_full_form() {
        let raw = r#"{
            "name": "do thing",
            "description": "details",
            "autostart": false,
            "project_id": "proj_abc"
        }"#;
        let item: BulkCreateItem = serde_json::from_str(raw).unwrap();
        assert_eq!(item.name, "do thing");
        assert_eq!(item.description, "details");
        assert_eq!(item.autostart, Some(false));
        assert_eq!(item.project_id.as_deref(), Some("proj_abc"));
    }

    #[test]
    fn bulk_create_item_rejects_unknown_fields() {
        let raw = r#"{ "name": "x", "description": "y", "autosatrt": true }"#;
        let err = serde_json::from_str::<BulkCreateItem>(raw).expect_err("typo must fail");
        assert!(err.to_string().contains("autosatrt"), "{err}");
    }

    #[test]
    fn parses_project_set_design_doc_with_path() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "set-design-doc",
            "pointer",
            "--product",
            "boss",
            "--path",
            "tools/boss/docs/designs/foo.md",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::SetDesignDoc(args),
            } => {
                assert_eq!(args.selector, "pointer");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(
                    args.path.as_deref(),
                    Some("tools/boss/docs/designs/foo.md"),
                );
                assert!(!args.unset);
                assert!(args.repo.is_none());
                assert!(args.branch.is_none());
            }
            _ => panic!("expected project set-design-doc command"),
        }
    }

    #[test]
    fn parses_project_set_design_doc_with_repo_and_branch() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "set-design-doc",
            "pointer",
            "--product",
            "boss",
            "--path",
            "designs/foo.md",
            "--repo",
            "https://github.com/myorg/wiki.git",
            "--branch",
            "trunk",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::SetDesignDoc(args),
            } => {
                assert_eq!(
                    args.repo.as_deref(),
                    Some("https://github.com/myorg/wiki.git"),
                );
                assert_eq!(args.branch.as_deref(), Some("trunk"));
            }
            _ => panic!("expected project set-design-doc command"),
        }
    }

    #[test]
    fn parses_project_set_design_doc_with_unset() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "set-design-doc",
            "pointer",
            "--product",
            "boss",
            "--unset",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::SetDesignDoc(args),
            } => {
                assert!(args.unset);
                assert!(args.path.is_none());
            }
            _ => panic!("expected project set-design-doc command"),
        }
    }

    /// Clap enforces the mutual exclusion between `--unset` and the
    /// pointer-set flags so the engine never sees an ambiguous
    /// request.
    #[test]
    fn rejects_unset_combined_with_path() {
        let err = Cli::try_parse_from([
            "boss",
            "project",
            "set-design-doc",
            "pointer",
            "--unset",
            "--path",
            "designs/foo.md",
        ])
        .expect_err("unset + path must conflict");
        let rendered = err.to_string();
        assert!(rendered.contains("--unset") || rendered.contains("--path"), "{rendered}");
    }

    /// `--repo` without `--path` is meaningless — clap rejects it at
    /// parse time so the user fixes the call rather than seeing a
    /// confusing engine error.
    #[test]
    fn rejects_repo_without_path() {
        let err = Cli::try_parse_from([
            "boss",
            "project",
            "set-design-doc",
            "pointer",
            "--repo",
            "https://github.com/x/y.git",
        ])
        .expect_err("repo without path must error");
        assert!(err.to_string().contains("--path"), "{err}");
    }

    #[test]
    fn parses_project_open_design_print_and_web() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "open-design",
            "pointer",
            "--product",
            "boss",
            "--web",
            "--print",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::OpenDesign(args),
            } => {
                assert_eq!(args.selector, "pointer");
                assert!(args.web);
                assert!(args.print);
            }
            _ => panic!("expected project open-design command"),
        }
    }

    fn resolved_state(
        kind: ResolvedDesignDocKind,
        local: bool,
    ) -> ProjectDesignDocState {
        ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
                branch: "main".to_owned(),
                path: "tools/boss/docs/designs/foo.md".to_owned(),
                kind,
            },
            workspace_path: local
                .then(|| "/tmp/mono-agent-007".to_owned()),
            web_url: "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/designs/foo.md"
                .to_owned(),
            raw_content_url: Some(
                "https://raw.githubusercontent.com/spinyfin/mono/main/tools/boss/docs/designs/foo.md"
                    .to_owned(),
            ),
        }
    }

    /// Same-product pointer with a leased workspace picks the
    /// filesystem fast path (renderer / `$EDITOR`), not the web URL.
    #[test]
    fn open_design_same_product_with_workspace_uses_local_file() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct { product_id: "prod_1".into() },
            true,
        );
        let action = decide_open_design_action(&state, false).unwrap();
        match action {
            OpenDesignAction::LocalFile { path, web_url } => {
                assert_eq!(path.to_string_lossy(), "tools/boss/docs/designs/foo.md");
                assert!(web_url.starts_with("https://github.com/"));
            }
            other => panic!("expected LocalFile, got {other:?}"),
        }
    }

    /// Without a leased workspace the fast path is unavailable —
    /// fall through to the web URL even for same-product pointers.
    #[test]
    fn open_design_same_product_without_workspace_falls_back_to_web() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct { product_id: "prod_1".into() },
            false,
        );
        let action = decide_open_design_action(&state, false).unwrap();
        assert!(matches!(action, OpenDesignAction::Web { .. }));
    }

    /// `--web` forces the web URL regardless of workspace state.
    #[test]
    fn open_design_force_web_overrides_local_path() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct { product_id: "prod_1".into() },
            true,
        );
        let action = decide_open_design_action(&state, true).unwrap();
        assert!(matches!(action, OpenDesignAction::Web { .. }));
    }

    /// External pointers always open in the browser — there's no
    /// workspace shortcut for repos Boss doesn't track.
    #[test]
    fn open_design_external_always_uses_web() {
        let state = resolved_state(ResolvedDesignDocKind::External, true);
        let action = decide_open_design_action(&state, false).unwrap();
        assert!(matches!(action, OpenDesignAction::Web { .. }));
    }

    #[test]
    fn open_design_not_set_errors() {
        let err = decide_open_design_action(&ProjectDesignDocState::NotSet, false)
            .expect_err("not-set must error");
        assert!(err.to_string().contains("no design-doc pointer"), "{err}");
    }

    #[test]
    fn open_design_broken_errors() {
        let state = ProjectDesignDocState::Broken {
            reason: "missing repo".to_owned(),
        };
        let err = decide_open_design_action(&state, false).expect_err("broken must error");
        assert!(err.to_string().contains("broken"), "{err}");
    }

    #[test]
    fn design_doc_line_omits_unset_state() {
        assert!(format_project_design_doc_line(&ProjectDesignDocState::NotSet).is_none());
    }

    #[test]
    fn design_doc_line_renders_resolved_state() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct { product_id: "prod_1".into() },
            false,
        );
        let line = format_project_design_doc_line(&state).expect("resolved → line");
        assert!(line.contains("tools/boss/docs/designs/foo.md"));
        assert!(line.contains("https://github.com/"));
    }

    #[test]
    fn design_doc_line_flags_broken_state() {
        let state = ProjectDesignDocState::Broken {
            reason: "no repo".to_owned(),
        };
        let line = format_project_design_doc_line(&state).expect("broken → line");
        assert!(line.contains("(broken)"));
        assert!(line.contains("no repo"));
    }

    #[test]
    fn parses_chore_create_with_repo_override() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "create",
            "--product",
            "work",
            "--name",
            "fix it",
            "--repo",
            "git@github.com:myorg/nimbus.git",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Create(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("work"));
                assert_eq!(
                    args.repo_remote_url.as_deref(),
                    Some("git@github.com:myorg/nimbus.git")
                );
            }
            _ => panic!("expected chore create command"),
        }
    }

    #[test]
    fn parses_task_create_with_repo_override() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "create",
            "--product",
            "boss",
            "--project",
            "plan",
            "--name",
            "n",
            "--repo",
            "https://github.com/myorg/wiki.git",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Create(args),
            } => {
                assert_eq!(
                    args.repo_remote_url.as_deref(),
                    Some("https://github.com/myorg/wiki.git")
                );
            }
            _ => panic!("expected task create command"),
        }
    }

    /// `--repo ""` on update is the canonical "clear the override"
    /// form (mirrors `--pr-url ""`). Clap surfaces it as
    /// `Some("")`; the engine canonicaliser turns the empty string
    /// into `None` so the task inherits from the product again.
    #[test]
    fn parses_task_update_with_repo_clear() {
        let cli = Cli::parse_from([
            "boss", "task", "update", "task_1", "--repo", "",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Update(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert_eq!(args.repo_remote_url.as_deref(), Some(""));
            }
            _ => panic!("expected task update command"),
        }
    }

    #[test]
    fn parses_chore_create_with_effort_and_model() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "create",
            "--product",
            "boss",
            "--name",
            "fix it",
            "--effort",
            "large",
            "--model",
            "claude-opus-4-7",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Create(args),
            } => {
                assert!(matches!(args.effort, Some(EffortLevelArg::Large)));
                assert_eq!(args.model.as_deref(), Some("claude-opus-4-7"));
            }
            _ => panic!("expected chore create command"),
        }
    }

    /// `--effort` only accepts the five documented values; anything
    /// else fails at parse time with a clear clap error listing the
    /// valid set.
    #[test]
    fn rejects_invalid_effort_level_at_parse_time() {
        let result = Cli::try_parse_from([
            "boss",
            "chore",
            "create",
            "--product",
            "boss",
            "--name",
            "x",
            "--effort",
            "galaxybrain",
        ]);
        let err = result.expect_err("expected clap to reject the value");
        let rendered = err.to_string();
        // clap renders the allowed set; the exact framing changes
        // between clap versions but the level names are stable.
        assert!(rendered.contains("trivial"), "{rendered}");
        assert!(rendered.contains("max"), "{rendered}");
    }

    #[test]
    fn parses_task_update_with_effort_clear_and_model_clear() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "update",
            "task_1",
            "--unset-effort",
            "--unset-model",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Update(args),
            } => {
                assert!(args.unset_effort);
                assert!(args.unset_model);
                assert!(args.effort.is_none());
                assert!(args.model.is_none());
            }
            _ => panic!("expected task update command"),
        }
    }

    /// `--effort` and `--unset-effort` are mutually exclusive — the
    /// `conflicts_with` attribute on the args struct makes clap
    /// reject the combination.
    #[test]
    fn task_update_rejects_effort_and_unset_effort_together() {
        let result = Cli::try_parse_from([
            "boss",
            "task",
            "update",
            "task_1",
            "--effort",
            "small",
            "--unset-effort",
        ]);
        assert!(result.is_err(), "expected clap to reject mutually exclusive flags");
    }

    #[test]
    fn parses_product_set_default_model_with_model() {
        let cli = Cli::parse_from([
            "boss",
            "product",
            "set-default-model",
            "boss",
            "--model",
            "sonnet",
        ]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::SetDefaultModel(args),
            } => {
                assert_eq!(args.selector, "boss");
                assert_eq!(args.model.as_deref(), Some("sonnet"));
                assert!(!args.unset);
            }
            _ => panic!("expected product set-default-model command"),
        }
    }

    #[test]
    fn parses_product_set_default_model_with_unset() {
        let cli = Cli::parse_from([
            "boss",
            "product",
            "set-default-model",
            "boss",
            "--unset",
        ]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::SetDefaultModel(args),
            } => {
                assert!(args.unset);
                assert!(args.model.is_none());
            }
            _ => panic!("expected product set-default-model command"),
        }
    }

    /// `set-default-model` rejects `--model` and `--unset` together
    /// at the parser; the "neither was supplied" case is caught in
    /// the runtime handler (the selector positional sits outside the
    /// mutual-exclusion group so the parser can still resolve it).
    #[test]
    fn product_set_default_model_rejects_model_with_unset() {
        let result = Cli::try_parse_from([
            "boss",
            "product",
            "set-default-model",
            "boss",
            "--model",
            "sonnet",
            "--unset",
        ]);
        assert!(
            result.is_err(),
            "expected clap to reject --model and --unset together",
        );
    }

    #[test]
    fn parses_chore_update_with_repo_set() {
        let cli = Cli::parse_from([
            "boss",
            "chore",
            "update",
            "task_xyz",
            "--repo",
            "git@github.com:myorg/nimbus.git",
        ]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Update(args),
            } => {
                assert_eq!(
                    args.repo_remote_url.as_deref(),
                    Some("git@github.com:myorg/nimbus.git")
                );
            }
            _ => panic!("expected chore update command"),
        }
    }

    #[test]
    fn parses_task_list_with_repo_filter() {
        let cli = Cli::parse_from([
            "boss", "task", "list", "--product", "work", "--repo", "nimbus",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::List(args),
            } => {
                assert_eq!(args.repo.as_deref(), Some("nimbus"));
            }
            _ => panic!("expected task list command"),
        }
    }

    #[test]
    fn short_name_for_handles_ssh_and_https() {
        assert_eq!(
            short_name_for("git@github.com:spinyfin/mono.git"),
            "mono"
        );
        assert_eq!(
            short_name_for("https://github.com/spinyfin/mono.git"),
            "mono"
        );
        assert_eq!(short_name_for("https://github.com/foo/bar"), "bar");
    }

    #[test]
    fn repo_selector_rejects_short_input() {
        assert!(RepoSelector::parse("").is_err());
        assert!(RepoSelector::parse("m").is_err());
        assert!(RepoSelector::parse("  m ").is_err(), "whitespace doesn't count");
        assert!(RepoSelector::parse("mo").is_ok());
    }

    #[test]
    fn repo_selector_short_name_prefix_match() {
        let sel = RepoSelector::parse("nim").unwrap();
        assert!(sel.matches(Some("git@github.com:myorg/nimbus.git")));
        assert!(sel.matches(Some("https://github.com/other/nimbus-platform.git")));
        // Wrong product but same repo short-name → match (Q3).
        assert!(sel.matches(Some("https://github.com/foo/nimbus")));
        // Different repo, prefix doesn't match.
        assert!(!sel.matches(Some("git@github.com:myorg/mono.git")));
        // Unresolved row never matches.
        assert!(!sel.matches(None));
    }

    /// Inherited match: a task with no override but whose parent
    /// product points at `nimbus` should match `--repo nimbus`. The
    /// CLI resolves against the *effective* repo, not the raw column.
    #[test]
    fn repo_selector_matches_inherited_product_default() {
        let sel = RepoSelector::parse("nimbus").unwrap();
        let task = dummy_task("task_1", "task");
        assert!(task.repo_remote_url.is_none());
        let resolved =
            super::resolved_repo_for_task(&task, Some("git@github.com:myorg/nimbus.git"));
        assert!(sel.matches(resolved));
    }

    #[test]
    fn repo_selector_full_url_form_is_exact_match() {
        let sel = RepoSelector::parse("git@github.com:myorg/nimbus.git").unwrap();
        // case-insensitive exact match
        assert!(sel.matches(Some("git@github.com:myorg/nimbus.git")));
        assert!(sel.matches(Some("GIT@GITHUB.COM:MYORG/NIMBUS.GIT")));
        // a different repo with the same short name does NOT match
        // when the selector is the URL form.
        assert!(!sel.matches(Some("git@github.com:other/nimbus.git")));
    }

    #[test]
    fn typed_work_item_id_prefixes_are_recognized() {
        assert!(is_typed_work_item_id("prod_18ae0000_1"));
        assert!(is_typed_work_item_id("proj_18ae0000_1"));
        assert!(is_typed_work_item_id("task_18ae0000_1"));
        // whitespace is tolerated — the resolver trims before lookup.
        assert!(is_typed_work_item_id("  proj_abc  "));
        // slugs / arbitrary names are not typed ids.
        assert!(!is_typed_work_item_id("boss"));
        assert!(!is_typed_work_item_id("work-cli"));
        assert!(!is_typed_work_item_id(""));
        // chore_ is not used at the engine row level — chores share
        // the task_ prefix.
        assert!(!is_typed_work_item_id("chore_18ae0000_1"));
    }

    #[test]
    fn friendly_tnnn_form_parses_as_short_id() {
        use super::{WorkItemSelector, parse_work_item_selector};
        // uppercase T
        assert!(matches!(parse_work_item_selector("T441"), WorkItemSelector::ShortId(441)));
        // lowercase t
        assert!(matches!(parse_work_item_selector("t441"), WorkItemSelector::ShortId(441)));
        // leading whitespace is trimmed
        assert!(matches!(parse_work_item_selector("  T12  "), WorkItemSelector::ShortId(12)));
        // P-form (projects)
        assert!(matches!(parse_work_item_selector("P7"), WorkItemSelector::ShortId(7)));
        assert!(matches!(parse_work_item_selector("p100"), WorkItemSelector::ShortId(100)));
        // zero is rejected (short_ids are positive)
        assert!(matches!(parse_work_item_selector("T0"), WorkItemSelector::Other(_)));
        // non-digit suffix is NOT a short id — falls through to Other
        assert!(matches!(parse_work_item_selector("Tabc"), WorkItemSelector::Other(_)));
        // plain primary id is still PrimaryId, not confused with T-form
        assert!(matches!(parse_work_item_selector("task_18ae0000_1"), WorkItemSelector::PrimaryId(_)));
    }

    /// `boss project show proj_…` accepts a globally-unique typed id
    /// without `--product`. The parser shape pin is the user-facing
    /// half of the inference fix; the engine half is exercised by
    /// the in-process integration test in `tests/infer_product.rs`.
    #[test]
    fn parses_project_show_with_typed_id_and_no_product() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "show",
            "proj_18aeacce8acf9140_27",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::Show(args),
            } => {
                assert_eq!(args.selector, "proj_18aeacce8acf9140_27");
                assert!(args.product.is_none());
            }
            _ => panic!("expected project show command"),
        }
    }

    #[test]
    fn parses_task_list_with_project_typed_id_and_no_product() {
        let cli = Cli::parse_from([
            "boss",
            "task",
            "list",
            "--project",
            "proj_18aeacce8acf9140_27",
        ]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::List(args),
            } => {
                assert_eq!(args.project.as_deref(), Some("proj_18aeacce8acf9140_27"));
                assert!(args.product.is_none());
            }
            _ => panic!("expected task list command"),
        }
    }

    fn product_with_id(id: &str, slug: &str) -> Product {
        Product {
            id: id.to_owned(),
            name: slug.to_owned(),
            slug: slug.to_owned(),
            description: String::new(),
            repo_remote_url: None,
            status: "active".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            default_model: None,
            dispatch_preamble: None,
            external_tracker_kind: None,
            external_tracker_config: None,
        }
    }

    #[test]
    fn explicit_product_validator_accepts_omitted_explicit() {
        let products = vec![product_with_id("prod_1", "boss")];
        assert!(
            ensure_explicit_product_matches(&products, None, "prod_1", "proj_x").is_ok()
        );
    }

    #[test]
    fn explicit_product_validator_accepts_matching_id_or_slug() {
        let products = vec![product_with_id("prod_1", "boss")];
        assert!(
            ensure_explicit_product_matches(&products, Some("prod_1"), "prod_1", "proj_x").is_ok()
        );
        assert!(
            ensure_explicit_product_matches(&products, Some("boss"), "prod_1", "proj_x").is_ok()
        );
    }

    /// When the user passes `--product` AND a typed id whose product
    /// disagrees, we surface a usage error naming both sides instead
    /// of silently picking one. Same shape as the engine-side
    /// "product/project disagree" check.
    #[test]
    fn explicit_product_validator_rejects_mismatch() {
        let products = vec![
            product_with_id("prod_1", "boss"),
            product_with_id("prod_2", "mono"),
        ];
        let err = ensure_explicit_product_matches(&products, Some("mono"), "prod_1", "proj_x")
            .expect_err("disagreement must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("mono"), "{msg}");
        assert!(msg.contains("prod_1"), "{msg}");
    }
}
