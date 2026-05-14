use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::conflict_diagnosis::ConflictDiagnosis;
use crate::coordinator::slot_id_from_worker_id;
use crate::effort::{SpawnConfig, resolve_spawn_config};
use crate::pane_summary;
use crate::spawn_flow::{StartWorkerInput, start_worker};
use crate::work::{ConflictResolution, Project, Task, WorkDb, WorkExecution, WorkItem};
use boss_protocol::WorkItemBinding;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAttention {
    pub kind: String,
    pub title: String,
    pub body_markdown: String,
}

/// What a worker is waiting for after a run ends. Drives the lease
/// retain/release decision in the coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunWaitState {
    /// Run finished cleanly with no further work expected (`completed` or
    /// equivalent terminal status). Workspace is released.
    Terminal,
    /// Worker is blocked on an upstream dependency. Workspace is released
    /// and re-leased when the work becomes ready again.
    WaitingDependency,
    /// Worker is awaiting human input/redirect. Workspace is retained so
    /// the next run can continue in-place.
    WaitingHuman,
    /// Worker is awaiting human review of an open PR. Workspace retained.
    WaitingReview,
    /// Worker is awaiting merge of an approved PR. Workspace retained.
    WaitingMerge,
}

impl RunWaitState {
    pub fn execution_status(self) -> &'static str {
        match self {
            RunWaitState::Terminal => "completed",
            RunWaitState::WaitingDependency => "waiting_dependency",
            RunWaitState::WaitingHuman => "waiting_human",
            RunWaitState::WaitingReview => "waiting_review",
            RunWaitState::WaitingMerge => "waiting_merge",
        }
    }

    pub fn release_workspace(self) -> bool {
        matches!(
            self,
            RunWaitState::Terminal | RunWaitState::WaitingDependency
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub wait_state: RunWaitState,
    pub result_summary: Option<String>,
    pub attention: Option<RunAttention>,
    /// Pane slot the worker was actually allocated into, if this run
    /// hosts a libghostty pane. The coordinator stamps this onto the
    /// run record's `agent_id` (as `worker-{slot_id}`) so `bossctl
    /// agents list` shows one entry per active pane instead of
    /// collapsing every run into the worker-pool placeholder. `None`
    /// means the runner doesn't have a pane (e.g., a test fake);
    /// the coordinator leaves agent_id alone.
    pub slot_id: Option<u8>,
    /// Resolved per-execution effort + model knobs the runner used
    /// to construct the worker's `claude` invocation. The coordinator
    /// surfaces this on the `pane_spawned` dispatch event so
    /// `bossctl dispatch diagnose <exec-id>` shows what model and
    /// effort value the worker actually launched with — design §Q2:
    /// "surfaces the chosen model, effort value, and level on the
    /// dispatch instrumentation stream." `None` for fake runners that
    /// don't go through the spawn-config resolver.
    pub spawn_config: Option<SpawnConfig>,
}

#[async_trait]
pub trait ExecutionRunner: Send + Sync {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome>;
}

/// `ExecutionRunner` that drives the libghostty pane RPC: writes the
/// per-lease worker config files, asks the macOS app to host a
/// worker pane, and registers the returned shell pid against the
/// run id so events-socket hook deliveries can correlate.
///
/// Returns `WaitingHuman` immediately on a successful spawn — the
/// pane stays alive in the app and the workspace lease is retained
/// until a human or follow-up flow concludes the run. Real lifecycle
/// (the pane signaling "Stop" → run completes) lands once the
/// events-socket consumer drives state transitions.
pub struct PaneSpawnRunner {
    cfg: Arc<RuntimeConfig>,
    /// Backing store for the pane-titlebar summary cache. Looked up
    /// in `run_execution` to compute a 2–4 word label for the work
    /// item before asking the app to spawn the pane.
    work_db: Arc<WorkDb>,
    /// Set after construction via [`PaneSpawnRunner::set_server_state`].
    /// Stored as `Weak` to avoid the runner ↔ ServerState reference
    /// cycle. Resolved each call.
    server_state: std::sync::OnceLock<Weak<dyn crate::spawn_flow::WorkerSpawner>>,
}

impl PaneSpawnRunner {
    pub fn new(cfg: Arc<RuntimeConfig>, work_db: Arc<WorkDb>) -> Self {
        Self {
            cfg,
            work_db,
            server_state: std::sync::OnceLock::new(),
        }
    }

    pub fn set_server_state(&self, server_state: Weak<dyn crate::spawn_flow::WorkerSpawner>) {
        let _ = self.server_state.set(server_state);
    }

    fn events_socket_path(&self) -> PathBuf {
        if let Ok(override_path) = std::env::var("BOSS_EVENTS_SOCKET") {
            return override_path.into();
        }
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/Boss/events.sock")
    }

    fn boss_event_binary(&self) -> PathBuf {
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        resolve_boss_event_binary(
            &engine_path,
            workspace.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
        )
    }
}

/// Pure resolver for the absolute path of the `boss-event` shim
/// that the worker pane invokes from `settings.json`. Pulled out
/// as a free function so tests can pass synthetic `engine_path` /
/// `workspace_dir` / env values without monkey-patching globals.
///
/// Resolution order:
///   1. `BOSS_EVENT_BIN` env override (caller-controlled).
///   2. `$BOSS_BIN_DIR/boss-event` — installed-mode path. The app
///      sets `BOSS_BIN_DIR` to `Boss.app/Contents/Resources/bin/` and
///      passes it to the engine; all bundled CLIs and the shim live
///      there. This is checked ahead of the dev-mode paths so an
///      installed bundle never falls through to a workspace clone.
///   3. Bazel runfiles next to the engine binary
///      (`<engine_path>.runfiles/_main/tools/boss/event-shim/boss-event`).
///      Requires the engine `rust_binary` to declare a `data` dep
///      on `//tools/boss/event-shim:boss-event` — without it bazel
///      doesn't include the shim in the engine's runfiles.
///   4. Workspace `bazel-bin` symlink
///      (`<workspace>/bazel-bin/tools/boss/event-shim/boss-event`)
///      when `BUILD_WORKSPACE_DIRECTORY` is set (i.e., the engine
///      was launched via `bazel run` from a checkout).
///   5. Cargo / hand-built sibling: `<engine_dir>/boss-event`.
///   6. Bare name `boss-event` — only useful if the worker's PATH
///      happens to include it (today it doesn't, on purpose).
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
) -> PathBuf {
    if let Some(override_path) = env_override {
        return override_path.to_path_buf();
    }

    // Installed mode: BOSS_BIN_DIR is Boss.app/Contents/Resources/bin/.
    if let Some(bin_dir) = boss_bin_dir {
        let candidate = bin_dir.join("boss-event");
        if candidate.exists() {
            return candidate;
        }
    }

    // Bazel constructs runfiles at `<binary>.runfiles/_main/<workspace_relative_path>`.
    let mut runfiles_root = engine_path.as_os_str().to_owned();
    runfiles_root.push(".runfiles");
    let runfiles_candidate = PathBuf::from(runfiles_root)
        .join("_main")
        .join("tools/boss/event-shim/boss-event");
    if runfiles_candidate.exists() {
        return runfiles_candidate;
    }

    if let Some(workspace) = workspace_dir {
        let candidate = workspace.join("bazel-bin/tools/boss/event-shim/boss-event");
        if candidate.exists() {
            return candidate;
        }
    }

    if let Some(engine_dir) = engine_path.parent() {
        let sibling = engine_dir.join("boss-event");
        if sibling.exists() {
            return sibling;
        }
    }

    PathBuf::from("boss-event")
}

