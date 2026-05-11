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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub product_id: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub goal: String,
    pub status: String,
    pub priority: String,
    pub created_at: String,
    pub updated_at: String,
    /// `'human'` (default) when the most recent status change came
    /// from a CLI / app caller; `'engine'` when the engine flipped
    /// the status itself (e.g. dependency auto-block / unblock). The
    /// dependencies auto-unblock path only flips a `blocked` row
    /// back to `todo` when this is `'engine'` — manual blocks stick.
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
    pub execution_id: String,
    pub kind: String,
    pub status: String,
    pub title: String,
    pub body_markdown: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
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
    pub execution_id: String,
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
    /// a hint about whether cube currently has a workspace leased for
    /// the resolved repo (so the open dispatcher can pick the
    /// filesystem fast path), and a pre-rendered GitHub web URL for
    /// the kanban tooltip / right-click "copy link."
    Resolved {
        resolved: ResolvedDesignDoc,
        /// True when at least one cube workspace is leased for
        /// `resolved.repo_remote_url`.
        local_workspace_available: bool,
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
            local_workspace_available: true,
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
                local_workspace_available: false,
                web_url: "https://github.com/foo/bar/blob/main/docs/x.md".into(),
            },
        };
        let raw = serde_json::to_value(&output).unwrap();
        let back: ResolveProjectDesignDocOutput = serde_json::from_value(raw).unwrap();
        assert_eq!(output, back);
    }
}
