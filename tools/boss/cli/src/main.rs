use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::Result;
use boss_client::{
    BossClient, Discovery, engine_socket_reachable, ensure_engine_running, running_engine_pid, stop_engine,
};
use boss_protocol::{
    AddDependencyInput, Attention, AttentionGroup, Automation, AutomationPatch, AutomationRun, AutomationTrigger,
    CREATED_VIA_CLI, CiBudgetSnapshot, CiRemediation, ConflictResolution, CreateAttentionInput, CreateAutomationInput,
    CreateChoreInput, CreateInvestigationInput, CreateManyChoresInput, CreateManyTasksInput, CreateProductInput,
    CreateProjectInput, CreateRevisionInput, CreateTaskInput, DependencyDirection, DependencyEdge, DependencyFilter,
    EditorialAction, EditorialRules, EffortAuditReport, EffortLevel, EngineAttemptListEntry, FrontendEvent,
    FrontendRequest, GitHubAuthStateDto, LinkExternalRefInput, ListDependenciesInput, OrgAuthState, PrWorkItemMatch,
    Product, Project, ProjectDesignDocState, RemoveDependencyInput, ResolveProjectDesignDocOutput,
    ResolvedDesignDocKind, SetProductEditorialRulesInput, SetProductExternalTrackerInput, SetProjectDesignDocInput,
    Task, TaskRuntime, WorkExecution, WorkItem, WorkItemDependency, WorkItemDependencyDetail, WorkItemDependencyView,
    WorkItemPatch,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use serde::Serialize;

mod buildkite_release;
mod repo_resolution;
use boss_github as github_app;
use git_utils::repo_slug::short_name_for;

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

    /// Don't auto-dispatch a worker for newly created work items.
    ///
    /// This is purely about **worker dispatch**, not engine
    /// availability — the CLI still transparently starts the engine
    /// if needed, because the engine is the system of record for any
    /// work item. To suppress transparent engine startup, use
    /// `--no-engine-autostart` instead.
    ///
    /// Two effects, both off-by-default:
    ///   1. `boss task create` / `boss chore create` create the work
    ///      item but the engine will NOT auto-dispatch a worker for
    ///      it. The new chore/task stays in the `todo` column until
    ///      something explicitly schedules it (`bossctl work start
    ///      <id>` or a kanban drag-to-Doing).
    ///   2. `boss project create` still files the project AND its
    ///      auto-spawned `kind=design` seed task, but the seed task
    ///      is born with `autostart=false` so the engine does not
    ///      dispatch a worker against it. Use this to author the
    ///      design brief on the seed task (via `boss task update
    ///      <design-task-id> --description ...`) before releasing it
    ///      with `bossctl work start <design-task-id>`.
    #[arg(long, global = true)]
    no_autostart: bool,

    /// Don't transparently start the engine when its socket is
    /// unreachable.
    ///
    /// By default the CLI brings the engine up on demand so it can
    /// service the request (the engine is the system of record for
    /// all work items). Pass this when you explicitly do not want the
    /// CLI to spawn an engine — the command then fails if the engine
    /// is not already reachable. This is independent of
    /// `--no-autostart`, which only governs worker dispatch.
    #[arg(long, global = true)]
    no_engine_autostart: bool,

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
    /// Manage automations: standing, scheduled maintenance instructions that
    /// periodically triage and spawn work outside the normal backlog.
    ///
    /// Automations live in a per-product `A<n>` namespace (`A1`, `A2`, …)
    /// and run in a dedicated 3-agent pool. The tasks they produce carry
    /// `source_automation_id` and are surfaced only in the Automations tab
    /// (excluded from the main kanban).
    ///
    /// See `tools/boss/docs/designs/maintenance-tasks.md` for the full design.
    Automation {
        #[command(subcommand)]
        command: AutomationCommand,
    },
    /// Manage attentions: actionable notifications agents raise to pull the
    /// human into the loop (questions and followups).
    ///
    /// Attentions group into attention groups (`A<n>` or `atg_…` ids); the
    /// group is the unit the human reads and acts on, producing a single
    /// downstream artifact when actioned.
    ///
    /// See `tools/boss/docs/designs/attentions.md` for the full design.
    Attention {
        #[command(subcommand)]
        command: AttentionCommand,
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
    /// File a bug or feature request against Boss itself.
    ///
    /// Reads a markdown bug report from the given FILE (or stdin if FILE
    /// is `-`) and opens a GitHub issue against `spinyfin/mono` — the
    /// upstream repo where Boss is developed.
    ///
    /// The first non-blank line of the file is taken as the issue title.
    /// If it begins with `# ` (a markdown H1) the marker is stripped.
    /// The remainder of the file becomes the issue body. Pass `--title`
    /// to override; in that case the entire file body is used verbatim.
    ///
    /// Credentials: authenticates as a registered GitHub App using
    /// credentials embedded at build time. Signs a short-lived JWT
    /// with the App's private key, swaps it for an installation access
    /// token, then files via the REST API. `boss shake` deliberately
    /// does NOT fall back to `gh issue create` — the user's corporate
    /// environment has a non-standard `gh` install that would silently
    /// mask failures.
    Shake(ShakeArgs),
    /// Trigger a Boss release build via the configured Buildkite pipeline.
    ///
    /// Posts a new build to the `flunge/mono` Buildkite pipeline
    /// (branch=main) and prints the URL of the triggered build so you
    /// can follow progress in the BK UI. Exits immediately after
    /// triggering — does not wait for the build to complete.
    ///
    /// The triggered build runs the boss-release step regardless of
    /// whether there are Boss-affecting changes since the last tag
    /// (manual trigger overrides change-detection).
    ///
    /// Reads BK_API_TOKEN from the environment. See
    /// tools/boss/docs/buildkite-release-setup.md for provisioning.
    Release,
    /// GitHub integration management.
    ///
    /// Subcommands for managing the Boss ↔ GitHub OAuth connection used
    /// by issue sync. Drives the same engine RPCs as the macOS app's
    /// issue-sync settings UI — useful for headless setups and testing.
    Github {
        #[command(subcommand)]
        command: GithubCommand,
    },
    /// Inspect and test editorial rules that control what agents write
    /// into PR bodies, comments, and other GitHub-visible text.
    ///
    /// See `boss product set-editorial-rules` to configure rules on a
    /// product and `boss product show` to inspect the current settings.
    Editorial {
        #[command(subcommand)]
        command: EditorialCommand,
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
    /// Set (or clear) editorial rules for this product.
    ///
    /// Editorial rules constrain what agents write into PR bodies,
    /// comments, and other GitHub-visible text. Useful when running
    /// Boss in a work environment where leaking internal taxonomy or
    /// ignoring PR-template conventions is unacceptable.
    ///
    /// `--from-file PATH` reads a JSON file containing an `EditorialRules`
    /// object and stores it on the product. `--unset` clears any existing
    /// rules (all-defaults behaviour resumes).
    ///
    /// Use `boss editorial test` to validate rules against a sample body
    /// before applying them. Use `boss editorial show` to inspect the
    /// audit trail of hook decisions.
    #[command(name = "set-editorial-rules")]
    SetEditorialRules(ProductSetEditorialRulesArgs),
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
    /// Run an immediate external-tracker reconcile pass for one product.
    ///
    /// Triggers the same per-product logic as the periodic background loop,
    /// but synchronously for the named product. Useful when you want to pull
    /// upstream changes into Boss without waiting for the next scheduled tick.
    ///
    /// Prints the per-product outcome summary on success.
    #[command(name = "sync-external-tracker")]
    SyncExternalTracker(ProductSelectorArg),
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
    /// Batch-scan every project's design-doc pointer and print the
    /// ones that need attention. Surfaces three failure modes: the
    /// resolver itself returning `Broken` (e.g. path set but no repo
    /// to resolve against); pointers that resolve cleanly but whose
    /// file is missing in the leased workspace (stale-on-rename, the
    /// common case); and — opt-in via `--include-unverified` —
    /// pointers we could not check because no workspace is leased for
    /// the doc's repo. Exits non-zero when any broken entries are
    /// found so the verb is usable from CI.
    #[command(name = "lint-design-docs")]
    LintDesignDocs(ProjectLintDesignDocsArgs),
    /// Manage dependency edges (`A depends on B` ⇒ B gates A).
    Depend {
        #[command(subcommand)]
        command: DependCommand,
    },
}

/// Subcommands under `boss task ...`.
///
/// The kind-agnostic verbs (`show`, `update`, `move`, `delete`,
/// `restore`, `depend`, `bind-pr`) operate on any leaf work item by id. A chore
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
    /// Look up the work item that owns a GitHub PR, by PR number.
    ///
    /// Spans the *entire* work-item space — every kind (`project_task`,
    /// `chore`, `design`, `investigation`, `revision`) across every
    /// product — so a chore- or revision-backed PR is found just as
    /// readily as a project task. This sidesteps the `task list` blind
    /// spot (it omits chores and revisions), which is the only other way
    /// to map a PR back to its work item.
    ///
    /// `--repo` is optional: a PR number is unique within a repo, so it
    /// is only needed when the same number exists in more than one repo.
    /// Accepts a full remote URL or a short name (basename minus `.git`),
    /// matched against the repo parsed from the PR URL.
    ///
    /// Revisions commit to the owner's PR without owning a `pr_url`, so
    /// they are surfaced under the owning row rather than returned alone.
    #[command(name = "by-pr")]
    ByPr(ByPrArgs),
    /// Show any leaf work item (task or chore) by id.
    Show(TaskIdArg),
    /// Update any leaf work item (task or chore) by id.
    Update(TaskUpdateArgs),
    /// Move any leaf work item (task or chore) into a different status.
    Move(TaskMoveArgs),
    /// Delete any leaf work item (task or chore) by id.
    Delete(TaskDeleteArgs),
    /// Restore a soft-deleted leaf work item (task or chore) — the
    /// inverse of `delete`. Clears the `deleted_at` tombstone so the
    /// item is visible again. Idempotent on an already-live item.
    /// Accepts the canonical id (`task_…`) or a friendly short id
    /// (`T43`). Find tombstoned ids with `boss task list --deleted`.
    #[command(alias = "undelete")]
    Restore(TaskRestoreArgs),
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
    /// Create a `kind = 'investigation'` task. The worker that runs
    /// this task is given a doc-output prelude: deliverable is a single
    /// markdown file committed via PR to the product's `docs_repo` (or
    /// `BOSS_USER_DOCS_REPO`). No code changes.
    #[command(name = "create-investigation")]
    CreateInvestigation(InvestigationCreateArgs),
    /// Create a `kind = 'revision'` task targeting an existing open PR.
    /// The worker's deliverable is a new commit on the *parent task's*
    /// existing PR branch — no new PR is opened. Gated: the parent task
    /// must have an open, unmerged PR; the gate fires against the chain
    /// root's PR even when `--parent` itself is a revision.
    #[command(name = "create-revision")]
    CreateRevision(RevisionCreateArgs),
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
    /// Alias for `boss task restore`. Accepts any leaf work item id.
    #[command(alias = "undelete")]
    Restore(TaskRestoreArgs),
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

/// Subcommands under `boss automation …`.
///
/// Automations are standing scheduled instructions in a per-product `A<n>`
/// namespace. Selectors accept either `A<n>` (requires `--product`) or the
/// canonical `auto_…` id (product is inferred from the row).
#[derive(Debug, Subcommand)]
enum AutomationCommand {
    /// Create a new automation for a product.
    ///
    /// `--schedule` accepts either a preset keyword (`weekday-2pm`, `nightly`,
    /// `weekly-mon-am`, `hourly`) or a raw 5-field cron expression
    /// (`"0 14 * * 1-5"`). Raw expressions are validated before being sent to
    /// the engine. `--timezone` is an IANA name (e.g. `America/Los_Angeles`);
    /// defaults to `UTC`.
    Create(AutomationCreateArgs),
    /// List all automations for a product.
    List(AutomationListArgs),
    /// Show details for one automation.
    Show(AutomationSelectorArgs),
    /// Update mutable fields on an automation. Only supplied flags are changed.
    Update(AutomationUpdateArgs),
    /// Re-enable a disabled automation. Idempotent.
    Enable(AutomationSelectorArgs),
    /// Disable an automation so the scheduler skips its fires. Idempotent.
    Disable(AutomationSelectorArgs),
    /// Permanently delete an automation and its run history.
    /// Produced tasks keep their `source_automation_id` and continue through
    /// their lifecycle normally.
    Delete(AutomationSelectorArgs),
    /// Fire an immediate out-of-schedule triage for an automation.
    ///
    /// Respects the open-task cap unless `--force` is passed. Requires the
    /// scheduler loop (maintenance-tasks.md task 5) to be running.
    Run(AutomationRunArgs),
    /// List the run history (`automation_runs`) for an automation.
    Runs(AutomationSelectorArgs),
    /// List the tasks produced by an automation and their current status.
    Tasks(AutomationSelectorArgs),
}

/// Subcommands under `boss attention …`.
///
/// An attention group collects related questions or followups raised by an
/// agent. Group selectors accept `A<n>` (requires `--product`) or the
/// canonical `atg_…` id. Individual attention members are referenced by
/// their `atn_…` id.
#[derive(Debug, Subcommand)]
enum AttentionCommand {
    /// List attention groups for a product.
    ///
    /// Defaults to open and partially-answered groups.
    List(AttentionListArgs),
    /// Show a single attention group.
    ///
    /// Note: `A<n>` selectors only resolve active (open / partially-answered)
    /// groups. Use the `atg_…` primary id to show actioned or dismissed groups.
    Show(AttentionGroupSelectorArgs),
    /// Create a new attention member (question or followup).
    ///
    /// The engine finds or creates the owning group based on the association
    /// and source fields.
    Create(AttentionCreateArgs),
    /// Record an answer for one attention member (`atn_…`).
    Answer(AttentionAnswerArgs),
    /// Dismiss an attention group or member without producing an artifact.
    ///
    /// Accepts `A<n>`, `atg_…` (group), or `atn_…` (member).
    Dismiss(AttentionDismissArgs),
    /// Finalize a group: produce the downstream artifact and close the group.
    ///
    /// For question groups: creates a revision task (open PR) or fresh design
    /// task (merged doc). For followup groups: batch-creates accepted followups
    /// as tasks. Requires all members to be in a terminal answer-state; use
    /// `--skip-unanswered` to automatically skip any remaining open members.
    Action(AttentionActionArgs),
}

#[derive(Debug, Args)]
struct AttentionListArgs {
    /// Product whose attention groups to list.
    #[arg(long)]
    product: Option<String>,
    /// Filter to groups associated with this project (`P<n>` or `proj_…`).
    #[arg(long)]
    project: Option<String>,
    /// Filter to groups associated with this task (`T<n>` or `task_…`).
    #[arg(long)]
    task: Option<String>,
    /// Filter by kind: `question` or `followup`.
    #[arg(long)]
    kind: Option<String>,
    /// Filter by state: `open`, `partially_answered`, `actioned`, `dismissed`.
    /// Defaults to `open` + `partially_answered` when omitted.
    #[arg(long)]
    state: Option<String>,
    /// Also expand individual attention members for each group.
    ///
    /// Member data is not yet available via the current protocol; this flag
    /// is reserved for a future protocol update.
    #[arg(long)]
    members: bool,
}

#[derive(Debug, Args)]
struct AttentionGroupSelectorArgs {
    /// Attention group selector: `A<n>` (e.g. `A3`) or canonical `atg_…` id.
    selector: String,
    /// Product context for `A<n>` selectors. Not needed for `atg_…` ids.
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Args)]
struct AttentionCreateArgs {
    /// Kind of attention to create: `question` or `followup`.
    #[arg(long)]
    kind: String,
    /// Associated project (`P<n>` or `proj_…`). Exactly one of
    /// `--project` / `--task` is required.
    #[arg(long)]
    project: Option<String>,
    /// Associated task (`T<n>` or `task_…`). Exactly one of
    /// `--project` / `--task` is required.
    #[arg(long)]
    task: Option<String>,
    /// Join an existing open group (`A<n>` or `atg_…`) rather than letting
    /// the engine derive the group from the association and source fields.
    #[arg(long)]
    group: Option<String>,
    /// Explicit grouping-key override. Ignored when `--group` is set.
    #[arg(long)]
    group_key: Option<String>,
    // --- question fields ---
    /// Question type: `yes_no`, `multiple_choice`, or `prompt` (free text).
    #[arg(long)]
    question_type: Option<String>,
    /// The question text shown to the human.
    #[arg(long)]
    prompt: Option<String>,
    /// Choice option for `multiple_choice` questions. Pass multiple times.
    #[arg(long = "choice")]
    choices: Vec<String>,
    // --- followup fields ---
    /// Proposed task name (for `followup` kind).
    #[arg(long)]
    name: Option<String>,
    /// Proposed task description (for `followup` kind).
    #[arg(long)]
    description: Option<String>,
    /// Effort hint: `trivial`, `small`, `medium`, `large`, `max`.
    #[arg(long)]
    effort: Option<String>,
    /// Proposed work kind: `task`, `chore`, or `project`.
    #[arg(long)]
    work_kind: Option<String>,
    /// Why the agent suggested this followup.
    #[arg(long)]
    rationale: Option<String>,
}

#[derive(Debug, Args)]
struct AttentionAnswerArgs {
    /// Attention member id (`atn_…`).
    id: String,
    /// Answer `yes` (for `yes_no` questions).
    #[arg(long)]
    yes: bool,
    /// Answer `no` (for `yes_no` questions).
    #[arg(long)]
    no: bool,
    /// Chosen value or index (for `multiple_choice` questions).
    #[arg(long)]
    choice: Option<String>,
    /// Free-text answer (for `prompt` questions).
    #[arg(long)]
    answer: Option<String>,
    /// Mark the member `skipped` without providing an answer.
    #[arg(long)]
    skip: bool,
}

#[derive(Debug, Args)]
struct AttentionDismissArgs {
    /// What to dismiss: `A<n>` or `atg_…` (whole group) or `atn_…` (one member).
    id: String,
    /// Product context for `A<n>` group selectors.
    #[arg(long)]
    product: Option<String>,
    /// Optional reason for the dismissal.
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Debug, Args)]
struct AttentionActionArgs {
    /// Attention group selector: `A<n>` (e.g. `A3`) or canonical `atg_…` id.
    selector: String,
    /// Product context for `A<n>` selectors. Not needed for `atg_…` ids.
    #[arg(long)]
    product: Option<String>,
    /// Automatically skip any unanswered members before actioning.
    ///
    /// Without this flag every member must be in a terminal answer-state
    /// (`answered`, `skipped`, or `dismissed`) before the group can be
    /// actioned.
    #[arg(long)]
    skip_unanswered: bool,
    /// Proceed without the interactive confirmation prompt.
    #[arg(long)]
    confirm: bool,
}

#[derive(Debug, Args)]
struct AutomationCreateArgs {
    /// Product to create the automation in.
    #[arg(long)]
    product: Option<String>,
    /// Display name for the automation.
    #[arg(long)]
    name: Option<String>,
    /// The standing instruction passed to the triage agent on every fire.
    #[arg(long)]
    instruction: Option<String>,
    /// Schedule: preset keyword or raw 5-field cron expression.
    ///
    /// Preset keywords: `weekday-2pm`, `nightly`, `weekly-mon-am`, `hourly`.
    /// Raw cron format: `"min hour dom month dow"` (5 fields, space-separated).
    #[arg(long)]
    schedule: Option<String>,
    /// IANA timezone name for the schedule (e.g. `America/Los_Angeles`).
    /// Defaults to `UTC`.
    #[arg(long, default_value = "UTC")]
    timezone: String,
    /// Explicit target repo for the triage worker lease. Defaults to the
    /// product's primary repo when omitted.
    #[arg(long)]
    repo: Option<String>,
    /// Maximum number of open produced tasks allowed simultaneously.
    /// The scheduler skips a fire when the live count reaches this limit.
    /// Defaults to 1.
    #[arg(long, default_value_t = 1)]
    open_task_limit: i64,
    /// Create the automation in disabled state (will not fire until enabled).
    #[arg(long)]
    disabled: bool,
}

#[derive(Debug, Args)]
struct AutomationListArgs {
    /// Product whose automations to list. Required when more than one product
    /// exists.
    #[arg(long)]
    product: Option<String>,
}

/// Shared selector args used by show, enable, disable, delete, runs, tasks.
#[derive(Debug, Args)]
struct AutomationSelectorArgs {
    /// Automation selector: `A<n>` (e.g. `A1`) or canonical `auto_…` id.
    selector: String,
    /// Product context for `A<n>` selectors. Not needed when passing a
    /// canonical `auto_…` id.
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Args)]
struct AutomationUpdateArgs {
    /// Automation selector: `A<n>` or `auto_…` id.
    selector: String,
    /// Product context for `A<n>` selectors.
    #[arg(long)]
    product: Option<String>,
    /// New display name.
    #[arg(long)]
    name: Option<String>,
    /// New standing instruction.
    #[arg(long)]
    instruction: Option<String>,
    /// New schedule: preset keyword or raw 5-field cron expression.
    #[arg(long)]
    schedule: Option<String>,
    /// New IANA timezone name.
    #[arg(long)]
    timezone: Option<String>,
    /// New target repo URL (or `""` to clear and fall back to the product
    /// primary).
    #[arg(long)]
    repo: Option<String>,
    /// New open-task cap.
    #[arg(long)]
    open_task_limit: Option<i64>,
}

#[derive(Debug, Args)]
struct AutomationRunArgs {
    /// Automation selector: `A<n>` or `auto_…` id.
    selector: String,
    /// Product context for `A<n>` selectors.
    #[arg(long)]
    product: Option<String>,
    /// Bypass the open-task cap and fire even when the limit is reached.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ShakeArgs {
    /// Path to the markdown bug report. Use `-` to read from stdin.
    file: String,

    /// Override the issue title. When set, the entire FILE contents are
    /// used as the body and no title is extracted from the first line.
    #[arg(long)]
    title: Option<String>,

    /// Target repo (`owner/repo`). Defaults to `spinyfin/mono`. Mainly a
    /// hook for tests / sandbox runs against a scratch repo.
    #[arg(long, default_value = "spinyfin/mono")]
    repo: String,

    /// Add a GitHub label to the issue. Pass multiple times to add
    /// multiple labels. The labels must already exist on the target
    /// repo or `gh issue create` will reject the call.
    #[arg(long = "label")]
    labels: Vec<String>,

    /// GitHub Project V2 node ID to associate the filed issue with.
    /// The issue is added to this project via the `addProjectV2ItemById`
    /// GraphQL mutation immediately after creation. Pass an empty string
    /// (`--github-project ""`) to skip project association.
    /// Defaults to spinyfin Project #1 ("Boss").
    #[arg(long, default_value = github_app::DEFAULT_PROJECT_NODE_ID)]
    github_project: String,