#[async_trait]
impl ExecutionRunner for PaneSpawnRunner {
    async fn run_execution(
        &self,
        worker_id: &str,
        execution: &WorkExecution,
        work_item: &WorkItem,
        workspace_path: &Path,
        cube_change_id: Option<&str>,
    ) -> Result<RunOutcome> {
        let weak = self
            .server_state
            .get()
            .ok_or_else(|| anyhow!("PaneSpawnRunner not bound to ServerState"))?;
        let spawner = weak
            .upgrade()
            .ok_or_else(|| anyhow!("ServerState dropped before run_execution"))?;

        let lease_id = execution
            .cube_lease_id
            .clone()
            .context("execution missing cube_lease_id; coordinator must lease before spawn")?;

        // The coordinator already claimed a slot via WorkerPool —
        // `worker_id` is `worker-{N}` and N is the slot the engine
        // owns. Decode it here and thread it into the spawn so the
        // app hosts the pane in this exact slot rather than running
        // its own (now-deleted) firstIndex(where:) heuristic.
        let slot_id = slot_id_from_worker_id(worker_id).ok_or_else(|| {
            anyhow!(
                "PaneSpawnRunner received worker_id {worker_id:?} that does not parse as worker-{{N}}"
            )
        })?;

        // Compose the worker prompt and stash it on disk so the
        // libghostty pane can `claude "$(cat .claude/initial-prompt.txt)"`
        // — Claude Code's positional arg is treated as the first user
        // message, which gets the worker working without us having to
        // wait for a "Claude is ready" signal and then SendToPane.
        // Going through a file (rather than embedding the prompt in
        // the typed command) avoids shell quoting hell on multi-line,
        // backtick-bearing markdown.
        // For any project-scoped task (the synthetic `kind = 'design'`
        // task and ordinary `project_task` rows alike), the richer
        // brief — what the project is for, what its goal is — lives
        // on the parent project rather than on the task row. Look it
        // up at spawn time so the worker prompt is always anchored on
        // the current project state, not whatever was copied at
        // create time.
        let parent_project = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task
                .project_id
                .as_deref()
                .and_then(|project_id| self.work_db.get_project(project_id).ok()),
            _ => None,
        };
        // For conflict-resolution executions, the worker's prompt
        // embeds the engine's pre-spawn diagnosis. The attempt row is
        // created at conflict-detection time (Phase 2 wiring) and
        // updated with the diagnosis JSON pre-spawn. Look it up by
        // work_item_id so the prompt composer renders the templated
        // markdown surface regardless of how the row got there.
        let conflict_attempt = if execution.kind == "conflict_resolution" {
            self.work_db
                .active_conflict_resolution_for_work_item(&execution.work_item_id)
                .ok()
                .flatten()
        } else {
            None
        };
        let prompt_text = compose_execution_prompt(
            execution,
            work_item,
            parent_project.as_ref(),
            workspace_path,
            cube_change_id,
            conflict_attempt.as_ref(),
        );

        // Resolve the per-execution effort + model knobs (design §Q3
        // precedence). Read both columns off the row, the parent
        // product's `default_model`, and let the resolver pick the
        // first non-empty value. The resolver also derives the
        // `--effort` value and the optional prompt addendum from the
        // row's `effort_level` (model_override never changes those —
        // design §Q3).
        let (row_effort, row_model_override, product_default_model, product_dispatch_preamble) =
            match work_item {
                WorkItem::Task(task) | WorkItem::Chore(task) => {
                    let product = self
                        .work_db
                        .get_product(&task.product_id)
                        .ok()
                        .flatten();
                    let product_default_model =
                        product.as_ref().and_then(|p| p.default_model.clone());
                    let dispatch_preamble =
                        product.and_then(|p| p.dispatch_preamble).filter(|s| !s.is_empty());
                    (
                        task.effort_level,
                        task.model_override.clone(),
                        product_default_model,
                        dispatch_preamble,
                    )
                }
                _ => (None, None, None, None),
            };
        let spawn_config = resolve_spawn_config(
            row_effort,
            row_model_override.as_deref(),
            product_default_model.as_deref(),
        );

        // Per-level prompt addendum lands at the very top of the file
        // (design §Q2: "concatenated to .claude/initial-prompt.txt
        // BEFORE the existing prompt body"). The existing task /
        // design / conflict-resolution framing must stay byte-identical
        // when the addendum is `None`.
        let prompt_text = match spawn_config.prompt_addendum {
            Some(addendum) => format!("{}\n\n{}", addendum, prompt_text),
            None => prompt_text,
        };

        // Product dispatch preamble is prepended before the effort
        // addendum, with visible bracket markers so humans reading
        // transcripts know what was injected by the engine.
        // Empty / null preamble → today's behaviour, no change.
        let prompt_text = match product_dispatch_preamble {
            Some(preamble) => {
                format!("[product-preamble]\n{}\n[/product-preamble]\n\n{}", preamble, prompt_text)
            }
            None => prompt_text,
        };

        let prompt_path = workspace_path.join(".claude").join("initial-prompt.txt");
        if let Some(parent) = prompt_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&prompt_path, &prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;
        let initial_input = spawn_config.claude_invocation();

        // Look up (or generate) a 2–4 word pane-titlebar summary for
        // this work item. The full run id is still used for logs and
        // every other identifier — this label is purely visual. We
        // resolve the API key lazily and let the helper handle every
        // failure mode (missing key, API error, cache miss) so a
        // slow or unreachable Anthropic never blocks the spawn.
        let api_key = self
            .cfg
            .agent()
            .ok()
            .and_then(|agent| agent.anthropic_api_key.clone());
        // For conflict-resolution executions the pane summary must reflect
        // resolution activity, not the original task's gerund. We skip the
        // cache and Claude call entirely — the phrase is fully determined by
        // the execution kind and a truncation of the parent task name.
        let title_summary = if execution.kind == "conflict_resolution" {
            pane_summary::conflict_resolution_summary(work_item_name(work_item))
        } else {
            pane_summary::get_or_generate(&self.work_db, api_key.as_deref(), work_item).await
        };

        let work_item_binding = Some(WorkItemBinding {
            work_item_id: work_item_id(work_item).to_owned(),
            work_item_name: work_item_name(work_item).to_owned(),
            execution_id: execution.id.clone(),
        });

        let started = start_worker(
            spawner.as_ref(),
            StartWorkerInput {
                run_id: execution.id.clone(),
                lease_id,
                slot_id,
                workspace_path: workspace_path.to_path_buf(),
                events_socket_path: self.events_socket_path(),
                boss_event_path: self.boss_event_binary(),
                initial_input,
                extra_env: vec![],
                title_summary,
                work_item_binding,
                model: spawn_config.model.clone(),
            },
            StdDuration::from_secs(30),
        )
        .await
        .with_context(|| format!("spawning worker pane for run {}", execution.id))?;

        tracing::info!(
            worker_id,
            execution_id = %execution.id,
            slot_id = started.slot_id,
            shell_pid = started.shell_pid,
            effort_level = spawn_config
                .effort_level
                .map(|level| level.as_str())
                .unwrap_or("none"),
            claude_effort = spawn_config.claude_effort.unwrap_or("default"),
            model = %spawn_config.model,
            "pane spawned for execution",
        );

        Ok(RunOutcome {
            wait_state: RunWaitState::WaitingHuman,
            result_summary: Some(format!(
                "Spawned worker pane in slot {} (shell pid {}). Hook events from this run will surface on the engine events socket.",
                started.slot_id, started.shell_pid,
            )),
            attention: None,
            slot_id: Some(started.slot_id),
            spawn_config: Some(spawn_config),
        })
    }
}

