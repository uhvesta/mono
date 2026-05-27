use std::io;
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
use crate::work::{CiRemediation, ConflictResolution, Project, Task, WorkDb, WorkExecution, WorkItem};
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
        let stable_bin_dir = std::env::var_os("HOME").map(|h| {
            PathBuf::from(h).join("Library/Application Support/Boss/bin")
        });
        resolve_boss_event_binary(
            &engine_path,
            workspace.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            stable_bin_dir.as_deref(),
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
///   3. `stable_bin_dir/boss-event` — the copy installed by the engine
///      at startup into `~/Library/Application Support/Boss/bin/`. In
///      dev mode the engine copies boss-event there on every startup so
///      the path baked into worker settings.json is stable across
///      `bazel clean` and workspace re-leases.
///   4. Bazel runfiles next to the engine binary
///      (`<engine_path>.runfiles/_main/tools/boss/event-shim/boss-event`).
///      Requires the engine `rust_binary` to declare a `data` dep
///      on `//tools/boss/event-shim:boss-event` — without it bazel
///      doesn't include the shim in the engine's runfiles.
///   5. Workspace `bazel-bin` symlink
///      (`<workspace>/bazel-bin/tools/boss/event-shim/boss-event`)
///      when `BUILD_WORKSPACE_DIRECTORY` is set (i.e., the engine
///      was launched via `bazel run` from a checkout).
///   6. Cargo / hand-built sibling: `<engine_dir>/boss-event`.
///   7. Bare name `boss-event` — only useful if the worker's PATH
///      happens to include it (today it doesn't, on purpose).
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
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

    // Stable dev-mode location. The engine copies boss-event here at
    // startup so hook paths baked into worker settings.json survive
    // `bazel clean` and workspace re-leases.
    if let Some(bin_dir) = stable_bin_dir {
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

/// Copy the boss-event shim binary to a stable location in the Boss
/// support directory. Called at engine startup so the path baked into
/// new worker settings.json files remains valid after a `bazel clean`.
///
/// `source_shim` is the currently-valid binary (from the runfiles tree
/// or bazel-bin). `stable_bin_dir` is the target directory
/// (`~/Library/Application Support/Boss/bin/`). Returns the stable path
/// on success. If `source_shim` is already inside `stable_bin_dir`,
/// returns `Ok(source_shim)` without copying (no-op for installed mode).
pub(crate) fn install_boss_event_to_stable_bin(
    source_shim: &Path,
    stable_bin_dir: &Path,
) -> io::Result<PathBuf> {
    let stable_path = stable_bin_dir.join("boss-event");
    if stable_path == source_shim {
        return Ok(stable_path);
    }
    std::fs::create_dir_all(stable_bin_dir)?;
    std::fs::copy(source_shim, &stable_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&stable_path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&stable_path, perms)?;
    }
    Ok(stable_path)
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
        // Detect whether this is a respawn after a crash: if the work item has
        // no task-level pr_url (handled by the existing RESUME EXISTING PR path)
        // but has a prior orphaned execution with no pr_url, derive its expected
        // branch so the new worker can attempt to resume it.
        let recovery_branch: Option<String> = if work_item_pr_url(work_item).is_none() {
            match self.work_db.get_prior_orphaned_execution(
                &execution.work_item_id,
                &execution.id,
            ) {
                Ok(Some(prior)) => {
                    let branch = crate::completion::expected_branch_name(
                        prior.worker_branch_prefix.as_deref(),
                        &prior.id,
                    );
                    tracing::info!(
                        execution_id = %execution.id,
                        prior_execution_id = %prior.id,
                        recovery_branch = %branch,
                        "startup recovery: prior orphaned execution found; directing worker to attempt branch resume",
                    );
                    Some(branch)
                }
                Ok(None) => {
                    tracing::debug!(
                        execution_id = %execution.id,
                        "startup recovery: no prior orphaned execution found; worker will start from main",
                    );
                    None
                }
                Err(err) => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        error = %format!("{err:#}"),
                        "startup recovery: failed to query prior orphaned execution; worker will start from main",
                    );
                    None
                }
            }
        } else {
            None
        };

        // For ci_remediation executions, the worker's prompt embeds the
        // engine's pre-spawn log excerpt and the failing-check list.
        // The attempt row is created at CI-failure detection time
        // (`ci_watch::on_ci_failure_detected`) and updated by the
        // coordinator's `collect_ci_log_excerpt_pre_spawn` before spawn.
        let ci_attempt = if execution.kind == "ci_remediation" {
            self.work_db
                .active_ci_remediation_for_work_item(&execution.work_item_id)
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
            recovery_branch.as_deref(),
            ci_attempt.as_ref(),
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
        // Scrub ANTHROPIC_API_KEY from the worker shell's environment before
        // invoking claude. The engine needs the var in its own process for
        // pane-summary LLM calls; workers must authenticate via OAuth
        // credentials (~/.claude/.credentials.json) and inherit nothing.
        // Without this unset, a user who sets ANTHROPIC_API_KEY in their
        // shell profile (or via `launchctl setenv`) causes every worker
        // spawn to show: "Auth conflict: Using ANTHROPIC_API_KEY instead of
        // Anthropic Console key."
        // The worker's session settings (boss-event hooks, deny rules)
        // live outside the workspace tree; point claude at them with
        // `--settings`. `write_workspace_files` writes the same path.
        let worker_settings_path =
            crate::worker_setup::worker_settings_path(workspace_path);
        let initial_input = format!(
            "unset ANTHROPIC_API_KEY; {}",
            spawn_config.claude_invocation(
                spawner.non_opus_auto_mode(),
                Some(&worker_settings_path),
            ),
        );

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
        } else if execution.kind == "ci_remediation" {
            pane_summary::ci_remediation_summary(work_item_name(work_item))
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
                task_title: Some(work_item_name(work_item).to_owned()),
                work_item_binding,
                model: spawn_config.model.clone(),
                draft_pr_mode: spawner.draft_pr_mode(),
                execution_kind: execution.kind.clone(),
                task_kind: work_item_task_kind(work_item).map(str::to_owned),
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
    recovery_branch: Option<&str>,
    ci_attempt: Option<&CiRemediation>,
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
    // Phase 9 #29: ci_remediation has its own templated prompt — embed
    // the engine-collected log excerpt, the failing-check set, and the
    // attempt-kind-specific playbook (rebase-first for `fix`, just the
    // retrigger CLI for `retrigger`).
    if execution.kind == "ci_remediation" {
        if let Some(attempt) = ci_attempt {
            return compose_ci_remediation_prompt(
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

    // If the chore already has a PR, inject a high-prominence resume
    // directive BEFORE the execution context so it outweighs the
    // workspace-rules default of `jj git fetch && jj new main`.
    let existing_pr_url = work_item_pr_url(work_item);
    if let Some(pr_url) = existing_pr_url {
        let pr_number = extract_pr_number(pr_url).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
        prompt.push_str(&format!(
            "## RESUME EXISTING PR\n\
             \n\
             This task has an existing open PR (#{pr_number}) at {pr_url}.\n\
             You MUST add commits to that branch — do NOT start from `jj new main` and do NOT open a new PR.\n\
             \n\
             After leasing your workspace:\n\
             ```\n\
             jj git fetch\n\
             GIT_DIR=.jj/repo/store/git gh pr checkout {pr_number}   # lands you on the PR branch\n\
             ```\n\
             Then make your changes on that branch and push:\n\
             ```\n\
             jj git push --bookmark <branch-name>   # or: GIT_DIR=.jj/repo/store/git git push\n\
             ```\n\
             \n\
             If the branch cannot be resumed (deleted upstream, conflict you cannot resolve, etc.),\n\
             STOP and surface the blocker — do NOT silently open a parallel PR.\n\n",
        ));
    } else if let Some(prior_branch) = recovery_branch {
        // No PR URL on the work item, but the prior execution was orphaned
        // mid-flight (engine crash / UI crash). The prior worker may have
        // pushed commits to its expected branch before the session died.
        // Direct the new worker to resume that branch rather than starting
        // from main — fall back cleanly if the branch doesn't exist on
        // the remote.
        let expected_branch_new = crate::completion::expected_branch_name(
            execution.worker_branch_prefix.as_deref(),
            &execution.id,
        );
        prompt.push_str(&format!(
            "## STARTUP RECOVERY\n\
             \n\
             This execution was respawned after the previous worker session was interrupted \
             (engine or UI crash). The prior worker may have pushed commits to \
             `{prior_branch}` on the remote.\n\
             \n\
             After leasing your workspace, attempt to resume the prior branch:\n\
             ```\n\
             jj git fetch\n\
             jj edit {prior_branch}@origin   # resumes prior commits if branch was pushed\n\
             ```\n\
             If that command fails (branch not found on remote — prior worker hadn't pushed \
             yet), fall back to `jj new main` instead.\n\
             \n\
             If you successfully resumed the prior branch, continue from those commits and \
             push using the new expected branch name `{expected_branch_new}` (see the \
             `expected branch name` line in the execution context below). Do NOT reuse the \
             prior branch name.\n\n",
        ));
    }

    let expected_branch = crate::completion::expected_branch_name(
        execution.worker_branch_prefix.as_deref(),
        &execution.id,
    );
    prompt.push_str("Execution context:\n");
    prompt.push_str(&format!("- execution id: `{}`\n", execution.id));
    prompt.push_str(&format!("- execution kind: `{}`\n", execution.kind));
    prompt.push_str(&format!("- workspace: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("- work item: `{}`\n", work_item_name(work_item)));
    // The "expected branch name" line directs the worker to push to a fresh
    // `boss/exec_<id>` bookmark and is correct only for executions that open
    // their OWN PR. A revision's deliverable is a new commit on the parent
    // PR's existing branch (see `compose_revision_directive`), so templating a
    // `boss/exec_*` branch name here would directly contradict that block's
    // "Do NOT create a `boss/exec_*` bookmark" instruction — and the revision
    // exec id has no corresponding branch anyway, so pushing it would create a
    // dangling branch no PR points at (issue #842). Omit the line for
    // revisions and let the revision directive be the only word on branching.
    // (`existing_pr_url` is the work item's PR; revisions carry the parent PR
    // on `execution.pr_url`, so this guard is checked independently.)
    if existing_pr_url.is_none() && execution.kind != "revision_implementation" {
        prompt.push_str(&format!(
            "- expected branch name: `{expected_branch}` — the engine reconstructs this from your execution id and uses it to find your PR. Push to this exact bookmark name.\n",
        ));
    }
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
        "investigation_implementation" => {
            prompt.push_str(&compose_investigation_directive(work_item));
        }
        "revision_implementation" => {
            prompt.push_str(&compose_revision_directive(execution, work_item, workspace_path));
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
    // Issue #804: code-touching implementation chores were pushing to PR
    // branches without a local build, and CI repeatedly caught errors a
    // local `bazel build`/`bazel test` of the touched targets would have
    // surfaced. Inject a hard pre-push build gate, but only when the
    // workspace is actually a Bazel workspace — non-Bazel repos
    // (gradle/maven/npm/…) must not see irrelevant build instructions.
    // Docs-only kinds (design/investigation) are excluded; revisions get
    // the gate inside `compose_revision_directive`.
    if matches!(
        execution.kind.as_str(),
        "task_implementation" | "chore_implementation"
    ) {
        if let Some(gate) = bazel_prepush_gate_block(workspace_path) {
            prompt.push_str(&gate);
        }
    }
    if matches!(
        execution.kind.as_str(),
        "task_implementation" | "chore_implementation" | "project_design" | "investigation_implementation"
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
        //
        // When the chore already has a pr_url, the acceptance criterion
        // changes: the worker pushes to the existing PR branch instead of
        // creating a new one. The engine's staged-URL detector captures
        // the URL from `gh pr view` output at the end of the run.
        if let Some(pr_url) = existing_pr_url {
            let pr_number = extract_pr_number(pr_url).map(|n| n.to_string()).unwrap_or_else(|| "?".into());
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Push your commits to the existing PR branch (see the ## RESUME EXISTING PR block above). Do NOT open a new PR.\n\
                 - Confirm the PR is updated with `GIT_DIR=.jj/repo/store/git gh pr view {pr_number}`.\n\
                 - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        } else {
            prompt.push_str(&format!(
                "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
                 - Use the engine-supplied branch name from the `expected branch name` line above (`{expected_branch}`) when creating your bookmark and pushing — do NOT invent a different name.\n\
                 - Push your branch (`jj bookmark create {expected_branch} -r @ && jj git push -b {expected_branch} --allow-new`) and open a PR with `gh pr create --head {expected_branch} --base main` if one does not already exist.\n\
                 - Alternatively, use `cube pr ensure --branch {expected_branch}` which pushes the branch and creates or reuses the PR in one step (jj-aware, no GIT_DIR needed).\n\
                 - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
                 - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        }
    }
    prompt.push_str("\nRespond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// True when `workspace_path` is the root of a Bazel workspace — i.e. a
/// `MODULE.bazel`, `WORKSPACE`, or `WORKSPACE.bazel` marker file sits at
/// the root. Bazel ownership is what gates the pre-push build
/// requirement (issue #804): many target repos are gradle/maven/npm/etc.
/// and must not be told to run `bazel build`.
fn is_bazel_workspace(workspace_path: &Path) -> bool {
    ["MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"]
        .iter()
        .any(|marker| workspace_path.join(marker).exists())
}

/// Pre-push build gate for Bazel workspaces (issue #804). Workers were
/// pushing code-touching chores to PR branches without a local build,
/// and CI repeatedly caught errors a local `bazel build`/`bazel test` of
/// the touched targets would have surfaced (stale crate_universe
/// lockfiles, gazelle validation, clippy `await_holding_lock`). The
/// loose "please verify" prose in chore descriptions did not hold, so
/// this states the requirement as a hard gate in the worker prompt.
///
/// Returns `None` for non-Bazel repos so the block is only injected when
/// bazel actually owns the workspace.
fn bazel_prepush_gate_block(workspace_path: &Path) -> Option<String> {
    if !is_bazel_workspace(workspace_path) {
        return None;
    }
    Some(
        "\n## Pre-push build gate (Bazel workspace)\n\
         \n\
         This repository is a Bazel workspace (a `MODULE.bazel`/`WORKSPACE` marker was found at the workspace root). Before you push a branch or update a PR with code changes, you MUST run a clean local build and test of what you touched and confirm both pass. \"I think it should work\" or \"the change looks correct\" is NOT a substitute for running the build — repeated rounds of CI breakage have come from workers skipping this step.\n\
         \n\
         Required before pushing:\n\
         - `bazel build` every target you changed and `bazel test` their tests. Use `bazel query` to resolve the target labels covering the files you edited if you are unsure which they are.\n\
         - If reverse dependencies are quick to enumerate, build them too so you don't break consumers: `bazel query 'rdeps(//..., <changed-target>)'`, then build the results.\n\
         - If a CI workflow file exists (`.github/workflows/*.yml`), open it and mirror the exact bazel target set it builds/tests (these repos typically run `bazel build //...` or a curated rollup). Run that same command locally so your gate matches what CI will enforce.\n\
         - Both `bazel build` and `bazel test` must finish clean — exit 0, no build errors, no failing tests, no clippy/lint failures — before you push.\n\
         \n\
         If the build or tests fail and you cannot make them pass within this run, do NOT push red code. Emit an `[effort-escalation]` marker in your final response with the failing command and its error output, and stop. Escalating a blocker is correct; pushing a known-broken branch is not.\n"
            .to_string(),
    )
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

/// Directive block for `kind = 'investigation'` tasks. States the
/// deliverable shape (one markdown doc, PR only, no code) and the
/// repo routing rules so the worker doesn't need to infer them.
///
/// Key divergence from design tasks:
/// - Destination repo is the product's `docs_repo` (or
///   `BOSS_USER_DOCS_REPO`) — NOT the product's code repo.
/// - No section template: free-form markdown. The investigation brief
///   drives the structure.
/// - PR is mandatory even on the user's personal docs repo. The
///   direct-push shortcut in the user's CLAUDE.md does NOT apply here:
///   the PR review window is the user's opportunity to edit the doc
///   before it is saved for posterity. Always open a PR.
/// - After opening the PR, record the doc pointer with
///   `boss task set-investigation-doc` so the kanban affordance can
///   link to the doc.
fn compose_investigation_directive(work_item: &WorkItem) -> String {
    let task_id = work_item_id(work_item);
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **markdown document**, not code. Do not edit source code, build files, or anything other than the investigation doc.\n");
    out.push_str("- the PR for this run contains **only the markdown doc** (one new file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    out.push_str("- choose a filename that reflects the topic (e.g. `docs/investigations/my-topic.md`). Use an `investigations/` subdirectory if one exists in the repo, or create it.\n");
    out.push_str("- open a PR with the doc regardless of which repo it lands in. Do NOT push directly to `main` even on the user's personal docs repo (e.g. `brianduff/docs`). The PR is the user's edit window.\n");
    out.push_str("- after the PR is open, register the doc pointer so the kanban card shows the doc affordance:\n");
    out.push_str(&format!(
        "  `boss task set-investigation-doc --task {task_id} --path <repo-relative-path> --branch <pr-branch>`\n"
    ));
    out.push_str("- investigations do not touch code. If the description asks for both research and a code change, write only the investigation doc and note the follow-up code changes at the end of the doc for the user to file separately.\n");
    out
}

/// Directive block for `kind = 'revision'` tasks.
///
/// A revision's deliverable is a NEW COMMIT on an EXISTING pull request —
/// the PR owned by the parent task's chain root.  The revision worker must
/// NOT open a new PR.  The parent's PR URL is carried in
/// `execution.pr_url` (set at dispatch time).
fn compose_revision_directive(
    execution: &crate::work::WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
) -> String {
    let description = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.description.trim().to_owned(),
        _ => String::new(),
    };
    let parent_pr_url = execution.pr_url.as_deref().unwrap_or("(unknown)");
    let pr_number = crate::completion::pr_number_from_url(parent_pr_url)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());

    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- This is a **REVISION** task. Your deliverable is a NEW COMMIT on an EXISTING pull request. Do NOT open a new PR. Do NOT create a `boss/exec_*` bookmark.\n");
    out.push_str(&format!(
        "- The parent PR is #{pr_number} at {parent_pr_url}.\n"
    ));
    out.push_str(&format!(
        "- What this revision should change: {description}\n"
    ));
    // Issue #804: revision chores (T30–T34 on PR #250) were the worst
    // offenders for pushing red code. Apply the same pre-push build gate
    // when the workspace is a Bazel workspace.
    if let Some(gate) = bazel_prepush_gate_block(workspace_path) {
        out.push_str(&gate);
    }
    out.push('\n');
    out.push_str("Steps:\n");
    out.push_str("1. `jj git fetch`   # the parent branch lives on GitHub; sync before editing.\n");
    out.push_str(&format!(
        "2. `GIT_DIR=.jj/repo/store/git gh pr checkout {pr_number}`   # checks out the parent PR branch.\n"
    ));
    out.push_str("3. Make the requested change.\n");
    out.push_str("4. `jj describe -m \"<short message describing the revision>\"`\n");
    out.push_str("   Then identify the parent branch name from `jj log` and advance it:\n");
    out.push_str("   `jj bookmark set <parent-branch-name> -r @`\n");
    out.push_str(&format!(
        "5. `GIT_DIR=.jj/repo/store/git jj git push -b <parent-branch-name>`   # NO --allow-new; the branch already exists.\n"
    ));
    out.push_str(&format!(
        "6. Confirm the new commit is on the PR: `GIT_DIR=.jj/repo/store/git gh pr view {pr_number}`\n"
    ));
    out.push_str(&format!(
        "7. Print the parent PR URL on its own line as the FINAL thing in your final response: {parent_pr_url}\n"
    ));
    out.push('\n');
    out.push_str("Constraints:\n");
    out.push_str("- Do NOT run `gh pr create` — this revision has no PR of its own.\n");
    out.push_str("- Do NOT create a `boss/exec_*` bookmark — push to the existing parent branch.\n");
    out.push_str("- Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, stop and explain.\n");
    out.push('\n');
    out.push_str(&format!(
        "\nAcceptance criterion: when you believe the work is done, the deliverable is the parent PR URL.\n\
         - Push your commit to the parent branch (see step 5 above). Do NOT open a new PR.\n\
         - Confirm the parent PR shows your new commit with `GIT_DIR=.jj/repo/store/git gh pr view {pr_number}`.\n\
         - Print {parent_pr_url} on its own line as the final thing in your final response so the engine can pick it up.\n\
         - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, stop and explain.\n"
    ));
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

/// Templated prompt for the `ci_remediation` execution kind
/// (`tools/boss/docs/designs/merge-conflict-handling-in-review.md` §Q4).
///
/// Two attempt kinds (mirroring `ci_remediations.attempt_kind`):
///
/// - `retrigger`: the engine pre-classified every failure as
///   `STARTUP_FAILURE` / `CANCELLED` (unambiguous infra). The worker's
///   only job is to re-run the failing build via the per-provider CLI
///   and call `boss engine ci mark-retriggered`. No code change, no
///   budget consumed.
///
/// - `fix`: at least one failure has a non-infra conclusion. Per the
///   reconciled 2026-05-17 design call (project description: "Always
///   try a rebase-onto-base BEFORE consuming a fix-attempt budget
///   slot"), the worker's **first** action is a rebase onto the base
///   branch HEAD followed by a force-push. If post-rebase CI goes
///   green the worker calls `boss engine ci mark-succeeded-via-rebase`
///   and the engine refunds the detection-side budget bump. Only when
///   post-rebase CI is still red does the worker proceed to a code
///   fix (the budget slot is then consumed as today).
fn compose_ci_remediation_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    attempt: &CiRemediation,
    test_command: Option<&str>,
) -> String {
    let is_rebounce = attempt
        .failure_kind
        .as_deref()
        .map_or(false, |k| k == "merge_queue_rebounce");

    let mut prompt = String::new();

    if is_rebounce {
        prompt.push_str(&format!(
            "## [merge-queue-rebounce] CI remediation: PR #{pr_num} ({kind}) — merge-queue FAILED_CHECKS\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        // Clear preamble so the worker doesn't waste time on the PR's own CI.
        prompt.push_str(
            "> **Important**: this is a **merge-queue rebounce**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA. Do NOT look at them.\n\
             > - The failure happened on the **synthetic merge commit** GitHub assembled when the PR\n\
             >   entered the queue. See `Synthetic merge SHA` below.\n\
             > - Root cause: something landed on `main` between this PR's CI run and its queue turn\n\
             >   that is semantically incompatible (a new required field, a renamed type, etc.). This\n\
             >   is a textbook semantic merge conflict that the merge queue exists to catch.\n\
             > - After fixing, **re-enqueue** the PR — the merge queue does not auto-retry.\n\n",
        );
    } else {
        prompt.push_str(&format!(
            "## CI remediation: PR #{pr_num} ({kind}) — required checks failing\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
    }

    prompt.push_str(&format!("**PR**: {}\n", attempt.pr_url));
    if !attempt.head_branch.is_empty() {
        prompt.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    if is_rebounce {
        if let Some(ref sha) = attempt.before_commit_sha {
            prompt.push_str(&format!(
                "**Synthetic merge SHA** (fetch CI logs from here): `{sha}`\n",
            ));
        }
    }
    prompt.push_str(&format!(
        "**Head sha at trigger**: `{}`\n",
        attempt.head_sha_at_trigger,
    ));
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

    // Failing-check list — same JSON the engine seeded on the row at
    // detection time. Rendered as a bulleted summary; the worker has the
    // raw `failed_checks` field if it wants to read further.
    prompt.push_str("### Failing required checks\n\n");
    match render_failed_checks_markdown(&attempt.failed_checks) {
        Some(md) => prompt.push_str(&md),
        None => prompt.push_str(
            "_The engine did not record a parseable `failed_checks` blob for this attempt. \
             Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
        ),
    }
    prompt.push('\n');

    if attempt.attempt_kind == "retrigger" {
        // §Q4 retrigger playbook: every failure is unambiguous infra,
        // no log read needed, no code change.
        prompt.push_str("### Action: retrigger the failing build\n\n");
        prompt.push_str(
            "The engine has pre-classified this failure as infra (every failing check has \
             `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
        );
        prompt.push_str(
            "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
             for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
             check's `target_url` above carries the right id.\n\
             2. Call `boss engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
             so the engine records the new run id and stays out of the budget path. Do NOT call \
             `mark-failed` or push code.\n\
             3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
        );
    } else {
        // §"fix" path with the reconciled-2026-05-17 rebase-first step.
        if is_rebounce {
            prompt.push_str("### Action: rebase onto current main, then fix the semantic conflict\n\n");
            prompt.push_str(
                "A merge-queue rebounce almost always means something landed on `main` between \
                 this PR's CI run and its queue turn that is **semantically incompatible** — the \
                 two patches don't textually conflict (GitHub's merge was clean) but the combined \
                 code doesn't compile or fails tests. The fix is:\n\
                 1. Rebase onto current `main` HEAD.\n\
                 2. Look at the CI failure on the **synthetic merge SHA** (not the PR head) to \
                    understand what became incompatible.\n\
                 3. Add a focused fix, push, and re-enqueue the PR.\n\n",
            );
        } else {
            prompt.push_str("### Action: rebase first, then fix\n\n");
            prompt.push_str(
                "Many CI failures on long-running PRs are caused by `main` moving (a dep bump, a fix \
                 that landed, an env change). The cheapest experiment is rebasing onto `main` HEAD \
                 before changing any code — if CI goes green after the rebase, no fix-attempt slot is \
                 consumed against this PR's budget.\n\n",
            );
        }
        prompt.push_str("**Step 1 — Rebase onto base HEAD and force-push.**\n\n");
        prompt.push_str(&format!(
            "```\n\
             jj git fetch\n\
             jj edit {branch}\n\
             jj rebase -d main -b {branch}\n\
             jj git push -b {branch}      # force-push: prior approvals will be dismissed by branch protection\n\
             ```\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        if is_rebounce {
            prompt.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, call \
                 `boss engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>` and stop. \
                 Then re-enqueue the PR (see Step 3 below). The engine flips the attempt to \
                 `succeeded` and the budget slot is not consumed.\n\
                 - **If post-rebase CI is still red**, the semantic conflict requires a code fix — \
                 continue to Step 2.\n\n",
            );
        } else {
            prompt.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, call \
                 `boss engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>` and stop. The \
                 engine flips the attempt to `succeeded`, sets `consumes_budget = 0`, and decrements \
                 `tasks.ci_attempts_used` so this attempt does not count against the PR's budget.\n\
                 - **If post-rebase CI is still red**, continue to Step 2. The budget slot is now \
                 consumed; this is the fix attempt the engine pre-classified.\n\n",
            );
        }

        prompt.push_str("**Step 2 — Read the log, classify, fix, push.**\n\n");
        if is_rebounce {
            // For rebounce, direct the worker to the synthetic merge SHA.
            // The PR head's logs are green and uninformative.
            let sha_hint = attempt
                .before_commit_sha
                .as_deref()
                .unwrap_or("<synthetic-merge-sha>");
            prompt.push_str(&format!(
                "Fetch CI logs from the **synthetic merge SHA `{sha_hint}`**, not the PR head \
                 (whose checks are green). Use the per-provider CLI:\n\n\
                 - Buildkite: `bk job log <job-id>` (job id from the failing check URL)\n\
                 - GitHub Actions: `gh run view --log-failed --job <job-id>` (job id from failing check URL)\n\n",
            ));
        } else {
            prompt.push_str("Engine-collected log excerpt (failing job tail):\n\n");
            match attempt.log_excerpt.as_deref().map(str::trim) {
                Some(tail) if !tail.is_empty() => {
                    prompt.push_str("```\n");
                    prompt.push_str(tail);
                    prompt.push_str("\n```\n\n");
                }
                _ => {
                    prompt.push_str(
                        "_The engine's pre-spawn log fetch did not produce an excerpt for this attempt. \
                         Shell out to the per-provider CLI (`bk job log <job-id>` / \
                         `gh run view --log-failed --job <job-id>`) from the failing check's `target_url`._\n\n",
                    );
                }
            }
        }
        prompt.push_str(
            "1. Classify the failure with `boss engine ci classify --attempt-id <attempt-id> --class <tractable|flaky_or_infra|unfixable>`.\n   \
                - `tractable` → there's a clear code change that resolves it. Make it. Push.\n   \
                - `flaky_or_infra` → the failure is environmental / not caused by this PR's diff. \
                Pivot to the retrigger playbook (re-run the failing build via the provider CLI \
                and call `mark-retriggered`).\n   \
                - `unfixable` → the failure is real and out of scope (e.g. a hard gate the PR \
                cannot satisfy). Call `boss engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` \
                and stop. Do NOT push.\n",
        );
        match test_command {
            Some(cmd) => prompt.push_str(&format!(
                "2. Before pushing a code change, validate locally with `{cmd}`.\n",
            )),
            None => prompt.push_str(
                "2. No `test_command` is configured for this product; rely on CI to verify the push.\n",
            ),
        }
        prompt.push_str(&format!(
            "3. Push your fix with `jj git push -b {branch}` (force-push if your worker rebased \
                first). The merge-poller will observe the new head sha and re-evaluate CI on the \
                next sweep — when green it flips the attempt to `succeeded` and unblocks the parent.\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        if is_rebounce {
            prompt.push_str(
                "**Step 3 (after CI is green) — Re-enqueue the PR.**\n\n\
                 The merge queue does **not** auto-retry after a dequeue. After your push produces \
                 green CI, re-add the PR to the merge queue:\n\n\
                 ```\n\
                 gh pr merge --auto --squash  # or --merge / --rebase per repo policy\n\
                 ```\n\n",
            );
        }
    }

    prompt.push_str("### Stop conditions\n\n");
    prompt.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (rebase, infra retrigger, or a focused fix).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\
         - **Always pass `-m \"…\"` to `git commit` / `jj describe` / `jj squash`.** The worker \
         environment has no usable `$EDITOR`.\n\n",
    );
    prompt.push_str("Respond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// Render the `failed_checks` JSON blob (one entry per failing required
/// check at trigger time) as a small bulleted list for the worker
/// prompt. Returns `None` when the blob is missing or malformed — the
/// caller falls back to a generic instruction.
fn render_failed_checks_markdown(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        conclusion: String,
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;
    if entries.is_empty() {
        return None;
    }
    let mut out = String::new();
    for e in &entries {
        out.push_str(&format!(
            "- `{name}` — {conclusion} ({provider}): {url}",
            name = e.name,
            conclusion = e.conclusion,
            provider = e.provider,
            url = e.target_url,
        ));
        if let Some(job_id) = e.provider_job_id.as_deref() {
            out.push_str(&format!(" (job `{job_id}`)"));
        }
        out.push('\n');
    }
    Some(out)
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

/// Return the task `kind` string (e.g. `"revision"`, `"chore"`) for task
/// work items. Returns `None` for products and projects, which have no
/// task-kind concept.
fn work_item_task_kind(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(&task.kind),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

fn work_item_pr_url(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task_bound_pr_url(task),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// The PR this task is bound to, if any.
///
/// Returns the structured `task.pr_url` column as set by
/// `reconciler_attach_pr_url` and the `pr_url_capture` pipeline.
/// Returns `None` when that field is empty or null.
///
/// **No description scanning.** An earlier version fell back to
/// pattern-matching a PR URL out of `task.description` (mono#742).
/// That fallback was removed because it fires on any description that
/// *mentions* a PR in passing (e.g. an issue-imported chore whose body
/// cites a repro session's PR as an example — incident T683 /
/// exec_18b341df81251750_4). A misfire sends the worker to a foreign
/// repo's PR, which is strictly worse than a duplicate-PR restart.
/// The reconciler path (`reconciler_attach_pr_url`) is responsible for
/// populating `task.pr_url` before dispatch; if it has not done so yet
/// the dispatcher should treat the task as PR-less and start fresh.
pub(crate) fn task_bound_pr_url(task: &crate::work::Task) -> Option<&str> {
    task.pr_url.as_deref().filter(|u| !u.is_empty())
}

/// Find a single canonical GitHub PR URL inside arbitrary text.
///
/// Returns `Some(&str)` when exactly one distinct
/// `https://github.com/<owner>/<repo>/pull/<N>` URL appears anywhere
/// in `text`. Returns `None` if the text has no PR URL, or has two
/// or more *distinct* PR URLs (we never guess which one is meant —
/// the worker is better off in the new-PR flow than bound to the
/// wrong existing PR).
///
/// The returned slice is the canonical form ending at the last digit
/// of `<N>`: trailing path segments (`/files`, `/commits/<sha>`),
/// query strings, fragments, and surrounding punctuation are all
/// dropped so the same URL appearing twice with different decorations
/// counts as one match.
pub(crate) fn extract_pr_url_from_text(text: &str) -> Option<&str> {
    const SCHEME: &str = "https://github.com/";
    let mut found: Option<&str> = None;
    let mut offset: usize = 0;
    while let Some(rel) = text[offset..].find(SCHEME) {
        let start = offset + rel;
        let after_scheme = start + SCHEME.len();
        match parse_canonical_pr_url(text, after_scheme) {
            Some(end) => {
                let canonical = &text[start..end];
                match found {
                    None => found = Some(canonical),
                    Some(prev) if prev == canonical => {}
                    Some(_) => return None,
                }
                offset = end;
            }
            None => {
                offset = after_scheme;
            }
        }
    }
    found
}

/// Given `after_scheme` = byte index just past `https://github.com/`
/// in `text`, try to parse `<owner>/<repo>/pull/<N>` and return the
/// byte index just past the last digit of `<N>`. `None` if the
/// structure doesn't match (e.g. the URL is for an issue, a tree, the
/// repo root, etc.).
fn parse_canonical_pr_url(text: &str, after_scheme: usize) -> Option<usize> {
    let rest = text.get(after_scheme..)?;
    let slash1 = rest.find('/')?;
    let owner = &rest[..slash1];
    if !is_github_path_segment(owner) {
        return None;
    }
    let after_owner = slash1 + 1;
    let slash2_rel = rest.get(after_owner..)?.find('/')?;
    let slash2 = after_owner + slash2_rel;
    let repo = &rest[after_owner..slash2];
    if !is_github_path_segment(repo) {
        return None;
    }
    let after_repo = slash2 + 1;
    let tail = rest.get(after_repo..)?;
    let tail = tail.strip_prefix("pull/")?;
    let digit_len = tail.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    Some(after_scheme + after_repo + "pull/".len() + digit_len)
}

fn is_github_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn extract_pr_number(pr_url: &str) -> Option<u64> {
    let tail = pr_url.rsplit_once("/pull/")?.1;
    let n = tail.split(|c: char| !c.is_ascii_digit()).next()?;
    n.parse::<u64>().ok()
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
        WorkExecution::builder()
            .id("exec-cr-1")
            .work_item_id("task_1")
            .kind("conflict_resolution")
            .status("running")
            .repo_remote_url("git@example.invalid:foo/bar.git")
            .cube_repo_id("foo")
            .cube_lease_id("lease-1")
            .cube_workspace_id("ws-1")
            .workspace_path("/tmp/workspace")
            .created_at("1700000000")
            .started_at("1700000010")
            .build()
    }

    fn sample_work_item() -> WorkItem {
        WorkItem::Chore(
            crate::work::Task::builder()
                .id("task_1")
                .product_id("prod_1")
                .kind("chore")
                .name("Some in-review chore")
                .description("")
                .status("blocked")
                .pr_url("https://github.com/foo/bar/pull/42")
                .created_at("1700000000")
                .updated_at("1700000000")
                .autostart(false)
                .last_status_actor("engine")
                .created_via("engine_auto")
                .blocked_reason("merge_conflict")
                .blocked_attempt_id("crz_x")
                .build(),
        )
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
mod compose_prompt_tests {
    use super::*;
    use crate::work::Task;

    fn base_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_abc123_01")
            .work_item_id("task-1")
            .kind("chore_implementation")
            .status("pending")
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn chore_without_pr() -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id("task-1")
                .product_id("prod-1")
                .kind("chore")
                .name("Fix the thing")
                .description("Description here.")
                .status("todo")
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .autostart(false)
                .build(),
        )
    }

    fn chore_with_pr(pr_url: &str) -> WorkItem {
        match chore_without_pr() {
            WorkItem::Chore(mut task) => {
                task.pr_url = Some(pr_url.into());
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    #[test]
    fn no_resume_directive_when_pr_url_is_absent() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("RESUME EXISTING PR"),
            "should have no resume block when pr_url is None:\n{prompt}",
        );
    }

    #[test]
    fn no_resume_directive_when_pr_url_is_empty() {
        let chore = chore_with_pr("");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("RESUME EXISTING PR"),
            "should have no resume block when pr_url is empty:\n{prompt}",
        );
    }

    #[test]
    fn resume_directive_present_when_pr_url_is_set() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/42");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("## RESUME EXISTING PR"),
            "missing resume block when pr_url is set:\n{prompt}",
        );
        assert!(
            prompt.contains("https://github.com/org/repo/pull/42"),
            "resume block should include the PR URL:\n{prompt}",
        );
        assert!(
            prompt.contains("#42"),
            "resume block should include the PR number:\n{prompt}",
        );
    }

    #[test]
    fn resume_directive_appears_before_execution_context() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/99");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        let resume_pos = prompt.find("## RESUME EXISTING PR").expect("missing resume block");
        let exec_pos = prompt.find("Execution context:").expect("missing execution context");
        assert!(
            resume_pos < exec_pos,
            "resume block must appear before execution context:\n{prompt}",
        );
    }

    #[test]
    fn expected_branch_name_suppressed_when_pr_url_set() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/42");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("expected branch name"),
            "expected-branch-name line should be suppressed when resuming a PR:\n{prompt}",
        );
    }

    #[test]
    fn expected_branch_name_present_when_no_pr_url() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("expected branch name"),
            "expected-branch-name line must be present for fresh dispatches:\n{prompt}",
        );
    }

    #[test]
    fn acceptance_criterion_references_existing_pr_when_pr_url_set() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/42");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("Do NOT open a new PR"),
            "acceptance criterion should prohibit opening a new PR:\n{prompt}",
        );
        assert!(
            prompt.contains("gh pr view 42"),
            "acceptance criterion should reference gh pr view for the existing PR:\n{prompt}",
        );
    }

    #[test]
    fn acceptance_criterion_uses_fresh_branch_when_no_pr_url() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("jj bookmark create"),
            "acceptance criterion should guide fresh branch creation:\n{prompt}",
        );
        assert!(
            prompt.contains("gh pr create") || prompt.contains("cube pr ensure"),
            "acceptance criterion should guide opening a new PR:\n{prompt}",
        );
    }

    #[test]
    fn no_recovery_block_when_no_prior_branch() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("STARTUP RECOVERY"),
            "no recovery block expected when recovery_branch is None:\n{prompt}",
        );
    }

    #[test]
    fn recovery_block_injected_when_prior_branch_provided() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            Some("boss/exec_prior123_09"),
            None,
        );
        assert!(
            prompt.contains("## STARTUP RECOVERY"),
            "recovery block should be present when recovery_branch is Some:\n{prompt}",
        );
        assert!(
            prompt.contains("boss/exec_prior123_09"),
            "recovery block should name the prior branch:\n{prompt}",
        );
        assert!(
            prompt.contains("jj edit boss/exec_prior123_09@origin"),
            "recovery block should instruct jj edit on the remote branch:\n{prompt}",
        );
    }

    #[test]
    fn recovery_block_suppressed_when_pr_url_set() {
        // When the work item already has a PR URL, the existing RESUME
        // EXISTING PR path takes precedence; the recovery block must not
        // also appear (that would be contradictory).
        let chore = chore_with_pr("https://github.com/org/repo/pull/42");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            Some("boss/exec_prior123_09"),
            None,
        );
        assert!(
            !prompt.contains("STARTUP RECOVERY"),
            "recovery block must not appear when existing PR URL takes precedence:\n{prompt}",
        );
        assert!(
            prompt.contains("## RESUME EXISTING PR"),
            "RESUME EXISTING PR block should still be present:\n{prompt}",
        );
    }

    #[test]
    fn recovery_block_appears_before_execution_context() {
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            Some("boss/exec_prior123_09"),
            None,
        );
        let recovery_pos = prompt.find("## STARTUP RECOVERY").expect("missing recovery block");
        let exec_pos = prompt.find("Execution context:").expect("missing execution context");
        assert!(
            recovery_pos < exec_pos,
            "recovery block must appear before execution context:\n{prompt}",
        );
    }

    #[test]
    fn recovery_block_mentions_new_expected_branch() {
        // The new worker should push under the NEW expected branch name
        // (derived from the current execution id), not the prior one.
        let prompt = compose_execution_prompt(
            &base_execution(),   // id = "exec_abc123_01"
            &chore_without_pr(),
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            Some("boss/exec_prior123_09"),
            None,
        );
        // "boss/exec_abc123_01" is the new expected branch
        assert!(
            prompt.contains("boss/exec_abc123_01"),
            "recovery block should mention the new expected branch name:\n{prompt}",
        );
    }

    #[test]
    fn work_item_pr_url_returns_none_for_project() {
        let project = WorkItem::Project(
            crate::work::Project::builder()
                .id("proj-1")
                .product_id("prod-1")
                .name("My Project")
                .description("")
                .goal("")
                .status("active")
                .slug("my-project")
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .build(),
        );
        assert!(work_item_pr_url(&project).is_none());
    }

    #[test]
    fn extract_pr_number_parses_standard_github_url() {
        assert_eq!(
            extract_pr_number("https://github.com/org/repo/pull/123"),
            Some(123),
        );
    }

    #[test]
    fn extract_pr_number_returns_none_for_malformed_url() {
        assert_eq!(extract_pr_number("https://github.com/org/repo"), None);
        assert_eq!(extract_pr_number("not-a-url"), None);
    }

    #[test]
    fn extract_pr_url_from_text_finds_bare_url() {
        let s = "see https://github.com/org/repo/pull/42 for context";
        assert_eq!(
            extract_pr_url_from_text(s),
            Some("https://github.com/org/repo/pull/42"),
        );
    }

    #[test]
    fn extract_pr_url_from_text_strips_trailing_punctuation() {
        let s = "follow-up on https://github.com/org/repo/pull/42.";
        assert_eq!(
            extract_pr_url_from_text(s),
            Some("https://github.com/org/repo/pull/42"),
        );
    }

    #[test]
    fn extract_pr_url_from_text_strips_subpath() {
        let s = "see https://github.com/org/repo/pull/42/files";
        assert_eq!(
            extract_pr_url_from_text(s),
            Some("https://github.com/org/repo/pull/42"),
        );
    }

    #[test]
    fn extract_pr_url_from_text_handles_markdown_link() {
        let s = "[PR](https://github.com/org/repo/pull/7) is in review";
        assert_eq!(
            extract_pr_url_from_text(s),
            Some("https://github.com/org/repo/pull/7"),
        );
    }

    #[test]
    fn extract_pr_url_from_text_returns_none_for_issue_url() {
        let s = "> Imported from https://github.com/org/repo/issues/742";
        assert_eq!(extract_pr_url_from_text(s), None);
    }

    #[test]
    fn extract_pr_url_from_text_returns_none_for_no_url() {
        assert_eq!(extract_pr_url_from_text("just a #235 reference"), None);
        assert_eq!(extract_pr_url_from_text(""), None);
    }

    #[test]
    fn extract_pr_url_from_text_returns_none_when_two_distinct_prs() {
        // Two distinct PR URLs in the same text — abort rather than
        // guess; the worker is safer in the new-PR flow than bound to
        // the wrong existing PR.
        let s = "rebase https://github.com/org/repo/pull/10 onto https://github.com/org/repo/pull/20";
        assert_eq!(extract_pr_url_from_text(s), None);
    }

    #[test]
    fn extract_pr_url_from_text_dedupes_same_url() {
        // The same PR mentioned twice (once bare, once with /files) is
        // still one match.
        let s = "PR https://github.com/org/repo/pull/42 also at https://github.com/org/repo/pull/42/files";
        assert_eq!(
            extract_pr_url_from_text(s),
            Some("https://github.com/org/repo/pull/42"),
        );
    }

    #[test]
    fn task_bound_pr_url_prefers_explicit_column() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/99");
        let task = match &chore {
            WorkItem::Chore(t) => t,
            _ => unreachable!(),
        };
        assert_eq!(
            task_bound_pr_url(task),
            Some("https://github.com/org/repo/pull/99"),
        );
    }

    #[test]
    fn task_bound_pr_url_returns_none_when_description_has_only_issue_url() {
        let chore = match chore_without_pr() {
            WorkItem::Chore(mut task) => {
                task.description = "> Imported from https://github.com/org/repo/issues/742".into();
                WorkItem::Chore(task)
            }
            other => other,
        };
        let task = match &chore {
            WorkItem::Chore(t) => t,
            _ => unreachable!(),
        };
        assert!(task_bound_pr_url(task).is_none());
    }

    #[test]
    fn task_bound_pr_url_ignores_pr_url_in_description() {
        // Regression for T683 / exec_18b341df81251750_4: a chore imported
        // from an issue whose body *mentions* a PR URL (e.g. as a repro
        // example) must NOT cause a RESUME EXISTING PR block. The structured
        // `pr_url` field is the only authoritative source.
        let chore = match chore_without_pr() {
            WorkItem::Chore(mut task) => {
                task.description =
                    "Parent chore C19 landed at https://github.com/linkedin-multiproduct/dev-infra/pull/250 \
                     as a repro example — this chore has no PR yet."
                        .into();
                WorkItem::Chore(task)
            }
            other => other,
        };
        let task = match &chore {
            WorkItem::Chore(t) => t,
            _ => unreachable!(),
        };
        assert!(
            task_bound_pr_url(task).is_none(),
            "description-embedded PR URL must not be treated as the task's PR",
        );
    }

    #[test]
    fn resume_directive_absent_when_pr_url_is_null() {
        // Regression for T683: a chore with pr_url=null and a description
        // mentioning a PR must not generate a RESUME EXISTING PR block.
        let chore = match chore_without_pr() {
            WorkItem::Chore(mut task) => {
                task.description =
                    "Ref: https://github.com/linkedin-multiproduct/dev-infra/pull/250".into();
                WorkItem::Chore(task)
            }
            other => other,
        };
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("## RESUME EXISTING PR"),
            "RESUME block must NOT fire when task.pr_url is null, even if description mentions a PR:\n{prompt}",
        );
    }

    #[test]
    fn resume_directive_present_when_structured_pr_url_is_set() {
        // Positive case: task with an explicit pr_url gets the RESUME block.
        let chore = chore_with_pr("https://github.com/org/repo/pull/235");
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore,
            None,
            std::path::Path::new("/tmp/workspace"),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("## RESUME EXISTING PR"),
            "resume block must fire when task.pr_url is set:\n{prompt}",
        );
        assert!(
            prompt.contains("https://github.com/org/repo/pull/235"),
            "resume block must quote the structured PR URL:\n{prompt}",
        );
        assert!(
            prompt.contains("#235"),
            "resume block must surface the PR number:\n{prompt}",
        );
    }

    fn revision_execution(pr_url: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_rev_01")
            .work_item_id("task-1")
            .kind("revision_implementation")
            .status("pending")
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .pr_url(pr_url)
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    /// Lay down a `MODULE.bazel` marker so `is_bazel_workspace` treats
    /// the tempdir as a Bazel workspace (issue #804).
    fn bazel_workspace() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("MODULE.bazel"), "module(name = \"x\")\n").unwrap();
        dir
    }

    #[test]
    fn bazel_gate_present_for_chore_on_bazel_workspace() {
        let ws = bazel_workspace();
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            ws.path(),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("## Pre-push build gate (Bazel workspace)"),
            "bazel pre-push gate must fire for code chores on a Bazel workspace:\n{prompt}",
        );
        assert!(
            prompt.contains("bazel build") && prompt.contains("bazel test"),
            "gate must require both bazel build and bazel test:\n{prompt}",
        );
        assert!(
            prompt.contains("[effort-escalation]"),
            "gate must direct failures to an effort-escalation marker:\n{prompt}",
        );
    }

    #[test]
    fn bazel_gate_absent_on_non_bazel_workspace() {
        // Empty tempdir — no MODULE.bazel / WORKSPACE marker.
        let ws = tempfile::TempDir::new().unwrap();
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            ws.path(),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("Pre-push build gate"),
            "bazel gate must NOT fire on a non-Bazel repo:\n{prompt}",
        );
    }

    #[test]
    fn bazel_gate_present_for_revision_on_bazel_workspace() {
        let ws = bazel_workspace();
        let prompt = compose_execution_prompt(
            &revision_execution("https://github.com/org/repo/pull/250"),
            &chore_without_pr(),
            None,
            ws.path(),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("## Pre-push build gate (Bazel workspace)"),
            "revision chores (the #804 offenders) must get the bazel gate:\n{prompt}",
        );
    }

    #[test]
    fn revision_prompt_omits_expected_branch_line() {
        // Issue #842: the preamble "expected branch name" line directs the
        // worker to push a fresh `boss/exec_*` bookmark, which directly
        // contradicts the revision directive's "Do NOT create a
        // `boss/exec_*` bookmark". A revision lands its commit on the
        // parent PR's existing branch, so the line must be omitted.
        let ws = tempfile::TempDir::new().unwrap();
        let prompt = compose_execution_prompt(
            &revision_execution("https://github.com/org/repo/pull/250"),
            &chore_without_pr(),
            None,
            ws.path(),
            None,
            None,
            None,
            None,
        );
        assert!(
            !prompt.contains("expected branch name"),
            "revision prompt must NOT template the expected-branch line (issue #842):\n{prompt}",
        );
        // The revision directive remains the only — and now uncontradicted —
        // word on branching.
        assert!(
            prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
            "revision directive must still forbid creating a boss/exec_* bookmark:\n{prompt}",
        );
    }

    #[test]
    fn chore_prompt_keeps_expected_branch_line() {
        // Guard the inverse: a fresh chore opens its own PR off a
        // `boss/exec_<id>` branch, so it must still be told the
        // engine-supplied branch name to push to.
        let ws = tempfile::TempDir::new().unwrap();
        let prompt = compose_execution_prompt(
            &base_execution(),
            &chore_without_pr(),
            None,
            ws.path(),
            None,
            None,
            None,
            None,
        );
        assert!(
            prompt.contains("expected branch name"),
            "a fresh chore must still receive the expected-branch line:\n{prompt}",
        );
    }

    #[test]
    fn bazel_gate_recognizes_workspace_marker_files() {
        for marker in ["WORKSPACE", "WORKSPACE.bazel"] {
            let dir = tempfile::TempDir::new().unwrap();
            std::fs::write(dir.path().join(marker), "").unwrap();
            assert!(
                is_bazel_workspace(dir.path()),
                "`{marker}` at the root must be recognized as a Bazel workspace",
            );
        }
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
        WorkExecution::builder()
            .id("exec-test-1")
            .work_item_id("task-1")
            .kind("chore_implementation")
            .status("running")
            .repo_remote_url("git@example.com:foo.git")
            .cube_repo_id("foo")
            .cube_lease_id("lease-1")
            .cube_workspace_id("foo-agent-001")
            .workspace_path(workspace_path.display().to_string())
            .created_at("2026-05-06T20:00:00Z")
            .started_at("2026-05-06T20:00:00Z")
            .build()
    }

    fn sample_chore() -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id("task-1")
                .product_id("prod-1")
                .kind("chore")
                .name("Improve top header (agent card) styling")
                .description("The gray header at the top is too cramped.")
                .status("todo")
                .created_at("2026-05-06T20:00:00Z")
                .updated_at("2026-05-06T20:00:00Z")
                .build(),
        )
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
            prompt.contains("gh pr create") || prompt.contains("gh pr view") || prompt.contains("cube pr ensure"),
            "implementation prompt must mention gh pr commands or cube pr ensure: {prompt}",
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
        let expected_branch = crate::completion::expected_branch_name(None, "exec-test-1");
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
            input.initial_input.starts_with("unset ANTHROPIC_API_KEY; claude"),
            "expected initial_input to unset ANTHROPIC_API_KEY and invoke claude, got: {:?}",
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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

        // The worker settings file lives outside the workspace; the
        // engine points claude at it with `--settings '<abs-path>'`,
        // positioned before the positional prompt arg.
        let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
        assert_eq!(
            input.initial_input,
            format!(
                "unset ANTHROPIC_API_KEY; claude --model {} --permission-mode auto --settings '{}' \"$(cat .claude/initial-prompt.txt)\"\n",
                crate::effort::ENGINE_DEFAULT_MODEL,
                settings_path.display(),
            ),
            "untagged row should spawn with the engine default model, --permission-mode auto (Opus), --settings <worker file>, and no --effort",
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
    /// `trivial` row dispatches with `--model claude-sonnet-4-6
    /// --effort low` and no prompt addendum. (Trivial originally
    /// mapped to Haiku, but Haiku doesn't honour the unattended-permission
    /// flags on every CLI build, so trivial rows now fall through to
    /// Sonnet — see [`crate::effort::default_model_for_level`].)
    #[tokio::test]
    async fn trivial_row_spawn_uses_sonnet_at_low_effort() {
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
                .contains("--model claude-sonnet-4-6"),
            "trivial row must spawn Sonnet, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--effort low"),
            "trivial row must pass --effort low, got: {:?}",
            input.initial_input,
        );
        assert!(
            input.initial_input.contains("--dangerously-skip-permissions"),
            "trivial row (Sonnet, non-Opus) must carry --dangerously-skip-permissions, got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--permission-mode"),
            "trivial row (Sonnet, non-Opus) must NOT carry --permission-mode, got: {:?}",
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
        assert_eq!(spawn.model, "claude-sonnet-4-6");
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
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
        // The settings file lives outside the workspace tree, keyed by
        // workspace name (see worker_setup); it must NOT be written into
        // the workspace `.claude/`.
        let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
        assert!(
            !workspace.path().join(".claude").join("settings.json").exists(),
            "engine must not write .claude/settings.json into the workspace",
        );
        let settings = std::fs::read_to_string(&settings_path).unwrap();

        // Hooks must invoke an absolute path; the bare name
        // `boss-event` is what produced the production
        // `command not found` failures because the worker's sanitized
        // PATH doesn't include the bazel-out directory.
        assert!(
            settings.contains("/opt/boss/bin/boss-event"),
            "expected absolute boss-event path in settings file, got: {}",
            settings,
        );
        assert!(
            !settings.contains("\"boss-event\"") || settings.contains("/opt/boss/bin/boss-event"),
            "settings file must not invoke `boss-event` as a bare name",
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
        let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path), None, None);
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

        let resolved = resolve_boss_event_binary(&engine, None, None, Some(&bundle_bin), None);
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

        let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
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

        let resolved = resolve_boss_event_binary(&engine, Some(&workspace), None, None, None);
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
        let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
        assert_eq!(resolved, PathBuf::from("boss-event"));
    }

    /// The stable bin dir (installed by the engine at startup) is
    /// preferred over bazel runfiles and bazel-bin so a `bazel clean`
    /// doesn't break hook paths already baked into worker settings.json.
    #[test]
    fn resolve_boss_event_prefers_stable_bin_dir_over_runfiles() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();

        // Synthesize the stable bin dir (engine startup installs it here).
        let stable_bin = dir.path().join("stable-bin");
        std::fs::create_dir_all(&stable_bin).unwrap();
        let stable_shim = stable_bin.join("boss-event");
        std::fs::write(&stable_shim, b"stable").unwrap();

        // Also synthesize runfiles — must NOT be picked when stable exists.
        let runfiles = dir.path().join("engine.runfiles/_main/tools/boss/event-shim");
        std::fs::create_dir_all(&runfiles).unwrap();
        std::fs::write(runfiles.join("boss-event"), b"runfiles").unwrap();

        let resolved = resolve_boss_event_binary(&engine, None, None, None, Some(&stable_bin));
        assert_eq!(resolved, stable_shim);
    }

    /// `install_boss_event_to_stable_bin` copies the shim and marks it
    /// executable so workers can invoke it directly.
    #[test]
    fn install_boss_event_to_stable_bin_copies_and_makes_executable() {
        let dir = TempDir::new().unwrap();
        let source = dir.path().join("boss-event-source");
        std::fs::write(&source, b"#!/bin/sh\necho ok\n").unwrap();

        let stable_bin = dir.path().join("stable/bin");
        let result = install_boss_event_to_stable_bin(&source, &stable_bin);
        assert!(result.is_ok(), "install should succeed: {result:?}");
        let stable = result.unwrap();
        assert_eq!(stable, stable_bin.join("boss-event"));
        assert!(stable.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&stable).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "boss-event must be executable after install");
        }
    }

    /// Installing when src == dst is a no-op (doesn't fail or corrupt the file).
    #[test]
    fn install_boss_event_to_stable_bin_no_op_when_already_stable() {
        let dir = TempDir::new().unwrap();
        let stable_bin = dir.path().join("bin");
        std::fs::create_dir_all(&stable_bin).unwrap();
        let stable = stable_bin.join("boss-event");
        std::fs::write(&stable, b"#!/bin/sh\n").unwrap();

        let result = install_boss_event_to_stable_bin(&stable, &stable_bin);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), stable);
    }
}