    /// Print the parsed title and body without filing the issue. Useful
    /// for verifying that the file parses the way you expect.
    #[arg(long)]
    dry_run: bool,
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
    /// Inspect and manage the CI-remediation attempt table
    /// (`ci_remediations`) plus the per-PR CI attempt budget.
    /// Phase 9 #30 / Phase 11 #35 of
    /// `tools/boss/docs/designs/merge-conflict-handling-in-review.md`.
    Ci {
        #[command(subcommand)]
        command: EngineCiCommand,
    },
    /// Unified view across the three engine attempt subsystems
    /// (`conflict_resolutions`, `rebase_attempts`, `ci_remediations`).
    /// Design Phase 11 #36.
    Attempts {
        #[command(subcommand)]
        command: EngineAttemptsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum EngineCiCommand {
    /// List `ci_remediations` rows, freshest first. Filters are
    /// AND-ed; omit them all to see every attempt. Human output is a
    /// table; `--json` emits the full row vector.
    List(EngineCiListArgs),
    /// Show a single `ci_remediations` row by id. Carries every
    /// column the engine has for the attempt, including the
    /// `failed_checks` JSON blob and `log_excerpt` — useful when
    /// debugging what the worker was handed.
    Show(EngineCiShowArgs),
    /// Reset a parent's CI-attempt counter to 0 and (when the parent
    /// is in `blocked: ci_failure_exhausted`) flip it back to
    /// `in_review`. The next merge-poller sweep observes the failing
    /// CI and re-fires the auto-fix flow. Accepts either a
    /// `ci_remediations` attempt id or a work-item id.
    Retry(EngineCiRetryArgs),
    /// Mark a non-terminal `ci_remediations` attempt `abandoned`
    /// (distinct from `mark-failed`: the caller is explicitly
    /// stepping away rather than declaring the worker gave up).
    Abandon(EngineCiAbandonArgs),
    /// Stamp the worker's post-log triage decision on a
    /// `ci_remediations` attempt. Canonical values:
    /// `tractable`, `flaky_or_infra`, `unfixable`. Pure metadata
    /// column on the attempt row.
    Classify(EngineCiClassifyArgs),
    /// Flip a non-terminal `ci_remediations` attempt to `failed` with
    /// a reason. The worker calls this when triage classifies the
    /// failure as `unfixable` (or otherwise gives up without pushing).
    MarkFailed(EngineCiMarkFailedArgs),
    /// Record that the worker re-triggered the failing build via the
    /// per-provider CLI. The engine logs `new_id` (Buildkite returns
    /// a fresh build id; GHA reuses the original run id) and waits for
    /// the merge-poller to observe the re-run's outcome.
    MarkRetriggered(EngineCiMarkRetriggeredArgs),
    /// Record that a rebase-onto-base-HEAD followed by a force-push
    /// produced green CI without any code change (reconciled 2026-05-17
    /// design call). The engine flips the attempt to `succeeded` with
    /// `consumes_budget = 0` and decrements `tasks.ci_attempts_used`
    /// to refund the detection-side bump.
    MarkSucceededViaRebase(EngineCiMarkSucceededViaRebaseArgs),
    /// Per-PR / per-product CI attempt budget management.
    Budget {
        #[command(subcommand)]
        command: EngineCiBudgetCommand,
    },
}

#[derive(Debug, Subcommand)]
enum EngineCiBudgetCommand {
    /// Print the effective CI attempt budget for a work item — the
    /// per-PR override (if set), the product default, the effective
    /// value the engine uses, and the current `ci_attempts_used`
    /// counter.
    Show(EngineCiBudgetShowArgs),
    /// Set (or clear) the per-PR `tasks.ci_attempt_budget` override.
    /// Pass `--budget N` (clamped server-side to 0..=10) or `--clear`
    /// to remove the override and inherit the product default.
    Set(EngineCiBudgetSetArgs),
}

#[derive(Debug, Subcommand)]
enum EngineAttemptsCommand {
    /// List rows from any of the three engine attempt subsystems
    /// with a `kind` discriminator column. Mirrors `boss engine
    /// conflicts list` / `boss engine ci list` for callers who want
    /// one merged view (design Phase 11 #36).
    List(EngineAttemptsListArgs),
}

#[derive(Debug, Subcommand)]
enum GithubCommand {
    /// Manage the GitHub OAuth token used by issue sync.
    Auth {
        #[command(subcommand)]
        command: GithubAuthCommand,
    },
}

#[derive(Debug, Subcommand)]
enum GithubAuthCommand {
    /// Authenticate with GitHub via the OAuth device flow.
    ///
    /// Initiates a device-flow authorization against the Boss OAuth App.
    /// The engine requests a device code from GitHub, prints it for you to
    /// enter at github.com/login/device (or via the printed URL), and polls
    /// until authorization completes or expires.
    ///
    /// On success the token is stored in the macOS keychain and issue sync
    /// will use it on the next reconcile tick. To check the stored state
    /// afterwards, use `boss github auth status`.
    Login,
    /// Print the current GitHub auth state.
    ///
    /// Reports whether a stored OAuth token exists, the GitHub login it
    /// belongs to, the granted scopes, and the org/SSO access state for
    /// the bound org. Also triggers a re-probe of the org/SSO state when
    /// a token is present (clears the approval banner if the org owner
    /// has since granted access or the user has SSO-authorized the token).
    Status,
    /// Remove the stored GitHub OAuth token.
    ///
    /// Deletes the token from the macOS keychain. Issue sync falls back to
    /// the ambient `gh auth` credential after this. Does not revoke the
    /// token server-side — to fully revoke, visit
    /// https://github.com/settings/applications.
    Logout,
}

#[derive(Debug, Subcommand)]
enum EditorialCommand {
    /// List recent editorial hook decisions for a product.
    ///
    /// Prints the audit trail of allow / rewrite / deny decisions the
    /// editorial hook recorded for every `gh pr|issue` invocation by a
    /// worker on this product. Ordered freshest first.
    ///
    /// Use `--pr N` to narrow to a specific pull request number.
    /// Use `--limit N` to cap how many rows are returned (default 50).
    Show(EditorialShowArgs),
    /// Locally test editorial rules against a PR body file.
    ///
    /// Reads the product's configured `editorial_rules`, runs
    /// `editorial::evaluate` against the body in `--body-file`, and prints
    /// the decision (allow / rewrite / deny) with a description of any
    /// findings. Does NOT touch GitHub — safe to run as many times as you
    /// like while authoring rules.
    Test(EditorialTestArgs),
}

#[derive(Debug, Clone, Args)]
struct EditorialShowArgs {
    /// Product id or slug.
    selector: String,

    /// Filter to editorial actions recorded for this PR number.
    #[arg(long, value_name = "N")]
    pr: Option<u64>,

    /// Maximum number of actions to return (default 50).
    #[arg(long, value_name = "N")]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct EditorialTestArgs {
    /// Product id or slug.
    selector: String,

    /// Path to the PR body file to evaluate.
    #[arg(long, value_name = "PATH")]
    body_file: PathBuf,

    /// PR title to include in the evaluation (optional).
    #[arg(long, value_name = "TITLE", default_value = "")]
    title: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiListArgs {
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
    /// readable. Pass `--limit 0` for no cap (useful for JSON callers).
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct EngineCiShowArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    attempt_id: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiRetryArgs {
    /// Either a `ci_remediations` attempt id (`cir_…`) or a work-item
    /// id. The engine resolves an attempt id to its parent and acts
    /// on the parent.
    selector: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiAbandonArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    attempt_id: String,
    /// Free-form reason stored verbatim in `failure_reason`.
    /// Default: `manual_abandon`.
    #[arg(long, default_value = "manual_abandon")]
    reason: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiBudgetShowArgs {
    /// Work item id (`chr_…` / `tsk_…`). Friendly numeric / short ids
    /// are not resolved at the CLI level — pass the canonical id.
    work_item_id: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiBudgetSetArgs {
    /// Work item id.
    work_item_id: String,
    /// New per-PR override. Clamped server-side to `0..=10`.
    /// `--budget 0` means "notify only" (no auto-fix attempts).
    #[arg(long, value_name = "N", conflicts_with = "clear")]
    budget: Option<i64>,
    /// Clear the per-PR override so the product default applies.
    #[arg(long, conflicts_with = "budget")]
    clear: bool,
}

#[derive(Debug, Clone, Args)]
struct EngineAttemptsListArgs {
    /// Filter to one or more attempt kinds. Repeatable /
    /// comma-separated. Documented values: `conflict`, `rebase`, `ci`.
    /// Omit to include all three.
    #[arg(long, value_delimiter = ',')]
    kind: Vec<String>,
    /// Filter to a single product (id or slug). Omit for all products.
    #[arg(long)]
    product: Option<String>,
    /// Filter by status. Repeatable / comma-separated. Applied per
    /// kind against each table's own `status` column.
    #[arg(long, value_delimiter = ',')]
    status: Vec<String>,
    /// Filter to a single parent work item id.
    #[arg(long = "work-item")]
    work_item: Option<String>,
    /// Cap the number of returned rows. Defaults to 50; pass
    /// `--limit 0` for no cap.
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct EngineCiClassifyArgs {
    /// Attempt id from the `ci_remediations` table (`cir_…`).
    #[arg(long = "attempt-id")]
    attempt_id: String,
    /// Worker's classification of the failure: `tractable`,
    /// `flaky_or_infra`, or `unfixable`. Stored verbatim.
    #[arg(long = "class")]
    class: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiMarkFailedArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    attempt_id: String,
    /// Free-form failure reason. Stored verbatim on the attempt row.
    #[arg(long)]
    reason: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiMarkRetriggeredArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    attempt_id: String,
    /// Provider-emitted identifier for the new run/build the worker
    /// just triggered. Buildkite returns a fresh build id; GHA reuses
    /// the original run id.
    #[arg(long = "new-id")]
    new_id: String,
}

#[derive(Debug, Clone, Args)]
struct EngineCiMarkSucceededViaRebaseArgs {
    /// Attempt id from the `ci_remediations` table.
    #[arg(long = "attempt-id")]
    attempt_id: String,
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
    /// `opus`, `sonnet`, `haiku`, `claude-opus-4-8`). Stored verbatim
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
struct ProductSetEditorialRulesArgs {
    /// Product id or slug.
    selector: String,

    /// Path to a JSON file containing an `EditorialRules` object.
    /// Mutually exclusive with `--unset`.
    #[arg(long, value_name = "PATH", conflicts_with = "unset")]
    from_file: Option<PathBuf>,

    /// Clear the product's editorial rules (restores all-defaults behaviour).
    /// Mutually exclusive with `--from-file`.
    #[arg(long)]
    unset: bool,
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

    /// Per-product override for `kind=design` tasks. When set, design
    /// tasks on this product resolve to this repo (e.g. a docs site)
    /// instead of `--repo`. Implementation tasks are unaffected.
    /// Per-task `--repo` overrides still win.
    #[arg(long = "design-repo")]
    design_repo: Option<String>,

    /// Per-product override for `kind=investigation` tasks. When set,
    /// investigation writeups on this product open their doc PR against
    /// this repo (e.g. a docs site) instead of `--repo`. Unset → fall
    /// through to `BOSS_USER_DOCS_REPO`, then `--repo`. Implementation
    /// tasks are unaffected; per-task `--repo` overrides still win.
    #[arg(long = "docs-repo")]
    docs_repo: Option<String>,

    /// Leading prefix for worker branch names on this product. Workers
    /// push to `<prefix>exec_<id>`; only this prefix is configurable
    /// (the `exec_<id>` suffix is fixed). Set it to satisfy orgs that
    /// enforce per-developer branch prefixes via local hooks, e.g.
    /// `--worker-branch-prefix bduff/`. Omit → engine default `boss/`.
    /// A trailing `/` is added if you omit it.
    #[arg(long = "worker-branch-prefix")]
    worker_branch_prefix: Option<String>,
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

    /// Set or clear the per-product design-task repo override. Pass a
    /// URL to set it, `""` to clear, or omit to leave unchanged. See
    /// `ProductCreateArgs::design_repo`.
    #[arg(long = "design-repo")]
    design_repo: Option<String>,

    /// Set or clear the per-product investigation-task ("docs") repo
    /// override. Pass a URL to set it, `""` to clear (→ fall through to
    /// `BOSS_USER_DOCS_REPO`), or omit to leave unchanged. See
    /// `ProductCreateArgs::docs_repo`.
    #[arg(long = "docs-repo")]
    docs_repo: Option<String>,

    #[arg(long)]
    status: Option<ProductStatus>,

    /// Text prepended to every worker's initial context at spawn time,
    /// wrapped in visible `[product-preamble]…[/product-preamble]`
    /// markers. Pass `""` to clear an existing preamble.
    #[arg(long)]
    dispatch_preamble: Option<String>,

    /// Set or clear the leading prefix for worker branch names. Pass a
    /// prefix to set it (e.g. `bduff/`), `""` to clear (→ engine
    /// default `boss/`), or omit to leave unchanged. A trailing `/` is
    /// added if you omit it. See `ProductCreateArgs::worker_branch_prefix`.
    #[arg(long = "worker-branch-prefix")]
    worker_branch_prefix: Option<String>,
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
    status: Vec<ProjectStatusArg>,

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
    status: Option<ProjectStatusArg>,

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

/// Args for `boss project lint-design-docs`. Scans all products by
/// default; `--product` narrows to a single product. The two opt-in
/// flags expand the report beyond hard breakage: `--include-missing`
/// adds projects that never had a pointer set, and
/// `--include-unverified` adds resolved pointers whose file we could
/// not stat because no cube workspace is currently leased for the
/// doc's repo.
#[derive(Debug, Clone, Args)]
struct ProjectLintDesignDocsArgs {
    /// Restrict the scan to a single product (slug or id). Omit to
    /// scan every product the engine knows about.
    #[arg(long)]
    product: Option<String>,

    /// Also list projects whose `design_doc_path` is unset. By
    /// default the lint focuses on broken pointers — projects with
    /// no pointer are a *missing* affordance, not a stale one, and
    /// most callers don't want them in the report.
    #[arg(long)]
    include_missing: bool,

    /// Also list pointers we could not verify locally. A pointer is
    /// "unverified" when the resolver returns `Resolved` but no cube
    /// workspace is leased for the doc's repo, so we can't stat the
    /// file. These are *not* counted as broken (the file might
    /// exist), but surfacing them is useful when running the lint
    /// against work-environment pointers that live in unleased
    /// docs-only repos.
    #[arg(long)]
    include_unverified: bool,
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

    /// Repo override for this task. Accepts a full remote URL or a
    /// registered cube repo slug (e.g. `bduff`), which the engine
    /// resolves to its canonical origin URL at create time. Omit to
    /// inherit from the product default; pass `""` later via
    /// `task update --repo ""` to clear an override.
    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    /// Effort estimate (`trivial`/`small`/`medium`/`large`/`max`).
    /// Omitted → no level set; the dispatcher falls through to
    /// product / engine default per the design's Q3 precedence.
    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    /// Claude model slug override (e.g. `opus`, `sonnet`, `haiku`,
    /// or a fully-qualified id like `claude-opus-4-8`). Stored verbatim —
    /// claude is the source of truth on slugs.
    #[arg(long, value_name = "SLUG")]
    model: Option<String>,

    /// Bypass the duplicate guard. When a task with the same name
    /// already exists in this product and was created within the last
    /// 60 seconds, the engine rejects the create to catch fat-finger
    /// retries. Pass this flag to override and insert a second row
    /// unconditionally.
    #[arg(long = "force-duplicate", default_value_t = false)]
    force_duplicate: bool,

    /// Mark this task as produced by an automation's triage phase. Accepts
    /// an automation selector — a canonical `auto_…` id (resolves on its
    /// own) or an `A<n>` short id (requires `--product`). The engine stamps
    /// `source_automation_id`, transactionally re-checks the automation's
    /// open-task cap (the fan-out backstop), inherits the automation's repo,
    /// and runs the task in the dedicated automations pool. Intended for the
    /// triage agent; `--project` is ignored when this is set.
    #[arg(long)]
    automation: Option<String>,
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
    status: Vec<TaskStatusArg>,

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

    /// Include soft-deleted (tombstoned) tasks in the listing. Use this
    /// to find a `deleted_at` row to `boss task restore`. The DELETED
    /// column appears whenever any listed row carries a tombstone.
    #[arg(long = "deleted", alias = "include-deleted")]
    include_deleted: bool,

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
struct ByPrArgs {
    /// The GitHub PR number to look up (e.g. `959` for `…/pull/959`).
    pr_number: i64,

    /// Disambiguate when the same PR number exists in more than one
    /// repo. Accepts a full remote URL or a short name (basename of the
    /// URL minus `.git`), matched against the repo parsed from each
    /// match's PR URL. Short-name match is case-insensitive prefix;
    /// selectors shorter than 2 chars are rejected. Unnecessary in a
    /// single-repo context.
    #[arg(long = "repo")]
    repo: Option<String>,
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

    /// Repo override for this chore. Accepts a full remote URL or a
    /// registered cube repo slug (e.g. `bduff`), which the engine
    /// resolves to its canonical origin URL at create time. Omit to
    /// inherit from the product default; pass `""` later via
    /// `chore update --repo ""` to clear an override.
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

/// Args for `boss task create-investigation`.
#[derive(Debug, Args)]
struct InvestigationCreateArgs {
    #[arg(long)]
    product: Option<String>,

    /// Optional project scope. Investigation appears under the project
    /// on the kanban when set.
    #[arg(long)]
    project: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    priority: Option<TaskPriority>,

    /// Repo URL for the investigation deliverable. Omit to resolve from
    /// the product's `docs_repo` or `BOSS_USER_DOCS_REPO`.
    #[arg(long = "repo")]
    repo_remote_url: Option<String>,

    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    #[arg(long, value_name = "SLUG")]
    model: Option<String>,

    #[arg(long = "force-duplicate", default_value_t = false)]
    force_duplicate: bool,
}

/// Args for `boss task create-revision`.
#[derive(Debug, Args)]
struct RevisionCreateArgs {
    /// The parent task whose PR this revision will commit to. Accepts
    /// `T<n>` short ids (e.g. `T651`) or full `task_<hex>` ids.
    /// May itself be a revision task; the gate is evaluated against
    /// the chain root's PR.
    #[arg(long)]
    parent: String,

    /// The operator's verbatim ask. Stored as the task description and
    /// shown in the Review-lane rollup affordance so reviewers can see what
    /// each new commit was for.
    #[arg(long)]
    description: String,

    /// Concise summary title for the revision card (1–10 words). When
    /// provided by the coordinator, this becomes the card title displayed
    /// on the kanban; the verbatim ask stays in `--description`. Omit to
    /// let the engine derive the title from the first line of the
    /// description (legacy behaviour).
    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    priority: Option<TaskPriority>,

    #[arg(long, value_enum)]
    effort: Option<EffortLevelArg>,

    #[arg(long, value_name = "SLUG")]
    model: Option<String>,

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
    status: Vec<TaskStatusArg>,

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

    /// Include soft-deleted (tombstoned) chores in the listing. See
    /// `boss task list --help`.
    #[arg(long = "deleted", alias = "include-deleted")]
    include_deleted: bool,

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

    /// Resolve a friendly short id (`T42`, `42`, `#42`) against this product
    /// (slug or id). Ignored when the selector already embeds a product slug
    /// (`boss/42`) or when the selector is a primary id.
    #[arg(long)]
    product: Option<String>,

    /// Resolve a friendly short id against the product that owns this project.
    /// Accepts a typed project id (`project_…`) to infer the product
    /// automatically. Combined with `--product` when passing a slug; ignored
    /// for primary ids.
    #[arg(long)]
    project: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    status: Option<TaskStatusArg>,

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
    target: ProjectStatusArg,
}

#[derive(Debug, Clone, Args)]
struct TaskDeleteArgs {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskRestoreArgs {
    /// Task/chore id to restore. Accepts the canonical primary id
    /// (`task_…`) or a friendly short id (`T43` / `t43`). Bare `#43` /
    /// `43` and cross-product `boss/43` forms are not accepted here —
    /// a soft-deleted row is hidden from the per-product short-id
    /// resolver, so pass the globally-unique `T43` or canonical id.
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
enum ProjectStatusArg {
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

/// Translation between the leaf work-item (task/chore) status taxonomy
/// as the engine *stores* it and the names the kanban board *shows*.
///
/// The board lanes are Backlog / Doing / Review / Done / Blocked. The
/// engine has always stored the left-hand legacy strings below. As of
/// the taxonomy-alignment change the CLI speaks the board's vocabulary
/// everywhere a human or `--json` consumer can see it, while the engine
/// and stored rows keep the legacy strings untouched. The legacy names
/// remain accepted on input as aliases (see [`TaskStatusArg`] /
/// [`MoveTarget`]) so old scripts and stored data keep working.
mod status_vocab {
    /// `(stored, ui)` pairs for every status whose name differs between
    /// the two vocabularies. `done` and `blocked` are identical in both
    /// and so are absent here — [`to_ui`] passes them (and any unknown
    /// value) through unchanged.
    const RENAMED: [(&str, &str); 3] = [("todo", "backlog"), ("active", "doing"), ("in_review", "review")];

    /// Map a stored status string to the board (UI) name shown to
    /// humans and emitted in `--json`. Unknown values pass through so
    /// the CLI never hides a status the engine starts emitting before
    /// this table is updated.
    pub fn to_ui(stored: &str) -> &str {
        RENAMED.iter().find(|(s, _)| *s == stored).map_or(stored, |(_, ui)| *ui)
    }
}

/// Identity function kept for call-site symmetry: all display boundaries
/// call `with_display_status` to mark the intent. The actual board (UI)
/// label is produced at each display site via
/// `task.status.display_label()` rather than by mutating the typed field.
fn with_display_status(task: Task) -> Task {
    task
}

/// [`with_display_status`] for the `WorkItem` envelope: passes through
/// task/chore variants unchanged (display transformation happens at each
/// display site); leaves products / projects untouched.
fn work_item_with_display_status(item: WorkItem) -> WorkItem {
    item
}

/// `boss task|chore update --status` and `--status` list filters.
///
/// The variants are the board (UI) names; the legacy stored names are
/// accepted as hidden aliases for backward compatibility. [`Self::as_str`]
/// always returns the stored string, so both the wire patch sent to the
/// engine and the status-filter comparison stay in the stored vocabulary.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum TaskStatusArg {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    Blocked,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
}

/// `boss task|chore move --to`. Same board-name-primary,
/// legacy-name-alias scheme as [`TaskStatusArg`]; [`Self::as_status`]
/// returns the stored string.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum MoveTarget {
    #[value(alias = "todo")]
    Backlog,
    #[value(alias = "active")]
    Doing,
    #[value(alias = "in-review", alias = "in_review")]
    Review,
    Done,
    Blocked,
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

impl ProjectStatusArg {
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

impl TaskStatusArg {
    /// The stored status string sent to the engine and used for
    /// status-filter comparisons. Maps the board (UI) variant name back
    /// to the legacy stored vocabulary.
    fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "todo",
            Self::Doing => "active",
            Self::Blocked => "blocked",
            Self::Review => "in_review",
            Self::Done => "done",
        }
    }
}

impl MoveTarget {
    /// The stored status string the engine persists. Maps the board (UI)
    /// variant name back to the legacy stored vocabulary.
    fn as_status(self) -> &'static str {
        match self {
            Self::Backlog => "todo",
            Self::Doing => "active",
            Self::Review => "in_review",
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

#[derive(bon::Builder, Debug, Serialize)]
#[builder(on(String, into))]
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
    /// Mirror of the global `--no-autostart` flag. Gates per-work-item
    /// auto-dispatch (`boss chore create --no-autostart` → engine
    /// creates the chore in `todo` but does not spin up a worker for
    /// it). It does NOT affect transparent engine startup — that is
    /// governed by `--no-engine-autostart` via `discovery.autostart`.
    no_autostart: bool,
}

// Stamped build-info constants (BOSS_VERSION, BOSS_GIT_SHA, BOSS_BUILD_TIME).
// BOSS_BUILD_INFO_RS is set to an absolute path by:
//   - Bazel: via compile_data + $(execpath) in rustc_env (stamped release value)
//   - Cargo: via build.rs pointing to src/build_info_default.rs ("unknown" fallback)
mod build_info_stamp {
    include!(env!("BOSS_BUILD_INFO_RS"));
}

fn boss_version_string() -> &'static str {
    build_info_stamp::BOSS_VERSION
}

#[tokio::main]
async fn main() -> ExitCode {
    // Intercept --version/-V before Cli::parse() so we print the
    // canonical version string.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--version") || argv.get(1).map(|s| s.as_str()) == Some("-V") {
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
        Commands::Automation { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_automation_command(command, &ctx).await
        }
        Commands::Attention { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_attention_command(command, &ctx).await
        }
        Commands::Engine { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_engine_command(command, &ctx).await
        }
        Commands::Uninstall(args) => run_uninstall_command(args, &cli.global).await,
        Commands::Shake(args) => run_shake_command(args, &cli.global).await,
        Commands::Release => run_release_command(&cli.global).await,
        Commands::Github { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_github_command(command, &ctx).await
        }
        Commands::Editorial { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_editorial_command(command, &ctx).await
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
            serde_json::to_writer_pretty(io::stdout().lock(), &reference).map_err(CliError::internal)?;
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
            "Omit --no-autostart unless you explicitly need to suppress worker auto-dispatch on `task create` / `chore create` (also gates the auto-spawned `kind=design` seed task on `project create`). --no-autostart does NOT prevent the CLI from transparently starting the engine — the engine is always needed to track work. To forbid transparent engine startup, use --no-engine-autostart (independent of --no-autostart).",
            "Kind-agnostic verbs (show, update, move, delete, restore, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        selector_semantics: vec![
            "Product selectors accept a product id, slug, or 1-based interactive index. For agent use, prefer slug or id, not numeric indexes.",
            "Project selectors accept a project id, slug, short id (#42 or 42), or 1-based interactive index within the selected product. For agent use, prefer slug, short id, or primary id; avoid numeric indexes.",
            "Task and chore selectors accept: (1) primary id (task_…); (2) friendly short id — `T441` / `t441` / `42` / `#42` within the context product, or `boss/42` / `boss/#42` for a specific product. Projects accept `P7` / `p7` in the same position. For agent use, prefer the short id form (T-prefix or #42) when talking to a human, and the primary id when calling other engine RPCs.",
            "Kind-agnostic verbs (show, update, move, delete, restore, depend, bind-pr, link-external, unlink-external) accept any leaf work item id under either `boss task` or `boss chore` — a chore is a kind of task. Use whichever noun reads more naturally for the call site; the engine resolves the kind from the id.",
            "Kind-specific verbs (create, create-many, list, reorder) stay split by kind because their inputs and filters genuinely differ (e.g. tasks have a project, chores don't; reorder is project-task-only).",
        ],
        status_semantics: vec![
            "Task and chore status uses the board (kanban) names: backlog, doing, review, done, blocked. These are the canonical values shown in --status help and emitted in --json.",
            "The legacy stored names are accepted as aliases on input: todo->backlog, active->doing, in_review (or in-review)->review. They remain how rows are stored, so --json/human output always shows the board name regardless of how a row was set.",
            "boss task|chore update --status and --status list filters accept either vocabulary; boss task|chore move --to backlog|doing|review|done|blocked (legacy names also accepted).",
            "Product move/delete: --to active|paused|archived. delete is a soft archive (sets status=archived).",
            "Project move/delete: --to planned|active|blocked|done|archived. delete is a soft archive (sets status=archived).",
            "Task/chore delete is a soft delete (sets deleted_at). Recover an accidentally deleted leaf work item with `boss task restore <id>` (alias `undelete`); it clears deleted_at and is idempotent. Find tombstoned rows to restore with `boss task list --deleted` / `boss chore list --deleted`.",
        ],
        workflow_guidance: vec![
            "Use the current UI or conversational context first when deciding where new work belongs.",
            "If you need to compare against existing projects in a product, use boss project list --product <product-selector> --json --no-input.",
            "If the work fits an existing project, create a task in that project.",
            "If it does not fit an existing project and is small and self-contained, create a chore.",
            "If it does not fit an existing project and is broad, ambiguous, investigative, or multi-stage, create a project.",
            "`boss project create` auto-spawns a `kind=design` seed task under the new project (surfaced as `design_task` in the --json response). Do NOT follow up by filing a parallel \"Design\" task; populate the brief by running `boss task update <design_task.id> --description ...` on the seed task. Use `--no-autostart` on `project create` if you want to author the brief before the engine dispatches a worker against the seed task. Use `--no-design-task` for non-design-shaped projects (postmortems, checklists, milestone aggregators) where no seed task is needed; the project is filed with zero child tasks.",
            "Revision tasks (`boss task create-revision`): the engine auto-sequences revisions on the same parent PR — filing a new revision while a prior one is still in flight is SAFE; they run in order with no workspace clobbering. File revisions normally and let them autostart. Do NOT defensively pass `--no-autostart` on `create-revision`, and do NOT wait for the prior revision to land before filing the next, UNLESS the user explicitly asked to queue without dispatching. The pre-filing check (parent PR still open and unmerged) remains required as always.",
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
    command.write_long_help(&mut buffer).map_err(CliError::internal)?;
    let help = String::from_utf8(buffer).map_err(CliError::internal)?;
    Ok(help.trim().to_owned())
}

fn print_cli_reference_human(reference: &CliReferenceDocument) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Boss CLI reference:")?;
    writeln!(stdout)?;
    print_reference_list(&mut stdout, "General rules", &reference.usage_rules)?;
    print_reference_list(&mut stdout, "Selector semantics", &reference.selector_semantics)?;
    print_reference_list(&mut stdout, "Status semantics", &reference.status_semantics)?;
    print_reference_list(&mut stdout, "Workflow guidance", &reference.workflow_guidance)?;
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
        let allow_input = !flags.no_input && io::stdin().is_terminal() && io::stdout().is_terminal();
        let discovery = Discovery::from_env(flags.socket_path.as_deref())
            .map_err(CliError::internal)?
            .with_autostart(!flags.no_engine_autostart);

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
            let design_repo = args.design_repo;
            let docs_repo = args.docs_repo;

            let product = create_product(
                &mut client,
                CreateProductInput {
                    name,
                    description,
                    repo_remote_url,
                    design_repo,
                    docs_repo,
                    worker_branch_prefix: args.worker_branch_prefix,
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
                design_repo: args.design_repo,
                docs_repo: args.docs_repo,
                dispatch_preamble: args.dispatch_preamble,
                worker_branch_prefix: args.worker_branch_prefix,
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
            let archived = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
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
                return Err(CliError::usage("provide either --model <slug> or --unset"));
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
                FrontendEvent::EffortAuditReport { report } => {
                    print_entity(ctx, &serde_json::json!({ "report": report }), || {
                        print_effort_audit_report(&report)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product audit-effort", &other)),
            }
        }
        ProductCommand::SetEditorialRules(args) => {
            if !args.unset && args.from_file.is_none() {
                return Err(CliError::usage("provide either --from-file <path> or --unset"));
            }
            let selector = args.selector.clone();
            let product = resolve_product(&mut client, Some(selector), ctx).await?;
            let rules: Option<EditorialRules> = if args.unset {
                None
            } else {
                let path = args.from_file.as_ref().unwrap();
                let contents = std::fs::read_to_string(path)
                    .map_err(|e| CliError::usage(format!("could not read {}: {e}", path.display())))?;
                let parsed: EditorialRules = serde_json::from_str(&contents)
                    .map_err(|e| CliError::usage(format!("invalid EditorialRules JSON in {}: {e}", path.display())))?;
                Some(parsed)
            };
            let input = SetProductEditorialRulesInput {
                product_id: product.id.clone(),
                rules,
            };
            let response = client
                .send_request(&FrontendRequest::SetProductEditorialRules { input })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::WorkItemUpdated { item } => {
                    let updated = expect_product(item)?;
                    print_entity(ctx, &serde_json::json!({ "product": updated }), || {
                        if args.unset {
                            if !ctx.quiet {
                                println!("Editorial rules cleared from product {}.", updated.slug);
                            }
                        } else {
                            print_product_details("Updated product", &updated);
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product set-editorial-rules", &other)),
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
        ProductCommand::SyncExternalTracker(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let response = client
                .send_request(&FrontendRequest::SyncProductExternalTracker {
                    product_id: product.id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ExternalTrackerSyncStarted { product_id } => print_entity(
                    ctx,
                    &serde_json::json!({ "product_id": product_id, "synced": true }),
                    || {
                        if !ctx.quiet {
                            println!("External tracker sync complete for product {}.", product.slug);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("product sync-external-tracker", &other)),
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
            let design_task = list_tasks(&mut client, &product.id, Some(&project.id), None, false)
                .await?
                .into_iter()
                .find(|t| t.kind == boss_protocol::TaskKind::Design)
                .map(with_display_status);

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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProjectStatusArg::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
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
            let product = resolve_product_inferable(&mut client, args.product, Some(&args.selector), ctx).await?;
            let project = resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
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
        ProjectCommand::LintDesignDocs(args) => {
            let products = match args.product {
                Some(selector) => vec![resolve_product(&mut client, Some(selector), ctx).await?],
                None => list_products(&mut client).await?,
            };
            let mut entries: Vec<LintDesignDocEntry> = Vec::new();
            for product in &products {
                let projects = list_projects(&mut client, &product.id, None).await?;
                for project in projects {
                    let state = if project.design_doc_path.is_some() {
                        Some(resolve_project_design_doc(&mut client, &project.id).await?.state)
                    } else {
                        None
                    };
                    if let Some(entry) = classify_lint_finding(
                        product,
                        &project,
                        state.as_ref(),
                        check_design_doc_file_exists,
                        args.include_missing,
                        args.include_unverified,
                    ) {
                        entries.push(entry);
                    }
                }
            }
            entries.sort_by(|a, b| {
                a.product_slug
                    .cmp(&b.product_slug)
                    .then_with(|| a.project_slug.cmp(&b.project_slug))
            });
            let broken_count = entries
                .iter()
                .filter(|entry| entry.severity == LintSeverity::Broken)
                .count();
            print_entity(
                ctx,
                &serde_json::json!({
                    "entries": entries,
                    "scanned_products": products.iter().map(|p| &p.id).collect::<Vec<_>>(),
                    "broken_count": broken_count,
                }),
                || print_lint_design_docs_table(&entries),
            )?;
            if broken_count > 0 {
                Err(CliError::application(format!(
                    "{broken_count} project(s) have broken design-doc pointers"
                )))
            } else {
                Ok(())
            }
        }
        ProjectCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
    }
}

async fn run_task_command(command: TaskCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        TaskCommand::Create(args) => {
            // `--automation`: the triage agent's create path. The produced
            // task is product-level (no project) and routed to the automations
            // pool; the engine owns provenance stamping + the cap re-check.
            if let Some(selector) = args.automation.clone() {
                let product = resolve_optional_product(&mut client, args.product.clone(), ctx).await?;
                let automation = resolve_automation(&mut client, &selector, product.as_ref()).await?;
                let name = required_text(args.name, "Task name", ctx)?;
                let description = optional_text(args.description, "Description", ctx)?;
                let task = create_automation_task(&mut client, &automation.id, name, description).await?;
                let task = with_display_status(task);
                return print_entity(ctx, &serde_json::json!({ "task": task }), || {
                    print_task_details("Created automation task", &task, None, false);
                });
            }
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
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
            let task = with_display_status(task);
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task, None, false);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
            let project = match args.project {
                Some(selector) => Some(resolve_project(&mut client, &product.id, Some(selector), ctx).await?),
                None => None,
            };
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let tasks = list_tasks(
                &mut client,
                &product.id,
                project.as_ref().map(|project| project.id.as_str()),
                dep_filter,
                args.include_deleted,
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
            // Status filtering above runs against the stored vocabulary;
            // remap to the board names only for output.
            let tasks: Vec<Task> = tasks.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                print_tasks_table(&tasks, args.with_primary_id)
            })
        }
        TaskCommand::ByPr(args) => run_by_pr(&mut client, ctx, args).await,
        TaskCommand::Show(args) => run_show_leaf(&mut client, ctx, args, false).await,
        TaskCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        TaskCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        TaskCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        TaskCommand::Restore(args) => run_restore_leaf(&mut client, ctx, args).await,
        TaskCommand::Reorder(args) => {
            let product = resolve_product_inferable(&mut client, args.product, args.project.as_deref(), ctx).await?;
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
                        println!("Reordered {} tasks for project {}", args.ids.len(), project.name);
                    }
                },
            )
        }
        TaskCommand::Depend { command } => run_depend_command(command, &mut client, ctx).await,
        TaskCommand::BindPr(args) => run_bind_pr(&mut client, ctx, args).await,
        TaskCommand::LinkExternal(args) => run_link_external(&mut client, ctx, args).await,
        TaskCommand::UnlinkExternal(args) => run_unlink_external(&mut client, ctx, args).await,
        TaskCommand::CreateMany(args) => run_task_create_many(&mut client, ctx, args).await,
        TaskCommand::CreateInvestigation(args) => run_create_investigation(&mut client, ctx, args).await,
        TaskCommand::CreateRevision(args) => run_create_revision(&mut client, ctx, args).await,
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
            let chore = with_display_status(chore);
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore, None, false);
            })
        }
        ChoreCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let dep_filter = args.dep.into_filter();
            let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
            let chores = list_chores(&mut client, &product.id, dep_filter, args.include_deleted).await?;
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
            // Status filtering above runs against the stored vocabulary;
            // remap to the board names only for output.
            let chores: Vec<Task> = chores.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "chores": chores }), || {
                print_tasks_table(&chores, args.with_primary_id)
            })
        }
        ChoreCommand::Show(args) => run_show_leaf(&mut client, ctx, args, true).await,
        ChoreCommand::Update(args) => run_update_leaf(&mut client, ctx, args).await,
        ChoreCommand::Move(args) => run_move_leaf(&mut client, ctx, args).await,
        ChoreCommand::Delete(args) => run_delete_leaf(&mut client, ctx, args).await,
        ChoreCommand::Restore(args) => run_restore_leaf(&mut client, ctx, args).await,
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
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => get_work_item(client, &id).await?,
    };
    let (item, label) = expect_leaf_work_item(work_item)?;
    let item = with_display_status(item);
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
async fn run_update_leaf(client: &mut BossClient, ctx: &RunContext, args: TaskUpdateArgs) -> Result<(), CliError> {
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
    // Resolve the product from --product or --project (typed project id infers its product).
    let product_hint = match (args.product, args.project) {
        (Some(prod), _) => Some(prod),
        (None, Some(proj)) => product_id_from_typed_selector(client, &proj).await?,
        (None, None) => None,
    };
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, product_hint).await?;
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    let item = with_display_status(item);
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Updated {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task move` and `boss chore move`.
async fn run_move_leaf(client: &mut BossClient, ctx: &RunContext, args: TaskMoveArgs) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let patch = WorkItemPatch {
        status: Some(args.target.as_status().to_owned()),
        ..WorkItemPatch::default()
    };
    let (item, label) = expect_leaf_work_item(update_work_item(client, &resolved_id, patch).await?)?;
    let item = with_display_status(item);
    print_entity(ctx, &serde_json::json!({ label: item }), || {
        print_task_details(&format!("Moved {label}"), &item, None, false);
    })
}

/// Shared handler for `boss task delete` and `boss chore delete`. The
/// engine doesn't need the kind to delete; we read it back from the
/// pre-delete fetch only so the human-mode message names the right
/// noun.
async fn run_delete_leaf(client: &mut BossClient, ctx: &RunContext, args: TaskDeleteArgs) -> Result<(), CliError> {
    let resolved_id = resolve_selector_to_primary_id(client, ctx, &args.id, None).await?;
    let label = match get_work_item(client, &resolved_id).await {
        Ok(item) => expect_leaf_work_item(item).map(|(_, l)| l).unwrap_or("item"),
        Err(_) => "item",
    };
    delete_work_item(client, &resolved_id).await?;
    print_entity(ctx, &serde_json::json!({ "id": resolved_id, "deleted": true }), || {
        if !ctx.quiet {
            println!("Deleted {label} {resolved_id}");
        }
    })
}

async fn run_restore_leaf(client: &mut BossClient, ctx: &RunContext, args: TaskRestoreArgs) -> Result<(), CliError> {
    // Restore resolution is intentionally not routed through
    // `resolve_selector_to_primary_id`: a soft-deleted row is hidden
    // from the per-product short-id resolver, so bare `#43` / `boss/43`
    // can't reach it. The engine resolves the globally-unique `T43`
    // form (and canonical `task_…` ids) against tombstoned rows itself,
    // so we pass the raw selector straight through.
    let item = work_item_with_display_status(restore_work_item(client, args.id.trim()).await?);
    let (label, friendly) = match &item {
        WorkItem::Task(t) => ("Task", t.short_id.map(|n| format!("T{n}"))),
        WorkItem::Chore(t) => ("Chore", t.short_id.map(|n| format!("T{n}"))),
        _ => ("Item", None),
    };
    let friendly = friendly.unwrap_or_else(|| work_item_primary_id(&item).to_owned());
    print_entity(ctx, &serde_json::json!({ "item": item }), || {
        if !ctx.quiet {
            println!("Restored {label} {friendly}");
        }
    })
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

async fn run_github_command(command: GithubCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        GithubCommand::Auth { command } => run_github_auth_command(command, ctx).await,
    }
}

async fn run_github_auth_command(command: GithubAuthCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        GithubAuthCommand::Login => run_github_auth_login(ctx).await,
        GithubAuthCommand::Status => run_github_auth_status(ctx).await,
        GithubAuthCommand::Logout => run_github_auth_logout(ctx).await,
    }
}

async fn run_github_auth_login(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;

    let response = client
        .send_request(&FrontendRequest::GitHubAuthStart)
        .await
        .map_err(CliError::internal)?;

    let mut state = match response {
        FrontendEvent::GitHubAuthState { state } => state,
        other => {
            return Err(CliError::internal(anyhow::anyhow!(
                "unexpected response to GitHubAuthStart: {other:?}"
            )));
        }
    };

    let mut code_shown = false;

    loop {
        let poll_secs: u64 = match &state {
            GitHubAuthStateDto::Authorized {
                login,
                granted_scopes,
                org_state,
            } => {
                let json = serde_json::json!({
                    "status": "authorized",
                    "login": login,
                    "granted_scopes": granted_scopes,
                    "org_state": org_state,
                });
                let (login, granted_scopes, org_state) = (login.clone(), granted_scopes.clone(), org_state.clone());
                return print_entity(ctx, &json, move || {
                    println!("Authorized as @{login}");
                    println!("Scopes: {}", granted_scopes.join(", "));
                    print_org_state_human(&org_state);
                });
            }
            GitHubAuthStateDto::Expired => {
                return Err(CliError::application(
                    "Device code expired. Run `boss github auth login` again to start over.",
                ));
            }
            GitHubAuthStateDto::Denied => {
                return Err(CliError::application(
                    "Authorization denied. Run `boss github auth login` again to start over.",
                ));
            }
            GitHubAuthStateDto::Error { message } => {
                return Err(CliError::application(format!("Auth error: {message}")));
            }
            GitHubAuthStateDto::PendingUserAuth {
                user_code,
                verification_uri,
                verification_uri_complete,
                interval_seconds,
                ..
            } => {
                if !code_shown && matches!(ctx.output_mode, OutputMode::Human) {
                    println!("Open this URL in a browser to authorize Boss:");
                    if let Some(complete) = verification_uri_complete {
                        println!("  {complete}");
                        println!("Or visit {} and enter code: {user_code}", verification_uri);
                    } else {
                        println!("  {verification_uri}");
                        println!("Enter code: {user_code}");
                    }
                    println!("Waiting for authorization...");
                }
                code_shown = true;
                *interval_seconds as u64
            }
            GitHubAuthStateDto::RequestingCode | GitHubAuthStateDto::Disconnected => 2,
        };

        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;

        let response = client
            .send_request(&FrontendRequest::GitHubAuthStatus)
            .await
            .map_err(CliError::internal)?;
        state = match response {
            FrontendEvent::GitHubAuthState { state } => state,
            other => {
                return Err(CliError::internal(anyhow::anyhow!(
                    "unexpected response to GitHubAuthStatus: {other:?}"
                )));
            }
        };
    }
}

async fn run_github_auth_status(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GitHubAuthStatus)
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::GitHubAuthState { state } => {
            let json = serde_json::to_value(&state).unwrap_or(serde_json::Value::Null);
            print_entity(ctx, &json, || print_auth_state_human(&state))
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected response to GitHubAuthStatus: {other:?}"
        ))),
    }
}

async fn run_github_auth_logout(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::GitHubAuthDisconnect)
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::GitHubAuthState { .. } => {
            print_entity(ctx, &serde_json::json!({ "status": "disconnected" }), || {
                if !ctx.quiet {
                    println!(
                        "Disconnected. Token removed from keychain. Issue sync will fall back \
                         to ambient `gh auth` credentials."
                    );
                }
            })
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected response to GitHubAuthDisconnect: {other:?}"
        ))),
    }
}

fn print_auth_state_human(state: &GitHubAuthStateDto) {
    match state {
        GitHubAuthStateDto::Disconnected => {
            println!("Not connected. Run `boss github auth login` to authenticate.");
        }
        GitHubAuthStateDto::RequestingCode => {
            println!("Requesting device code from GitHub...");
        }
        GitHubAuthStateDto::PendingUserAuth {
            user_code,
            verification_uri,
            verification_uri_complete,
            ..
        } => {
            println!("Pending authorization. Open this URL in a browser:");
            if let Some(complete) = verification_uri_complete {
                println!("  {complete}");
                println!("Or visit {} and enter code: {user_code}", verification_uri);
            } else {
                println!("  {verification_uri}");
                println!("Enter code: {user_code}");
            }
        }
        GitHubAuthStateDto::Authorized {
            login,
            granted_scopes,
            org_state,
        } => {
            println!("Authorized as @{login}");
            println!("Scopes: {}", granted_scopes.join(", "));
            print_org_state_human(org_state);
        }
        GitHubAuthStateDto::Expired => {
            println!("Device code expired. Run `boss github auth login` to start over.");
        }
        GitHubAuthStateDto::Denied => {
            println!("Authorization denied. Run `boss github auth login` to start over.");
        }
        GitHubAuthStateDto::Error { message } => {
            println!("Auth error: {message}");
        }
    }
}

fn print_org_state_human(org_state: &OrgAuthState) {
    match org_state {
        OrgAuthState::Ok => println!("Org access: OK"),
        OrgAuthState::NeedsOrgApproval { request_url } => {
            println!("Org access: needs org-owner approval");
            println!("  Approval page: {request_url}");
        }
        OrgAuthState::NeedsSso { sso_url } => {
            println!("Org access: needs SAML SSO authorization");
            println!("  Authorize: {sso_url}");
        }
        OrgAuthState::Unknown => println!("Org access: unknown (probe failed)"),
    }
}

// ---------------------------------------------------------------------------
// Automation short-id / selector support
// ---------------------------------------------------------------------------

/// Parsed form of an automation selector.
#[derive(Debug)]
enum AutomationSelector {
    /// `auto_…` canonical id — used directly without a product lookup.
    PrimaryId(String),
    /// `A<n>` or `a<n>` (or plain integer) — short id within a product.
    ShortId(i64),
}

fn parse_automation_selector(s: &str) -> Result<AutomationSelector, CliError> {
    let s = s.trim();
    if s.starts_with("auto_") {
        return Ok(AutomationSelector::PrimaryId(s.to_owned()));
    }
    // `A<n>` or `a<n>`
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'A' || first == b'a')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(AutomationSelector::ShortId(n));
        }
    }
    // Plain positive integer → short id
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(AutomationSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "automation selector must be A<n> (e.g. A1) or an auto_… id; got {s:?}"
    )))
}

