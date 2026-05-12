use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use boss_client::{
    BossClient, Discovery, engine_socket_reachable, ensure_engine_running, running_engine_pid,
    stop_engine,
};
use boss_protocol::{
    AddDependencyInput, CREATED_VIA_CLI, CreateChoreInput, CreateManyChoresInput,
    CreateManyTasksInput, CreateProductInput, CreateProjectInput, CreateTaskInput,
    DependencyDirection, DependencyEdge, DependencyFilter, FrontendEvent, FrontendRequest,
    ListDependenciesInput, Product, Project, ProjectDesignDocState, RemoveDependencyInput,
    ResolveProjectDesignDocOutput, ResolvedDesignDocKind, SetProjectDesignDocInput, Task,
    WorkItem, WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView, WorkItemPatch,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use serde::Serialize;

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
    /// Two effects, both off-by-default:
    ///   1. The CLI will not transparently start the engine when
    ///      its socket is unreachable.
    ///   2. `boss task create` / `boss chore create` create the work
    ///      item but the engine will NOT auto-dispatch a worker for
    ///      it. The new chore/task stays in the `todo` column until
    ///      something explicitly schedules it (`bossctl work start
    ///      <id>` or a kanban drag-to-Doing).
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
    /// Flip a `conflict_resolutions` attempt to `failed` with a
    /// reason. Worker-facing escape hatch: the resolution worker calls
    /// this when it hits a stop condition (semantic obsolescence,
    /// product decision required, architectural mismatch) and chooses
    /// not to push.
    MarkFailed(EngineConflictsMarkFailedArgs),
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
}

#[derive(Debug, Clone, Args)]
struct ProjectListArgs {
    #[arg(long)]
    product: Option<String>,

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
}

#[derive(Debug, Clone, Args)]
struct TaskListArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

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
    id: String,
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

#[tokio::main]
async fn main() -> ExitCode {
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
            "Omit --no-autostart unless you explicitly need to forbid engine startup or auto-dispatch on `task create` / `chore create`.",
            "Kind-agnostic verbs (show, update, move, delete, depend, bind-pr) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        selector_semantics: vec![
            "Product selectors accept a product id, slug, or 1-based interactive index. For agent use, prefer slug or id, not numeric indexes.",
            "Project selectors accept a project id, slug, or 1-based interactive index within the selected product. For agent use, prefer slug or id, not numeric indexes.",
            "Task and chore commands that operate on an existing item use the item id, not slug. The id alone disambiguates kind, so `boss task move <chore-id>` and `boss chore move <task-id>` are accepted equivalents of `boss task move <task-id>` / `boss chore move <chore-id>`.",
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
                    product_id: product.id,
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
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Created project", &project, None);
            })
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
                || print_projects_table(&projects),
            )
        }
        ProjectCommand::Show(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
                    print_project_details("Project", &project, Some(&product));
                    if let Some(line) = format_project_design_doc_line(&design_doc.state) {
                        println!("Design doc: {line}");
                    }
                    print_dependency_section(&detail);
                },
            )
        }
        ProjectCommand::Update(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
                print_project_details("Updated project", &project, None);
            })
        }
        ProjectCommand::Delete(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "project": moved }), || {
                print_project_details("Moved project", &moved, None);
            })
        }
        ProjectCommand::SetDesignDoc(args) => {
            if !args.unset && args.path.is_none() {
                return Err(CliError::usage(
                    "provide --path <p> (with optional --repo/--branch) or --unset",
                ));
            }
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
                    print_project_details("Updated project", &updated, None);
                    if let Some(line) = format_project_design_doc_line(&resolved.state) {
                        println!("Design doc: {line}");
                    }
                },
            )
        }
        ProjectCommand::OpenDesign(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            let name = required_text(args.name, "Task name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
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
                    repo_remote_url: normalize_non_empty(args.repo_remote_url),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task, None);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
                print_tasks_table(&tasks)
            })
        }
        TaskCommand::Show(args) => run_show_leaf(&mut client, ctx, args).await,
        TaskCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        TaskCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        TaskCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        TaskCommand::Reorder(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
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
            let chore = create_chore(
                &mut client,
                CreateChoreInput {
                    product_id: product.id,
                    name,
                    description,
                    autostart: !ctx.no_autostart,
                    priority: args.priority.map(|priority| priority.as_str().to_owned()),
                    created_via: Some(CREATED_VIA_CLI.to_owned()),
                    repo_remote_url: normalize_non_empty(args.repo_remote_url),
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore, None);
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
                print_tasks_table(&chores)
            })
        }
        ChoreCommand::Show(args) => run_show_leaf(&mut client, ctx, args).await,
        ChoreCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        ChoreCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        ChoreCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        ChoreCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        ChoreCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        ChoreCommand::CreateMany(args) => run_chore_create_many(&mut client, ctx, args).await,
    }
}

/// Shared handler for `boss task show <id>` and `boss chore show <id>`.
/// Routes any leaf work item id through the same path; the JSON key
/// and human-mode label match the actual kind of the returned item.
async fn run_show_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskIdArg,
) -> Result<(), CliError> {
    let (item, label) = expect_leaf_work_item(get_work_item(client, &args.id).await?)?;
    let product = expect_product(get_work_item(client, &item.product_id).await?)?;
    let detail = list_dependencies_detailed(
        client,
        ListDependenciesInput {
            work_item: item.id.clone(),
            direction: Some(DependencyDirection::Both),
        },
    )
    .await?;
    print_entity(
        ctx,
        &serde_json::json!({ label: item, "dependencies": detail }),
        || {
            print_task_details(label_titlecase(label), &item, Some(&product));
            print_dependency_section(&detail);
        },
    )
}