fn compose_execution_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    parent_project: Option<&Project>,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    conflict_attempt: Option<&ConflictResolution>,
) -> String {
    // The conflict_resolution kind has a wholly different shape than
    // implementation/design kinds — it carries an embedded diagnosis,
    // a tight playbook, and explicit stop conditions. Render it via
    // a dedicated composer instead of trying to splice into the
    // generic flow. Falls through to the generic prompt if (somehow)
    // the conflict-resolution attempt row is missing — better to ship
    // a generic worker prompt than to fail the spawn.
    if execution.kind == "conflict_resolution" {
        if let Some(attempt) = conflict_attempt {
            return compose_conflict_resolution_prompt(
                execution,
                work_item,
                workspace_path,
                cube_change_id,
                attempt,
                /* test_command */ None,
            );
        }
    }
    let mut prompt = String::new();
    prompt.push_str(
        "You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n",
    );
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");
    let expected_branch = crate::completion::expected_branch_name(&execution.id);
    prompt.push_str("Execution context:\n");
    prompt.push_str(&format!("- execution id: `{}`\n", execution.id));
    prompt.push_str(&format!("- execution kind: `{}`\n", execution.kind));
    prompt.push_str(&format!("- workspace: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("- work item: `{}`\n", work_item_name(work_item)));
    prompt.push_str(&format!(
        "- expected branch name: `{expected_branch}` — the engine reconstructs this from your execution id and uses it to find your PR. Push to this exact bookmark name.\n",
    ));
    if let Some(cube_change_id) = cube_change_id {
        prompt.push_str(&format!("- local change: `{}`\n", cube_change_id));
    }
    // For any project-scoped task — the synthetic `kind = 'design'`
    // task plus ordinary `project_task` rows — the interesting
    // context (what the project is for, its goal) lives on the
    // parent project rather than on the task row. Surface it inline
    // so the worker has the project's name/goal/description to
    // anchor against, regardless of the execution kind.
    if let Some(project) = parent_project {
        prompt.push_str(&format!("- parent project: `{}`\n", project.name));
        if let Some(details) = project_details(project) {
            prompt.push_str("- project details:\n");
            prompt.push_str(details.trim_end());
            prompt.push('\n');
        }
    }
    if let Some(details) = work_item_details(work_item) {
        prompt.push_str("- details:\n");
        prompt.push_str(details.trim_end());
        prompt.push('\n');
    }
    prompt.push('\n');
    match execution.kind.as_str() {
        "project_design" => {
            prompt.push_str(&compose_design_directive(parent_project));
        }
        "task_implementation" | "chore_implementation" => {
            prompt.push_str(
                "Expected outcome for this run:\n- implement the requested change in the workspace,\n- run relevant local validation when practical,\n- stop once the work is ready for a human to review or redirect.\n",
            );
        }
        _ => {
            prompt.push_str(
                "Expected outcome for this run:\n- make concrete progress on the assigned work,\n- leave the workspace in a reviewable state,\n- stop with a concise review summary.\n",
            );
        }
    }
    if matches!(
        execution.kind.as_str(),
        "task_implementation" | "chore_implementation" | "project_design"
    ) {
        // Acceptance criterion: the engine watches for a PR URL on the
        // run's branch when claude stops. If the worker stops without
        // pushing/opening one, the run is treated as incomplete and
        // the worker is automatically probed to produce a PR. Stating
        // this up front avoids the probe round-trip when the worker
        // would otherwise have stopped at "I made the changes" with
        // nothing pushed.
        //
        // AI #6 (incident 001): the branch name is engine-supplied —
        // `expected branch name` above. Workers MUST push to that
        // bookmark name, because the cold-path detector now reads
        // `gh pr list --head <expected-branch>` (a unique-by-construction
        // signal) instead of the structurally-unsafe shared-store jj
        // bookmark scan that produced the May 14 PR fan-out.
        prompt.push_str(&format!(
            "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
             - Use the engine-supplied branch name from the `expected branch name` line above (`{expected_branch}`) when creating your bookmark and pushing — do NOT invent a different name.\n\
             - Push your branch (`jj bookmark create {expected_branch} -r @ && jj git push -b {expected_branch} --allow-new`) and open a PR with `gh pr create --head {expected_branch} --base main` if one does not already exist.\n\
             - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
             - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
             - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
        ));
    }
    prompt.push_str("\nRespond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// Directive block for the synthetic `kind = 'design'` task that the
/// engine auto-creates with every project. Without this block the
/// `project_design` worker only sees the generic "draft or update a
/// repo-backed design artifact" line and frequently starts
/// implementing — observed against worker O'Brien
/// (exec_18aebf0caa1187e8_b). State up front that the deliverable is
/// a markdown design doc (not code), name the canonical path, and
/// list the section shape the reader expects so the worker doesn't
/// invent its own.
fn compose_design_directive(parent_project: Option<&Project>) -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **design document**, not an implementation. Do not edit code, do not start prototyping, do not open partial implementation PRs.\n");
    out.push_str("- the PR for this run contains **only the design doc** (one new or updated markdown file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    if let Some(path_line) = canonical_design_doc_path_line(parent_project) {
        out.push_str(&path_line);
    }
    out.push_str("- the design must cover, at minimum, these sections (use these as headings unless the parent project's description specifies a different shape):\n");
    out.push_str("  - **Goals** — what this project is trying to achieve, pulled from the parent project's goal/description above.\n");
    out.push_str("  - **Non-goals** — what is explicitly out of scope so reviewers don't have to guess.\n");
    out.push_str("  - **Alternatives considered** — at least two distinct approaches and why they were not chosen.\n");
    out.push_str("  - **Chosen approach** — the design itself, with enough detail that a follow-up implementation task can be filed against it.\n");
    out.push_str("  - **Risks / open questions** — anything the author wants a human reviewer to land on before implementation starts.\n");
    out.push_str("- when the doc is ready for review, push it and open a PR (see the acceptance criterion below). Do not start implementation tasks — those come from follow-up work items the human files after the design is approved.\n");
    out
}

/// If the parent project has an explicit `design_doc_path` pointer
/// (set via `boss project design-doc`), emit that as the canonical
/// path. Otherwise fall back to the `<repo>/docs/designs/<slug>.md`
/// convention, anchored on the project's slug so two design tasks
/// don't collide. Returns `None` only when we have no project at
/// all — in practice the dispatcher always has one for
/// `kind = 'design'` rows, but the runner stays defensive.
fn canonical_design_doc_path_line(parent_project: Option<&Project>) -> Option<String> {
    let project = parent_project?;
    if let Some(path) = project.design_doc_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        return Some(format!(
            "- the canonical path for this design doc is `{path}` (set on the project's `design_doc_path` pointer). Write the doc there.\n",
        ));
    }
    let slug = if project.slug.trim().is_empty() {
        "design"
    } else {
        project.slug.trim()
    };
    Some(format!(
        "- the project's `design_doc_path` pointer is not yet set. Place the doc at `docs/designs/{slug}.md` (the repo's convention; adjust to the product's docs layout if the repo already has one — e.g. `tools/boss/docs/designs/{slug}.md` for the Boss product). After you create the file, set the pointer with `boss project set-design-doc --project <id> --path <path>` so the next run resolves it directly.\n",
    ))
}

/// Templated prompt for the `conflict_resolution` execution kind
/// (design Q4 of `tools/boss/docs/designs/merge-conflict-handling-in-review.md`).
///
/// Embeds the engine's pre-spawn diagnosis (parsed back from the JSON
/// the engine stored on `conflict_resolutions.conflict_diagnosis`)
/// and the project's `test_command` if one is configured. The worker
/// is *not* asked to add scope — the prompt is explicit that the only
/// allowed change is resolving the rebase conflict and pushing the
/// resolved branch.
fn compose_conflict_resolution_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    attempt: &ConflictResolution,
    test_command: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "## Conflict resolution: PR #{pr_num} has merge conflicts against `{base}`\n\n",
        pr_num = attempt.pr_number,
        base = attempt.base_branch,
    ));
    prompt.push_str(&format!("**PR**: {}\n", attempt.pr_url));
    prompt.push_str(&format!(
        "**Branch**: `{}` based off `{}`\n",
        attempt.head_branch, attempt.base_branch,
    ));
    if let Some(base_sha) = attempt.base_sha_at_trigger.as_deref() {
        prompt.push_str(&format!(
            "**Base sha at conflict detection**: `{base_sha}` (current `{}` may be ahead)\n",
            attempt.base_branch,
        ));
    }
    prompt.push_str(&format!("**Workspace**: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("**Attempt id**: `{}`\n", attempt.id));
    prompt.push_str(&format!("**Execution id**: `{}`\n", execution.id));
    if let Some(change) = cube_change_id {
        prompt.push_str(&format!("**Local change**: `{change}`\n"));
    }
    prompt.push_str(&format!(
        "**Work item**: `{}`\n\n",
        work_item_name(work_item),
    ));
    prompt.push_str(
        "This PR was in code review when `main` moved under it. The PR's diff against\n\
         the current `main` does not apply cleanly. Your job is to resolve the conflicts,\n\
         push the resolved branch, and stop. **You are not adding new work to this PR.**\n\n",
    );
    prompt.push_str("### Steps\n\n");
    prompt.push_str("1. `jj git fetch`\n");
    prompt.push_str(&format!("2. `jj edit {}`\n", attempt.head_branch));
    prompt.push_str(&format!(
        "3. `jj rebase -d {} -b {}`\n",
        attempt.base_branch, attempt.head_branch,
    ));
    prompt.push_str(
        "4. If the rebase reports a conflict:\n\
            - Inspect with `jj st`, `jj resolve --list <file>`.\n\
            - Resolve each conflict. Read the conflict diagnosis below for what was\n  \
              touched on the upstream side.\n",
    );
    match test_command {
        Some(cmd) => prompt.push_str(&format!(
            "5. Run the project's tests with `{cmd}`. If green, push. If red and the\n   \
                failure is rebase-induced, fix it. If red and the failure was pre-existing,\n   \
                stop and surface it via the stop-condition path below.\n",
        )),
        None => prompt.push_str(
            "5. No `test_command` is configured for this product; skip the local test\n   \
                run and rely on CI to verify the push.\n",
        ),
    }
    prompt.push_str(&format!(
        "6. `jj git push --bookmark {}`\n",
        attempt.head_branch,
    ));
    prompt.push_str(&format!(
        "7. `gh pr comment {} --body \"<post-resolution comment — see template below>\"`\n",
        attempt.pr_number,
    ));
    prompt.push_str(
        "8. Stop. Do not change the PR base, do not change the PR title or description,\n   \
            do not push new commits beyond the resolved rebase.\n\n",
    );
    prompt.push_str("### Conflict diagnosis (from the engine's pre-spawn pass)\n\n");
    match attempt
        .conflict_diagnosis
        .as_deref()
        .map(serde_json::from_str::<ConflictDiagnosis>)
    {
        Some(Ok(diagnosis)) => prompt.push_str(&render_diagnosis_markdown(&diagnosis)),
        Some(Err(err)) => {
            prompt.push_str(&format!(
                "_Engine could not re-parse the diagnosis JSON (error: {err}). The\n\
                 raw blob is on `conflict_resolutions.conflict_diagnosis` if you need it._\n",
            ));
        }
        None => {
            prompt.push_str(
                "_No engine-collected diagnosis is available for this attempt. Use\n\
                 `jj st` and `jj resolve --list` after the rebase to discover the\n\
                 conflicts directly._\n",
            );
        }
    }
    prompt.push_str("\n### Stop conditions\n\n");
    prompt.push_str(
        "If any of the following applies, comment on the PR explaining the situation,\n\
         do NOT push, and run `boss engine conflicts mark-failed <attempt-id> --reason <r>`\n\
         with the appropriate reason — the engine will mark the attempt `failed`:\n\n\
            1. **Semantic obsolescence** — the upstream change accomplished what this PR\n   \
            was trying to do. Reason: `obsolescence_suspected`.\n\
            2. **Product decision required** — the conflict needs a human choice between\n   \
            two valid resolutions. Reason: `product_decision_required`.\n\
            3. **Architectural mismatch** — the upstream removed an abstraction this PR\n   \
            was extending. Reason: `architectural_mismatch`.\n\n\
         Do NOT close the PR yourself. Closing is the human's call.\n\n",
    );
    prompt.push_str("### Post-resolution PR comment template\n\n");
    prompt.push_str(
        "```\n\
         🤖 boss resolved merge conflicts on this PR after `main` moved.\n\n\
         Resolutions:\n\
         - <per-file resolution summary>\n\n\
         <If a test command was configured, paste its result here.>\n\
         Branch force-pushed; per branch protection, prior approvals have been dismissed.\n\
         Re-review when ready.\n\
         ```\n\n",
    );
    prompt.push_str("Respond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

fn render_diagnosis_markdown(diagnosis: &ConflictDiagnosis) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Schema v{}. Base sha `{}`, dependent head sha `{}`.\n\n",
        diagnosis.schema_version, diagnosis.base_sha, diagnosis.head_sha,
    ));
    if let Some(err) = diagnosis.error.as_deref() {
        out.push_str(&format!(
            "_Engine-side probe failed: {err}. The list below may be incomplete; trust\n\
             `jj st` after the rebase as the source of truth._\n\n",
        ));
    }
    if diagnosis.files.is_empty() {
        if diagnosis.error.is_none() {
            out.push_str(
                "_No conflicted files reported by the engine's pre-spawn probe. The\n\
                 conflict may have been transient; run `jj rebase` and trust `jj st`._\n",
            );
        }
        return out;
    }
    out.push_str(&format!("Conflicted files ({}):\n\n", diagnosis.files.len()));
    for file in &diagnosis.files {
        out.push_str(&format!("- `{}` — {}", file.path, file.shape));
        if let Some(count) = file.marker_count {
            out.push_str(&format!(" ({count} marker block(s))"));
        }
        out.push('\n');
    }
    out
}