/// Resolve an automation selector to a full `Automation` row.
///
/// For `auto_…` ids, the product is not needed. For `A<n>` selectors, a
/// `product` must be provided (resolved by the caller beforehand).
async fn resolve_automation(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<Automation, CliError> {
    match parse_automation_selector(selector)? {
        AutomationSelector::PrimaryId(id) => get_automation(client, &id).await,
        AutomationSelector::ShortId(n) => {
            let product = product.ok_or_else(|| {
                CliError::usage("A<n> selectors require --product to identify the automation namespace")
            })?;
            let automations = list_automations(client, &product.id).await?;
            automations
                .into_iter()
                .find(|a| a.short_id == Some(n))
                .ok_or_else(|| CliError::not_found(format!("no automation A{n} found in product '{}'", product.slug)))
        }
    }
}

// ---------------------------------------------------------------------------
// Preset → cron compilation
// ---------------------------------------------------------------------------

/// Well-known schedule preset keywords.
///
/// Each preset compiles to a standard 5-field cron expression (min hour dom month dow).
/// The timezone is supplied separately via `--timezone`.
const SCHEDULE_PRESETS: &[(&str, &str, &str)] = &[
    ("weekday-2pm", "0 14 * * 1-5", "Every weekday at 2:00 pm"),
    ("nightly", "0 2 * * *", "Every day at 2:00 am"),
    ("weekly-mon-am", "0 9 * * 1", "Every Monday at 9:00 am"),
    ("hourly", "0 * * * *", "Every hour"),
];

/// Compile a `--schedule` value to a cron expression.
///
/// Accepts either a preset keyword (case-insensitive) or a raw 5-field cron
/// string. Raw strings are validated: they must have exactly 5 whitespace-
/// separated fields and each field must contain only cron-legal characters
/// (`0-9`, `*`, `/`, `-`, `,`, alpha for named months/days).
fn compile_schedule(schedule: &str) -> Result<String, CliError> {
    let trimmed = schedule.trim();

    // Check presets first (case-insensitive).
    if let Some((_, cron, _)) = SCHEDULE_PRESETS
        .iter()
        .find(|(k, _, _)| k.eq_ignore_ascii_case(trimmed))
    {
        return Ok((*cron).to_owned());
    }

    // Treat as a raw cron expression and validate.
    validate_cron_expression(trimmed)
}

/// Validate a raw 5-field cron expression.
///
/// Checks that the string has exactly 5 whitespace-separated fields and each
/// field contains only characters valid in cron: digits, `*`, `/`, `-`, `,`,
/// and ASCII alpha (for named days/months like `MON`, `JAN`). Does not check
/// numeric ranges — the engine (once the cron library is wired up in task 5)
/// will reject semantically invalid values.
fn validate_cron_expression(cron: &str) -> Result<String, CliError> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CliError::usage(format!(
            "cron expression must have exactly 5 fields (got {}); \
             format: \"min hour dom month dow\" (e.g. \"0 14 * * 1-5\")",
            fields.len()
        )));
    }
    for field in &fields {
        if field
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '*' | '/' | '-' | ','))
        {
            return Err(CliError::usage(format!(
                "cron field {:?} contains invalid characters; \
                 allowed: digits, *, /, -, , and alpha (for named months/days)",
                field
            )));
        }
    }
    Ok(cron.to_owned())
}