/// Shared handler for `boss task update` and `boss chore update`.
async fn run_update_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskUpdateArgs,
) -> Result<(), CliError> {
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
        ..WorkItemPatch::default()
    };
    ensure_patch_present(
        &patch,
        "provide at least one field to update, such as --status, --priority, --pr-url, or --repo",
    )?;
    let (item, label) = expect_leaf_work_item(update_work_item(client, &args.id, patch).await?)?;
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Updated {label}"), &item, None);
    })
}

/// Shared handler for `boss task move` and `boss chore move`.
async fn run_move_leaf(
    client: &mut BossClient,
    ctx: &RunContext,
    args: TaskMoveArgs,
) -> Result<(), CliError> {
    let patch = WorkItemPatch {
        status: Some(args.target.as_status().to_owned()),
        ..WorkItemPatch::default()
    };
    let (item, label) = expect_leaf_work_item(update_work_item(client, &args.id, patch).await?)?;
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Moved {label}"), &item, None);
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
    let label = match get_work_item(client, &args.id).await {
        Ok(item) => expect_leaf_work_item(item).map(|(_, l)| l).unwrap_or("item"),
        Err(_) => "item",
    };
    delete_work_item(client, &args.id).await?;
    print_entity(
        ctx,
        &serde_json::json!({ "id": args.id, "deleted": true }),
        || {
            if !ctx.quiet {
                println!("Deleted {label} {}", args.id);
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

    let (existing, label) = expect_leaf_work_item(get_work_item(client, &args.id).await?)?;
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
    let (updated, _) = expect_leaf_work_item(update_work_item(client, &args.id, patch).await?)?;

    let title = format!("Bound PR to {label}");
    print_entity(
        ctx,
        &serde_json::json!({
            label: updated,
            "rebinding": prior_url.is_some(),
            "previous_pr_url": prior_url,
        }),
        || print_task_details(&title, &updated, None),
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
    let product = resolve_product(client, args.product, ctx).await?;
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
                print_tasks_table(&created);
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
        });
    }

    let created = create_many_chores(client, CreateManyChoresInput { items: inputs }).await?;
    print_entity(
        ctx,
        &serde_json::json!({ "chores": created, "count": created.len() }),
        || {
            if !ctx.quiet {
                println!("Created {} chores:", created.len());
                print_tasks_table(&created);
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
            let edge = add_dependency(
                client,
                AddDependencyInput {
                    dependent: args.dependent,
                    prerequisite: args.prerequisite,
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
            let removed = remove_dependency(
                client,
                RemoveDependencyInput {
                    dependent: args.dependent.clone(),
                    prerequisite: args.prerequisite.clone(),
                    relation: Some(args.relation.clone()),
                },
            )
            .await?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "dependent_id": args.dependent,
                    "prerequisite_id": args.prerequisite,
                    "relation": args.relation,
                    "removed": removed,
                }),
                || {
                    if !ctx.quiet {
                        if removed {
                            println!(
                                "Removed dependency: {} → {}",
                                args.dependent, args.prerequisite,
                            );
                        } else {
                            println!(
                                "No dependency {} → {} (no-op)",
                                args.dependent, args.prerequisite,
                            );
                        }
                    }
                },
            )
        }
        DependCommand::List(args) => {
            let view = list_dependencies(
                client,
                ListDependenciesInput {
                    work_item: args.selector.clone(),
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
        || patch.ordinal.is_some();

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
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "REPO"]);
    for product in products {
        table.add_row(vec![
            product.id.as_str(),
            product.slug.as_str(),
            product.name.as_str(),
            product.status.as_str(),
            product.repo_remote_url.as_deref().unwrap_or(""),
        ]);
    }
    println!("{table}");
}

fn print_projects_table(projects: &[Project]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "PRIORITY", "GOAL"]);
    for project in projects {
        table.add_row(vec![
            project.id.as_str(),
            project.slug.as_str(),
            project.name.as_str(),
            project.status.as_str(),
            project.priority.as_str(),
            project.goal.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_tasks_table(tasks: &[Task]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "ID", "NAME", "STATUS", "PRIORITY", "PROJECT", "ORDINAL", "PR URL",
        ]);
    for task in tasks {
        let ordinal = task
            .ordinal
            .map(|value| value.to_string())
            .unwrap_or_default();
        table.add_row(vec![
            task.id.as_str(),
            task.name.as_str(),
            task.status.as_str(),
            task.priority.as_str(),
            task.project_id.as_deref().unwrap_or(""),
            ordinal.as_str(),
            task.pr_url.as_deref().unwrap_or(""),
        ]);
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

fn print_project_details(title: &str, project: &Project, parent_product: Option<&Product>) {
    println!("{title}");
    println!("ID: {}", project.id);
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
            local_workspace_available,
            web_url,
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
            ) && *local_workspace_available;
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

fn print_task_details(title: &str, task: &Task, parent_product: Option<&Product>) {
    println!("{title}");
    println!("ID: {}", task.id);
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        BindPrAction, BulkCreateItem, ChoreCommand, Cli, Commands, MoveTarget, OpenDesignAction,
        ProductCommand, ProductStatus, ProjectCommand, ProjectStatus, RepoSelector, TaskCommand,
        classify_bind_pr, decide_open_design_action, expect_leaf_work_item, format_repo_line,
        format_project_design_doc_line, pick_by_index, short_name_for, validate_github_pr_url,
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
            local_workspace_available: local,
            web_url: "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/designs/foo.md"
                .to_owned(),
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
}