fn work_item_name(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.name,
        WorkItem::Project(project) => &project.name,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.name,
    }
}

fn work_item_id(work_item: &WorkItem) -> &str {
    match work_item {
        WorkItem::Product(product) => &product.id,
        WorkItem::Project(project) => &project.id,
        WorkItem::Task(task) | WorkItem::Chore(task) => &task.id,
    }
}

fn work_item_details(work_item: &WorkItem) -> Option<String> {
    match work_item {
        WorkItem::Product(product) => {
            if product.description.trim().is_empty() {
                None
            } else {
                Some(format!("  - description: {}", product.description.trim()))
            }
        }
        WorkItem::Project(project) => project_details(project),
        WorkItem::Task(task) | WorkItem::Chore(task) => task_details(task),
    }
}

fn project_details(project: &Project) -> Option<String> {
    let mut lines = Vec::new();
    if !project.description.trim().is_empty() {
        lines.push(format!("  - description: {}", project.description.trim()));
    }
    if !project.goal.trim().is_empty() {
        lines.push(format!("  - goal: {}", project.goal.trim()));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn task_details(task: &Task) -> Option<String> {
    let mut lines = Vec::new();
    if !task.description.trim().is_empty() {
        lines.push(format!("  - description: {}", task.description.trim()));
    }
    if let Some(pr_url) = task.pr_url.as_deref() {
        if !pr_url.trim().is_empty() {
            lines.push(format!("  - pr_url: {}", pr_url.trim()));
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod conflict_resolution_prompt_tests {
    //! Pure tests for the conflict-resolution prompt composer. The
    //! function takes a `ConflictResolution` + execution context and
    //! emits the templated markdown the worker pane will see.

    use super::*;
    use crate::conflict_diagnosis::{ConflictDiagnosis, ConflictedFile};
    use crate::work::ConflictResolution;
    use boss_protocol::WorkExecution;

    fn sample_execution() -> WorkExecution {
        WorkExecution {
            id: "exec-cr-1".into(),
            work_item_id: "task_1".into(),
            kind: "conflict_resolution".into(),
            status: "running".into(),
            repo_remote_url: "git@example.invalid:foo/bar.git".into(),
            cube_repo_id: Some("foo".into()),
            cube_lease_id: Some("lease-1".into()),
            cube_workspace_id: Some("ws-1".into()),
            workspace_path: Some("/tmp/workspace".into()),
            priority: 0,
            preferred_workspace_id: None,
            created_at: "1700000000".into(),
            started_at: Some("1700000010".into()),
            finished_at: None,
            pre_start_failure_count: 0,
            dispatch_not_before: None,
        }
    }

    fn sample_work_item() -> WorkItem {
        WorkItem::Chore(crate::work::Task {
            id: "task_1".into(),
            product_id: "prod_1".into(),
            project_id: None,
            kind: "chore".into(),
            name: "Some in-review chore".into(),
            description: String::new(),
            status: "blocked".into(),
            ordinal: None,
            pr_url: Some("https://github.com/foo/bar/pull/42".into()),
            deleted_at: None,
            created_at: "1700000000".into(),
            updated_at: "1700000000".into(),
            autostart: false,
            last_status_actor: "engine".into(),
            priority: "medium".into(),
            created_via: "engine_auto".into(),
            repo_remote_url: None,
            blocked_reason: Some("merge_conflict".into()),
            blocked_attempt_id: Some("crz_x".into()),
            effort_level: None,
            model_override: None,
            ci_attempt_budget: None,
            ci_attempts_used: 0,
            short_id: None,
            blocked_signals: Vec::new(),
        })
    }

    fn attempt_with_diagnosis(diag_json: Option<String>) -> ConflictResolution {
        ConflictResolution {
            id: "crz_42".into(),
            product_id: "prod_1".into(),
            work_item_id: "task_1".into(),
            pr_url: "https://github.com/foo/bar/pull/42".into(),
            pr_number: 42,
            head_branch: "riker/feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("abc123".into()),
            head_sha_before: Some("def456".into()),
            head_sha_after: None,
            status: "running".into(),
            failure_reason: None,
            cube_lease_id: Some("lease-1".into()),
            cube_workspace_id: Some("ws-1".into()),
            worker_id: Some("worker-1".into()),
            conflict_diagnosis: diag_json,
            created_at: "1700000000".into(),
            started_at: Some("1700000010".into()),
            finished_at: None,
        }
    }

    #[test]
    fn prompt_embeds_pr_branch_and_attempt_id() {
        let diag = ConflictDiagnosis {
            schema_version: 1,
            base_sha: "abc123".into(),
            head_sha: "def456".into(),
            files: vec![ConflictedFile {
                path: "src/foo.rs".into(),
                marker_count: Some(2),
                shape: "content".into(),
            }],
            error: None,
        };
        let json = serde_json::to_string(&diag).unwrap();
        let attempt = attempt_with_diagnosis(Some(json));

        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            Some("chg_1"),
            &attempt,
            None,
        );

        assert!(
            prompt.contains("## Conflict resolution: PR #42"),
            "missing PR-number header:\n{prompt}",
        );
        assert!(
            prompt.contains("riker/feature"),
            "missing head branch:\n{prompt}",
        );
        assert!(
            prompt.contains("`crz_42`"),
            "missing attempt id:\n{prompt}",
        );
        assert!(
            prompt.contains("`exec-cr-1`"),
            "missing execution id:\n{prompt}",
        );
        assert!(
            prompt.contains("Base sha at conflict detection"),
            "missing base sha line:\n{prompt}",
        );
        // Steps refer to the head branch verbatim.
        assert!(
            prompt.contains("jj edit riker/feature"),
            "missing rebase step:\n{prompt}",
        );
        assert!(
            prompt.contains("jj git push --bookmark riker/feature"),
            "missing push step:\n{prompt}",
        );
        // Diagnosis surface renders the conflicted file.
        assert!(
            prompt.contains("`src/foo.rs`"),
            "missing diagnosis file:\n{prompt}",
        );
    }

    #[test]
    fn prompt_includes_test_command_when_configured() {
        let attempt = attempt_with_diagnosis(None);
        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            None,
            &attempt,
            Some("bazel test //..."),
        );
        assert!(
            prompt.contains("bazel test //..."),
            "configured test command should appear verbatim:\n{prompt}",
        );
    }

    #[test]
    fn prompt_omits_test_step_when_test_command_is_none() {
        let attempt = attempt_with_diagnosis(None);
        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            None,
            &attempt,
            None,
        );
        assert!(
            prompt.contains("No `test_command` is configured"),
            "prompt should explicitly note the omission:\n{prompt}",
        );
    }

    #[test]
    fn prompt_calls_out_mark_failed_for_stop_conditions() {
        let attempt = attempt_with_diagnosis(None);
        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            None,
            &attempt,
            None,
        );
        assert!(
            prompt.contains("boss engine conflicts mark-failed"),
            "prompt must point workers at the mark-failed CLI:\n{prompt}",
        );
        // All three canonical reasons appear by name.
        for reason in [
            "obsolescence_suspected",
            "product_decision_required",
            "architectural_mismatch",
        ] {
            assert!(
                prompt.contains(reason),
                "stop-condition reason {reason} missing:\n{prompt}",
            );
        }
    }

    #[test]
    fn prompt_handles_unparsable_diagnosis_gracefully() {
        let attempt = attempt_with_diagnosis(Some("{not valid json".into()));
        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            None,
            &attempt,
            None,
        );
        assert!(
            prompt.contains("could not re-parse the diagnosis JSON"),
            "missing fallback for bad diagnosis JSON:\n{prompt}",
        );
    }

    #[test]
    fn prompt_renders_diagnosis_error_surface() {
        let diag = ConflictDiagnosis::errored("abc", "def", "git not on PATH");
        let attempt = attempt_with_diagnosis(Some(serde_json::to_string(&diag).unwrap()));
        let prompt = compose_conflict_resolution_prompt(
            &sample_execution(),
            &sample_work_item(),
            std::path::Path::new("/tmp/workspace"),
            None,
            &attempt,
            None,
        );
        assert!(
            prompt.contains("Engine-side probe failed"),
            "prompt should surface the engine probe error:\n{prompt}",
        );
        assert!(
            prompt.contains("git not on PATH"),
            "prompt should include the probe error message:\n{prompt}",
        );
    }
}