// ---------------------------------------------------------------------------
// Automation RPC helpers
// ---------------------------------------------------------------------------

async fn create_automation(client: &mut BossClient, input: CreateAutomationInput) -> Result<Automation, CliError> {
    match client
        .send_request(&FrontendRequest::CreateAutomation { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationCreated { automation } => Ok(automation),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation create", &other)),
    }
}

async fn list_automations(client: &mut BossClient, product_id: &str) -> Result<Vec<Automation>, CliError> {
    match client
        .send_request(&FrontendRequest::ListAutomations {
            product_id: product_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationsList { automations, .. } => Ok(automations),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation list", &other)),
    }
}

async fn get_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    match client
        .send_request(&FrontendRequest::GetAutomation { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationResult { automation } => Ok(automation),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation show", &other)),
    }
}

async fn update_automation(client: &mut BossClient, id: &str, patch: AutomationPatch) -> Result<Automation, CliError> {
    match client
        .send_request(&FrontendRequest::UpdateAutomation {
            id: id.to_owned(),
            patch,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationUpdated { automation } => Ok(automation),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation update", &other)),
    }
}

async fn enable_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    match client
        .send_request(&FrontendRequest::EnableAutomation { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationUpdated { automation } => Ok(automation),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation enable", &other)),
    }
}

async fn disable_automation(client: &mut BossClient, id: &str) -> Result<Automation, CliError> {
    match client
        .send_request(&FrontendRequest::DisableAutomation { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationUpdated { automation } => Ok(automation),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation disable", &other)),
    }
}

async fn delete_automation(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::DeleteAutomation { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationDeleted { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation delete", &other)),
    }
}

async fn list_automation_runs(client: &mut BossClient, automation_id: &str) -> Result<Vec<AutomationRun>, CliError> {
    match client
        .send_request(&FrontendRequest::ListAutomationRuns {
            automation_id: automation_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationRunsList { runs, .. } => Ok(runs),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation runs", &other)),
    }
}

async fn list_automation_tasks(client: &mut BossClient, automation_id: &str) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListAutomationTasks {
            automation_id: automation_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AutomationTasksList { tasks, .. } => Ok(tasks),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation tasks", &other)),
    }
}

// ---------------------------------------------------------------------------
// Display helpers for automations
// ---------------------------------------------------------------------------

fn print_automations_table(automations: &[Automation]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic).set_header([
        "#",
        "NAME",
        "SCHEDULE",
        "ENABLED",
        "OPEN",
        "LAST OUTCOME",
        "NEXT DUE",
    ]);
    for a in automations {
        let short = a.short_id.map(|n| format!("A{n}")).unwrap_or_default();
        let schedule = match &a.trigger {
            AutomationTrigger::Schedule { cron, timezone } => {
                format!("{cron} ({timezone})")
            }
        };
        let enabled = if a.enabled { "yes" } else { "no" };
        let last_outcome = a.last_outcome.as_deref().unwrap_or("-");
        let next_due = a.next_due_at.as_deref().unwrap_or("-");
        table.add_row([&short, a.name.as_str(), &schedule, enabled, last_outcome, next_due]);
    }
    println!("{table}");
}

fn print_automation_details(label: &str, a: &Automation) {
    println!("{label}:");
    let short = a.short_id.map(|n| format!("A{n}")).unwrap_or_default();
    println!("  ID:          {} ({})", a.id, short);
    println!("  Product:     {}", a.product_id);
    println!("  Name:        {}", a.name);
    let (cron, tz) = match &a.trigger {
        AutomationTrigger::Schedule { cron, timezone } => (cron.as_str(), timezone.as_str()),
    };
    println!("  Cron:        {cron}");
    println!("  Timezone:    {tz}");
    println!("  Instruction: {}", a.standing_instruction);
    println!("  Enabled:     {}", if a.enabled { "yes" } else { "no" });
    println!("  Open limit:  {}", a.open_task_limit);
    if let Some(repo) = &a.repo_remote_url {
        println!("  Repo:        {repo}");
    }
    if let Some(last) = &a.last_fired_at {
        println!("  Last fired:  {last}");
    }
    if let Some(outcome) = &a.last_outcome {
        println!("  Last outcome:{outcome}");
    }
    if let Some(next) = &a.next_due_at {
        println!("  Next due:    {next}");
    }
    println!("  Created:     {}", a.created_at);
    println!("  Updated:     {}", a.updated_at);
}

fn print_automation_runs_table(runs: &[AutomationRun]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic).set_header([
        "SCHEDULED FOR",
        "OUTCOME",
        "STARTED",
        "PRODUCED TASK",
        "DETAIL",
    ]);
    for r in runs {
        let produced = r.produced_task_id.as_deref().unwrap_or("-");
        let detail = r.detail.as_deref().unwrap_or("-");
        table.add_row([
            r.scheduled_for.as_str(),
            r.outcome.as_str(),
            r.started_at.as_str(),
            produced,
            detail,
        ]);
    }
    println!("{table}");
}

// ---------------------------------------------------------------------------
// Editorial command handler
// ---------------------------------------------------------------------------

async fn run_editorial_command(command: EditorialCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EditorialCommand::Show(args) => run_editorial_show(args, ctx).await,
        EditorialCommand::Test(args) => run_editorial_test(args, ctx).await,
    }
}

async fn run_editorial_show(args: EditorialShowArgs, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
    let response = client
        .send_request(&FrontendRequest::ListEditorialActions {
            product_id: product.id.clone(),
            limit: args.limit,
        })
        .await
        .map_err(CliError::internal)?;
    match response {
        FrontendEvent::EditorialActionsList { actions, .. } => {
            let filtered: Vec<&EditorialAction> = if let Some(pr_num) = args.pr {
                let suffix = format!("/{pr_num}");
                actions
                    .iter()
                    .filter(|a| a.pr_url.as_deref().map(|u| u.ends_with(&suffix)).unwrap_or(false))
                    .collect()
            } else {
                actions.iter().collect()
            };
            print_entity(
                ctx,
                &serde_json::json!({ "product_id": product.id, "actions": filtered }),
                || {
                    if filtered.is_empty() {
                        if !ctx.quiet {
                            println!("No editorial actions recorded for product {}.", product.slug);
                        }
                    } else {
                        println!("Editorial actions for product {} ({}):", product.name, product.slug);
                        for action in &filtered {
                            let pr = action.pr_url.as_deref().unwrap_or("(no PR)");
                            let first_reason_line = action.reason.lines().next().unwrap_or("");
                            println!("  [{}] {} — {}", action.action, pr, first_reason_line);
                            println!("    at {}", action.created_at);
                        }
                    }
                },
            )
        }
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("editorial show", &other)),
    }
}

async fn run_editorial_test(args: EditorialTestArgs, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
    let body = std::fs::read_to_string(&args.body_file)
        .map_err(|e| CliError::usage(format!("could not read {}: {e}", args.body_file.display())))?;
    let rules = product.editorial_rules.clone().unwrap_or_default();
    let compiled = boss_editorial::CompiledRules::compile(rules)
        .map_err(|e| CliError::application(format!("invalid redaction regex in editorial_rules: {e}")))?;
    let decision = boss_editorial::evaluate(&body, &args.title, &compiled, None);
    let (decision_str, findings): (&str, Vec<String>) = match &decision {
        boss_editorial::EditorialDecision::Allow => ("allow", vec![]),
        boss_editorial::EditorialDecision::Rewrite { findings, .. } => {
            ("rewrite", findings.iter().map(|f| f.description.clone()).collect())
        }
        boss_editorial::EditorialDecision::Block { findings } => {
            ("deny", findings.iter().map(|f| f.description.clone()).collect())
        }
    };
    let rewritten_body: Option<&str> = match &decision {
        boss_editorial::EditorialDecision::Rewrite { body, .. } => Some(body.as_str()),
        _ => None,
    };
    print_entity(
        ctx,
        &serde_json::json!({
            "product_id": product.id,
            "decision": decision_str,
            "findings": findings,
        }),
        || {
            println!("Decision: {decision_str}");
            if findings.is_empty() {
                println!("No findings.");
            } else {
                println!("Findings:");
                for f in &findings {
                    println!("  - {f}");
                }
            }
            if let Some(new_body) = rewritten_body {
                println!("\nRewritten body:");
                println!("{new_body}");
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Automation command handler
// ---------------------------------------------------------------------------

async fn run_automation_command(command: AutomationCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        AutomationCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Automation name", ctx)?;
            let instruction = required_text(args.instruction, "Standing instruction", ctx)?;
            let schedule_raw = required_text(args.schedule, "Schedule", ctx)?;
            let cron = compile_schedule(&schedule_raw)?;
            let trigger = AutomationTrigger::Schedule {
                cron,
                timezone: args.timezone,
            };
            let automation = create_automation(
                &mut client,
                CreateAutomationInput::builder()
                    .product_id(product.id)
                    .name(name)
                    .trigger(trigger)
                    .standing_instruction(instruction)
                    .open_task_limit(args.open_task_limit)
                    .enabled(!args.disabled)
                    .maybe_repo_remote_url(args.repo)
                    .created_via(boss_protocol::CREATED_VIA_CLI)
                    .build(),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "automation": automation }), || {
                print_automation_details("Created automation", &automation);
            })
        }

        AutomationCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let automations = list_automations(&mut client, &product.id).await?;
            print_entity(ctx, &serde_json::json!({ "automations": automations }), || {
                if automations.is_empty() {
                    println!("No automations for product '{}'.", product.slug);
                } else {
                    print_automations_table(&automations);
                }
            })
        }

        AutomationCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            print_entity(ctx, &serde_json::json!({ "automation": automation }), || {
                print_automation_details("Automation", &automation);
            })
        }

        AutomationCommand::Update(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;

            // Build a trigger patch only when schedule or timezone changed.
            let trigger_patch = match (&args.schedule, &args.timezone) {
                (None, None) => None,
                _ => {
                    // Start from the existing trigger so partial updates work.
                    let AutomationTrigger::Schedule {
                        cron: existing_cron,
                        timezone: existing_tz,
                    } = &automation.trigger;
                    let cron = if let Some(sched) = &args.schedule {
                        compile_schedule(sched)?
                    } else {
                        existing_cron.clone()
                    };
                    let timezone = args.timezone.clone().unwrap_or_else(|| existing_tz.clone());
                    Some(AutomationTrigger::Schedule { cron, timezone })
                }
            };

            let patch = AutomationPatch {
                name: args.name,
                repo_remote_url: args.repo,
                trigger: trigger_patch,
                standing_instruction: args.instruction,
                open_task_limit: args.open_task_limit,
                catch_up_window_secs: None,
                enabled: None,
            };
            let updated = update_automation(&mut client, &automation.id, patch).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                print_automation_details("Updated automation", &updated);
            })
        }

        AutomationCommand::Enable(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let updated = enable_automation(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                if !ctx.quiet {
                    println!("Enabled automation {}", automation.id);
                }
            })
        }

        AutomationCommand::Disable(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let updated = disable_automation(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "automation": updated }), || {
                if !ctx.quiet {
                    println!("Disabled automation {}", automation.id);
                }
            })
        }

        AutomationCommand::Delete(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            delete_automation(&mut client, &automation.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "deleted_automation_id": automation.id }),
                || {
                    if !ctx.quiet {
                        println!("Deleted automation {}", automation.id);
                    }
                },
            )
        }

        AutomationCommand::Run(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            match client
                .send_request(&FrontendRequest::RunAutomation {
                    automation_id: automation.id.clone(),
                    force: args.force,
                })
                .await
                .map_err(CliError::internal)?
            {
                FrontendEvent::AutomationRunEnqueued { .. } => print_entity(
                    ctx,
                    &serde_json::json!({ "automation_id": automation.id, "enqueued": true }),
                    || {
                        if !ctx.quiet {
                            println!("Triage enqueued for automation {}", automation.id);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("automation run", &other)),
            }
        }

        AutomationCommand::Runs(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let runs = list_automation_runs(&mut client, &automation.id).await?;
            print_entity(ctx, &serde_json::json!({ "runs": runs }), || {
                if runs.is_empty() {
                    println!("No runs recorded for automation {}.", automation.id);
                } else {
                    print_automation_runs_table(&runs);
                }
            })
        }

        AutomationCommand::Tasks(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let automation = resolve_automation(&mut client, &args.selector, product.as_ref()).await?;
            let tasks = list_automation_tasks(&mut client, &automation.id).await?;
            let tasks: Vec<Task> = tasks.into_iter().map(with_display_status).collect();
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                if tasks.is_empty() {
                    println!("No tasks produced by automation {}.", automation.id);
                } else {
                    print_tasks_table(&tasks, false);
                }
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Attention group selector parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum AttentionGroupSelector {
    /// `atg_…` primary id.
    PrimaryId(String),
    /// `A<n>` per-product short id (requires product context at resolution time).
    ShortId(i64),
}

fn parse_attention_group_selector(s: &str) -> Result<AttentionGroupSelector, CliError> {
    let s = s.trim();
    if s.starts_with("atg_") {
        return Ok(AttentionGroupSelector::PrimaryId(s.to_owned()));
    }
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'A' || first == b'a')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return Ok(AttentionGroupSelector::ShortId(n));
        }
    }
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return Ok(AttentionGroupSelector::ShortId(n));
    }
    Err(CliError::usage(format!(
        "attention group selector must be A<n> (e.g. A1) or an atg_… id; got {s:?}"
    )))
}

/// Resolve an attention group selector to a full `AttentionGroup` row.
///
/// For `atg_…` ids the product is not needed. For `A<n>` selectors, a
/// `product` must be provided (resolved by the caller beforehand).
///
/// Note: `A<n>` resolution lists only open/partially-answered groups. Use
/// the `atg_…` primary id to reference actioned or dismissed groups.
async fn resolve_attention_group(
    client: &mut BossClient,
    selector: &str,
    product: Option<&Product>,
) -> Result<AttentionGroup, CliError> {
    match parse_attention_group_selector(selector)? {
        AttentionGroupSelector::PrimaryId(id) => get_attention_group(client, &id).await,
        AttentionGroupSelector::ShortId(n) => {
            let product = product.ok_or_else(|| {
                CliError::usage("A<n> selectors require --product to identify the attention group namespace")
            })?;
            let groups = list_attention_groups(client, &product.id, None, None, None, None).await?;
            groups.into_iter().find(|g| g.short_id == Some(n)).ok_or_else(|| {
                CliError::not_found(format!(
                    "no active attention group A{n} found in product '{}' \
                         (use the atg_… id to reference actioned or dismissed groups)",
                    product.slug
                ))
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Attention RPC helpers
// ---------------------------------------------------------------------------

async fn list_attention_groups(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<String>,
    task_id: Option<String>,
    kind: Option<String>,
    state: Option<String>,
) -> Result<Vec<AttentionGroup>, CliError> {
    match client
        .send_request(&FrontendRequest::ListAttentionGroups {
            product_id: product_id.to_owned(),
            project_id,
            task_id,
            kind,
            state,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionGroupsList { groups, .. } => Ok(groups),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention list", &other)),
    }
}

async fn get_attention_group(client: &mut BossClient, id: &str) -> Result<AttentionGroup, CliError> {
    match client
        .send_request(&FrontendRequest::GetAttentionGroup { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionGroupResult { group, .. } => Ok(group),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention show", &other)),
    }
}

async fn create_attention_rpc(
    client: &mut BossClient,
    input: CreateAttentionInput,
) -> Result<(Attention, AttentionGroup), CliError> {
    match client
        .send_request(&FrontendRequest::CreateAttention { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionCreated { attention, group } => Ok((attention, group)),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention create", &other)),
    }
}

async fn answer_attention_rpc(
    client: &mut BossClient,
    id: &str,
    answer: Option<String>,
    skip: bool,
    dismiss: bool,
) -> Result<AttentionGroup, CliError> {
    match client
        .send_request(&FrontendRequest::AnswerAttention {
            id: id.to_owned(),
            answer,
            skip,
            dismiss,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionGroupUpdated { group, .. } => Ok(group),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention answer", &other)),
    }
}

async fn action_attention_group_rpc(
    client: &mut BossClient,
    id: &str,
    skip_unanswered: bool,
) -> Result<AttentionGroup, CliError> {
    match client
        .send_request(&FrontendRequest::ActionAttentionGroup {
            id: id.to_owned(),
            skip_unanswered,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionGroupActioned { group, .. } => Ok(group),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention action", &other)),
    }
}

async fn dismiss_attention_rpc(
    client: &mut BossClient,
    id: &str,
    reason: Option<String>,
) -> Result<AttentionGroup, CliError> {
    match client
        .send_request(&FrontendRequest::DismissAttention {
            id: id.to_owned(),
            reason,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::AttentionGroupUpdated { group, .. } => Ok(group),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("attention dismiss", &other)),
    }
}

// ---------------------------------------------------------------------------
// Attention display helpers
// ---------------------------------------------------------------------------

fn print_attention_groups_table(groups: &[AttentionGroup]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic).set_header([
        "ID",
        "SHORT",
        "KIND",
        "STATE",
        "ASSOCIATION",
        "CREATED",
    ]);
    for g in groups {
        let short = g.short_id.map(|n| format!("A{n}")).unwrap_or_default();
        let assoc = g
            .association_project_id
            .as_deref()
            .or(g.association_task_id.as_deref())
            .unwrap_or("-");
        table.add_row([
            g.id.as_str(),
            short.as_str(),
            g.kind.as_str(),
            g.state.as_str(),
            assoc,
            g.created_at.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_attention_group_details(label: &str, g: &AttentionGroup) {
    println!("{label}: {}", g.id);
    if let Some(n) = g.short_id {
        println!("  Short ID  : A{n}");
    }
    println!("  Kind      : {}", g.kind);
    println!("  State     : {}", g.state);
    println!("  Source    : {}", g.source_kind);
    if let Some(ref id) = g.association_project_id {
        println!("  Project   : {id}");
    }
    if let Some(ref id) = g.association_task_id {
        println!("  Task      : {id}");
    }
    if let Some(ref path) = g.source_doc_path {
        println!("  Doc path  : {path}");
    }
    if let Some(ref task_id) = g.source_task_id {
        println!("  Source task: {task_id}");
    }
    if let Some(ref kind) = g.produced_artifact_kind {
        println!("  Artifact  : {kind}");
        if let Some(ref r) = g.produced_artifact_ref {
            println!("  Ref       : {r}");
        }
    }
    println!("  Created   : {}", g.created_at);
    if let Some(ref t) = g.actioned_at {
        println!("  Actioned  : {t}");
    }
    if let Some(ref t) = g.dismissed_at {
        println!("  Dismissed : {t}");
    }
}

// ---------------------------------------------------------------------------
// Attention command handler
// ---------------------------------------------------------------------------

async fn run_attention_command(command: AttentionCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        AttentionCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project_id = if let Some(sel) = args.project {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let task_id = if let Some(sel) = args.task {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let groups =
                list_attention_groups(&mut client, &product.id, project_id, task_id, args.kind, args.state).await?;
            print_entity(ctx, &serde_json::json!({ "attention_groups": groups }), || {
                if groups.is_empty() {
                    println!("No attention groups found for product '{}'.", product.slug);
                } else {
                    print_attention_groups_table(&groups);
                }
            })
        }

        AttentionCommand::Show(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let group = resolve_attention_group(&mut client, &args.selector, product.as_ref()).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                print_attention_group_details("Attention group", &group);
            })
        }

        AttentionCommand::Create(args) => {
            if args.project.is_none() && args.task.is_none() {
                return Err(CliError::usage("exactly one of --project or --task is required"));
            }
            if args.project.is_some() && args.task.is_some() {
                return Err(CliError::usage("--project and --task are mutually exclusive"));
            }
            let association_project_id = if let Some(sel) = args.project {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let association_task_id = if let Some(sel) = args.task {
                Some(resolve_selector_to_primary_id(&mut client, ctx, &sel, None).await?)
            } else {
                None
            };
            let choice_options = if args.choices.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&args.choices).map_err(CliError::internal)?)
            };
            let input = CreateAttentionInput::builder()
                .kind(args.kind)
                .maybe_group_id(args.group)
                .maybe_group_key(args.group_key)
                .maybe_association_project_id(association_project_id)
                .maybe_association_task_id(association_task_id)
                .maybe_source_kind(Some("manual".to_owned()))
                .maybe_question_type(args.question_type)
                .maybe_prompt_text(args.prompt)
                .maybe_choice_options(choice_options)
                .maybe_proposed_name(args.name)
                .maybe_proposed_description(args.description)
                .maybe_proposed_effort(args.effort)
                .maybe_proposed_work_kind(args.work_kind)
                .maybe_rationale(args.rationale)
                .build();
            let (attention, group) = create_attention_rpc(&mut client, input).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "attention": attention, "attention_group": group }),
                || {
                    if !ctx.quiet {
                        let short = group
                            .short_id
                            .map(|n| format!("A{n}"))
                            .unwrap_or_else(|| group.id.clone());
                        println!(
                            "Created attention {} in group {short} (state: {})",
                            attention.id, group.state
                        );
                    }
                },
            )
        }

        AttentionCommand::Answer(args) => {
            let flag_count = [
                args.yes,
                args.no,
                args.skip,
                args.choice.is_some(),
                args.answer.is_some(),
            ]
            .iter()
            .filter(|&&b| b)
            .count();
            if flag_count > 1 {
                return Err(CliError::usage(
                    "--yes, --no, --choice, --answer, and --skip are mutually exclusive",
                ));
            }
            if flag_count == 0 {
                return Err(CliError::usage(
                    "one of --yes, --no, --choice <v>, --answer <text>, or --skip is required",
                ));
            }
            let (answer, skip, dismiss) = if args.yes {
                (Some("yes".to_owned()), false, false)
            } else if args.no {
                (Some("no".to_owned()), false, false)
            } else if let Some(choice) = args.choice {
                (Some(choice), false, false)
            } else if let Some(ans) = args.answer {
                (Some(ans), false, false)
            } else {
                (None, true, false)
            };
            let group = answer_attention_rpc(&mut client, &args.id, answer, skip, dismiss).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                if !ctx.quiet {
                    println!("Recorded answer for {} (group state: {})", args.id, group.state);
                }
            })
        }

        AttentionCommand::Dismiss(args) => {
            // The engine discriminates atg_… (group) vs atn_… (member) by prefix.
            // A<n> selectors refer to groups and need product resolution.
            let resolved_id = if args.id.starts_with("atg_") || args.id.starts_with("atn_") {
                args.id.clone()
            } else {
                let product = resolve_optional_product(&mut client, args.product.clone(), ctx).await?;
                let group = resolve_attention_group(&mut client, &args.id, product.as_ref()).await?;
                group.id
            };
            let group = dismiss_attention_rpc(&mut client, &resolved_id, args.reason).await?;
            print_entity(ctx, &serde_json::json!({ "attention_group": group }), || {
                if !ctx.quiet {
                    println!("Dismissed {} (group state: {})", resolved_id, group.state);
                }
            })
        }

        AttentionCommand::Action(args) => {
            let product = resolve_optional_product(&mut client, args.product, ctx).await?;
            let group = resolve_attention_group(&mut client, &args.selector, product.as_ref()).await?;
            if !args.confirm {
                if !ctx.allow_input {
                    return Err(CliError::usage(
                        "pass --confirm to action the group non-interactively (or --no-input is set)",
                    ));
                }
                // Interactive confirmation.
                let short = group
                    .short_id
                    .map(|n| format!("A{n}"))
                    .unwrap_or_else(|| group.id.clone());
                print!(
                    "Action group {short} ({kind}, {state})? [y/N]: ",
                    kind = group.kind,
                    state = group.state
                );
                io::stdout().flush().map_err(CliError::internal)?;
                let mut line = String::new();
                io::stdin().read_line(&mut line).map_err(CliError::internal)?;
                if !matches!(line.trim(), "y" | "Y" | "yes" | "Yes") {
                    if !ctx.quiet {
                        println!("Aborted.");
                    }
                    return Ok(());
                }
            }
            let actioned = action_attention_group_rpc(&mut client, &group.id, args.skip_unanswered).await?;
            let produced_kind = actioned.produced_artifact_kind.clone();
            let produced_ref = actioned.produced_artifact_ref.clone();
            print_entity(
                ctx,
                &serde_json::json!({
                    "attention_group": actioned,
                    "produced": {
                        "kind": produced_kind,
                        "ref": produced_ref,
                    }
                }),
                || {
                    if !ctx.quiet {
                        let artifact = produced_kind.as_deref().unwrap_or("none");
                        let artifact_ref = produced_ref.as_deref().unwrap_or("");
                        println!("Actioned group {} → produced {artifact} {artifact_ref}", actioned.id);
                    }
                },
            )
        }
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
                .await
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
        EngineCommand::Ci { command } => run_engine_ci_command(command, ctx).await,
        EngineCommand::Attempts { command } => run_engine_attempts_command(command, ctx).await,
    }
}

async fn run_engine_ci_command(command: EngineCiCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCiCommand::Classify(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::ClassifyCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                    triage_class: args.class.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationClassified { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} triage_class set to {}.",
                                attempt.id,
                                attempt.triage_class.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci classify", &other)),
            }
        }
        EngineCiCommand::MarkFailed(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationFailed {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationMarkedFailed { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} marked failed (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-failed", &other)),
            }
        }
        EngineCiCommand::MarkRetriggered(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationRetriggered {
                    attempt_id: args.attempt_id.clone(),
                    new_id: args.new_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationRetriggered { attempt, new_id } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempt": attempt, "new_id": new_id }),
                    || {
                        if !ctx.quiet {
                            println!("ci_remediation {} retrigger recorded (new id: {}).", attempt.id, new_id,);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-retriggered", &other)),
            }
        }
        EngineCiCommand::MarkSucceededViaRebase(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationSucceededViaRebase {
                    attempt,
                    budget_refunded,
                } => print_entity(
                    ctx,
                    &serde_json::json!({
                        "attempt": attempt,
                        "budget_refunded": budget_refunded,
                    }),
                    || {
                        if !ctx.quiet {
                            let refund = if budget_refunded {
                                "budget refunded"
                            } else {
                                "no budget change"
                            };
                            println!(
                                "ci_remediation {} marked succeeded_via_rebase ({}).",
                                attempt.id, refund,
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-succeeded-via-rebase", &other)),
            }
        }
        EngineCiCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
                None => None,
            };
            // Mirror conflicts: `--limit 0` → no cap, default 50.
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListCiRemediations {
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_ci_remediations_table(&attempts)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci list", &other)),
            }
        }
        EngineCiCommand::Show(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::GetCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediation { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        print_ci_remediation_detail(&attempt)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci show", &other)),
            }
        }
        EngineCiCommand::Retry(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::RetryCiRemediation {
                    selector: args.selector.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationRetryDone {
                    work_item_id,
                    budget,
                    was_exhausted,
                } => print_entity(
                    ctx,
                    &serde_json::json!({
                        "work_item_id": work_item_id,
                        "budget": budget,
                        "was_exhausted": was_exhausted,
                    }),
                    || {
                        if !ctx.quiet {
                            print_ci_budget_after_retry(&work_item_id, &budget, was_exhausted);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci retry", &other)),
            }
        }
        EngineCiCommand::Abandon(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AbandonCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationMarkedAbandoned { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} marked abandoned (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci abandon", &other)),
            }
        }
        EngineCiCommand::Budget { command } => match command {
            EngineCiBudgetCommand::Show(args) => {
                let mut client = connect_for_work(ctx).await?;
                let response = client
                    .send_request(&FrontendRequest::GetCiBudget {
                        work_item_id: args.work_item_id.clone(),
                    })
                    .await
                    .map_err(CliError::internal)?;
                match response {
                    FrontendEvent::CiBudget { budget } => {
                        print_entity(ctx, &serde_json::json!({ "budget": budget }), || {
                            if !ctx.quiet {
                                print_ci_budget_snapshot(&budget);
                            }
                        })
                    }
                    FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                        Err(CliError::application(message))
                    }
                    other => Err(unexpected_event("ci budget show", &other)),
                }
            }
            EngineCiBudgetCommand::Set(args) => {
                if args.budget.is_none() && !args.clear {
                    return Err(CliError::usage(
                        "specify --budget <n> to set a per-PR override or --clear to remove it",
                    ));
                }
                let mut client = connect_for_work(ctx).await?;
                let response = client
                    .send_request(&FrontendRequest::SetCiBudget {
                        work_item_id: args.work_item_id.clone(),
                        budget: if args.clear { None } else { args.budget },
                    })
                    .await
                    .map_err(CliError::internal)?;
                match response {
                    FrontendEvent::CiBudgetUpdated { budget } => {
                        print_entity(ctx, &serde_json::json!({ "budget": budget }), || {
                            if !ctx.quiet {
                                print_ci_budget_snapshot(&budget);
                            }
                        })
                    }
                    FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                        Err(CliError::application(message))
                    }
                    other => Err(unexpected_event("ci budget set", &other)),
                }
            }
        },
    }
}

async fn run_engine_attempts_command(command: EngineAttemptsCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineAttemptsCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
                None => None,
            };
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListEngineAttempts {
                    kinds: args.kind.clone(),
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::EngineAttemptsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_engine_attempts_table(&attempts)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("attempts list", &other)),
            }
        }
    }
}

async fn run_engine_conflicts_command(command: EngineConflictsCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineConflictsCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
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
                FrontendEvent::ConflictResolutionsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_conflict_resolutions_table(&attempts)
                    })
                }
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
                FrontendEvent::ConflictResolution { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        print_conflict_resolution_detail(&attempt)
                    })
                }
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
                FrontendEvent::ConflictResolutionRetried { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} reset to pending; engine will re-dispatch a worker.",
                                attempt.id,
                            );
                        }
                    })
                }
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
                FrontendEvent::ConflictResolutionMarkedAbandoned { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} marked abandoned (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    })
                }
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
        .set_header(vec!["ID", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
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
        ("worker_id", attempt.worker_id.clone().unwrap_or_else(|| unset.clone())),
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

fn print_ci_remediations_table(attempts: &[CiRemediation]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "KIND", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
    for attempt in attempts {
        table.add_row(vec![
            attempt.id.as_str(),
            attempt.attempt_kind.as_str(),
            attempt.status.as_str(),
            attempt.pr_url.as_str(),
            attempt.work_item_id.as_str(),
            attempt.failure_reason.as_deref().unwrap_or(""),
            attempt.created_at.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_ci_remediation_detail(attempt: &CiRemediation) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["FIELD", "VALUE"]);
    let unset = "<unset>".to_owned();
    let rows: Vec<(&str, String)> = vec![
        ("id", attempt.id.clone()),
        ("status", attempt.status.clone()),
        ("attempt_kind", attempt.attempt_kind.clone()),
        ("consumes_budget", attempt.consumes_budget.to_string()),
        ("product_id", attempt.product_id.clone()),
        ("work_item_id", attempt.work_item_id.clone()),
        ("pr_url", attempt.pr_url.clone()),
        ("pr_number", attempt.pr_number.to_string()),
        ("head_branch", attempt.head_branch.clone()),
        ("head_sha_at_trigger", attempt.head_sha_at_trigger.clone()),
        (
            "head_sha_after",
            attempt.head_sha_after.clone().unwrap_or_else(|| unset.clone()),
        ),
        (
            "triage_class",
            attempt.triage_class.clone().unwrap_or_else(|| unset.clone()),
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
        ("worker_id", attempt.worker_id.clone().unwrap_or_else(|| unset.clone())),
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
    if !attempt.failed_checks.is_empty() {
        println!();
        println!("failed_checks (raw):");
        println!("{}", attempt.failed_checks);
    }
    if let Some(log) = &attempt.log_excerpt {
        println!();
        println!("log_excerpt:");
        println!("{log}");
    }
}

fn print_ci_budget_snapshot(snapshot: &CiBudgetSnapshot) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["FIELD", "VALUE"]);
    let override_text = match snapshot.per_pr_override {
        Some(n) => n.to_string(),
        None => "<inherit>".to_owned(),
    };
    let blocked = snapshot.blocked_reason.clone().unwrap_or_else(|| "—".to_owned());
    let rows = vec![
        ("work_item_id", snapshot.work_item_id.clone()),
        ("per_pr_override", override_text),
        ("product_default", snapshot.product_default.to_string()),
        ("effective", snapshot.effective.to_string()),
        ("used", snapshot.used.to_string()),
        ("blocked_reason", blocked),
    ];
    for (field, value) in &rows {
        table.add_row(vec![*field, value.as_str()]);
    }
    println!("{table}");
}

fn print_ci_budget_after_retry(work_item_id: &str, budget: &CiBudgetSnapshot, was_exhausted: bool) {
    if was_exhausted {
        println!(
            "Reset ci_attempts_used for {} (used: {}/{} effective).",
            work_item_id, budget.used, budget.effective,
        );
        println!("Cleared blocked_reason='ci_failure_exhausted'.");
        println!("Parent will re-enter in_review on next probe; engine will auto-fix on detection of failure.",);
    } else {
        println!(
            "Reset ci_attempts_used for {} (used: {}/{} effective).",
            work_item_id, budget.used, budget.effective,
        );
        println!("Parent was not exhausted; no status change.");
    }
}

fn print_engine_attempts_table(attempts: &[EngineAttemptListEntry]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["KIND", "ID", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
    for row in attempts {
        table.add_row(vec![
            row.kind.as_str(),
            row.id.as_str(),
            row.status.as_str(),
            row.pr_url.as_str(),
            row.work_item_id.as_deref().unwrap_or(""),
            row.failure_reason.as_deref().unwrap_or(""),
            row.created_at.as_str(),
        ]);
    }
    println!("{table}");
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
    include_deleted: bool,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
            dep_filter,
            include_deleted,
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
    include_deleted: bool,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
            dep_filter,
            include_deleted,
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

async fn find_work_items_by_pr(client: &mut BossClient, pr_number: i64) -> Result<Vec<PrWorkItemMatch>, CliError> {
    match client
        .send_request(&FrontendRequest::FindWorkItemsByPr { pr_number })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemsByPrResult { matches, .. } => Ok(matches),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work items by pr", &other)),
    }
}

async fn create_product(client: &mut BossClient, input: CreateProductInput) -> Result<Product, CliError> {
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
            let org = args
                .org
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CliError::usage("--org is required for --kind github"))?;
            let repo = args
                .repo
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CliError::usage("--repo is required for --kind github"))?;
            let project_number = args
                .project
                .ok_or_else(|| CliError::usage("--project is required for --kind github"))?;
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

async fn create_project(client: &mut BossClient, input: CreateProjectInput) -> Result<Project, CliError> {
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

async fn set_project_design_doc(client: &mut BossClient, input: SetProjectDesignDocInput) -> Result<Project, CliError> {
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

/// Create the single task produced by an automation's triage phase
/// (`boss task create --automation`). The engine resolves provenance, the
/// open-task-cap re-check, repo inheritance, and execution dispatch; the CLI
/// is a thin pass-through. A cap-reached rejection surfaces as a `WorkError`.
async fn create_automation_task(
    client: &mut BossClient,
    automation_id: &str,
    name: String,
    description: Option<String>,
) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateAutomationTask {
            automation_id: automation_id.to_owned(),
            name,
            description,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("automation task create", &other)),
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

async fn create_investigation(client: &mut BossClient, input: CreateInvestigationInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateInvestigation { input })
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
            "An investigation named {name:?} was created {age_secs}s ago as T{existing_short_id} \
             ({existing_id}); pass --force-duplicate to create another."
        ))),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("investigation create", &other)),
    }
}

async fn run_create_investigation(
    client: &mut BossClient,
    ctx: &RunContext,
    args: InvestigationCreateArgs,
) -> Result<(), CliError> {
    let product = resolve_product_inferable(client, args.product, args.project.as_deref(), ctx).await?;
    let project_id = if let Some(proj) = args.project {
        let project = resolve_project(client, &product.id, Some(proj), ctx).await?;
        Some(project.id)
    } else {
        None
    };
    let name = required_text(args.name, "Investigation name", ctx)?;
    let description = optional_text(args.description, "Description", ctx)?;
    let task = create_investigation(
        client,
        CreateInvestigationInput {
            product_id: product.id,
            project_id,
            name: name.clone(),
            description,
            autostart: !ctx.no_autostart,
            priority: args.priority.map(|p| p.as_str().to_owned()),
            created_via: Some("cli".to_owned()),
            repo_remote_url: args.repo_remote_url,
            effort_level: args.effort.map(boss_protocol::EffortLevel::from),
            model_override: args.model,
            force_duplicate: args.force_duplicate,
        },
    )
    .await?;
    print_entity(ctx, &serde_json::json!({ "task": task }), || {
        if !ctx.quiet {
            println!("created investigation T{}: {}", task.short_id.unwrap_or(0), name);
        }
    })?;
    Ok(())
}

async fn create_revision_rpc(client: &mut BossClient, input: CreateRevisionInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateRevision { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("revision create", &other)),
    }
}

/// Resolve a `--parent` selector for `create-revision` to a primary task id.
///
/// Unlike the generic [`resolve_selector_to_primary_id`], this variant does
/// not require a product context: `T<n>` short ids are globally unique, so
/// we pass them straight to `GetWorkItem` which resolves them DB-globally
/// (via `get_work_item_resolving_short_id` in the engine). This is the only
/// product-free resolution we allow here; `#42` / `42` bare forms still need
/// a product and are rejected with a helpful message.
async fn resolve_create_revision_parent(client: &mut BossClient, selector: &str) -> Result<String, CliError> {
    match parse_work_item_selector(selector) {
        // T-form short ids are globally unique — pass the friendly form
        // straight to GetWorkItem; the engine resolves it without a product.
        WorkItemSelector::ShortId(_) => {
            let item = get_work_item(client, selector).await?;
            Ok(work_item_primary_id(&item).to_owned())
        }
        // Already a primary id or opaque slug — pass through unchanged.
        WorkItemSelector::PrimaryId(id) | WorkItemSelector::Other(id) => Ok(id),
        // Cross-product slug form (boss/42) — also unambiguous.
        WorkItemSelector::ProductShortId { .. } => {
            // Shouldn't normally appear given the --parent doc, but handle it
            // via the standard resolution with an empty product context.
            // This will fail clearly if the product slug can't be resolved.
            let item = get_work_item(client, selector).await?;
            Ok(work_item_primary_id(&item).to_owned())
        }
    }
}