#[cfg(test)]
mod pane_spawn_tests {
    //! End-to-end-ish tests for `PaneSpawnRunner`: drive `run_execution`
    //! against a stub `WorkerSpawner`, then assert on what was actually
    //! sent to the app and what files were written into the workspace.
    //! These tests would have caught the bugs surfaced manually:
    //!   - missing prompt injection (worker idle at bash prompt),
    //!   - boss-event resolved to bare relative path (hooks fail),
    //!   - sanitized PATH not threaded through to the app.
    //!
    //! Anything reachable via `WorkerSpawner` is fair game without
    //! standing up a full engine; the broadcast / coordinator side
    //! lives in `coordinator.rs` tests.
    use super::*;
    use crate::app::SendToAppError;
    use crate::protocol::{
        EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput,
        SpawnWorkerPaneResult,
    };
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::work::{
        CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, EffortLevel,
        Task, WorkExecution, WorkItem,
    };
    use crate::worker_registry::WorkerRegistry;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    /// Records the spawn request the runner sent so tests can assert
    /// on env, initial_input, etc.
    struct CapturingSpawner {
        registry: WorkerRegistry,
        live_states: LiveWorkerStateRegistry,
        last: StdMutex<Option<SpawnWorkerPaneInput>>,
    }

    impl CapturingSpawner {
        fn new() -> Self {
            Self {
                registry: WorkerRegistry::new(),
                live_states: LiveWorkerStateRegistry::new(),
                last: StdMutex::new(None),
            }
        }

        fn spawn_input(&self) -> SpawnWorkerPaneInput {
            self.last
                .lock()
                .unwrap()
                .clone()
                .expect("expected SpawnWorkerPane to be sent")
        }
    }

    #[async_trait]
    impl crate::spawn_flow::WorkerSpawner for CapturingSpawner {
        async fn send_to_app_request(
            &self,
            request: EngineToAppRequest,
            _timeout: tokio::time::Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            match request {
                EngineToAppRequest::SpawnWorkerPane(input) => {
                    // Echo the slot the engine claimed; the
                    // engine-owns-slots refactor makes the response
                    // slot a confirmation echo rather than an
                    // independent allocator pick.
                    let slot_id = input.slot_id;
                    *self.last.lock().unwrap() = Some(input);
                    Ok(EngineToAppResponse::SpawnWorkerPane {
                        result: Ok(SpawnWorkerPaneResult {
                            slot_id,
                            shell_pid: 0,
                        }),
                    })
                }
                other => panic!("unexpected request kind: {other:?}"),
            }
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
        }
    }

    fn sample_execution(workspace_path: &Path) -> WorkExecution {
        WorkExecution {
            id: "exec-test-1".into(),
            work_item_id: "task-1".into(),
            kind: "chore_implementation".into(),
            status: "running".into(),
            repo_remote_url: "git@example.com:foo.git".into(),
            cube_repo_id: Some("foo".into()),
            cube_lease_id: Some("lease-1".into()),
            cube_workspace_id: Some("foo-agent-001".into()),
            workspace_path: Some(workspace_path.display().to_string()),
            priority: 0,
            preferred_workspace_id: None,
            created_at: "2026-05-06T20:00:00Z".into(),
            started_at: Some("2026-05-06T20:00:00Z".into()),
            finished_at: None,
            pre_start_failure_count: 0,
            dispatch_not_before: None,
        }
    }

    fn sample_chore() -> WorkItem {
        WorkItem::Chore(Task {
            id: "task-1".into(),
            product_id: "prod-1".into(),
            project_id: None,
            kind: "chore".into(),
            name: "Improve top header (agent card) styling".into(),
            description: "The gray header at the top is too cramped.".into(),
            status: "todo".into(),
            ordinal: None,
            pr_url: None,
            deleted_at: None,
            created_at: "2026-05-06T20:00:00Z".into(),
            updated_at: "2026-05-06T20:00:00Z".into(),
            autostart: true,
            last_status_actor: "human".into(),
            priority: "medium".into(),
            created_via: "unknown".to_owned(),
            repo_remote_url: None,
            blocked_reason: None,
            blocked_attempt_id: None,
            effort_level: None,
            model_override: None,
            ci_attempt_budget: None,
            ci_attempts_used: 0,
            short_id: None,
            blocked_signals: Vec::new(),
        })
    }

    /// Build a runner already bound to a `CapturingSpawner` and drive a
    /// run_execution against `workspace`. Returns the spawner so tests
    /// can inspect the captured request.
    async fn run_once(workspace: &TempDir) -> Result<Arc<CapturingSpawner>> {
        // We need a Weak<dyn WorkerSpawner> the runner can upgrade.
        // Box-leak the Arc so it lives for the test's duration; the
        // tempdir guards the workspace lifetime.
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;

        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        runner
            .run_execution(
                "worker-1",
                &sample_execution(workspace.path()),
                &sample_chore(),
                workspace.path(),
                Some("change-1"),
            )
            .await?;

        Ok(spawner)
    }

    #[tokio::test]
    async fn writes_initial_prompt_to_workspace_dot_claude() {
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace).await.unwrap();

        let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
        assert!(
            prompt_path.exists(),
            "expected {} to exist",
            prompt_path.display()
        );
        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        // Spot-check: the prompt should mention the work item title and
        // execution id so the worker actually has its task in hand.
        assert!(prompt.contains("Improve top header"), "prompt missing work item name");
        assert!(prompt.contains("exec-test-1"), "prompt missing execution id");
        assert!(
            prompt.contains("## Summary"),
            "prompt missing required output section header"
        );
    }

    #[tokio::test]
    async fn implementation_prompt_states_pr_url_acceptance_criterion() {
        // Workers that stop without producing a PR are now blocked
        // from completing — they get probed to push and open one. The
        // dispatch prompt must telegraph that up front so the worker
        // doesn't waste a round-trip discovering it from the probe.
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace).await.unwrap();
        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            prompt.contains("the deliverable is a PR URL"),
            "implementation prompt must state the PR-URL acceptance criterion: {prompt}",
        );
        assert!(
            prompt.contains("on its own line"),
            "implementation prompt must tell the worker to print the URL on its own line: {prompt}",
        );
        assert!(
            prompt.contains("gh pr create") || prompt.contains("gh pr view"),
            "implementation prompt must mention gh pr commands: {prompt}",
        );
        assert!(
            prompt.contains("jj diff -r @"),
            "implementation prompt must tell the worker to verify the diff before pushing: {prompt}",
        );
    }

    /// AI #6 (incident 001): the prompt must name the engine-supplied
    /// branch the worker is expected to push to. The detector reads
    /// this same name back out of `state.db` (via
    /// `completion::expected_branch_name`) and queries
    /// `gh pr list --head <branch>` against it. If a worker pushes to
    /// a different bookmark, the fallback returns `None` instead of
    /// misbinding — but the happy path requires the worker to follow
    /// the engine's name, so the prompt must state it.
    #[tokio::test]
    async fn implementation_prompt_dictates_engine_supplied_branch_name() {
        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace).await.unwrap();
        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        let expected_branch = crate::completion::expected_branch_name("exec-test-1");
        assert!(
            prompt.contains(&expected_branch),
            "prompt must name the engine-supplied branch `{expected_branch}`, got: {prompt}",
        );
        assert!(
            prompt.contains("expected branch name"),
            "prompt must include the `expected branch name` context line, got: {prompt}",
        );
    }

    #[tokio::test]
    async fn initial_input_reads_prompt_from_disk() {
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace).await.unwrap();
        let input = spawner.spawn_input();

        // The pane needs to type a `claude` invocation that picks up
        // the rendered prompt as its first user message — going
        // through a file avoids shell-quoting issues with multi-line
        // markdown. Without this, the worker just sits at the bash
        // prompt forever (as it did before #174).
        assert!(
            input.initial_input.contains(".claude/initial-prompt.txt"),
            "expected initial_input to read from prompt file, got: {:?}",
            input.initial_input
        );
        assert!(
            input.initial_input.starts_with("claude"),
            "expected initial_input to invoke claude, got: {:?}",
            input.initial_input
        );
    }

    /// Build a runner driven against a real product + chore row so
    /// the dispatcher's effort/model lookup hits actual SQLite rather
    /// than the synthetic `sample_chore` fixture. Returns the spawner
    /// and the created chore id so the caller can re-use the row.
    async fn run_once_with_chore(
        workspace: &TempDir,
        chore_input: CreateChoreInput,
        product_default_model: Option<&str>,
    ) -> Result<(Arc<CapturingSpawner>, Task)> {
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());

        let product = work_db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            })
            .unwrap();
        if let Some(model) = product_default_model {
            work_db
                .set_product_default_model(&product.id, Some(model))
                .unwrap();
        }
        let mut chore_input = chore_input;
        chore_input.product_id = product.id.clone();
        let chore = work_db.create_chore(chore_input).unwrap();

        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.work_item_id = chore.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Chore(chore.clone()),
                workspace.path(),
                Some("change-1"),
            )
            .await?;

        Ok((spawner, chore))
    }

    /// Untagged row (NULL effort_level, NULL model_override, no
    /// product default) must produce the same spawn line today's
    /// engine produces — minus the implicit `claude` model selection,
    /// plus an explicit `--model <engine-default-slug>`. No
    /// `--effort` flag, no prompt addendum. Design §Q2 / task spec
    /// regression test: "byte-equivalent to today's `claude
    /// "$(cat .claude/initial-prompt.txt)"` plus the explicit
    /// `--model <engine-default-slug>`."
    #[tokio::test]
    async fn untagged_row_spawn_matches_engine_default() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Untagged chore".to_owned(),
            description: Some("plain row, no effort/model".to_owned()),
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        };
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None)
            .await
            .unwrap();
        let input = spawner.spawn_input();

        assert_eq!(
            input.initial_input,
            format!(
                "claude --model {} --permission-mode auto \"$(cat .claude/initial-prompt.txt)\"\n",
                crate::effort::ENGINE_DEFAULT_MODEL
            ),
            "untagged row should spawn with the engine default model, --permission-mode auto (Opus), and no --effort",
        );

        // No addendum prepended — the existing implementation framing
        // must be the first thing the worker sees.
        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            prompt.starts_with("You are a reusable Boss worker"),
            "untagged-row prompt must start with the original framing, got: {prompt:?}",
        );
        assert!(
            !prompt.contains("Sketch a brief plan"),
            "untagged-row prompt must not carry the medium addendum",
        );
        assert!(
            !prompt.starts_with("Begin with a written plan"),
            "untagged-row prompt must not carry the large/max addendum",
        );
    }

    /// Smoke test for the design-spec acceptance criterion: a
    /// `trivial` row dispatches with `--model claude-haiku-4-5-20251001
    /// --effort low` and no prompt addendum.
    #[tokio::test]
    async fn trivial_row_spawn_uses_haiku_at_low_effort() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Apply resize-cursor fix to nav divider".to_owned(),
            description: Some("one-line CSS tweak".to_owned()),
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: Some(EffortLevel::Trivial),
            model_override: None,
            force_duplicate: false,
        };
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None)
            .await
            .unwrap();
        let input = spawner.spawn_input();

        assert!(
            input
                .initial_input
                .contains("--model claude-haiku-4-5-20251001"),
            "trivial row must spawn Haiku, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort low"),
            "trivial row must pass --effort low, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--dangerously-skip-permissions"),
            "trivial row (Haiku, non-Opus) must carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--permission-mode"),
            "trivial row (Haiku, non-Opus) must NOT carry --permission-mode, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            !prompt.starts_with("Sketch") && !prompt.starts_with("Begin with"),
            "trivial row prompt must have no addendum prepended, got: {prompt:?}",
        );
    }

    /// Smoke test for the second design-spec acceptance criterion:
    /// `medium` + explicit `model_override = 'opus'` spawns `--model
    /// opus --effort high`, and the medium prompt addendum is
    /// prepended verbatim. Verifies that `model_override` changes only
    /// the model — the effort value and addendum still follow the
    /// row's `effort_level` (design §Q3).
    #[tokio::test]
    async fn medium_with_opus_override_uses_override_model_and_medium_addendum() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Add created_via provenance to chore/task creates".to_owned(),
            description: Some("multi-file edit with judgement calls".to_owned()),
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: Some(EffortLevel::Medium),
            model_override: Some("opus".to_owned()),
            force_duplicate: false,
        };
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None)
            .await
            .unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model opus"),
            "model_override should win precedence, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort high"),
            "medium effort_level must still produce --effort high, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--permission-mode auto"),
            "model_override=opus must carry --permission-mode auto, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--dangerously-skip-permissions"),
            "model_override=opus must NOT carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            prompt.starts_with("Sketch a brief plan before you start editing."),
            "medium addendum must be prepended verbatim, got: {prompt:?}",
        );
    }

    /// Large rows get Opus at `xhigh` plus the planning-heavy
    /// addendum. Confirms the third level boundary the design pins.
    #[tokio::test]
    async fn large_row_spawn_uses_opus_at_xhigh_with_planning_addendum() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Investigate isolated test instance".to_owned(),
            description: Some("multi-subsystem investigation".to_owned()),
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: Some(EffortLevel::Large),
            model_override: None,
            force_duplicate: false,
        };
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None)
            .await
            .unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model claude-opus-4-7"),
            "large row must spawn Opus, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort xhigh"),
            "large row must pass --effort xhigh, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--permission-mode auto"),
            "large row (Opus) must carry --permission-mode auto, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--dangerously-skip-permissions"),
            "large row (Opus) must NOT carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            prompt.starts_with("Begin with a written plan."),
            "large addendum must be prepended verbatim, got: {prompt:?}",
        );
    }

    /// `products.default_model` only kicks in when both
    /// `model_override` and `effort_level` are unset (design §Q3
    /// step 3). With a product default in place but no effort tag,
    /// the dispatch should pick the product slug rather than the
    /// engine default — and still omit `--effort`.
    #[tokio::test]
    async fn product_default_model_fills_in_when_row_is_untagged() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Untagged on Sonnet-defaulted product".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: None,
            model_override: None,
            force_duplicate: false,
        };
        let (spawner, _chore) =
            run_once_with_chore(&workspace, chore_input, Some("claude-sonnet-4-6"))
                .await
                .unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model claude-sonnet-4-6"),
            "product default_model should fill in, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--effort"),
            "untagged row must not pass --effort, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--dangerously-skip-permissions"),
            "Sonnet (non-Opus) must carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--permission-mode"),
            "Sonnet (non-Opus) must NOT carry --permission-mode, got: {:?}",
            input.initial_input,
        );
    }

    /// The runner must return the resolved spawn config on
    /// `RunOutcome.spawn_config` so the coordinator can attach it to
    /// the `pane_spawned` dispatch event. Drives `run_execution`
    /// directly (rather than through `run_once_with_chore`, which
    /// drops the outcome) so the returned tuple is observable.
    #[tokio::test]
    async fn run_outcome_carries_resolved_spawn_config() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());

        let product = work_db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            })
            .unwrap();
        let chore = work_db
            .create_chore(CreateChoreInput {
                product_id: product.id.clone(),
                name: "Trivial chore".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: Some(EffortLevel::Trivial),
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.work_item_id = chore.id.clone();

        let outcome = runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Chore(chore),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let spawn = outcome
            .spawn_config
            .expect("PaneSpawnRunner should always populate spawn_config");
        assert_eq!(spawn.effort_level, Some(EffortLevel::Trivial));
        assert_eq!(spawn.claude_effort, Some("low"));
        assert_eq!(spawn.model, "claude-haiku-4-5-20251001");
        assert_eq!(spawn.prompt_addendum, None);
    }

    /// **No env vars related to effort or token caps appear on the
    /// worker subprocess.** Design §Q2 §"Knobs explicitly not in v1"
    /// rejects `CLAUDE_CODE_MAX_OUTPUT_TOKENS`, `MAX_THINKING_TOKENS`,
    /// and any per-execution token cap explicitly — claude's
    /// `--effort` is the canonical control. Locks the rule in via the
    /// captured spawn env.
    #[tokio::test]
    async fn spawn_env_does_not_carry_effort_or_token_cap_env_vars() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput {
            product_id: String::new(),
            name: "Any chore".to_owned(),
            description: None,
            autostart: true,
            priority: None,
            created_via: None,
            repo_remote_url: None,
            effort_level: Some(EffortLevel::Large),
            model_override: None,
            force_duplicate: false,
        };
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None)
            .await
            .unwrap();
        let input = spawner.spawn_input();

        // The forbidden list from design §Q2 plus the obvious
        // adjacents an over-eager future patch might add.
        for forbidden in [
            "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
            "MAX_THINKING_TOKENS",
            "ANTHROPIC_MAX_TOKENS",
            "BOSS_EFFORT_LEVEL",
            "CLAUDE_EFFORT",
        ] {
            assert!(
                !input.env.iter().any(|EnvVar { key, .. }| key == forbidden),
                "env must not carry {forbidden} (design §Q2 forbids token-cap env knobs)",
            );
        }
    }

    #[tokio::test]
    async fn spawn_env_carries_sanitized_path_and_engine_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace).await.unwrap();
        let input = spawner.spawn_input();

        let path_var = input
            .env
            .iter()
            .find(|EnvVar { key, .. }| key == "PATH")
            .expect("PATH must be set on every worker spawn");
        assert!(
            !path_var.value.contains("/Users/"),
            "PATH must not contain the user home (would expose ~/bin/bossctl), got: {}",
            path_var.value
        );
        assert!(
            path_var.value.contains("/usr/bin"),
            "PATH must include system bins, got: {}",
            path_var.value
        );

        assert!(
            input
                .env
                .iter()
                .any(|EnvVar { key, .. }| key == "BOSS_LEASE_ID"),
            "expected BOSS_LEASE_ID to be set"
        );
        assert!(
            input
                .env
                .iter()
                .any(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET"),
            "expected BOSS_EVENTS_SOCKET to be set"
        );
    }

    /// The engine is now the source of truth for which slot a
    /// worker lands in. The runner derives the slot from the
    /// `worker-{N}` id the coordinator passes in and forwards it on
    /// `SpawnWorkerPaneInput.slot_id`. The app honors that slot
    /// rather than running its own allocator. This test pins down
    /// that wiring so a regression that drops the slot from the
    /// request (or computes it wrong) doesn't silently re-introduce
    /// the dual-allocator bug.
    #[tokio::test]
    async fn spawn_request_includes_engine_claimed_slot() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 8,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        // Engine claimed slot 6 (i.e. handed `worker-6` to the
        // runner). The spawn request must carry slot 6 — not 1, not
        // some random pick, not the lowest free.
        runner
            .run_execution(
                "worker-6",
                &sample_execution(workspace.path()),
                &sample_chore(),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let input = spawner.spawn_input();
        assert_eq!(
            input.slot_id, 6,
            "engine-claimed slot must reach the app verbatim, got {}",
            input.slot_id,
        );
    }

    #[tokio::test]
    async fn run_execution_stamps_work_item_binding_on_live_state() {
        // The bossctl coordinator joins `agents list` output back to a
        // chore via these fields — without them, asking "stop the
        // worker on chore X" forces the user to disambiguate slot
        // numbers manually.
        let workspace = TempDir::new().unwrap();
        let spawner = run_once(&workspace).await.unwrap();

        let state = spawner
            .live_states
            .get(1)
            .expect("expected live state for slot 1 after run_execution");
        assert_eq!(
            state.work_item_id.as_deref(),
            Some("task-1"),
            "work_item_id should match the chore the runner was driven against"
        );
        assert_eq!(
            state.work_item_name.as_deref(),
            Some("Improve top header (agent card) styling"),
            "work_item_name should be the chore's display name"
        );
        assert_eq!(
            state.execution_id.as_deref(),
            Some("exec-test-1"),
            "execution_id should match the WorkExecution row id"
        );
    }

    /// Any task whose `project_id` is set must surface the parent
    /// project's name/description/goal in its spawn prompt — the
    /// task row itself is intentionally a thin handle (the design
    /// task starts with `description = ''`; ordinary `project_task`
    /// rows often only carry an implementation brief that omits the
    /// project's *why*). Without the spawn-time walk the worker
    /// boots with no project context and has to ask, which defeats
    /// the point of having a project record at all.
    #[tokio::test]
    async fn spawn_prompt_for_project_scoped_task_includes_parent_project_context() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());

        // Stand up a real product → project → task chain so the
        // runner's `get_project` lookup hits a row with the
        // description/goal we want to assert on. `--no-autostart` on
        // the project keeps the auto-spawned design task parked so
        // it doesn't compete with our explicit run_execution call.
        let product = work_db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            })
            .unwrap();
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Engine dispatch instrumentation".to_owned(),
                description: Some(
                    "Instrument the auto-dispatcher so every spawn decision is traceable."
                        .to_owned(),
                ),
                goal: Some(
                    "Operators can answer 'why did this task spawn now' from logs alone."
                        .to_owned(),
                ),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        let task = work_db
            .create_task(CreateTaskInput {
                product_id: product.id.clone(),
                project_id: project.id.clone(),
                name: "Tag dispatch logs with execution kind".to_owned(),
                description: None,
                autostart: true,
                priority: None,
                created_via: None,
                repo_remote_url: None,
                effort_level: None,
                model_override: None,
                force_duplicate: false,
            })
            .unwrap();

        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = "task_implementation".into();
        execution.work_item_id = task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();
        assert!(
            prompt.contains("parent project: `Engine dispatch instrumentation`"),
            "prompt missing parent project name line:\n{prompt}",
        );
        assert!(
            prompt.contains("Instrument the auto-dispatcher"),
            "prompt missing parent project description:\n{prompt}",
        );
        assert!(
            prompt.contains("'why did this task spawn now'"),
            "prompt missing parent project goal:\n{prompt}",
        );
    }

    /// `boss project create` auto-files a `kind = 'design'` task as
    /// ordinal-0 of every new project. When that task dispatches it
    /// becomes a `project_design` execution. The worker prompt must
    /// state up front that the deliverable is a design document — not
    /// an implementation. Without this guard the worker has only the
    /// project's name/goal to go on and frequently starts coding;
    /// observed against worker O'Brien (exec_18aebf0caa1187e8_b).
    #[tokio::test]
    async fn spawn_prompt_for_auto_design_task_states_design_only_directive() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());

        let product = work_db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            })
            .unwrap();
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Worker live-status dashboard".to_owned(),
                description: Some(
                    "Surface every running worker's live state on the kanban without polling."
                        .to_owned(),
                ),
                goal: Some(
                    "Operators can see what every active worker is doing without opening panes."
                        .to_owned(),
                ),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        // Find the design task `create_project` auto-filed for this
        // project. It sorts ordinal-0 with `kind = 'design'`.
        let design_task = work_db
            .list_tasks(&product.id, Some(&project.id), None)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == "design")
            .expect("create_project should auto-file a kind='design' task");

        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = "project_design".into();
        execution.work_item_id = design_task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(design_task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();

        // The deliverable directive must be unmistakable.
        assert!(
            prompt.contains("the deliverable is a **design document**"),
            "design prompt must state the deliverable is a design doc:\n{prompt}",
        );
        assert!(
            prompt.contains("only the design doc"),
            "design prompt must scope the PR to the design doc only:\n{prompt}",
        );
        assert!(
            prompt.contains("Do not edit code"),
            "design prompt must forbid code edits:\n{prompt}",
        );

        // Canonical path uses the project slug since no design_doc_path
        // pointer is configured on this brand-new project.
        assert!(
            prompt.contains(&format!("docs/designs/{}.md", project.slug)),
            "design prompt must include the canonical doc path derived from the project slug `{}`:\n{prompt}",
            project.slug,
        );

        // Required section shape — all five anchors must be named so
        // the worker doesn't invent its own headings.
        for heading in [
            "**Goals**",
            "**Non-goals**",
            "**Alternatives considered**",
            "**Chosen approach**",
            "**Risks / open questions**",
        ] {
            assert!(
                prompt.contains(heading),
                "design prompt missing required section `{heading}`:\n{prompt}",
            );
        }

        // The parent project's goal must come through verbatim — that
        // is the whole point of pulling project context at spawn time.
        assert!(
            prompt.contains(
                "Operators can see what every active worker is doing without opening panes."
            ),
            "design prompt must surface the parent project's goal verbatim:\n{prompt}",
        );

        // The PR-URL acceptance criterion still applies to design
        // runs — they produce a PR, it just contains the doc only.
        assert!(
            prompt.contains("the deliverable is a PR URL"),
            "design prompt must keep the PR-URL acceptance criterion:\n{prompt}",
        );
    }

    /// When the project already has a `design_doc_path` pointer set
    /// (the resumed-design-pass case — a doc was filed, then the
    /// engine respawned the design task to revise it), the canonical
    /// path in the worker prompt must come from that pointer verbatim
    /// instead of the slug-derived default. Otherwise the worker
    /// could write to two different files across runs.
    #[tokio::test]
    async fn spawn_prompt_for_design_task_uses_explicit_design_doc_path() {
        use crate::work::SetProjectDesignDocInput;

        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig {
                cwd: workspace.path().to_path_buf(),
                db_path: workspace.path().join("state.db"),
                worker_pool_size: 1,
            },
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());

        let product = work_db
            .create_product(CreateProductInput {
                name: "Boss".to_owned(),
                description: None,
                repo_remote_url: Some("git@example.com:foo.git".to_owned()),
            })
            .unwrap();
        let project = work_db
            .create_project(CreateProjectInput {
                product_id: product.id.clone(),
                name: "Merge poller cadence tuning".to_owned(),
                description: Some("Pick the right merge-poller cadence.".to_owned()),
                goal: Some("Reduce GitHub API spend without lagging merges.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        work_db
            .set_project_design_doc(SetProjectDesignDocInput {
                project_id: project.id.clone(),
                design_doc_repo_remote_url: None,
                design_doc_branch: None,
                design_doc_path: Some("tools/boss/docs/designs/merge-poller-cadence.md".into()),
                unset: false,
            })
            .unwrap();

        let design_task = work_db
            .list_tasks(&product.id, Some(&project.id), None)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == "design")
            .expect("create_project should auto-file a kind='design' task");

        let runner = PaneSpawnRunner::new(cfg, work_db);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = "project_design".into();
        execution.work_item_id = design_task.id.clone();

        runner
            .run_execution(
                "worker-1",
                &execution,
                &WorkItem::Task(design_task),
                workspace.path(),
                Some("change-1"),
            )
            .await
            .unwrap();

        let prompt = std::fs::read_to_string(
            workspace.path().join(".claude").join("initial-prompt.txt"),
        )
        .unwrap();

        assert!(
            prompt.contains("`tools/boss/docs/designs/merge-poller-cadence.md`"),
            "design prompt must use the project's explicit design_doc_path pointer:\n{prompt}",
        );
        // And it should NOT also fall through to the slug-derived
        // suggestion line — that would be ambiguous.
        assert!(
            !prompt.contains("`design_doc_path` pointer is not yet set"),
            "design prompt should not emit the pointer-missing fallback when the pointer is set:\n{prompt}",
        );
    }

    #[tokio::test]
    async fn settings_json_uses_absolute_boss_event_path() {
        // BOSS_EVENT_BIN takes precedence — set it to a known absolute
        // path so we don't depend on the test runner's binary layout.
        // SAFETY: setting env in a Rust test process is racy with other
        // tests but this one isolates by writing files into a temp
        // workspace, so a stale env from a prior parallel test would
        // only confuse this test, not affect production code.
        unsafe { std::env::set_var("BOSS_EVENT_BIN", "/opt/boss/bin/boss-event") };

        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace).await.unwrap();
        let settings_path = workspace.path().join(".claude").join("settings.json");
        let settings = std::fs::read_to_string(&settings_path).unwrap();

        // Hooks must invoke an absolute path; the bare name
        // `boss-event` is what produced the production
        // `command not found` failures because the worker's sanitized
        // PATH doesn't include the bazel-out directory.
        assert!(
            settings.contains("/opt/boss/bin/boss-event"),
            "expected absolute boss-event path in settings.json, got: {}",
            settings,
        );
        assert!(
            !settings.contains("\"boss-event\"") || settings.contains("/opt/boss/bin/boss-event"),
            "settings.json must not invoke `boss-event` as a bare name",
        );

        unsafe { std::env::remove_var("BOSS_EVENT_BIN") };
    }

    /// `BOSS_EVENT_BIN` short-circuits everything else.
    #[test]
    fn resolve_boss_event_prefers_env_override() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let override_path = PathBuf::from("/opt/whatever/boss-event");
        let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path), None);
        assert_eq!(resolved, override_path);
    }

    /// `BOSS_BIN_DIR` is the installed-mode path; it wins over the
    /// dev-mode runfiles and workspace-bazel-bin candidates so a
    /// deployed Boss.app never silently falls through to a workspace clone.
    #[test]
    fn resolve_boss_event_prefers_boss_bin_dir_over_runfiles() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the bundle bin/ directory (installed mode).
        let bundle_bin = dir.path().join("bundle-bin");
        std::fs::create_dir_all(&bundle_bin).unwrap();
        let bundle_shim = bundle_bin.join("boss-event");
        std::fs::write(&bundle_shim, b"").unwrap();

        // Also synthesize runfiles (dev mode) — must NOT be picked.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        std::fs::write(runfiles.join("boss-event"), b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, Some(&bundle_bin));
        assert_eq!(resolved, bundle_shim);
    }

    /// When the engine binary has runfiles at the bazel-conventional
    /// path, the resolver must pick that up — this is the production
    /// path under `bazel run //tools/boss/engine:engine` once the
    /// engine `rust_binary` has the `data` dep on
    /// `//tools/boss/event-shim:boss-event`. The original #174 fix
    /// only covered the BOSS_EVENT_BIN branch; this test covers the
    /// branch that actually fires in real launches.
    #[test]
    fn resolve_boss_event_uses_runfiles_when_present() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the bazel runfiles tree the data dep produces.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        let shim = runfiles.join("boss-event");
        std::fs::write(&shim, b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, None);
        assert_eq!(resolved, shim);
    }

    /// Workspace `bazel-bin` symlink path is the secondary candidate
    /// — covers `bazel build` + non-`bazel run` scenarios where the
    /// engine binary is invoked directly but `BUILD_WORKSPACE_DIRECTORY`
    /// is set.
    #[test]
    fn resolve_boss_event_falls_back_to_workspace_bazel_bin() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        let workspace = dir.path().join("workspace");
        let bazel_bin = workspace.join("bazel-bin/tools/boss/event-shim");
        std::fs::create_dir_all(&bazel_bin).unwrap();
        let shim = bazel_bin.join("boss-event");
        std::fs::write(&shim, b"").unwrap();

        let resolved = resolve_boss_event_binary(&engine, Some(&workspace), None, None);
        assert_eq!(resolved, shim);
    }

    /// When nothing resolves we still return *something* so the
    /// system fails loud (worker logs `command not found`) rather
    /// than the engine crashing on path-construction. The bare
    /// fallback is intentional — see the resolver's doc comment.
    #[test]
    fn resolve_boss_event_falls_back_to_bare_name_when_nothing_resolves() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let resolved = resolve_boss_event_binary(&engine, None, None, None);
        assert_eq!(resolved, PathBuf::from("boss-event"));
    }
}