async fn run_create_revision(
    client: &mut BossClient,
    ctx: &RunContext,
    args: RevisionCreateArgs,
) -> Result<(), CliError> {
    // Resolve the --parent selector to a full task id before sending to
    // the engine, since the engine's CreateRevision RPC requires a full id.
    // We use a product-free resolver here: T<n> short ids are globally unique
    // so no --product flag is needed (or accepted) for create-revision.
    let parent_id = resolve_create_revision_parent(client, &args.parent).await?;
    let description = args.description.trim().to_owned();
    if description.is_empty() {
        return Err(CliError::usage("--description must be non-empty"));
    }
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let task = create_revision_rpc(
        client,
        CreateRevisionInput {
            parent_task_id: parent_id,
            description: description.clone(),
            name,
            priority: args.priority.map(|p| p.as_str().to_owned()),
            effort_level: args.effort.map(boss_protocol::EffortLevel::from),
            model_override: args.model,
            force_duplicate: args.force_duplicate,
            created_via: Some(boss_protocol::CREATED_VIA_CLI.to_owned()),
            autostart: !ctx.no_autostart,
        },
    )
    .await?;
    print_entity(ctx, &serde_json::json!({ "task": task }), || {
        if !ctx.quiet {
            println!("created revision T{}: {}", task.short_id.unwrap_or(0), description);
        }
    })?;
    Ok(())
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

async fn update_work_item(client: &mut BossClient, id: &str, patch: WorkItemPatch) -> Result<WorkItem, CliError> {
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

/// Recover a repo's base URL from a PR URL by dropping the
/// `/pull/<n>` segment (and anything after it):
/// `https://github.com/owner/repo/pull/959` →
/// `https://github.com/owner/repo`. Returns the input unchanged when
/// no `/pull/` segment is present, so a non-PR-shaped URL still flows
/// through the repo matcher.
fn repo_url_from_pr_url(pr_url: &str) -> &str {
    pr_url.split_once("/pull/").map_or(pr_url, |(base, _)| base)
}

/// Friendly `T<n>` id, falling back to the canonical id when a row
/// somehow lacks a short_id.
fn friendly_task_id(task: &Task) -> String {
    task.short_id
        .map(|n| format!("T{n}"))
        .unwrap_or_else(|| task.id.clone())
}

/// Apply [`with_display_status`] to the owner and every revision in a
/// PR match so rendered statuses use the board vocabulary.
fn with_display_pr_match(m: PrWorkItemMatch) -> PrWorkItemMatch {
    PrWorkItemMatch {
        owner: with_display_status(m.owner),
        revisions: m.revisions.into_iter().map(with_display_status).collect(),
    }
}

/// Human-readable rendering of a single PR → work-item match: the
/// owning row plus any revisions in the PR's chain.
fn print_pr_match(m: &PrWorkItemMatch) {
    let owner = &m.owner;
    let repo = owner.pr_url.as_deref().map(repo_url_from_pr_url).unwrap_or("");
    println!(
        "{}  {}  [{}]  {}",
        friendly_task_id(owner),
        owner.kind,
        owner.status,
        owner.name,
    );
    if !repo.is_empty() {
        println!("Repo: {repo}");
    }
    if let Some(pr_url) = &owner.pr_url {
        println!("PR URL: {pr_url}");
    }
    if m.revisions.is_empty() {
        return;
    }
    println!("Revisions in this PR's chain:");
    for revision in &m.revisions {
        let seq = revision.revision_seq.map(|n| format!("R{n} ")).unwrap_or_default();
        println!(
            "  {seq}{}  [{}]  {}",
            friendly_task_id(revision),
            revision.status,
            revision.name,
        );
    }
}

/// Handler for `boss task by-pr <pr-number> [--repo <r>]`. Resolves a
/// PR number to the work item that owns it, spanning all kinds. When
/// `--repo` is given, matches are filtered by the repo parsed from
/// each owner's PR URL; ambiguity (the same number in >1 repo) and
/// not-found are surfaced as clear errors.
async fn run_by_pr(client: &mut BossClient, ctx: &RunContext, args: ByPrArgs) -> Result<(), CliError> {
    if args.pr_number <= 0 {
        return Err(CliError::usage("PR number must be a positive integer"));
    }
    let repo_selector = args.repo.as_deref().map(RepoSelector::parse).transpose()?;
    let matches = find_work_items_by_pr(client, args.pr_number).await?;

    // Repo filter (when given) matches against the repo parsed from the
    // owner's PR URL — the PR URL, not the work item's repo override, is
    // what authoritatively places the PR in a repo.
    let matches: Vec<PrWorkItemMatch> = match repo_selector.as_ref() {
        Some(selector) => matches
            .into_iter()
            .filter(|m| {
                m.owner
                    .pr_url
                    .as_deref()
                    .is_some_and(|url| selector.matches(Some(repo_url_from_pr_url(url))))
            })
            .collect(),
        None => matches,
    };

    match matches.len() {
        0 => {
            let scope = match args.repo.as_deref() {
                Some(repo) => format!(" in a repo matching {repo:?}"),
                None => String::new(),
            };
            Err(CliError::not_found(format!(
                "no work item bound to PR #{}{scope}",
                args.pr_number,
            )))
        }
        1 => {
            let matched = with_display_pr_match(matches.into_iter().next().expect("len checked == 1"));
            print_entity(
                ctx,
                &serde_json::json!({ "match": &matched, "matches": [&matched] }),
                || {
                    print_pr_match(&matched);
                },
            )
        }
        _ => {
            let matched: Vec<PrWorkItemMatch> = matches.into_iter().map(with_display_pr_match).collect();
            print_entity(ctx, &serde_json::json!({ "matches": &matched }), || {
                for (i, m) in matched.iter().enumerate() {
                    if i > 0 {
                        println!();
                    }
                    print_pr_match(m);
                }
            })
        }
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
        Some("") => BindPrAction::FirstTime,
        Some(p) => BindPrAction::Overwrite { previous: p },
        None => BindPrAction::FirstTime,
    }
}

/// Shared handler for `boss task bind-pr` and `boss chore bind-pr`.
/// The kind is read from the actual item, not the noun the user
/// typed, so either invocation works against any leaf work item id.
async fn run_bind_pr(client: &mut BossClient, ctx: &RunContext, args: BindPrArgs) -> Result<(), CliError> {
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
async fn run_link_external(client: &mut BossClient, ctx: &RunContext, args: LinkExternalArgs) -> Result<(), CliError> {
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
    print_entity(ctx, &serde_json::json!({ label: updated }), || {
        print_task_details(&title, &updated, None, false)
    })
}

/// Shared handler for `boss task unlink-external` and `boss chore unlink-external`.
async fn run_unlink_external(client: &mut BossClient, ctx: &RunContext, args: TaskIdArg) -> Result<(), CliError> {
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
    print_entity(ctx, &serde_json::json!({ label: updated }), || {
        print_task_details(&title, &updated, None, false)
    })
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
    let product = resolve_product_inferable(client, args.product, args.project.as_deref(), ctx).await?;
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

async fn create_many_tasks(client: &mut BossClient, input: CreateManyTasksInput) -> Result<Vec<Task>, CliError> {
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

async fn create_many_chores(client: &mut BossClient, input: CreateManyChoresInput) -> Result<Vec<Task>, CliError> {
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

fn handle_create_many_response<F>(event: FrontendEvent, context: &str, extract: F) -> Result<Vec<Task>, CliError>
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
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .ok_or_else(|| CliError::usage("PR URL must be of the form https://github.com/<org>/<repo>/pull/<n>"))?;
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

async fn restore_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::RestoreWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemRestored { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item restore", &other)),
    }
}

async fn run_depend_command(command: DependCommand, client: &mut BossClient, ctx: &RunContext) -> Result<(), CliError> {
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
                            println!("Removed dependency: {} → {}", dependent, prerequisite,);
                        } else {
                            println!("No dependency {} → {} (no-op)", dependent, prerequisite,);
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

async fn add_dependency(client: &mut BossClient, input: AddDependencyInput) -> Result<WorkItemDependency, CliError> {
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

async fn remove_dependency(client: &mut BossClient, input: RemoveDependencyInput) -> Result<bool, CliError> {
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

async fn list_executions_for_item(client: &mut BossClient, work_item_id: &str) -> Result<Vec<WorkExecution>, CliError> {
    match client
        .send_request(&FrontendRequest::ListExecutions {
            work_item_id: Some(work_item_id.to_owned()),
            include_revision_chain: false,
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

async fn get_task_runtime(client: &mut BossClient, work_item_id: &str) -> Result<TaskRuntime, CliError> {
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
        print!(
            "  {} [{}] started={} finished={}",
            exec.id, exec.status, started, finished
        );
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
        lines.push(format!("  Prerequisites ({}):", detail.prerequisites.len()));
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
    // Projects carry their own status taxonomy (planned/active/…); only
    // task/chore edges share the board vocabulary, so remap just those.
    let status = if edge.id.starts_with("proj_") {
        edge.status.as_str()
    } else {
        status_vocab::to_ui(&edge.status)
    };
    format!("    {id:<32}  {status:<10}{name}{suffix}", id = edge.id,)
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

async fn reorder_project_tasks(client: &mut BossClient, project_id: &str, task_ids: &[String]) -> Result<(), CliError> {
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

/// Like [`resolve_product`] but returns `None` when no `--product` was
/// supplied and resolution is not needed (e.g. when the caller is about
/// to use a canonical `auto_…` id directly). Only resolves the product
/// when a `--product` flag is supplied or when there is exactly one
/// product (auto-selected). Does NOT prompt interactively.
async fn resolve_optional_product(
    client: &mut BossClient,
    selector: Option<String>,
    _ctx: &RunContext,
) -> Result<Option<Product>, CliError> {
    match selector {
        None => {
            // Try auto-select when exactly one product exists, so A<n>
            // selectors work without --product on single-product setups.
            let products = list_products(client).await?;
            if products.len() == 1 {
                Ok(Some(products.into_iter().next().unwrap()))
            } else {
                Ok(None)
            }
        }
        Some(sel) => {
            let products = list_products(client).await?;
            if products.is_empty() {
                return Err(CliError::not_found("no products exist"));
            }
            Ok(Some(match_products(&products, &sel)?))
        }
    }
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
        if !product_slug.is_empty()
            && let Ok(n) = rest.parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ProductShortId {
                product_slug: product_slug.to_owned(),
                n,
            };
        }
    }
    // `#42` form (explicit friendly-id prefix)
    if let Some(rest) = s.strip_prefix('#')
        && let Ok(n) = rest.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
    }
    // `T441` / `t441` / `P12` / `p12` — friendly kanban id (T for tasks/chores, P for projects).
    // Case-insensitive; the leading letter is just visual sugar for the short_id number.
    if s.len() >= 2 {
        let first = s.as_bytes()[0];
        if (first == b'T' || first == b't' || first == b'P' || first == b'p')
            && let Ok(n) = s[1..].parse::<i64>()
            && n > 0
        {
            return WorkItemSelector::ShortId(n);
        }
    }
    // Plain integer → short id (Q5 step 2: `#` is optional)
    if let Ok(n) = s.parse::<i64>()
        && n > 0
    {
        return WorkItemSelector::ShortId(n);
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
async fn product_id_from_typed_selector(client: &mut BossClient, selector: &str) -> Result<Option<String>, CliError> {
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
    let inferred = products.iter().find(|p| p.id == inferred_id).cloned().ok_or_else(|| {
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
        return Err(CliError::not_found("no projects exist for the selected product"));
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
        _ => Err(CliError::conflict("selector resolved to multiple work items")),
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

fn optional_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<Option<String>, CliError> {
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
        || patch.worker_branch_prefix.is_some()
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

/// Resolve a task / chore's effective repo: its own override wins;
/// fall back to the product's default. Used by the `--repo` filter
/// so `--repo nimbus` finds inherited matches too (design R10 / Q3).
fn resolved_repo_for_task<'a>(task: &'a Task, product_repo: Option<&'a str>) -> Option<&'a str> {
    task.repo_remote_url.as_deref().or(product_repo)
}

fn apply_task_list_filters(
    items: Vec<Task>,
    statuses: &[TaskStatusArg],
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
            if !allowed_priorities.is_empty() && !allowed_priorities.contains(&task.priority.as_str()) {
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
            if let Some(selector) = repo
                && !selector.matches(resolved_repo_for_task(task, product_repo))
            {
                return false;
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

fn apply_project_list_filters(
    items: Vec<Project>,
    statuses: &[ProjectStatusArg],
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
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&project.status.as_str()) {
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
            let friendly = project.short_id.map(|n| format!("P{n}")).unwrap_or_default();
            row.push(friendly);
        }
        if !show_short_id || with_primary_id {
            row.push(project.id.clone());
        }
        row.push(project.slug.clone());
        row.push(project.name.clone());
        row.push(project.status.to_string());
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
    // Surface the soft-delete tombstone only when a row actually carries
    // one — i.e. when the caller passed `--deleted`. Keeps the common
    // live-only listing unchanged. Mirrors the `show_effort` pattern.
    let show_deleted = tasks.iter().any(|t| t.deleted_at.is_some());
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
    if show_deleted {
        header.push("DELETED");
    }
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header);
    for task in tasks {
        let ordinal = task.ordinal.map(|value| value.to_string()).unwrap_or_default();
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
        row.push(task.status.display_label().to_owned());
        row.push(task.priority.clone());
        if show_effort {
            row.push(effort_str);
        }
        row.push(task.project_id.clone().unwrap_or_default());
        row.push(ordinal);
        row.push(task.pr_url.clone().unwrap_or_default());
        if show_deleted {
            row.push(task.deleted_at.clone().unwrap_or_default());
        }
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
    if let Some(design_repo) = product.design_repo.as_deref() {
        println!("Design repo: {design_repo}");
    }
    if let Some(docs_repo) = product.docs_repo.as_deref() {
        println!("Docs repo: {docs_repo}");
    }
    if let Some(prefix) = product.worker_branch_prefix.as_deref() {
        println!("Worker branch prefix: {prefix}");
    }
    if let Some(model) = product.default_model.as_deref() {
        println!("Default model: {model}");
    }
    if let Some(preamble) = product.dispatch_preamble.as_deref() {
        println!("Dispatch preamble: {preamble}");
    }
    if let Some(rules) = product.editorial_rules.as_ref() {
        println!("Editorial rules:");
        let branch_str = match &rules.branch_naming {
            boss_protocol::BranchNaming::BossExecPrefix => "boss-exec-prefix (default)".to_owned(),
            boss_protocol::BranchNaming::OpaqueHash => "opaque-hash".to_owned(),
            boss_protocol::BranchNaming::CustomPrefix { prefix } => {
                format!("custom-prefix ({prefix})")
            }
        };
        println!("  Branch naming: {branch_str}");
        let template_str = match rules.template_policy {
            boss_protocol::TemplatePolicy::Off => "off (default)",
            boss_protocol::TemplatePolicy::Advise => "advise",
            boss_protocol::TemplatePolicy::Enforce => "enforce",
        };
        println!("  Template policy: {template_str}");
        let trailer_str = match rules.commit_trailer_policy {
            boss_protocol::TrailerPolicy::Default => "default",
            boss_protocol::TrailerPolicy::NoAiTrailer => "no-ai-trailer",
        };
        println!("  Commit trailer: {trailer_str}");
        if !rules.redactions.is_empty() {
            println!("  Redactions: {} rule(s)", rules.redactions.len());
        }
        if let Some(instructions) = rules.instructions.as_deref() {
            println!("  Instructions: {instructions}");
        }
        if product.dispatch_preamble.is_some() && rules.instructions.is_some() {
            println!(
                "  [note] Both dispatch_preamble and editorial_rules.instructions are set — consider consolidating into editorial_rules.instructions (R11)."
            );
        }
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
    if let Some(url) = product.repo_remote_url.as_deref().filter(|s| !s.is_empty()) {
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

/// Severity classification for `boss project lint-design-docs`.
/// `Broken` entries drive the verb's non-zero exit code so the lint
/// is usable from CI; `Missing` / `Unverified` are advisory only and
/// only appear when the matching `--include-…` flag is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LintSeverity {
    /// The resolver returned `Broken`, or the resolved path doesn't
    /// exist on disk in the leased workspace.
    Broken,
    /// No pointer set on the project at all. Advisory; only included
    /// when `--include-missing` is passed.
    Missing,
    /// Resolver returned `Resolved` but no workspace was leased for
    /// the repo, so the file's existence couldn't be confirmed.
    /// Advisory; only included when `--include-unverified` is passed.
    Unverified,
}

/// One row in the `lint-design-docs` report. Carries enough state for
/// the human to act on the finding without re-resolving: project id +
/// slug for identification, product slug for the grouping context,
/// the current pointer fields (so the user can see what's set), the
/// reason the finding fired, and a copy-pasteable `suggested_fix`
/// CLI invocation.
#[derive(bon::Builder, Debug, Clone, Serialize)]
#[builder(on(String, into))]
struct LintDesignDocEntry {
    project_id: String,
    project_slug: String,
    project_name: String,
    product_id: String,
    product_slug: String,
    severity: LintSeverity,
    /// Current `design_doc_path` value on the project row, if any.
    design_doc_path: Option<String>,
    /// Current `design_doc_repo_remote_url` override, if any. `None`
    /// means the project inherits from `product.repo_remote_url`.
    design_doc_repo_remote_url: Option<String>,
    /// Current `design_doc_branch` override, if any. `None` means the
    /// branch falls back to `"main"`.
    design_doc_branch: Option<String>,
    /// Human-readable explanation of why this entry was flagged. The
    /// table renderer prints this verbatim; the JSON form carries it
    /// for programmatic consumers.
    reason: String,
    /// A `boss project ...` invocation the user can run to repair the
    /// finding. For `Broken` / `Missing` it's a `set-design-doc`
    /// template with the project selector pre-filled; the user fills
    /// in the new path. For `Unverified` it's `open-design --print
    /// --web` so the user can manually confirm the doc still exists.
    suggested_fix: String,
}

/// Pure classifier used by `boss project lint-design-docs`. Returns
/// `None` when the project is healthy (or its finding doesn't match
/// the caller's `--include-…` flags); returns `Some(entry)` when the
/// project should appear in the lint report. `file_check` is the
/// filesystem-probe callback (typically [`check_design_doc_file_exists`],
/// stubbed in unit tests).
fn classify_lint_finding<F>(
    product: &Product,
    project: &Project,
    state: Option<&ProjectDesignDocState>,
    file_check: F,
    include_missing: bool,
    include_unverified: bool,
) -> Option<LintDesignDocEntry>
where
    F: FnOnce(&str, &str) -> bool,
{
    let selector = format!("{}/{}", product.slug, project.slug);
    match state {
        None => {
            // `design_doc_path` is NULL — project has no pointer.
            if !include_missing {
                return None;
            }
            Some(LintDesignDocEntry {
                project_id: project.id.clone(),
                project_slug: project.slug.clone(),
                project_name: project.name.clone(),
                product_id: product.id.clone(),
                product_slug: product.slug.clone(),
                severity: LintSeverity::Missing,
                design_doc_path: None,
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                reason: "no design-doc pointer set".to_owned(),
                suggested_fix: format!("boss project set-design-doc {selector} --path <repo-relative-path>"),
            })
        }
        Some(ProjectDesignDocState::NotSet) => {
            // Should be unreachable when the caller only resolves
            // projects with `design_doc_path` set — but treat it as
            // equivalent to the `None` arm for robustness.
            if !include_missing {
                return None;
            }
            Some(LintDesignDocEntry {
                project_id: project.id.clone(),
                project_slug: project.slug.clone(),
                project_name: project.name.clone(),
                product_id: product.id.clone(),
                product_slug: product.slug.clone(),
                severity: LintSeverity::Missing,
                design_doc_path: None,
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                reason: "no design-doc pointer set".to_owned(),
                suggested_fix: format!("boss project set-design-doc {selector} --path <repo-relative-path>"),
            })
        }
        Some(ProjectDesignDocState::Broken { reason }) => Some(LintDesignDocEntry {
            project_id: project.id.clone(),
            project_slug: project.slug.clone(),
            project_name: project.name.clone(),
            product_id: product.id.clone(),
            product_slug: product.slug.clone(),
            severity: LintSeverity::Broken,
            design_doc_path: project.design_doc_path.clone(),
            design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
            design_doc_branch: project.design_doc_branch.clone(),
            reason: reason.clone(),
            suggested_fix: format!("boss project set-design-doc {selector} --path <p> --repo <repo-url>"),
        }),
        Some(ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            ..
        }) => match workspace_path.as_deref() {
            Some(workspace) => {
                if file_check(workspace, &resolved.path) {
                    None
                } else {
                    Some(LintDesignDocEntry {
                        project_id: project.id.clone(),
                        project_slug: project.slug.clone(),
                        project_name: project.name.clone(),
                        product_id: product.id.clone(),
                        product_slug: product.slug.clone(),
                        severity: LintSeverity::Broken,
                        design_doc_path: Some(resolved.path.clone()),
                        design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
                        design_doc_branch: project.design_doc_branch.clone(),
                        reason: format!(
                            "file not found at {}/{} (pointer may be stale after a rename)",
                            workspace, resolved.path,
                        ),
                        suggested_fix: format!("boss project set-design-doc {selector} --path <new-path>"),
                    })
                }
            }
            None => {
                if !include_unverified {
                    return None;
                }
                Some(LintDesignDocEntry {
                    project_id: project.id.clone(),
                    project_slug: project.slug.clone(),
                    project_name: project.name.clone(),
                    product_id: product.id.clone(),
                    product_slug: product.slug.clone(),
                    severity: LintSeverity::Unverified,
                    design_doc_path: Some(resolved.path.clone()),
                    design_doc_repo_remote_url: project.design_doc_repo_remote_url.clone(),
                    design_doc_branch: project.design_doc_branch.clone(),
                    reason: format!(
                        "no leased workspace for {} — cannot verify file exists",
                        resolved.repo_remote_url,
                    ),
                    suggested_fix: format!("boss project open-design {selector} --print --web"),
                })
            }
        },
    }
}

/// Filesystem probe used by the real CLI handler — `true` when the
/// resolved doc exists as a regular file inside the leased
/// workspace. Symlinks resolve through; broken symlinks return
/// `false`. The pure classifier takes this as an injectable callback
/// so the unit tests don't have to touch disk.
fn check_design_doc_file_exists(workspace_path: &str, repo_relative_path: &str) -> bool {
    PathBuf::from(workspace_path).join(repo_relative_path).is_file()
}

fn print_lint_design_docs_table(entries: &[LintDesignDocEntry]) {
    if entries.is_empty() {
        println!("No design-doc pointer issues found.");
        return;
    }
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["SEVERITY", "PROJECT", "PATH", "REASON"]);
    for entry in entries {
        table.add_row(vec![
            lint_severity_label(entry.severity).to_owned(),
            format!("{}/{}", entry.product_slug, entry.project_slug),
            entry.design_doc_path.clone().unwrap_or_default(),
            entry.reason.clone(),
        ]);
    }
    println!("{table}");
    println!();
    println!("Suggested fixes:");
    for entry in entries {
        println!(
            "  [{}] {}/{}: {}",
            lint_severity_label(entry.severity),
            entry.product_slug,
            entry.project_slug,
            entry.suggested_fix,
        );
    }
    println!();
    println!("{}", lint_summary_line(entries));
}

fn lint_severity_label(severity: LintSeverity) -> &'static str {
    match severity {
        LintSeverity::Broken => "broken",
        LintSeverity::Missing => "missing",
        LintSeverity::Unverified => "unverified",
    }
}

/// One-line tally of the lint findings, broken down by severity, for
/// the human report footer (the JSON form already carries
/// `broken_count`). Only severities actually present are listed, so a
/// run that surfaces nothing but stale pointers reads "2 finding(s): 2
/// broken" rather than padding the line with zero counts. Callers
/// invoke this only when `entries` is non-empty — the empty case is
/// handled earlier with a dedicated "no issues" message.
fn lint_summary_line(entries: &[LintDesignDocEntry]) -> String {
    let count = |severity| entries.iter().filter(|e| e.severity == severity).count();
    let parts: Vec<String> = [
        (LintSeverity::Broken, "broken"),
        (LintSeverity::Missing, "missing"),
        (LintSeverity::Unverified, "unverified"),
    ]
    .into_iter()
    .filter_map(|(severity, label)| match count(severity) {
        0 => None,
        n => Some(format!("{n} {label}")),
    })
    .collect();
    format!("{} finding(s): {}", entries.len(), parts.join(", "))
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
        ProjectDesignDocState::Resolved { resolved, web_url, .. } => Some(format!("{} ({})", resolved.path, web_url)),
        ProjectDesignDocState::Broken { reason } => Some(format!("(broken) {reason}")),
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
                    eprintln!("warning: $EDITOR not set; falling back to web URL ({web_url})",);
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
fn decide_open_design_action(state: &ProjectDesignDocState, force_web: bool) -> Result<OpenDesignAction, CliError> {
    match state {
        ProjectDesignDocState::NotSet => Err(CliError::not_found(
            "project has no design-doc pointer (set one with `boss project set-design-doc`)",
        )),
        ProjectDesignDocState::Broken { reason } => {
            Err(CliError::conflict(format!("design-doc pointer is broken: {reason}",)))
        }
        ProjectDesignDocState::Resolved {
            resolved,
            workspace_path,
            web_url,
            ..
        } => {
            if force_web {
                return Ok(OpenDesignAction::Web { url: web_url.clone() });
            }
            let can_use_filesystem = matches!(
                resolved.kind,
                ResolvedDesignDocKind::SameProduct { .. } | ResolvedDesignDocKind::OtherProduct { .. },
            ) && workspace_path.is_some();
            if can_use_filesystem {
                Ok(OpenDesignAction::LocalFile {
                    path: PathBuf::from(&resolved.path),
                    web_url: web_url.clone(),
                })
            } else {
                Ok(OpenDesignAction::Web { url: web_url.clone() })
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
    println!("Status: {}", task.status.display_label());
    if let Some(product) = parent_product {
        println!("Repo: {}", format_repo_line(task.repo_remote_url.as_deref(), product),);
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
    let home = std::env::var_os("HOME")
        .ok_or_else(|| CliError::internal(anyhow::anyhow!("HOME is not set; cannot resolve install root")))?;
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

async fn run_uninstall_command(args: UninstallArgs, flags: &GlobalFlags) -> Result<(), CliError> {
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
            eprintln!("boss uninstall: no installed Boss found at {}", app_path.display());
            eprintln!("If Boss is running from a dev build, uninstall is not applicable.");
        }
        return Err(CliError::internal(anyhow::anyhow!("no installed Boss to uninstall")));
    }

    let state_root = resolve_state_root_for_uninstall();

    if !flags.json {
        println!("This will remove:");
        println!("  {}", app_path.display());
        if args.purge_state
            && let Some(ref state) = state_root
        {
            println!("  {} (--purge-state)", state.display());
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
        let pid_path =
            std::env::var("BOSS_ENGINE_PID_PATH").unwrap_or_else(|_| boss_client::DEFAULT_PID_PATH.to_owned());
        let _ = stop_engine(&pid_path).await;
    } else {
        eprintln!(
            "note: not stopping engine: BOSS_INSTALL_ROOT is set; \
             assuming the caller manages their own engine lifecycle"
        );
    }

    std::fs::remove_dir_all(&app_path)
        .map_err(|e| CliError::internal(anyhow::anyhow!("failed to remove {}: {e}", app_path.display())))?;

    let mut removed = vec![app_path.display().to_string()];

    if args.purge_state
        && let Some(state) = state_root
        && state.exists()
    {
        std::fs::remove_dir_all(&state)
            .map_err(|e| CliError::internal(anyhow::anyhow!("failed to remove {}: {e}", state.display())))?;
        removed.push(state.display().to_string());
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

/// Split a bug-report blob into a `(title, body)` pair.
///
/// The first non-blank line is the title (with a leading `# ` stripped
/// so a markdown H1 also works as the report heading). The remainder of
/// the file — minus the blank lines that immediately follow the title —
/// becomes the body. An empty blob is rejected by the caller; here we
/// just trust the input has at least one non-blank line.
fn split_shake_report(blob: &str) -> Option<(String, String)> {
    let mut lines = blob.lines();
    let title_line = lines.by_ref().find(|line| !line.trim().is_empty())?;
    let title = title_line
        .trim_start()
        .strip_prefix("# ")
        .unwrap_or(title_line)
        .trim()
        .to_owned();
    if title.is_empty() {
        return None;
    }

    let mut body_lines: Vec<&str> = lines.collect();
    while body_lines.first().is_some_and(|line| line.trim().is_empty()) {
        body_lines.remove(0);
    }
    while body_lines.last().is_some_and(|line| line.trim().is_empty()) {
        body_lines.pop();
    }
    let body = body_lines.join("\n");

    Some((title, body))
}

async fn run_shake_command(args: ShakeArgs, flags: &GlobalFlags) -> Result<(), CliError> {
    let blob = if args.file == "-" {
        let mut s = String::new();
        io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| CliError::internal(anyhow::anyhow!("read stdin: {e}")))?;
        s
    } else {
        std::fs::read_to_string(&args.file)
            .map_err(|e| CliError::usage(format!("cannot read bug report {}: {e}", args.file)))?
    };

    let (title, body) = if let Some(explicit_title) = args.title.as_deref() {
        let title = explicit_title.trim();
        if title.is_empty() {
            return Err(CliError::usage("--title cannot be blank".to_owned()));
        }
        (title.to_owned(), blob.trim_end_matches('\n').to_owned())
    } else {
        split_shake_report(&blob).ok_or_else(|| {
            CliError::usage("bug report is empty — need at least one non-blank line for a title".to_owned())
        })?
    };

    if args.dry_run {
        if flags.json {
            println!(
                "{}",
                serde_json::json!({
                    "status": "dry_run",
                    "repo": args.repo,
                    "title": title,
                    "body": body,
                    "labels": args.labels,
                    "github_project": args.github_project,
                })
            );
        } else {
            println!("repo:  {}", args.repo);
            println!("title: {title}");
            if !args.labels.is_empty() {
                println!("labels: {}", args.labels.join(", "));
            }
            if !args.github_project.is_empty() {
                println!("github-project: {}", args.github_project);
            }
            println!("---");
            println!("{body}");
        }
        return Ok(());
    }

    let cfg = github_app::embedded_config().map_err(|e| CliError::application(e.to_string()))?;
    let api_base = std::env::var("BOSS_GITHUB_API_BASE").unwrap_or_else(|_| github_app::DEFAULT_API_BASE.to_owned());

    let issue = github_app::file_issue(&cfg, &api_base, &args.repo, &title, &body, &args.labels)
        .await
        .map_err(|e| CliError::application(format!("{e:#}")))?;

    // Associate the new issue with the configured GitHub Project so the
    // Boss importer (which scopes to that project) can reconcile it.
    // Skip if the caller explicitly passed an empty project node ID.
    if !args.github_project.is_empty() {
        github_app::add_issue_to_project_with_embedded_token(&cfg, &api_base, &args.github_project, &issue.node_id)
            .await
            .map_err(|e| CliError::application(format!("add issue to project: {e:#}")))?;
    }

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "filed",
                "repo": args.repo,
                "url": issue.html_url,
                "number": issue.number,
                "title": title,
            })
        );
    } else {
        println!(
            "filed issue against {}: {} (#{})",
            args.repo, issue.html_url, issue.number
        );
    }

    Ok(())
}

async fn run_release_command(flags: &GlobalFlags) -> Result<(), CliError> {
    let token = std::env::var("BK_API_TOKEN").map_err(|_| {
        CliError::application(
            "BK_API_TOKEN is not set. See tools/boss/docs/buildkite-release-setup.md \
             for provisioning instructions."
                .to_owned(),
        )
    })?;

    if token.is_empty() {
        return Err(CliError::application(
            "BK_API_TOKEN is set but empty. See tools/boss/docs/buildkite-release-setup.md \
             for provisioning instructions."
                .to_owned(),
        ));
    }

    let api_base = std::env::var("BOSS_BK_API_BASE").unwrap_or_else(|_| buildkite_release::DEFAULT_API_BASE.to_owned());

    let build = buildkite_release::trigger_release_build(&api_base, &token)
        .await
        .map_err(|e| CliError::application(format!("{e:#}")))?;

    if flags.json {
        println!(
            "{}",
            serde_json::json!({
                "status": "triggered",
                "build_url": build.web_url,
                "build_number": build.number,
            })
        );
    } else {
        println!("triggered release build #{}: {}", build.number, build.web_url);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        AttentionGroupSelector, AutomationCommand, AutomationSelector, BindPrAction, BulkCreateItem, ChoreCommand, Cli,
        Commands, EffortLevelArg, LintSeverity, MoveTarget, OpenDesignAction, ProductCommand, ProductStatus,
        ProjectCommand, ProjectStatusArg, RepoSelector, RunContext, TaskCommand, TaskStatusArg, classify_bind_pr,
        classify_lint_finding, compile_schedule, decide_open_design_action, ensure_explicit_product_matches,
        expect_leaf_work_item, format_project_design_doc_line, format_repo_line, is_typed_work_item_id,
        lint_summary_line, parse_attention_group_selector, parse_automation_selector, pick_by_index,
        split_shake_report, status_vocab, validate_github_pr_url, with_display_status,
    };
    use boss_protocol::{
        Product, Project, ProjectDesignDocState, ProjectStatus, ResolvedDesignDoc, ResolvedDesignDocKind, Task,
        TaskKind, TaskStatus, WorkItem,
    };

    #[test]
    fn move_target_maps_board_names_to_stored() {
        assert_eq!(MoveTarget::Backlog.as_status(), "todo");
        assert_eq!(MoveTarget::Doing.as_status(), "active");
        assert_eq!(MoveTarget::Review.as_status(), "in_review");
        assert_eq!(MoveTarget::Done.as_status(), "done");
        assert_eq!(MoveTarget::Blocked.as_status(), "blocked");
    }

    #[test]
    fn task_status_arg_maps_board_names_to_stored() {
        // `--status`/filter values are the board names; the stored
        // string sent to the engine stays in the legacy vocabulary.
        assert_eq!(TaskStatusArg::Backlog.as_str(), "todo");
        assert_eq!(TaskStatusArg::Doing.as_str(), "active");
        assert_eq!(TaskStatusArg::Review.as_str(), "in_review");
        assert_eq!(TaskStatusArg::Done.as_str(), "done");
        assert_eq!(TaskStatusArg::Blocked.as_str(), "blocked");
    }

    #[test]
    fn status_vocab_maps_stored_to_board_names() {
        assert_eq!(status_vocab::to_ui("todo"), "backlog");
        assert_eq!(status_vocab::to_ui("active"), "doing");
        assert_eq!(status_vocab::to_ui("in_review"), "review");
        // done / blocked are identical in both vocabularies.
        assert_eq!(status_vocab::to_ui("done"), "done");
        assert_eq!(status_vocab::to_ui("blocked"), "blocked");
        // Unknown values pass through unchanged.
        assert_eq!(status_vocab::to_ui("archived"), "archived");
    }

    #[test]
    fn display_label_maps_stored_to_board_names() {
        // `with_display_status` is now an identity function; display
        // transformation happens at each display site via `display_label()`.
        let task = Task::builder()
            .id("task_1")
            .product_id("prod_1")
            .name("n")
            .description("d")
            .kind(TaskKind::Task)
            .status(TaskStatus::InReview)
            .created_at("t")
            .updated_at("t")
            .build();
        let shown = with_display_status(task);
        assert_eq!(shown.status.display_label(), "review");
        assert_eq!(shown.status, TaskStatus::InReview);
    }

    #[test]
    fn task_status_accepts_legacy_aliases_on_input() {
        // Board name resolves to its variant...
        let cli = Cli::parse_from(["boss", "task", "list", "--status", "backlog"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::List(args),
            } => assert!(matches!(args.status.as_slice(), [TaskStatusArg::Backlog])),
            _ => panic!("expected task list command"),
        }
        // ...and so does the legacy stored name as an alias.
        let cli = Cli::parse_from(["boss", "task", "list", "--status", "in-review"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::List(args),
            } => assert!(matches!(args.status.as_slice(), [TaskStatusArg::Review])),
            _ => panic!("expected task list command"),
        }
    }

    #[test]
    fn move_target_accepts_board_name_primary() {
        let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "backlog"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Move(args),
            } => assert!(matches!(args.target, MoveTarget::Backlog)),
            _ => panic!("expected task move command"),
        }
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
        let cli = Cli::parse_from(["boss", "task", "move", "task_18ad79226b0ca630_1a", "--to", "blocked"]);
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

    /// `boss chore move` is now a thin alias for the same handler. The
    /// legacy `active` value still parses (as an alias of the board name
    /// `doing`), exercising backward compatibility.
    #[test]
    fn parses_chore_move_command() {
        let cli = Cli::parse_from(["boss", "chore", "move", "task_xyz", "--to", "active"]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_xyz");
                assert!(matches!(args.target, MoveTarget::Doing));
            }
            _ => panic!("expected chore move command"),
        }
    }

    /// `--no-autostart` and `--no-engine-autostart` are distinct global
    /// flags (issue #787). `--no-autostart` governs only worker
    /// auto-dispatch; `--no-engine-autostart` governs transparent
    /// engine startup. Pin that they parse into independent fields.
    #[test]
    fn no_autostart_and_no_engine_autostart_are_independent_flags() {
        let cli = Cli::parse_from(["boss", "--no-autostart", "engine", "status"]);
        assert!(cli.global.no_autostart);
        assert!(!cli.global.no_engine_autostart);

        let cli = Cli::parse_from(["boss", "--no-engine-autostart", "engine", "status"]);
        assert!(!cli.global.no_autostart);
        assert!(cli.global.no_engine_autostart);
    }

    /// Regression for #787: `--no-autostart` must NOT suppress
    /// transparent engine startup — the engine is the system of record
    /// and must stay reachable to service the request. Only
    /// `--no-engine-autostart` flips `discovery.autostart` off.
    #[test]
    fn no_autostart_leaves_engine_autostart_enabled() {
        // `--no-autostart` alone: worker dispatch suppressed, engine
        // autostart still enabled.
        let cli = Cli::parse_from(["boss", "--no-autostart", "engine", "status"]);
        let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
        assert!(ctx.no_autostart, "no_autostart should propagate");
        assert!(
            ctx.discovery.autostart,
            "--no-autostart must not disable transparent engine startup"
        );

        // `--no-engine-autostart` alone: engine autostart suppressed,
        // worker dispatch untouched.
        let cli = Cli::parse_from(["boss", "--no-engine-autostart", "engine", "status"]);
        let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
        assert!(!ctx.no_autostart, "no_autostart should default to false");
        assert!(
            !ctx.discovery.autostart,
            "--no-engine-autostart must disable transparent engine startup"
        );

        // Neither flag: both default on/dispatching.
        let cli = Cli::parse_from(["boss", "engine", "status"]);
        let ctx = RunContext::from_flags(&cli.global).expect("from_flags");
        assert!(!ctx.no_autostart);
        assert!(ctx.discovery.autostart);
    }

    fn dummy_task(id: &str, kind: TaskKind) -> Task {
        Task::builder()
            .id(id)
            .product_id("prod_1")
            .kind(kind)
            .name("n")
            .description("")
            .status(TaskStatus::Todo)
            .created_at("")
            .updated_at("")
            .build()
    }

    #[test]
    fn expect_leaf_accepts_task_and_chore() {
        let task = dummy_task("task_1", TaskKind::Task);
        let (unwrapped, label) = expect_leaf_work_item(WorkItem::Task(task.clone())).unwrap();
        assert_eq!(unwrapped.id, "task_1");
        assert_eq!(label, "task");

        let chore = dummy_task("task_2", TaskKind::Chore);
        let (unwrapped, label) = expect_leaf_work_item(WorkItem::Chore(chore)).unwrap();
        assert_eq!(unwrapped.id, "task_2");
        assert_eq!(label, "chore");
    }

    #[test]
    fn expect_leaf_rejects_product_and_project() {
        let product = Product::builder()
            .id("prod_1")
            .name("n")
            .slug("n")
            .description("")
            .status("active")
            .created_at("")
            .updated_at("")
            .build();
        assert!(expect_leaf_work_item(WorkItem::Product(product)).is_err());

        let project = Project::builder()
            .id("proj_1")
            .product_id("prod_1")
            .name("n")
            .slug("n")
            .description("")
            .goal("")
            .status(ProjectStatus::Planned)
            .created_at("")
            .updated_at("")
            .build();
        assert!(expect_leaf_work_item(WorkItem::Project(project)).is_err());
    }

    /// Helper for the `format_repo_line` golden tests: build a Product
    /// with `repo_remote_url` set or unset and a fixed slug so the
    /// inherited-line text is predictable.
    fn dummy_product(slug: &str, repo: Option<&str>) -> Product {
        Product::builder()
            .id("prod_1")
            .name(slug)
            .slug(slug)
            .description("")
            .maybe_repo_remote_url(repo)
            .status("active")
            .created_at("")
            .updated_at("")
            .build()
    }

    /// Golden output: a work item with its own non-empty
    /// `repo_remote_url` reports "(override on this work item)" — the
    /// product's value is ignored in this branch even if it's also set.
    #[test]
    fn format_repo_line_override_on_work_item() {
        let product = dummy_product("boss", Some("git@github.com:spinyfin/mono.git"));
        let rendered = format_repo_line(Some("git@github.com:myorg/nimbus.git"), &product);
        assert_eq!(rendered, "git@github.com:myorg/nimbus.git (override on this work item)",);
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
    fn parses_task_restore_command() {
        let cli = Cli::parse_from(["boss", "task", "restore", "T43"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Restore(args),
            } => assert_eq!(args.id, "T43"),
            _ => panic!("expected task restore command"),
        }
    }

    #[test]
    fn parses_task_undelete_alias() {
        // `undelete` is an alias for `restore`.
        let cli = Cli::parse_from(["boss", "task", "undelete", "task_abc"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Restore(args),
            } => assert_eq!(args.id, "task_abc"),
            _ => panic!("expected task restore command via undelete alias"),
        }
    }

    #[test]
    fn parses_chore_restore_command() {
        let cli = Cli::parse_from(["boss", "chore", "restore", "T9"]);
        match cli.command {
            Commands::Chore {
                command: ChoreCommand::Restore(args),
            } => assert_eq!(args.id, "T9"),
            _ => panic!("expected chore restore command"),
        }
    }

    #[test]
    fn parses_task_list_deleted_flag() {
        // Both `--deleted` and its `--include-deleted` alias flip the flag.
        for flag in ["--deleted", "--include-deleted"] {
            let cli = Cli::parse_from(["boss", "task", "list", "--product", "boss", flag]);
            match cli.command {
                Commands::Task {
                    command: TaskCommand::List(args),
                } => assert!(args.include_deleted, "expected include_deleted for {flag}"),
                _ => panic!("expected task list command"),
            }
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
                assert!(matches!(args.target, ProjectStatusArg::Done));
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
        assert_eq!(ProjectStatusArg::Archived.as_str(), "archived");
        assert_eq!(ProjectStatusArg::Done.as_str(), "done");
        assert_eq!(ProjectStatusArg::Planned.as_str(), "planned");
    }

    #[test]
    fn numeric_selection_is_one_based() {
        let values = vec!["alpha".to_owned(), "beta".to_owned()];
        assert_eq!(pick_by_index(&values, "2").unwrap(), Some("beta".to_owned()));
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
            assert!(validate_github_pr_url(bad).is_err(), "expected `{bad}` to be rejected");
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
        let cli = Cli::parse_from(["boss", "task", "bind-pr", "task_1", "https://github.com/a/b/pull/9"]);
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
        let cli = Cli::parse_from(["boss", "chore", "bind-pr", "task_2", "https://github.com/a/b/pull/10"]);
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
        let cli = Cli::parse_from(["boss", "chore", "create-many", "--from-file", "-", "--product", "boss"]);
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
                assert_eq!(args.path.as_deref(), Some("tools/boss/docs/designs/foo.md"),);
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
                assert_eq!(args.repo.as_deref(), Some("https://github.com/myorg/wiki.git"),);
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
        assert!(
            rendered.contains("--unset") || rendered.contains("--path"),
            "{rendered}"
        );
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

    fn resolved_state(kind: ResolvedDesignDocKind, local: bool) -> ProjectDesignDocState {
        ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
                branch: "main".to_owned(),
                path: "tools/boss/docs/designs/foo.md".to_owned(),
                kind,
            },
            workspace_path: local.then(|| "/tmp/mono-agent-007".to_owned()),
            web_url: "https://github.com/spinyfin/mono/blob/main/tools/boss/docs/designs/foo.md".to_owned(),
            raw_content_url: Some(
                "https://raw.githubusercontent.com/spinyfin/mono/main/tools/boss/docs/designs/foo.md".to_owned(),
            ),
        }
    }

    /// Same-product pointer with a leased workspace picks the
    /// filesystem fast path (renderer / `$EDITOR`), not the web URL.
    #[test]
    fn open_design_same_product_with_workspace_uses_local_file() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
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
            ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
            false,
        );
        let action = decide_open_design_action(&state, false).unwrap();
        assert!(matches!(action, OpenDesignAction::Web { .. }));
    }

    /// `--web` forces the web URL regardless of workspace state.
    #[test]
    fn open_design_force_web_overrides_local_path() {
        let state = resolved_state(
            ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
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
        let err = decide_open_design_action(&ProjectDesignDocState::NotSet, false).expect_err("not-set must error");
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
            ResolvedDesignDocKind::SameProduct {
                product_id: "prod_1".into(),
            },
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

    fn lint_product() -> Product {
        Product::builder()
            .id("prod_1")
            .name("Boss")
            .slug("boss")
            .description("")
            .repo_remote_url("git@github.com:spinyfin/mono.git")
            .status("active")
            .created_at("")
            .updated_at("")
            .build()
    }

    fn lint_project(slug: &str, path: Option<&str>) -> Project {
        Project {
            id: format!("proj_{slug}"),
            product_id: "prod_1".to_owned(),
            name: slug.to_owned(),
            slug: slug.to_owned(),
            description: String::new(),
            goal: String::new(),
            status: ProjectStatus::Planned,
            priority: "medium".to_owned(),
            created_at: String::new(),
            updated_at: String::new(),
            last_status_actor: "human".to_owned(),
            design_doc_repo_remote_url: None,
            design_doc_branch: None,
            design_doc_path: path.map(str::to_owned),
            short_id: None,
        }
    }

    /// A resolved pointer with a leased workspace whose file exists
    /// on disk is healthy — the lint produces no entry.
    #[test]
    fn lint_skips_resolved_pointer_with_existing_file() {
        let product = lint_product();
        let project = lint_project("alpha", Some("tools/boss/docs/designs/alpha.md"));
        let state = ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
                branch: "main".to_owned(),
                path: "tools/boss/docs/designs/alpha.md".to_owned(),
                kind: ResolvedDesignDocKind::SameProduct {
                    product_id: "prod_1".into(),
                },
            },
            workspace_path: Some("/tmp/mono-agent-007".to_owned()),
            web_url: "https://example.test/blob/main/x.md".to_owned(),
            raw_content_url: None,
        };
        let entry = classify_lint_finding(&product, &project, Some(&state), |_, _| true, false, false);
        assert!(entry.is_none(), "healthy pointer must not appear in lint");
    }

    /// A resolved pointer whose file is missing in the leased
    /// workspace is the canonical stale-on-rename case. Always
    /// flagged as `Broken`, regardless of opt-in flags.
    #[test]
    fn lint_flags_resolved_pointer_with_missing_file_as_broken() {
        let product = lint_product();
        let project = lint_project("alpha", Some("tools/boss/docs/designs/alpha-renamed.md"));
        let state = ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "git@github.com:spinyfin/mono.git".to_owned(),
                branch: "main".to_owned(),
                path: "tools/boss/docs/designs/alpha-renamed.md".to_owned(),
                kind: ResolvedDesignDocKind::SameProduct {
                    product_id: "prod_1".into(),
                },
            },
            workspace_path: Some("/tmp/mono-agent-007".to_owned()),
            web_url: "https://example.test/blob/main/x.md".to_owned(),
            raw_content_url: None,
        };
        let entry = classify_lint_finding(
            &product,
            &project,
            Some(&state),
            |_, _| false,
            /*include_missing*/ false,
            /*include_unverified*/ false,
        )
        .expect("missing file must surface as a lint entry");
        assert_eq!(entry.severity, LintSeverity::Broken);
        assert!(entry.reason.contains("file not found"), "reason: {}", entry.reason);
        assert!(
            entry.suggested_fix.contains("boss project set-design-doc boss/alpha"),
            "fix template should pre-fill product/project selector: {}",
            entry.suggested_fix,
        );
    }

    /// The resolver's own `Broken` state — typically "path set but no
    /// repo to resolve against" — is always reported, no flags
    /// required.
    #[test]
    fn lint_flags_resolver_broken_state() {
        let product = lint_product();
        let project = lint_project("alpha", Some("designs/alpha.md"));
        let state = ProjectDesignDocState::Broken {
            reason: "no repo to resolve against".to_owned(),
        };
        let entry = classify_lint_finding(&product, &project, Some(&state), |_, _| true, false, false)
            .expect("broken resolver state must surface");
        assert_eq!(entry.severity, LintSeverity::Broken);
        assert!(entry.reason.contains("no repo"));
    }

    /// A resolved pointer with no leased workspace can't be probed.
    /// Default behaviour: silently skip (we can't confirm it's
    /// broken). With `--include-unverified`: surface as `Unverified`.
    #[test]
    fn lint_skips_unverified_pointer_by_default() {
        let product = lint_product();
        let project = lint_project("alpha", Some("designs/alpha.md"));
        let state = ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "https://github.com/myorg/wiki.git".to_owned(),
                branch: "main".to_owned(),
                path: "designs/alpha.md".to_owned(),
                kind: ResolvedDesignDocKind::External,
            },
            workspace_path: None,
            web_url: "https://example.test/blob/main/x.md".to_owned(),
            raw_content_url: None,
        };
        // The file_check callback must NOT be invoked when there's
        // no workspace — assert that by panicking from it.
        let entry = classify_lint_finding(
            &product,
            &project,
            Some(&state),
            |_, _| panic!("file_check must not run when no workspace is leased"),
            /*include_missing*/ false,
            /*include_unverified*/ false,
        );
        assert!(entry.is_none(), "unverified pointers are skipped by default");
    }

    #[test]
    fn lint_includes_unverified_when_flag_set() {
        let product = lint_product();
        let project = lint_project("alpha", Some("designs/alpha.md"));
        let state = ProjectDesignDocState::Resolved {
            resolved: ResolvedDesignDoc {
                repo_remote_url: "https://github.com/myorg/wiki.git".to_owned(),
                branch: "main".to_owned(),
                path: "designs/alpha.md".to_owned(),
                kind: ResolvedDesignDocKind::External,
            },
            workspace_path: None,
            web_url: "https://example.test/blob/main/x.md".to_owned(),
            raw_content_url: None,
        };
        let entry = classify_lint_finding(
            &product,
            &project,
            Some(&state),
            |_, _| true,
            false,
            /*include_unverified*/ true,
        )
        .expect("--include-unverified must surface unverified pointers");
        assert_eq!(entry.severity, LintSeverity::Unverified);
        assert!(entry.reason.contains("no leased workspace"));
    }

    /// Projects with no pointer set are silently skipped unless
    /// `--include-missing` is on; then they surface as `Missing`
    /// (advisory, not counted as broken for the exit code).
    #[test]
    fn lint_skips_missing_pointer_by_default() {
        let product = lint_product();
        let project = lint_project("alpha", None);
        let entry = classify_lint_finding(&product, &project, None, |_, _| true, false, false);
        assert!(entry.is_none(), "missing pointers are skipped by default");
    }

    #[test]
    fn lint_includes_missing_when_flag_set() {
        let product = lint_product();
        let project = lint_project("alpha", None);
        let entry = classify_lint_finding(
            &product,
            &project,
            None,
            |_, _| true,
            /*include_missing*/ true,
            false,
        )
        .expect("--include-missing must surface unset pointers");
        assert_eq!(entry.severity, LintSeverity::Missing);
        assert!(entry.design_doc_path.is_none());
        assert!(entry.suggested_fix.contains("set-design-doc boss/alpha"));
    }

    /// The footer tally lists each present severity with its count and
    /// omits severities with no findings.
    #[test]
    fn lint_summary_line_breaks_down_present_severities() {
        let product = lint_product();
        let broken = ProjectDesignDocState::Broken {
            reason: "no repo".to_owned(),
        };
        let entries = vec![
            classify_lint_finding(
                &product,
                &lint_project("a", Some("a.md")),
                Some(&broken),
                |_, _| true,
                false,
                false,
            )
            .unwrap(),
            classify_lint_finding(
                &product,
                &lint_project("b", Some("b.md")),
                Some(&broken),
                |_, _| true,
                false,
                false,
            )
            .unwrap(),
            classify_lint_finding(&product, &lint_project("c", None), None, |_, _| true, true, false).unwrap(),
        ];
        assert_eq!(lint_summary_line(&entries), "3 finding(s): 2 broken, 1 missing");
    }

    #[test]
    fn parses_project_lint_design_docs_defaults() {
        let cli = Cli::parse_from(["boss", "project", "lint-design-docs"]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::LintDesignDocs(args),
            } => {
                assert!(args.product.is_none());
                assert!(!args.include_missing);
                assert!(!args.include_unverified);
            }
            _ => panic!("expected project lint-design-docs command"),
        }
    }

    #[test]
    fn parses_project_lint_design_docs_with_flags() {
        let cli = Cli::parse_from([
            "boss",
            "project",
            "lint-design-docs",
            "--product",
            "boss",
            "--include-missing",
            "--include-unverified",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::LintDesignDocs(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert!(args.include_missing);
                assert!(args.include_unverified);
            }
            _ => panic!("expected project lint-design-docs command"),
        }
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
                assert_eq!(args.repo_remote_url.as_deref(), Some("git@github.com:myorg/nimbus.git"));
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
        let cli = Cli::parse_from(["boss", "task", "update", "task_1", "--repo", ""]);
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
        let cli = Cli::parse_from(["boss", "task", "update", "task_1", "--unset-effort", "--unset-model"]);
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
        let cli = Cli::parse_from(["boss", "product", "set-default-model", "boss", "--model", "sonnet"]);
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
        let cli = Cli::parse_from(["boss", "product", "set-default-model", "boss", "--unset"]);
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
        assert!(result.is_err(), "expected clap to reject --model and --unset together",);
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
                assert_eq!(args.repo_remote_url.as_deref(), Some("git@github.com:myorg/nimbus.git"));
            }
            _ => panic!("expected chore update command"),
        }
    }

    #[test]
    fn parses_task_list_with_repo_filter() {
        let cli = Cli::parse_from(["boss", "task", "list", "--product", "work", "--repo", "nimbus"]);
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
    fn repo_url_from_pr_url_strips_pull_segment() {
        assert_eq!(
            super::repo_url_from_pr_url("https://github.com/spinyfin/mono/pull/959"),
            "https://github.com/spinyfin/mono",
        );
        // Query/fragment after the number stay attached to the dropped
        // tail, so the base is still clean.
        assert_eq!(
            super::repo_url_from_pr_url("https://github.com/foo/bar/pull/12?x=1#c"),
            "https://github.com/foo/bar",
        );
        // No /pull/ segment → returned unchanged.
        assert_eq!(
            super::repo_url_from_pr_url("https://github.com/foo/bar"),
            "https://github.com/foo/bar",
        );
    }

    /// The `--repo` short-name selector matches against the repo parsed
    /// out of a PR URL, so `by-pr 959 --repo mono` resolves a
    /// `…/spinyfin/mono/pull/959` owner.
    #[test]
    fn repo_selector_matches_repo_parsed_from_pr_url() {
        let sel = RepoSelector::parse("mono").unwrap();
        let base = super::repo_url_from_pr_url("https://github.com/spinyfin/mono/pull/959");
        assert!(sel.matches(Some(base)));
        let other = super::repo_url_from_pr_url("https://github.com/spinyfin/other/pull/959");
        assert!(!sel.matches(Some(other)));
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
        let task = dummy_task("task_1", TaskKind::Task);
        assert!(task.repo_remote_url.is_none());
        let resolved = super::resolved_repo_for_task(&task, Some("git@github.com:myorg/nimbus.git"));
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
        assert!(matches!(
            parse_work_item_selector("T441"),
            WorkItemSelector::ShortId(441)
        ));
        // lowercase t
        assert!(matches!(
            parse_work_item_selector("t441"),
            WorkItemSelector::ShortId(441)
        ));
        // leading whitespace is trimmed
        assert!(matches!(
            parse_work_item_selector("  T12  "),
            WorkItemSelector::ShortId(12)
        ));
        // P-form (projects)
        assert!(matches!(parse_work_item_selector("P7"), WorkItemSelector::ShortId(7)));
        assert!(matches!(
            parse_work_item_selector("p100"),
            WorkItemSelector::ShortId(100)
        ));
        // zero is rejected (short_ids are positive)
        assert!(matches!(parse_work_item_selector("T0"), WorkItemSelector::Other(_)));
        // non-digit suffix is NOT a short id — falls through to Other
        assert!(matches!(parse_work_item_selector("Tabc"), WorkItemSelector::Other(_)));
        // plain primary id is still PrimaryId, not confused with T-form
        assert!(matches!(
            parse_work_item_selector("task_18ae0000_1"),
            WorkItemSelector::PrimaryId(_)
        ));
    }

    /// `boss project show proj_…` accepts a globally-unique typed id
    /// without `--product`. The parser shape pin is the user-facing
    /// half of the inference fix; the engine half is exercised by
    /// the in-process integration test in `tests/infer_product.rs`.
    #[test]
    fn parses_project_show_with_typed_id_and_no_product() {
        let cli = Cli::parse_from(["boss", "project", "show", "proj_18aeacce8acf9140_27"]);
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
        let cli = Cli::parse_from(["boss", "task", "list", "--project", "proj_18aeacce8acf9140_27"]);
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
        Product::builder()
            .id(id)
            .name(slug)
            .slug(slug)
            .description("")
            .status("active")
            .created_at("")
            .updated_at("")
            .build()
    }

    #[test]
    fn explicit_product_validator_accepts_omitted_explicit() {
        let products = vec![product_with_id("prod_1", "boss")];
        assert!(ensure_explicit_product_matches(&products, None, "prod_1", "proj_x").is_ok());
    }

    #[test]
    fn explicit_product_validator_accepts_matching_id_or_slug() {
        let products = vec![product_with_id("prod_1", "boss")];
        assert!(ensure_explicit_product_matches(&products, Some("prod_1"), "prod_1", "proj_x").is_ok());
        assert!(ensure_explicit_product_matches(&products, Some("boss"), "prod_1", "proj_x").is_ok());
    }

    /// When the user passes `--product` AND a typed id whose product
    /// disagrees, we surface a usage error naming both sides instead
    /// of silently picking one. Same shape as the engine-side
    /// "product/project disagree" check.
    #[test]
    fn explicit_product_validator_rejects_mismatch() {
        let products = vec![product_with_id("prod_1", "boss"), product_with_id("prod_2", "mono")];
        let err = ensure_explicit_product_matches(&products, Some("mono"), "prod_1", "proj_x")
            .expect_err("disagreement must error");
        let msg = format!("{err:?}");
        assert!(msg.contains("mono"), "{msg}");
        assert!(msg.contains("prod_1"), "{msg}");
    }

    #[test]
    fn shake_report_takes_first_line_as_title() {
        let (title, body) = split_shake_report("Engine wedges on close\n\nrepro: …").unwrap();
        assert_eq!(title, "Engine wedges on close");
        assert_eq!(body, "repro: …");
    }

    #[test]
    fn shake_report_strips_h1_marker_from_title() {
        let (title, body) = split_shake_report("# Engine wedges on close\n\nrepro: …\nstep two\n").unwrap();
        assert_eq!(title, "Engine wedges on close");
        assert_eq!(body, "repro: …\nstep two");
    }

    #[test]
    fn shake_report_skips_leading_blank_lines() {
        let (title, body) = split_shake_report("\n\n  \nFirst line is title\nbody here").unwrap();
        assert_eq!(title, "First line is title");
        assert_eq!(body, "body here");
    }

    #[test]
    fn shake_report_single_line_has_empty_body() {
        let (title, body) = split_shake_report("Only the title").unwrap();
        assert_eq!(title, "Only the title");
        assert_eq!(body, "");
    }

    #[test]
    fn shake_report_rejects_blank_blob() {
        assert!(split_shake_report("").is_none());
        assert!(split_shake_report("\n\n  \n").is_none());
    }

    // --- boss automation CLI tests ---

    #[test]
    fn parses_automation_create_command() {
        let cli = Cli::parse_from([
            "boss",
            "automation",
            "create",
            "--product",
            "boss",
            "--name",
            "Fix clippy",
            "--instruction",
            "Look for clippy warnings",
            "--schedule",
            "weekday-2pm",
            "--timezone",
            "America/Los_Angeles",
        ]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Create(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.name.as_deref(), Some("Fix clippy"));
                assert_eq!(args.instruction.as_deref(), Some("Look for clippy warnings"));
                assert_eq!(args.schedule.as_deref(), Some("weekday-2pm"));
                assert_eq!(args.timezone, "America/Los_Angeles");
                assert!(!args.disabled);
                assert_eq!(args.open_task_limit, 1);
            }
            _ => panic!("expected automation create command"),
        }
    }

    #[test]
    fn parses_automation_create_with_raw_cron_and_disabled() {
        let cli = Cli::parse_from([
            "boss",
            "automation",
            "create",
            "--product",
            "boss",
            "--name",
            "Weekly sweep",
            "--instruction",
            "Sweep old branches",
            "--schedule",
            "0 9 * * 1",
            "--disabled",
            "--open-task-limit",
            "3",
        ]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Create(args),
            } => {
                assert_eq!(args.schedule.as_deref(), Some("0 9 * * 1"));
                assert!(args.disabled);
                assert_eq!(args.open_task_limit, 3);
            }
            _ => panic!("expected automation create command"),
        }
    }

    #[test]
    fn parses_automation_list_command() {
        let cli = Cli::parse_from(["boss", "automation", "list", "--product", "boss"]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::List(args),
            } => {
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected automation list command"),
        }
    }

    #[test]
    fn parses_automation_show_command() {
        let cli = Cli::parse_from(["boss", "automation", "show", "A1", "--product", "boss"]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Show(args),
            } => {
                assert_eq!(args.selector, "A1");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected automation show command"),
        }
    }

    #[test]
    fn parses_automation_show_with_canonical_id() {
        let cli = Cli::parse_from(["boss", "automation", "show", "auto_abc123"]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Show(args),
            } => {
                assert_eq!(args.selector, "auto_abc123");
                assert!(args.product.is_none());
            }
            _ => panic!("expected automation show command"),
        }
    }

    #[test]
    fn parses_automation_update_command() {
        let cli = Cli::parse_from([
            "boss",
            "automation",
            "update",
            "A2",
            "--product",
            "boss",
            "--name",
            "New name",
            "--schedule",
            "nightly",
            "--open-task-limit",
            "2",
        ]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Update(args),
            } => {
                assert_eq!(args.selector, "A2");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert_eq!(args.name.as_deref(), Some("New name"));
                assert_eq!(args.schedule.as_deref(), Some("nightly"));
                assert_eq!(args.open_task_limit, Some(2));
            }
            _ => panic!("expected automation update command"),
        }
    }

    #[test]
    fn parses_automation_enable_disable_commands() {
        let cli_enable = Cli::parse_from(["boss", "automation", "enable", "A1", "--product", "boss"]);
        let cli_disable = Cli::parse_from(["boss", "automation", "disable", "A1", "--product", "boss"]);
        assert!(matches!(
            cli_enable.command,
            Commands::Automation {
                command: AutomationCommand::Enable(_)
            }
        ));
        assert!(matches!(
            cli_disable.command,
            Commands::Automation {
                command: AutomationCommand::Disable(_)
            }
        ));
    }

    #[test]
    fn parses_automation_run_command_with_force() {
        let cli = Cli::parse_from(["boss", "automation", "run", "A3", "--product", "boss", "--force"]);
        match cli.command {
            Commands::Automation {
                command: AutomationCommand::Run(args),
            } => {
                assert_eq!(args.selector, "A3");
                assert!(args.force);
            }
            _ => panic!("expected automation run command"),
        }
    }

    #[test]
    fn parses_automation_runs_and_tasks_commands() {
        let cli_runs = Cli::parse_from(["boss", "automation", "runs", "A1", "--product", "boss"]);
        let cli_tasks = Cli::parse_from(["boss", "automation", "tasks", "A1", "--product", "boss"]);
        assert!(matches!(
            cli_runs.command,
            Commands::Automation {
                command: AutomationCommand::Runs(_)
            }
        ));
        assert!(matches!(
            cli_tasks.command,
            Commands::Automation {
                command: AutomationCommand::Tasks(_)
            }
        ));
    }

    // --- cron validation tests ---

    #[test]
    fn compile_schedule_resolves_presets() {
        assert_eq!(compile_schedule("weekday-2pm").unwrap(), "0 14 * * 1-5");
        assert_eq!(compile_schedule("nightly").unwrap(), "0 2 * * *");
        assert_eq!(compile_schedule("weekly-mon-am").unwrap(), "0 9 * * 1");
        assert_eq!(compile_schedule("hourly").unwrap(), "0 * * * *");
        // Case-insensitive
        assert_eq!(compile_schedule("NIGHTLY").unwrap(), "0 2 * * *");
    }

    #[test]
    fn compile_schedule_accepts_valid_raw_cron() {
        assert_eq!(compile_schedule("0 14 * * 1-5").unwrap(), "0 14 * * 1-5");
        assert_eq!(compile_schedule("*/15 * * * *").unwrap(), "*/15 * * * *");
        assert_eq!(compile_schedule("0 9 1,15 * *").unwrap(), "0 9 1,15 * *");
    }

    #[test]
    fn compile_schedule_rejects_wrong_field_count() {
        assert!(compile_schedule("0 14 * *").is_err()); // 4 fields
        assert!(compile_schedule("0 14 * * 1-5 2026").is_err()); // 6 fields
        assert!(compile_schedule("").is_err());
    }

    #[test]
    fn compile_schedule_rejects_invalid_chars() {
        assert!(compile_schedule("0 14 * * 1-5; echo hi").is_err());
        assert!(compile_schedule("0 14 * * 1$5").is_err());
    }

    // --- automation selector parsing tests ---

    #[test]
    fn parse_automation_selector_primary_id() {
        let sel = parse_automation_selector("auto_abc123").unwrap();
        assert!(matches!(sel, AutomationSelector::PrimaryId(id) if id == "auto_abc123"));
    }

    #[test]
    fn parse_automation_selector_short_id_uppercase() {
        let sel = parse_automation_selector("A1").unwrap();
        assert!(matches!(sel, AutomationSelector::ShortId(1)));
    }

    #[test]
    fn parse_automation_selector_short_id_lowercase() {
        let sel = parse_automation_selector("a42").unwrap();
        assert!(matches!(sel, AutomationSelector::ShortId(42)));
    }

    #[test]
    fn parse_automation_selector_rejects_unknown_form() {
        assert!(parse_automation_selector("randomstring").is_err());
        assert!(parse_automation_selector("T42").is_err()); // task id — wrong namespace
    }

    // --- attention group selector parsing tests ---

    #[test]
    fn parse_attention_group_selector_primary_id() {
        let sel = parse_attention_group_selector("atg_abc123").unwrap();
        assert!(matches!(sel, AttentionGroupSelector::PrimaryId(id) if id == "atg_abc123"));
    }

    #[test]
    fn parse_attention_group_selector_short_id_uppercase() {
        let sel = parse_attention_group_selector("A3").unwrap();
        assert!(matches!(sel, AttentionGroupSelector::ShortId(3)));
    }

    #[test]
    fn parse_attention_group_selector_short_id_lowercase() {
        let sel = parse_attention_group_selector("a12").unwrap();
        assert!(matches!(sel, AttentionGroupSelector::ShortId(12)));
    }

    #[test]
    fn parse_attention_group_selector_rejects_unknown_form() {
        assert!(parse_attention_group_selector("randomstring").is_err());
        assert!(parse_attention_group_selector("auto_abc").is_err()); // automation id — wrong namespace
        assert!(parse_attention_group_selector("T42").is_err()); // task id — wrong namespace
    }
}
