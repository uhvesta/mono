use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use crate::ci_log_reader::{parse_buildkite_build_id, parse_buildkite_pipeline_slug};
use crate::config::RuntimeConfig;
use crate::conflict_diagnosis::ConflictDiagnosis;
use crate::coordinator::{pool_model_override_for_worker_id, slot_id_from_worker_id};
use crate::driver::AgentDriver;
use crate::effort::{SpawnConfig, resolve_spawn_config};
use crate::pane_summary;
use crate::spawn_flow::{StartWorkerInput, start_worker};
use crate::work::{CiRemediation, ConflictResolution, Project, Task, WorkDb, WorkExecution, WorkItem};
use crate::worker_setup::WorkerKind;
use boss_protocol::{EditorialRules, ExecutionKind, ExecutionStatus, TemplatePolicy, WorkItemBinding};
#[cfg(test)]
use boss_protocol::{TaskKind, TaskStatus};

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
    /// The execution was cancelled (kanban drag to Backlog, force-stop)
    /// *during* its spawn window — after the worker pane came up but
    /// before the run could be recorded. The runner has already reaped
    /// the just-spawned pane; the coordinator must release the cube
    /// lease the cancel path deliberately left held and skip the normal
    /// completion recording (the row is already terminal). See
    /// [`PaneSpawnRunner::run_execution`] and the T981 mid-spawn-cancel
    /// collision this closes.
    CancelledDuringSpawn,
    /// A `pr_review` reviewer pane was successfully spawned. The pane is
    /// alive and the reviewer agent is actively working. The execution
    /// stays in `running` (not `waiting_human`) until the Stop hook fires
    /// and `finalize_pr_review_pass` transitions it to `completed` via
    /// `record_worker_pr_completion`. Workspace is retained so the reviewer
    /// pane can continue.
    ///
    /// Using `running` (rather than `waiting_human`) is what keeps the
    /// "AI reviewing" badge visible on kanban cards for the duration of
    /// the review — the badge queries `pr_review` executions in `running`
    /// status. `waiting_human` is semantically wrong here: nobody is waiting
    /// for a human while the reviewer agent is working.
    ReviewerPaneAlive,
}

impl RunWaitState {
    pub fn execution_status(self) -> ExecutionStatus {
        match self {
            RunWaitState::Terminal => ExecutionStatus::Completed,
            RunWaitState::WaitingDependency => ExecutionStatus::WaitingDependency,
            RunWaitState::WaitingHuman => ExecutionStatus::WaitingHuman,
            RunWaitState::WaitingReview => ExecutionStatus::WaitingReview,
            RunWaitState::WaitingMerge => ExecutionStatus::WaitingMerge,
            // The row is already `cancelled`; the coordinator never
            // drives a status transition for this variant. Report the
            // terminal status for completeness.
            RunWaitState::CancelledDuringSpawn => ExecutionStatus::Cancelled,
            // Reviewer pane is alive; execution stays `running`.
            RunWaitState::ReviewerPaneAlive => ExecutionStatus::Running,
        }
    }

    pub fn release_workspace(self) -> bool {
        matches!(
            self,
            RunWaitState::Terminal | RunWaitState::WaitingDependency | RunWaitState::CancelledDuringSpawn
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

/// Absolute path of the engine's local events socket — where worker hook
/// shims connect. Honours the `BOSS_EVENTS_SOCKET` override and otherwise
/// falls back to the stable `~/Library/Application Support/Boss` location.
/// Shared by the local `PaneSpawnRunner` and the remote
/// `SshHostAdapterProvider` (the target of each remote run's reverse
/// `ssh -R` forward), so both agree on one path.
pub fn engine_events_socket_path() -> PathBuf {
    if let Ok(override_path) = std::env::var("BOSS_EVENTS_SOCKET") {
        return override_path.into();
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/Boss/events.sock")
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
    /// Feature flags store — checked at spawn time to decide whether
    /// editorial controls are active for this execution.
    feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    /// Set after construction via [`PaneSpawnRunner::set_server_state`].
    /// Stored as `Weak` to avoid the runner ↔ ServerState reference
    /// cycle. Resolved each call.
    server_state: std::sync::OnceLock<Weak<dyn crate::spawn_flow::WorkerSpawner>>,
    /// Test-injection override for the boss-event binary path. When set,
    /// `boss_event_binary()` returns this directly without consulting the
    /// environment — so tests don't depend on host PATH/filesystem layout.
    boss_event_path_override: std::sync::OnceLock<PathBuf>,
}

impl PaneSpawnRunner {
    pub fn new(
        cfg: Arc<RuntimeConfig>,
        work_db: Arc<WorkDb>,
        feature_flags: Arc<crate::feature_flags::FeatureFlagsStore>,
    ) -> Self {
        Self {
            cfg,
            work_db,
            feature_flags,
            server_state: std::sync::OnceLock::new(),
            boss_event_path_override: std::sync::OnceLock::new(),
        }
    }

    pub fn set_server_state(&self, server_state: Weak<dyn crate::spawn_flow::WorkerSpawner>) {
        let _ = self.server_state.set(server_state);
    }

    /// Inject a known absolute boss-event path for tests so they don't
    /// depend on the host filesystem or `BOSS_EVENT_BIN` env var.
    #[cfg(test)]
    pub(crate) fn set_boss_event_path(&self, path: PathBuf) {
        let _ = self.boss_event_path_override.set(path);
    }

    fn events_socket_path(&self) -> PathBuf {
        engine_events_socket_path()
    }

    fn boss_event_binary(&self) -> PathBuf {
        if let Some(injected) = self.boss_event_path_override.get() {
            return injected.clone();
        }
        let engine_path = std::env::current_exe().unwrap_or_default();
        let workspace = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map(PathBuf::from);
        let env_override = std::env::var_os("BOSS_EVENT_BIN").map(PathBuf::from);
        let boss_bin_dir = std::env::var_os("BOSS_BIN_DIR").map(PathBuf::from);
        let stable_bin_dir =
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Boss/bin"));
        resolve_boss_event_binary(
            &engine_path,
            workspace.as_deref(),
            env_override.as_deref(),
            boss_bin_dir.as_deref(),
            stable_bin_dir.as_deref(),
        )
        .unwrap_or_else(|| {
            panic!(
                "boss-event binary not found: none of BOSS_EVENT_BIN, BOSS_BIN_DIR, \
                 the stable bin dir, runfiles, bazel-bin, or the engine-sibling resolved \
                 to an existing file. A bare 'boss-event' in hook commands causes silent \
                 event-emission failures when the worker's sanitized PATH does not include it. \
                 Set BOSS_EVENT_BIN to the absolute boss-event path to fix this."
            )
        })
    }
}

/// Pure resolver for the absolute path of the `boss-event` shim
/// that the worker pane invokes from `settings.json`. Pulled out
/// as a free function so tests can pass synthetic `engine_path` /
/// `workspace_dir` / env values without monkey-patching globals.
///
/// Returns `Some(path)` when a candidate exists on disk, `None` when no
/// candidate resolves. The caller is responsible for treating `None` as a
/// hard error — a bare `boss-event` in hook commands causes silent
/// event-emission failures because the worker's sanitized PATH does not
/// include bazel-out or other non-standard directories.
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
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
    boss_bin_dir: Option<&Path>,
    stable_bin_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(override_path) = env_override {
        return Some(override_path.to_path_buf());
    }

    // Installed mode: BOSS_BIN_DIR is Boss.app/Contents/Resources/bin/.
    if let Some(bin_dir) = boss_bin_dir {
        let candidate = bin_dir.join("boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Stable dev-mode location. The engine copies boss-event here at
    // startup so hook paths baked into worker settings.json survive
    // `bazel clean` and workspace re-leases.
    if let Some(bin_dir) = stable_bin_dir {
        let candidate = bin_dir.join("boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Bazel constructs runfiles at `<binary>.runfiles/_main/<workspace_relative_path>`.
    let mut runfiles_root = engine_path.as_os_str().to_owned();
    runfiles_root.push(".runfiles");
    let runfiles_candidate = PathBuf::from(runfiles_root)
        .join("_main")
        .join("tools/boss/event-shim/boss-event");
    if runfiles_candidate.exists() {
        return Some(runfiles_candidate);
    }

    if let Some(workspace) = workspace_dir {
        let candidate = workspace.join("bazel-bin/tools/boss/event-shim/boss-event");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    if let Some(engine_dir) = engine_path.parent() {
        let sibling = engine_dir.join("boss-event");
        if sibling.exists() {
            return Some(sibling);
        }
    }

    None
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
pub(crate) fn install_boss_event_to_stable_bin(source_shim: &Path, stable_bin_dir: &Path) -> io::Result<PathBuf> {
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
        // `worker_id` is `worker-{N}` (main pool), `auto-worker-{N}`
        // (automation pool), or `review-{N}` (review pool); N is the slot
        // the engine owns. Decode it here and thread it into the spawn so
        // the app hosts the pane in this exact slot rather than running its
        // own (now-deleted) firstIndex(where:) heuristic.
        let slot_id = slot_id_from_worker_id(worker_id).ok_or_else(|| {
            anyhow!(
                "PaneSpawnRunner received worker_id {worker_id:?} that does not parse as worker-{{N}}, auto-worker-{{N}}, or review-{{N}}"
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
        //
        // Prompt composition + effort/model resolution live in the
        // shared `compose_worker_spawn` so the SSH-remote adapter
        // (`SshHostAdapter::spawn_worker`) launches workers with a
        // byte-identical prompt; see that function for the per-execution
        // collaborator lookups (parent project, conflict / CI attempt,
        // crash-recovery branch, automation-triage preamble).
        let editorial_enabled = self.feature_flags.is_enabled("editorial_controls");
        let ComposedWorkerSpawn {
            prompt_text,
            spawn_config,
        } = compose_worker_spawn(
            &self.work_db,
            worker_id,
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            editorial_enabled,
            self.cfg.work.max_review_embed_diff_lines,
        )
        .await;

        let prompt_path = workspace_path.join(".claude").join("initial-prompt.txt");
        if let Some(parent) = prompt_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&prompt_path, &prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;

        // Structured-output artifact (review findings / task followups): create
        // the engine-owned scratch dir and clear any stale file from a prior
        // run of this exact execution id, then hand the worker its absolute
        // path via `BOSS_STRUCTURED_OUTPUT`. The same path is embedded in the
        // worker prompt (see `compose_worker_spawn`); the completion handler
        // reads + validates it. Best-effort: a prepare failure is non-fatal
        // (the worker falls back to the transcript-scrape contract).
        let structured_output_dir = crate::structured_output::default_dir();
        let structured_output_path = match crate::structured_output::prepare(&structured_output_dir, &execution.id) {
            Ok(path) => Some(path.display().to_string()),
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    dir = %structured_output_dir.display(),
                    ?err,
                    "spawn: could not prepare structured-output dir; worker will rely on \
                     the transcript-scrape fallback",
                );
                None
            }
        };

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
        let worker_settings_path = crate::worker_setup::worker_settings_path(workspace_path);
        // Re-prepend BOSS_BIN_DIR to PATH in the worker's first shell line,
        // mirroring the Boss/coordinator pane (see BossPaneModel.swift and
        // the feba26d2 fix). `spawn_flow` already sets PATH with
        // BOSS_BIN_DIR ahead of a sanitized PATH in the pane *surface*
        // env, but the worker pane runs a login shell whose init scripts
        // (.zprofile, .zshrc) rebuild PATH from /etc/paths and the user's
        // dotfiles — which re-prepends `~/bin`, where a `repobin` shim of
        // `cube` / `boss` / `bossctl` typically lives. That shim is
        // independently versioned and has drifted from the bundled CLI
        // (e.g. it lacks `cube pr create`), so a worker that resolves the
        // shim instead of the bundled binary silently breaks. BOSS_BIN_DIR
        // itself survives init (init scripts don't unset custom env vars),
        // so we re-prepend it here: this line runs *after* init completes
        // and *before* claude launches, so claude — and every tool-issued
        // `cube`/`boss` subshell it spawns — inherits the bundled-first
        // PATH. The `[ -n "$BOSS_BIN_DIR" ]` guard is a no-op in dev /
        // bazel-run mode where BOSS_BIN_DIR is unset.
        let initial_input = format!(
            "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; {}",
            crate::driver::ClaudeDriver.spawn_invocation(
                &spawn_config.model,
                spawn_config.claude_effort,
                Some(&worker_settings_path),
                spawner.non_opus_auto_mode(),
            ),
        );

        // Look up (or generate) a 2–4 word pane-titlebar summary for
        // this work item. The full run id is still used for logs and
        // every other identifier — this label is purely visual. We
        // resolve the API key lazily and let the helper handle every
        // failure mode (missing key, API error, cache miss) so a
        // slow or unreachable Anthropic never blocks the spawn.
        let api_key = self.cfg.agent().ok().and_then(|agent| agent.anthropic_api_key.clone());
        let title_summary = if execution.kind == ExecutionKind::CiRemediation {
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
                extra_env: structured_output_path
                    .map(|p| vec![(crate::structured_output::STRUCTURED_OUTPUT_ENV.to_owned(), p)])
                    .unwrap_or_default(),
                title_summary,
                task_title: Some(work_item_name(work_item).to_owned()),
                work_item_binding,
                model: spawn_config.model.clone(),
                draft_pr_mode: spawner.draft_pr_mode(),
                execution_kind: execution.kind.as_str().to_owned(),
                task_kind: work_item_task_kind(work_item).map(str::to_owned),
                // A triage worker's deliverable is a decision marker, not a
                // PR — it must NOT get the Standard "PR is the deliverable"
                // CLAUDE.md, which otherwise overrides the marker contract and
                // leaves runs ending without a decision marker. A reviewer is
                // read-only. Everything else is a Standard implementer.
                worker_kind: match execution.kind {
                    ExecutionKind::PrReview => WorkerKind::Reviewer,
                    ExecutionKind::AutomationTriage => WorkerKind::Triage,
                    _ => WorkerKind::Standard,
                },
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

        // Mid-spawn cancel reconciliation (T981). A cancel / force-stop
        // can land while we were awaiting the `SpawnWorkerPane`
        // round-trip: it marks the execution row `cancelled` but, with
        // no pid yet materialized, cannot reap the worker and
        // deliberately leaves the cube lease held (see
        // `WorkerCompletionHandler::force_release`). Now that the spawn
        // has returned — pid registered, slot mapped, live state stamped
        // — reap the just-spawned pane so it cannot outlive its
        // cancellation, and signal the coordinator to release the lease
        // the cancel path left for us. Without this the worker survives
        // unreaped in a workspace the engine believes is free.
        match self.work_db.get_execution(&execution.id) {
            Ok(exec) if exec.status == ExecutionStatus::Cancelled => {
                tracing::warn!(
                    worker_id,
                    execution_id = %execution.id,
                    slot_id = started.slot_id,
                    shell_pid = started.shell_pid,
                    "spawn completed after the execution was cancelled mid-spawn; reaping the worker pane and releasing the deferred lease",
                );
                spawner.reap_worker_pane(&execution.id).await;
                return Ok(RunOutcome {
                    wait_state: RunWaitState::CancelledDuringSpawn,
                    result_summary: Some(format!(
                        "Execution cancelled during spawn; reaped worker pane in slot {} (shell pid {}).",
                        started.slot_id, started.shell_pid,
                    )),
                    attention: None,
                    // The pane is already torn down — don't ask the
                    // coordinator to keep the pool slot claimed for it.
                    slot_id: None,
                    spawn_config: Some(spawn_config),
                });
            }
            Ok(_) => {}
            Err(err) => {
                // A read failure here is non-fatal: fall through to the
                // normal completion path. The worst case is the existing
                // pre-fix behaviour, not a regression.
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "post-spawn cancel re-check failed; proceeding with normal completion",
                );
            }
        }

        // A `pr_review` reviewer pane stays in `running` after spawn so that
        // the "AI reviewing" kanban badge remains visible while the reviewer
        // agent is actively working. `waiting_human` is only correct once the
        // review is done and a human must act; the execution transitions to
        // `completed` when the Stop hook fires and `finalize_pr_review_pass`
        // calls `record_worker_pr_completion`. All other execution kinds use
        // `WaitingHuman` — the normal post-spawn park state.
        let wait_state = if execution.kind == ExecutionKind::PrReview {
            RunWaitState::ReviewerPaneAlive
        } else {
            RunWaitState::WaitingHuman
        };
        Ok(RunOutcome {
            wait_state,
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

/// Composed worker prompt + resolved effort/model config, the output of
/// [`compose_worker_spawn`].
pub(crate) struct ComposedWorkerSpawn {
    pub prompt_text: String,
    pub spawn_config: SpawnConfig,
}

/// Fetch authoritative PR metadata for a reviewer worker's initial prompt.
///
/// Calls `gh pr view <pr_url> --json baseRefOid,headRefOid,files` and returns
/// a [`crate::pr_review::PrReviewContext`] on success. Returns `None` on any
/// network or parse error — callers fall back to the URL-only prompt
/// gracefully without blocking the spawn.
async fn fetch_pr_review_context(pr_url: &str) -> Option<crate::pr_review::PrReviewContext> {
    #[derive(serde::Deserialize)]
    struct PrViewResponse {
        #[serde(rename = "baseRefOid")]
        base_ref_oid: String,
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        files: Vec<PrFile>,
    }

    #[derive(serde::Deserialize)]
    struct PrFile {
        path: String,
    }

    let pr_number: u64 = pr_url.split('/').next_back()?.parse().ok()?;

    let output = crate::gh_invocation::gh_output(&["pr", "view", pr_url, "--json", "baseRefOid,headRefOid,files"])
        .await
        .ok()?;

    if !output.status.success() {
        tracing::warn!(
            pr_url,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "fetch_pr_review_context: gh pr view failed; reviewer will use URL-only prompt",
        );
        return None;
    }

    let response: PrViewResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| {
            tracing::warn!(
                pr_url,
                error = %e,
                "fetch_pr_review_context: failed to parse gh pr view JSON",
            );
            e
        })
        .ok()?;

    Some(crate::pr_review::PrReviewContext {
        pr_number,
        base_sha: response.base_ref_oid,
        head_sha: response.head_ref_oid,
        changed_files: response.files.into_iter().map(|f| f.path).collect(),
        diff_content: None,
    })
}

/// Fetch the raw diff for a PR via `gh pr diff <pr_url>`.
///
/// Returns the full diff text on success. Returns `None` on any error —
/// callers fall back gracefully (reviewer fetches the diff itself). The
/// caller is responsible for deciding whether the diff is small enough to
/// embed.
async fn fetch_pr_diff(pr_url: &str) -> Option<String> {
    let output = crate::gh_invocation::gh_output(&["pr", "diff", pr_url]).await.ok()?;

    if !output.status.success() {
        tracing::warn!(
            pr_url,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "fetch_pr_diff: gh pr diff failed; reviewer will fetch diff itself",
        );
        return None;
    }

    String::from_utf8(output.stdout)
        .map_err(|e| {
            tracing::warn!(
                pr_url,
                error = %e,
                "fetch_pr_diff: diff output is not valid UTF-8",
            );
            e
        })
        .ok()
}

/// Per-execution prompt + spawn-config composition shared by every
/// worker transport.
///
/// [`PaneSpawnRunner`] (local libghostty panes) and
/// [`crate::host_adapter::SshHostAdapter`] (remote SSH workers) both call
/// this so the two launch paths hand the worker a byte-identical prompt
/// and resolve the same effort/model knobs (design §Q3). It gathers the
/// per-execution collaborator context (parent project, merge-conflict /
/// CI-remediation attempt, crash-recovery branch, automation-triage
/// preamble), composes the prompt via [`compose_execution_prompt`], then
/// prepends the effort addendum and the product dispatch preamble exactly
/// as the local runner historically did.
///
/// Transport-agnostic: it reads only from `work_db` (and, for `pr_review`
/// executions, calls `gh pr view` to pre-fetch the PR metadata for the
/// reviewer's initial prompt).
pub(crate) async fn compose_worker_spawn(
    work_db: &WorkDb,
    worker_id: &str,
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    editorial_enabled: bool,
    max_embed_diff_lines: u64,
) -> ComposedWorkerSpawn {
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
            .and_then(|project_id| work_db.get_project(project_id).ok()),
        _ => None,
    };
    // For revision_implementation executions with a merge-conflict
    // provenance, look up the linked attempt by the id embedded in
    // created_via (format: "merge-conflict:<crz_id>") so
    // compose_revision_directive can inject the conflict fragment.
    let conflict_attempt = if execution.kind == ExecutionKind::RevisionImplementation {
        work_item_created_via(work_item)
            .and_then(|cv| cv.strip_prefix("merge-conflict:"))
            .and_then(|id| work_db.get_conflict_resolution(id).ok().flatten())
    } else {
        None
    };
    // Detect whether this is a respawn after a crash: if the work item has
    // no task-level pr_url (handled by the existing RESUME EXISTING PR path)
    // but has a prior orphaned execution with no pr_url, derive its expected
    // branch so the new worker can attempt to resume it.
    let recovery_branch: Option<String> = if work_item_pr_url(work_item).is_none() {
        match work_db.get_prior_orphaned_execution(&execution.work_item_id, &execution.id) {
            Ok(Some(prior)) => {
                let branch = crate::completion::expected_branch_name(
                    &prior.id,
                    &prior.branch_naming,
                    prior.worker_branch_prefix.as_deref(),
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

    // For ci_remediation executions (retrigger-kind only after Phase 5),
    // look up the active attempt so the prompt can show the failing checks.
    //
    // For revision_implementation executions with a ci-fix provenance,
    // look up the linked attempt by the id embedded in created_via
    // (format: "ci-fix:<crm_id>") so compose_revision_directive can
    // inject the CI remediation fragment.
    let ci_attempt = if execution.kind == ExecutionKind::CiRemediation {
        work_db
            .active_ci_remediation_for_work_item(&execution.work_item_id)
            .ok()
            .flatten()
    } else if execution.kind == ExecutionKind::RevisionImplementation {
        work_item_created_via(work_item)
            .and_then(|cv| cv.strip_prefix("ci-fix:"))
            .and_then(|id| work_db.get_ci_remediation(id).ok().flatten())
    } else {
        None
    };
    // Fetch the product before composing the prompt so we can pass
    // editorial_rules and the PR template set into compose_execution_prompt.
    let (
        product_editorial_rules,
        row_effort,
        row_model_override,
        product_default_model,
        product_dispatch_preamble,
        row_driver,
        product_default_driver,
    ) = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            let product = work_db.get_product(&task.product_id).ok().flatten();
            let editorial_rules = product.as_ref().and_then(|p| p.editorial_rules.clone());
            let product_default_model = product.as_ref().and_then(|p| p.default_model.clone());
            let product_default_driver = product.as_ref().and_then(|p| p.default_driver.clone());
            let dispatch_preamble = product.and_then(|p| p.dispatch_preamble).filter(|s| !s.is_empty());
            (
                editorial_rules,
                task.effort_level,
                task.model_override.clone(),
                product_default_model,
                dispatch_preamble,
                task.driver.clone(),
                product_default_driver,
            )
        }
        _ => (None, None, None, None, None, None, None),
    };
    // Load the PR template for editorial-rules prompt injection.
    let pr_template_product_id = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.product_id.as_str(),
        _ => "",
    };
    let pr_template_lease_id = execution.cube_lease_id.as_deref().unwrap_or("");
    let pr_template_set = crate::pr_template::load(pr_template_product_id, pr_template_lease_id, workspace_path);
    // Maint task 6: an `automation_triage` execution renders the triage
    // preamble (decision-marker contract + "do not do the work / do not
    // open a PR" guardrails) instead of the ordinary implementer prompt.
    // Its `work_item_id` is the automation id, so we read the automation
    // directly. If the automation vanished mid-flight, fall back to the
    // generic prompt so the worker at least has workspace context.
    //
    // P992 task 6: a `pr_review` execution renders the reviewer prompt
    // instead of the ordinary implementer prompt. Its `work_item_id` is
    // the producing task id, so we read the task to get the PR context.
    // If the task or its pr_url cannot be resolved, fall back to the
    // generic prompt (reviewer still gets workspace context but a weaker
    // framing — better than no spawn at all).
    let prompt_text = if execution.kind == ExecutionKind::AutomationTriage {
        match work_db.get_automation(&execution.work_item_id) {
            Ok(Some(automation)) => {
                let product_name = work_db
                    .get_product(&automation.product_id)
                    .ok()
                    .flatten()
                    .map(|p| p.name)
                    .unwrap_or_else(|| automation.product_id.clone());
                crate::automation_triage::render_triage_preamble(&automation, &product_name)
            }
            other => {
                tracing::warn!(
                    execution_id = %execution.id,
                    automation_id = %execution.work_item_id,
                    resolved = ?other.as_ref().map(|o| o.is_some()),
                    "automation_triage execution could not resolve its automation; \
                     falling back to generic prompt",
                );
                compose_execution_prompt(
                    ExecutionPromptParams::builder()
                        .execution(execution)
                        .work_item(work_item)
                        .workspace_path(workspace_path)
                        .maybe_parent_project(parent_project.as_ref())
                        .maybe_cube_change_id(cube_change_id)
                        .maybe_conflict_attempt(conflict_attempt.as_ref())
                        .maybe_recovery_branch(recovery_branch.as_deref())
                        .maybe_ci_attempt(ci_attempt.as_ref())
                        .maybe_editorial_rules(product_editorial_rules.as_ref())
                        .pr_template_set(&pr_template_set)
                        .editorial_enabled(editorial_enabled)
                        .build(),
                )
            }
        }
    } else if execution.kind == ExecutionKind::PrReview {
        let task_name = work_item_name(work_item);
        let task_description = match work_item {
            WorkItem::Task(task) | WorkItem::Chore(task) => task.description.as_str(),
            _ => "",
        };
        let pr_url = work_item_pr_url(work_item).unwrap_or_default();
        if pr_url.is_empty() {
            tracing::warn!(
                execution_id = %execution.id,
                work_item_id = %execution.work_item_id,
                "pr_review execution: producing task has no pr_url; \
                 falling back to generic prompt — review will lack PR context",
            );
            compose_execution_prompt(
                ExecutionPromptParams::builder()
                    .execution(execution)
                    .work_item(work_item)
                    .workspace_path(workspace_path)
                    .maybe_parent_project(parent_project.as_ref())
                    .maybe_cube_change_id(cube_change_id)
                    .maybe_conflict_attempt(conflict_attempt.as_ref())
                    .maybe_recovery_branch(recovery_branch.as_deref())
                    .maybe_ci_attempt(ci_attempt.as_ref())
                    .maybe_editorial_rules(product_editorial_rules.as_ref())
                    .pr_template_set(&pr_template_set)
                    .editorial_enabled(editorial_enabled)
                    .build(),
            )
        } else {
            // Pre-fetch PR metadata so the reviewer starts with the full diff
            // context (base/head SHAs, changed files) rather than discovering
            // it turn-by-turn. Fail open on error — the URL-only prompt is
            // still functional.
            let mut pr_review_context = fetch_pr_review_context(pr_url).await;
            if let Some(ref ctx) = pr_review_context {
                tracing::info!(
                    execution_id = %execution.id,
                    pr_url,
                    pr_number = ctx.pr_number,
                    head_sha = %ctx.head_sha,
                    changed_files = ctx.changed_files.len(),
                    "pr_review execution: pre-fetched PR metadata for reviewer context",
                );
            } else {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    "pr_review execution: PR metadata fetch failed; reviewer will use URL-only prompt",
                );
            }
            // When the diff is small enough, pre-fetch it and embed it
            // directly in the reviewer's initial prompt so the reviewer
            // skips one `gh pr diff` tool call. Disabled when
            // max_embed_diff_lines is 0.
            if max_embed_diff_lines > 0
                && let Some(ref mut ctx) = pr_review_context
                && let Some(diff) = fetch_pr_diff(pr_url).await
            {
                let line_count = diff.lines().count() as u64;
                if line_count <= max_embed_diff_lines {
                    tracing::info!(
                        execution_id = %execution.id,
                        pr_url,
                        line_count,
                        max_embed_diff_lines,
                        "pr_review execution: embedding diff in reviewer prompt",
                    );
                    ctx.diff_content = Some(diff);
                } else {
                    tracing::debug!(
                        execution_id = %execution.id,
                        pr_url,
                        line_count,
                        max_embed_diff_lines,
                        "pr_review execution: diff too large to embed; \
                         reviewer will fetch it",
                    );
                }
            }
            // Use the changed-file list (when available) to classify the review
            // scope accurately, instead of always defaulting to Code.
            let scope = match &pr_review_context {
                Some(ctx) => {
                    let files: Vec<&str> = ctx.changed_files.iter().map(String::as_str).collect();
                    crate::pr_review::classify_changed_files(&files)
                }
                None => crate::pr_review::ReviewScope::Code,
            };
            let reviewer_repo_slug = crate::completion::parse_repo_slug(&execution.repo_remote_url)
                .unwrap_or_else(|_| "<owner/repo>".to_owned());
            crate::pr_review::render_reviewer_initial_prompt(
                task_name,
                task_description,
                pr_url,
                &crate::structured_output::default_path_string(&execution.id),
                scope,
                pr_review_context.as_ref(),
                &reviewer_repo_slug,
            )
        }
    } else {
        compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(execution)
                .work_item(work_item)
                .workspace_path(workspace_path)
                .maybe_parent_project(parent_project.as_ref())
                .maybe_cube_change_id(cube_change_id)
                .maybe_conflict_attempt(conflict_attempt.as_ref())
                .maybe_recovery_branch(recovery_branch.as_deref())
                .maybe_ci_attempt(ci_attempt.as_ref())
                .maybe_editorial_rules(product_editorial_rules.as_ref())
                .pr_template_set(&pr_template_set)
                .editorial_enabled(editorial_enabled)
                .build(),
        )
    };
    let spawn_config = resolve_spawn_config(
        row_effort,
        row_model_override.as_deref(),
        pool_model_override_for_worker_id(worker_id),
        product_default_model.as_deref(),
        row_driver.as_deref(),
        product_default_driver.as_deref(),
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
            format!(
                "[product-preamble]\n{}\n[/product-preamble]\n\n{}",
                preamble, prompt_text
            )
        }
        None => prompt_text,
    };

    ComposedWorkerSpawn {
        prompt_text,
        spawn_config,
    }
}

#[derive(bon::Builder)]
struct ExecutionPromptParams<'a> {
    execution: &'a WorkExecution,
    work_item: &'a WorkItem,
    workspace_path: &'a Path,
    parent_project: Option<&'a Project>,
    cube_change_id: Option<&'a str>,
    conflict_attempt: Option<&'a ConflictResolution>,
    recovery_branch: Option<&'a str>,
    ci_attempt: Option<&'a CiRemediation>,
    editorial_rules: Option<&'a EditorialRules>,
    pr_template_set: &'a crate::pr_template::PrTemplateSet,
    #[builder(default)]
    editorial_enabled: bool,
}

fn compose_execution_prompt(params: ExecutionPromptParams<'_>) -> String {
    let ExecutionPromptParams {
        execution,
        work_item,
        parent_project,
        workspace_path,
        cube_change_id,
        conflict_attempt,
        recovery_branch,
        ci_attempt,
        editorial_rules,
        pr_template_set,
        editorial_enabled,
    } = params;
    // Phase 9 #29: ci_remediation has its own templated prompt — embed
    // the engine-collected log excerpt, the failing-check set, and the
    // attempt-kind-specific playbook (rebase-first for `fix`, just the
    // retrigger CLI for `retrigger`).
    if execution.kind == ExecutionKind::CiRemediation
        && let Some(attempt) = ci_attempt
    {
        return compose_ci_remediation_prompt(
            execution,
            work_item,
            workspace_path,
            cube_change_id,
            attempt,
            /* test_command */ None,
        );
    }
    let mut prompt = String::new();
    prompt.push_str("You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n");
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");

    // If the chore already has a PR, inject a high-prominence resume
    // directive BEFORE the execution context so it outweighs the
    // workspace-rules default of `jj git fetch && jj new main`.
    let existing_pr_url = work_item_pr_url(work_item);
    if let Some(pr_url) = existing_pr_url {
        let pr_number = extract_pr_number(pr_url)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".into());
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
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
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
             yet), fall back to `jj new main@origin` instead.\n\
             \n\
             If you successfully resumed the prior branch, continue from those commits and \
             push using the new expected branch name `{expected_branch_new}` (see the \
             `expected branch name` line in the execution context below). Do NOT reuse the \
             prior branch name.\n\n",
        ));
    }

    let expected_branch = crate::completion::expected_branch_name(
        &execution.id,
        &execution.branch_naming,
        execution.worker_branch_prefix.as_deref(),
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
    if existing_pr_url.is_none() && execution.kind != ExecutionKind::RevisionImplementation {
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
    // Inject [editorial-rules] block when editorial controls are enabled (gated by
    // the `editorial_controls` feature flag — default OFF). When disabled the block
    // is omitted entirely so the worker gets no editorial instructions and the
    // PreToolUse hook is a no-op (nothing downstream enforces).
    if editorial_enabled {
        prompt.push_str(&render_editorial_rules_block(editorial_rules, pr_template_set));
        prompt.push('\n');
    }
    match execution.kind {
        ExecutionKind::ProjectDesign => {
            prompt.push_str(&compose_design_directive(parent_project));
        }
        ExecutionKind::InvestigationImplementation => {
            prompt.push_str(&compose_investigation_directive());
        }
        ExecutionKind::RevisionImplementation => {
            prompt.push_str(&compose_revision_directive(
                execution,
                work_item,
                workspace_path,
                conflict_attempt,
                ci_attempt,
            ));
        }
        ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation => {
            prompt.push_str(
                "Expected outcome for this run:\n- implement the requested change in the workspace,\n- run relevant local validation when practical,\n- stop once the work is ready for a human to review or redirect.\n",
            );
            prompt.push_str(check_bypass_prohibition_text());
        }
        ExecutionKind::AutomationTriage
        | ExecutionKind::CiRemediation
        | ExecutionKind::ConflictResolution
        | ExecutionKind::PrReview
        | ExecutionKind::ProductDesign => {
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
        execution.kind,
        ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
    ) && let Some(gate) = bazel_prepush_gate_block(workspace_path)
    {
        prompt.push_str(&gate);
    }
    if matches!(
        execution.kind,
        ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::ProjectDesign
            | ExecutionKind::InvestigationImplementation
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
            let pr_number = extract_pr_number(pr_url)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
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
                 - Alternatively, use `cube pr create --branch {expected_branch}` which pushes the branch and opens the PR in one step (jj-aware, no GIT_DIR needed). It errors if a PR already exists — use `cube pr update --branch {expected_branch}` in that case.\n\
                 - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
                 - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n\
                 - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty, you have made no changes — do NOT commit, push, or open a PR. Stop and explain what went wrong instead.\n",
            ));
        }
        // Warn that PR creation is terminal — the engine reaps the worker
        // immediately after the PR is opened. Workers must finish everything
        // BEFORE opening the PR; no followup turn is possible.
        prompt.push_str(&pr_terminal_directive());
        // Issue #899: hand the worker the engine's CI-completion definition
        // so it stops once CI is effectively green rather than polling
        // forever on human-gated checks (e.g. LinkedIn's `Owner Approval`).
        prompt.push_str(&ci_monitoring_directive(execution));
        // T1868: give a fresh-PR chore/task implementation worker a SANCTIONED
        // way to terminate as "the work was already done". Without it, a worker
        // that correctly finds an empty diff stops and explains — and the
        // engine's Stop-boundary handler then nudges it to "produce a PR"
        // forever. Only for the no-existing-PR flow: when a PR already exists,
        // an empty diff means "already pushed", handled by the push-to-existing
        // path, not by closing the task as a no-op.
        if existing_pr_url.is_none()
            && matches!(
                execution.kind,
                ExecutionKind::TaskImplementation | ExecutionKind::ChoreImplementation
            )
        {
            prompt.push_str(&no_op_completion_directive());
        }
    }
    // Attentions creation pipeline (design: attentions.md): implementation
    // workers may surface out-of-scope follow-on work as a `FOLLOWUPS:` block
    // the engine parses at completion. Design workers use the questions
    // manifest instead, so they are excluded here.
    if matches!(
        execution.kind,
        ExecutionKind::TaskImplementation
            | ExecutionKind::ChoreImplementation
            | ExecutionKind::InvestigationImplementation
            | ExecutionKind::RevisionImplementation
    ) {
        prompt.push_str(&followups_emission_block(
            &crate::structured_output::default_path_string(&execution.id),
        ));
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
    Some(bazel_prepush_gate_text())
}

/// The Bazel pre-push build-gate prompt block, independent of any
/// filesystem probe. Extracted so the SSH remote adapter can append it
/// when a *remote* workspace is a Bazel workspace: [`is_bazel_workspace`]
/// only probes the local filesystem, so a remote workspace path never
/// matches and the gate has to be injected from the result of an
/// over-SSH marker probe instead.
pub(crate) fn bazel_prepush_gate_text() -> String {
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
         Run the build gate in the FOREGROUND and read its exit code directly. Do NOT background bazel (no `&`, no `run_in_background`, no redirecting to a log file you then poll) and then idle in a self-paced wait-loop \"until the gate is green\". If the bazel server wedges (host contention, a hung toolchain), those log files may never appear and the completion notification never arrives — you will wait forever with no way out, stranding your slot. If you need an upper bound, wrap the command itself in a timeout (e.g. `timeout 1800 bazel test //...`) so it returns control to you on expiry; on a timeout, treat it as a blocker (below), do not retry-and-idle.\n\
         \n\
         If the build or tests fail, time out, or you cannot make them pass within this run, do NOT push red code and do NOT idle waiting on them. Emit an `[effort-escalation]` marker in your final response with the failing/timed-out command and its output, and stop. Escalating a blocker is correct; pushing a known-broken branch — or hanging on a wedged build — is not.\n"
        .to_string()
}

/// Pre-push gate for a **conflict-resolution** revision, when the
/// workspace is a Bazel workspace. Returns `None` for non-Bazel repos.
///
/// This deliberately differs from [`bazel_prepush_gate_block`]: a
/// conflict-resolution revision's job is to make the PR mergeable again
/// (the *merge-correctness* gate), not to certify the whole PR's test
/// suite. The full `bazel test //...` belongs to the PR's own CI, which
/// runs on the branch the worker pushes. Blocking the push behind a long
/// or flaky full-suite run is exactly how a correct resolution gets
/// stranded unpushed and lost on reap (the loop this fix addresses).
///
/// The verify gate is NOT skipped: the merged code must COMPILE
/// (`bazel build` of the touched/upstream targets) and any rebase-
/// invalidated generated artifact (e.g. `MODULE.bazel.lock`) must be
/// regenerated before pushing. Tests run post-push in CI.
fn bazel_conflict_resolution_gate_block(workspace_path: &Path) -> Option<String> {
    if !is_bazel_workspace(workspace_path) {
        return None;
    }
    Some(bazel_conflict_resolution_gate_text())
}

/// The conflict-resolution pre-push gate prompt block, independent of any
/// filesystem probe (so the SSH remote adapter can inject it after an
/// over-SSH marker probe). See [`bazel_conflict_resolution_gate_block`]
/// for why this is build-before-push rather than build-and-test-before-push.
pub(crate) fn bazel_conflict_resolution_gate_text() -> String {
    "\n## Pre-push gate for conflict resolution (Bazel workspace) — merge correctness first, then push\n\
         \n\
         This repository is a Bazel workspace. For a conflict-resolution revision the gate you MUST clear before pushing is **merge correctness**, not the full test suite.\n\
         \n\
         Required BEFORE you push (step 4):\n\
         - Regenerate any generated/lock artifact the rebase invalidated and include it in your commit. The common one is `MODULE.bazel.lock`: run `bazel mod deps --lockfile_mode=update` (or build any target, which refreshes it) and stage the result.\n\
         - `bazel build` the targets your resolution touched AND the targets the rebased-in upstream change touches. Use `bazel query` to resolve labels if unsure. The merged code MUST COMPILE — a conflict resolution that does not build is wrong and must not be pushed.\n\
         - Run the build in the FOREGROUND with a timeout (e.g. `timeout 1800 bazel build <targets>`) and read its exit code directly. Do NOT background it and idle in a wait-loop.\n\
         \n\
         Then PUSH (step 4) as soon as the build is clean. Do NOT block the push on a full `bazel test //...`.\n\
         \n\
         Why push before the full test suite: making the PR mergeable again is the conflict-resolution step's deliverable. The PR's own CI runs the full `bazel test` suite on the branch you push — that is where test regressions are caught and remediated, NOT a precondition for landing the resolution. Stalling the push behind a long or flaky full-suite run is exactly how a correct resolution gets stranded and never reaches the PR.\n\
         \n\
         After pushing you MAY run `bazel test` on the affected targets as a courtesy and report what you saw, but the push must not wait on it.\n\
         \n\
         If `bazel build` fails (the merge does not compile) and you cannot make it compile within this run, do NOT push. Fix the resolution, or — if it needs a human decision — follow the stop conditions below. Do NOT idle waiting on a wedged build; emit an `[effort-escalation]` marker and stop.\n"
        .to_string()
}

/// Hard constraint text forbidding check/CI bypasses. Injected into every
/// prompt surface where a worker might encounter a failing check or CI failure.
fn check_bypass_prohibition_text() -> &'static str {
    "\n**Hard constraint — fix failing checks at the root cause; never bypass them.**\n\n\
     Forbidden moves (each is a bypass, not a fix — do NOT do any of them):\n\
     - Adding a file to a check exclusion or allowlist (`CHECKS.yaml` `exclude_files`, checkleft excludes, lint-disable comments, etc.) to suppress the failure.\n\
     - Setting `allow_bypass`, using an override flag, or invoking any bypass/override mechanism on a check.\n\
     - Passing `--no-verify` / skipping git hooks; adding broad `#[allow(...)]` / `// swiftlint:disable` / `# noqa` annotations solely to suppress a warning or error.\n\
     - Deleting, `#[ignore]`-ing, `xfail`-ing, skipping, or weakening assertions in a failing test to make it pass.\n\
     - Raising a threshold or limit (e.g. `max_lines` in a file-size check) solely to accommodate the offending file without reducing its size.\n\n\
     Required behavior: fix the real problem — split the oversized file, fix the lint/compile error, fix the test failure, resolve the root cause. If a check genuinely SHOULD be relaxed (a legitimately needed exclusion or threshold change), that is a human decision — STOP and surface it for operator approval with full justification. Do not decide this autonomously.\n"
}

/// Render the `[editorial-rules]` block for the worker prompt (chore #5).
///
/// Always rendered — even for default-config products — because the baked-in
/// identifier-redaction rules apply to every execution. The optional
/// instructions / template / enforcement sections are only included when the
/// product has non-default editorial configuration (instructions set or
/// template_policy != Off). This matches the acceptance criterion: default-config
/// products get baked-in rules only; configured products get instructions +
/// template + enforcement banner.
fn render_editorial_rules_block(
    editorial_rules: Option<&EditorialRules>,
    pr_template_set: &crate::pr_template::PrTemplateSet,
) -> String {
    let instructions = editorial_rules
        .and_then(|r| r.instructions.as_deref())
        .filter(|s| !s.is_empty());
    let template_policy = editorial_rules.map(|r| r.template_policy.clone()).unwrap_or_default();
    let is_configured = instructions.is_some() || !matches!(template_policy, TemplatePolicy::Off);

    let mut out = String::new();
    out.push_str("[editorial-rules]\n");
    out.push_str("**Editorial rules for PRs / GitHub comments in this product.**\n");
    out.push_str(
        "Apply these rules to every PR title, PR body, PR / issue comment, \
         commit-message body, and merge-conflict note you write for this run.\n\n",
    );
    out.push_str("Baked-in rules (always apply):\n");
    out.push_str(
        "- Do not mention Boss execution / project / task / chore identifiers \
         in user-facing text. The shapes are `exec_…`, `proj_…`, `task_…`, \
         `chg_…`. They are internal vocabulary that humans on this repo have no \
         context for.\n",
    );
    out.push_str(
        "- Do not refer to \"Boss worker\", \"the engine\", \"the coordinator\", \
         \"cube workspace\", or \"work item\" in user-facing text — these are \
         internal Boss vocabulary.\n",
    );
    out.push_str(
        "- When referring to your branch in PR text, say \"this branch\" rather \
         than its full name — the branch name is engine bookkeeping (it associates \
         the PR with its originating execution) and is not meaningful to human \
         reviewers.\n",
    );

    if is_configured {
        if let Some(instr) = instructions {
            out.push_str("\nProduct-specific rules (configured on this product):\n");
            out.push_str(instr.trim_end());
            out.push('\n');
        }

        let policy_label = match template_policy {
            TemplatePolicy::Off => None,
            TemplatePolicy::Advise => Some("Advise"),
            TemplatePolicy::Enforce => Some("Enforce"),
        };
        if let Some(label) = policy_label {
            let template_path = pr_template_set
                .default_template
                .as_ref()
                .map(|t| t.source_path.display().to_string())
                .or_else(|| {
                    let mut stems: Vec<&str> = pr_template_set.named_templates.keys().map(String::as_str).collect();
                    stems.sort();
                    stems
                        .first()
                        .map(|stem| format!(".github/PULL_REQUEST_TEMPLATE/{stem}.md"))
                })
                .unwrap_or_else(|| ".github/PULL_REQUEST_TEMPLATE.md".to_string());
            out.push_str(&format!("\nTemplate policy: {label}: see {template_path}\n"));
            if !pr_template_set.is_empty() {
                out.push_str(
                    "The PR body must follow the structure of the template (rendered below), \
                     regardless of the final-response sectioning rules.\n",
                );
                let has_multiple = pr_template_set.named_templates.len() > 1
                    || (pr_template_set.default_template.is_some() && !pr_template_set.named_templates.is_empty());
                for tmpl in pr_template_set.all_templates() {
                    if has_multiple {
                        out.push_str(&format!("\nTemplate (`{}`):\n", tmpl.source_path.display()));
                    }
                    out.push_str("\n```\n");
                    out.push_str(tmpl.text.trim_end());
                    out.push_str("\n```\n");
                }
            }
        }

        out.push_str("\nEnforcement:\n");
        out.push_str(
            "The engine's PreToolUse hook intercepts `gh pr create`, `gh pr edit`, \
             `gh pr comment`, `gh pr review`, and `gh issue comment` invocations. \
             If your body / title violates a rule, the call is denied or rewritten and \
             you will see feedback. Comply on the first try when you can — denials cost \
             a worker turn.\n",
        );
    }

    out.push_str("[/editorial-rules]\n");
    out
}

/// Directive that warns workers PR creation is terminal: the engine reaps
/// them immediately after the PR is opened. No followup turn is possible.
/// Workers must finish all work — including consuming any in-flight reviews
/// they started — BEFORE opening the PR. Incident: a worker opened a PR,
/// then tried to wait for background review subagents and address their
/// findings as followup commits. The engine terminated the worker the moment
/// the PR was created, so the review was never consumed. This universal
/// guidance applies to every execution kind and prevents that pattern.
fn pr_terminal_directive() -> String {
    let mut out = String::new();
    out.push_str("\n## Important: PR creation is your terminal act\n\n");
    out.push_str(
        "Opening the PR is the LAST thing you do. The engine reaps you immediately after the PR is created.\n\n",
    );
    out.push_str("You will NOT get another turn after `gh pr create` / `cube pr create` (or `cube pr update` for an existing PR). Do not plan followup commits, do not defer work to \"after the PR\", do not open the PR while background work (subagent workflows, backgrounded builds, code reviews) is still in flight expecting to consume its results.\n\n");
    out.push_str("Therefore: finish everything — including consuming any review/self-review findings you started — BEFORE you open the PR. If a background review is still running and you care about its results, wait for it and address all findings FIRST, then open the PR. If you don't intend to wait, don't start the review.\n");
    out
}

/// Sanctioned no-op completion directive (T1868). A `chore_implementation`
/// / `task_implementation` worker sometimes investigates and finds the work
/// is *already done* — the change is already on `main`, so `jj diff -r @` is
/// empty and there is nothing to commit/push/open a PR for. That is a
/// legitimate success, not a failure. Before this directive the worker was
/// told only to "stop and explain", and the engine's Stop-boundary handler
/// then read the empty branch as "stopped without producing a PR" and nudged
/// it to `gh pr create` — the two instructions were in direct conflict and
/// the worker churned against the nudge until the breaker parked it.
///
/// This block reframes the already-done empty-diff case as a success and
/// gives the worker an unambiguous terminal signal: emit the
/// [`NO_CHANGES_NEEDED`](crate::no_op_signal::NO_CHANGES_NEEDED_MARKER) marker
/// on its own line and stop. The engine accepts that marker (combined with a
/// genuinely empty contribution — no PR pushed, none bound) as a clean
/// terminal and closes the task as done WITHOUT a PR, sending no nudge. The
/// marker is the *only* sanctioned way to signal this; a worker that simply
/// stops without it is still nudged, so this must NOT be used to bail out of
/// work that is merely hard or blocked.
fn no_op_completion_directive() -> String {
    let marker = crate::no_op_signal::NO_CHANGES_NEEDED_MARKER;
    let mut out = String::new();
    out.push_str("\n## If the work is already done: signal a sanctioned no-op\n\n");
    out.push_str(
        "Run `jj diff -r @` before you conclude. If the diff is empty because the work is ALREADY \
         DONE — the change is already present on `main` (e.g. another PR landed it), and there is \
         genuinely nothing left to change — that is a legitimate, SUCCESSFUL outcome, not a \
         failure.\n\n",
    );
    out.push_str(&format!(
        "In that case, do NOT commit, push, or open a PR, and do NOT push an empty/no-op PR to \
         manufacture a deliverable. Instead, emit a line containing exactly `{marker}` as the \
         final line of your response, then stop. The engine recognizes this marker and closes the \
         task as already-done — no PR is required and you will not be nudged to produce one.\n\n"
    ));
    out.push_str(&format!(
        "This replaces the generic \"stop and explain what went wrong\" for the already-done case: \
         an empty diff because the work is done is a success terminal, not an error. Do NOT emit \
         `{marker}` to abandon work you simply found hard or are blocked on — if you are blocked, \
         say what you need instead, and the engine will help you proceed.\n"
    ));
    out
}

/// Post-PR CI-monitoring directive (issue #899). A worker that opens a
/// PR and then sits in a `gh pr checks` poll-loop "until every check is
/// green" never completes under CI models where some required checks are
/// gated on a human action and never auto-resolve — LinkedIn's
/// `Owner Approval` is the canonical case. The engine's merge poller
/// already classifies CI correctly for these orgs: it partitions the
/// human-gated checks out of the CI rollup
/// (`merge_poller::review_signal_checks_for_owner`) before deciding the
/// PR is "effectively green", and auto-transitions the task to Review.
/// The worker had no share of that knowledge and so polled forever.
///
/// This block hands the worker the *same* CI-completion definition the
/// engine uses, sourced from the *same* table — when the PR's org ships
/// human-gated checks, they are named verbatim from
/// `review_signal_checks_for_owner` so the worker's "don't wait on these"
/// list and the engine's "these don't block CI-clean" set cannot drift.
fn ci_monitoring_directive(execution: &WorkExecution) -> String {
    let mut out = String::new();
    out.push_str("\n## After the PR is open: do not babysit CI\n\n");
    out.push_str(
        "Once your branch is pushed and the PR exists, your deliverable is done — print the PR URL and stop. Do NOT sit in a loop polling `gh pr checks` / `gh pr view` waiting for every check to turn green. That loop can run forever and strands your slot.\n\n",
    );
    out.push_str(
        "Why this is safe: the engine polls this PR's CI on its own cadence and auto-transitions the task to Review the moment CI is *effectively green*. \"Effectively green\" matches the engine's own definition — every required CI check has reached a passing terminal state (`SUCCESS`, `NEUTRAL`, or `SKIPPED`). It deliberately does NOT require checks that are gated on a human action and never resolve from CI alone; waiting on those is waiting forever.\n\n",
    );
    // Name the human-gated checks for this PR's org from the *same* table
    // the engine's CI classifier reclassifies on, so the two lists are
    // sourced once. Empty for orgs without review-signal rules — then the
    // general guidance above stands on its own.
    if let Ok(slug) = crate::completion::parse_repo_slug(&execution.repo_remote_url) {
        let owner = slug.split('/').next().unwrap_or("");
        let names = crate::merge_poller::review_signal_checks_for_owner(owner);
        if !names.is_empty() {
            let rendered = names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ");
            out.push_str(&format!(
                "This PR's org (`{owner}`) ships required check(s) that are human-gated and never auto-resolve from CI: {rendered}. The engine's CI-completion check treats them as NOT blocking — they stay pending until a human approves. You must do the same: their pending/running state is not a reason to keep this run alive.\n\n",
            ));
        }
    }
    out.push_str(
        "A required CI check that has genuinely *failed* (not merely pending) is different — fix it and push, or escalate per the build-gate rules above. But a still-running or human-gated check never blocks your completion.\n",
    );
    out
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
    out.push_str("  - **Proposed implementation task breakdown** — this section is **required** and must be the final section of the doc. It is the machine-findable handoff to scheduling (see below).\n");
    out.push_str("- the **Proposed implementation task breakdown** section must:\n");
    out.push_str("  - use exactly that heading (`## Proposed implementation task breakdown`) so a downstream parser can locate it reliably.\n");
    out.push_str("  - list PR-sized tasks in dependency order, where each entry contains:\n");
    out.push_str("    - a short **task name** (one line).\n");
    out.push_str("    - a one-paragraph **scope** description.\n");
    out.push_str("    - an **effort hint**: one of `trivial | small | medium | large`.\n");
    out.push_str("    - **explicit dependencies** — which other entries in this list gate this one (use the task names; \"none\" if it can start immediately).\n");
    out.push_str("  - note which tasks at the same dependency depth may run in parallel, so the task graph (not just a linear list) is expressible.\n");
    out.push_str("  - include items that are deferred or explicitly out of scope, marked as `future / not a v1 blocker` rather than silently omitting them — silent omissions force the coordinator to guess what was considered and rejected.\n");
    out.push_str("  - This section is what P783's auto-populate will consume to materialise dependent tasks with edges, so completeness matters.\n");
    out.push_str(&design_questions_manifest_block());
    out.push_str("- when the doc is ready for review, push it and open a PR (see the acceptance criterion below). Do not start implementation tasks — those come from follow-up work items the human files after the design is approved.\n");
    out
}

/// Attentions question-manifest emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline"). Appended
/// to the `project_design` directive: a design worker that has genuine open
/// questions for the human emits a sibling `<slug>.attentions.json` manifest
/// next to the doc. The engine's `DesignDetector` parses it off the PR
/// branch and upserts an inline question group the human answers in the doc
/// viewer, batched into a single revision.
fn design_questions_manifest_block() -> String {
    let mut out = String::new();
    out.push_str("- OPTIONAL — open questions for the human: if, while writing the doc, you have specific decisions you want a human to make (yes/no calls, multiple-choice forks, or free-text prompts), emit a **questions manifest** as a sibling file next to the design doc — the same path with the `.md` extension replaced by `.attentions.json` (e.g. `…/designs/<slug>.attentions.json`).\n");
    out.push_str("  - The file is a JSON array. Each entry is an object:\n");
    out.push_str("    - `question_type` (required): one of `yes_no` | `multiple_choice` | `prompt` (free text).\n");
    out.push_str("    - `prompt` (required): the question shown to the human.\n");
    out.push_str("    - `choices` (required only for `multiple_choice`): a JSON array of option strings.\n");
    out.push_str("    - `anchor` (optional but encouraged): the heading slug the question is about, so it renders next to the relevant section.\n");
    out.push_str("  - Example: `[{\"question_type\":\"yes_no\",\"prompt\":\"Gate extraction behind a flag?\",\"anchor\":\"rollout\"},{\"question_type\":\"multiple_choice\",\"prompt\":\"One table or two?\",\"choices\":[\"one\",\"two\"],\"anchor\":\"data-model\"}]`\n");
    out.push_str("  - Only emit this when you genuinely need the human to decide something; omit the file entirely otherwise. Do NOT restate the doc's \"Risks / open questions\" prose here — the manifest is just the machine-actionable subset you want answered. The engine batches all entries into one group, so answering them yields a single doc revision.\n");
    out
}

/// Followups emission instruction (design:
/// `tools/boss/docs/designs/attentions.md`, "Creation pipeline"). Appended to
/// the implementation-worker directive: a worker that notices concrete,
/// out-of-scope follow-on work near task completion **writes** it as a JSON
/// array to the engine-owned artifact at `output_path` (see
/// [`crate::structured_output`]). The engine reads + schema-validates that
/// file at completion and upserts a followup group keyed to this task; the
/// human turns accepted entries into tasks with one gesture. A `FOLLOWUPS:`
/// fenced-JSON sentinel in the final message is kept as a transitional
/// fallback (and to keep remote workers working until the artifact is fetched
/// cross-host).
fn followups_emission_block(output_path: &str) -> String {
    let mut out = String::new();
    out.push_str("\n## Optional: surface follow-on work as followups\n\n");
    out.push_str(
        "If, while completing this task, you noticed concrete follow-on work worth filing — a separate bug, a needed refactor, a missing test, a docs gap — that is OUT OF SCOPE for this PR, you may surface it for the human. This is OPTIONAL: only include genuine, actionable proposals, never invent work to fill it, and never list the change you just made.\n\n",
    );
    out.push_str(&format!(
        "If (and only if) you have followups, **write** a JSON array of them with the `Write` tool to this exact file (also exported as `$BOSS_STRUCTURED_OUTPUT`):\n\n`{output_path}`\n\nThis path is outside the repo/workspace, so the manifest never pollutes your PR. Each array element is an object:\n",
    ));
    out.push_str("- `proposed_name` (required): a short task title.\n");
    out.push_str("- `proposed_description` (required): one paragraph of scope.\n");
    out.push_str("- `proposed_effort` (optional): one of `trivial` | `small` | `medium` | `large` | `max`.\n");
    out.push_str("- `proposed_work_kind` (optional): one of `task` | `chore` | `project` (defaults to `task`).\n");
    out.push_str("- `rationale` (optional): why it is worth doing.\n\n");
    out.push_str("File contents example:\n\n```json\n[{\"proposed_name\": \"Add retry/backoff to the X client\", \"proposed_description\": \"The X client fails hard on transient 5xx; add bounded retry with jitter.\", \"proposed_effort\": \"small\", \"proposed_work_kind\": \"task\", \"rationale\": \"Observed flakes during this task.\"}]\n```\n\n");
    out.push_str("Do NOT write the file at all if you have no followups — an absent file means \"no followups\", which is the normal case. Writing it does not block this PR — it just files proposals for the human to review.\n\n");
    out.push_str("As a fallback only (e.g. if the file write is unavailable), you may instead append — after your `## Open Questions` section — a line containing exactly `FOLLOWUPS:` immediately followed by a fenced ```json code block holding the same JSON array.\n");
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
///
/// The kanban doc affordance is derived from the task's `pr_url`, which
/// the engine auto-detects when the worker opens the PR — exactly like a
/// design task. The worker does NOT register any doc pointer; opening the
/// PR is the whole job.
fn compose_investigation_directive() -> String {
    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- the deliverable is a **markdown document**, not code. Do not edit source code, build files, or anything other than the investigation doc.\n");
    out.push_str("- the PR for this run contains **only the markdown doc** (one new file). If you find yourself touching `.rs`, `.ts`, `.swift`, build files, or anything else, stop — you are out of scope.\n");
    out.push_str("- choose a filename that reflects the topic (e.g. `docs/investigations/my-topic.md`). Use an `investigations/` subdirectory if one exists in the repo, or create it.\n");
    out.push_str("- open a PR with the doc regardless of which repo it lands in. Do NOT push directly to `main` even on the user's personal docs repo (e.g. `brianduff/docs`). The PR is the user's edit window. The kanban card's doc link is derived from this PR automatically — there is no separate pointer to register.\n");
    out.push_str("- investigations do not touch code. If the description asks for both research and a code change, write only the investigation doc and note the follow-up code changes at the end of the doc for the user to file separately.\n");
    out
}

/// Directive block for `kind = 'revision'` tasks.
///
/// A revision's deliverable is a NEW COMMIT on an EXISTING pull request —
/// the PR owned by the parent task's chain root.  The revision worker must
/// NOT open a new PR.  The parent's PR URL is carried in
/// `execution.pr_url` (set at dispatch time).
///
/// When `conflict_attempt` or `ci_attempt` is `Some`, a signal-specific
/// diagnostic fragment is appended (design Q3 of
/// `unify-pr-remediation-on-revisions.md`): the existing diagnosis/log
/// rendering from the bespoke composers is lifted into the shared revision
/// directive rather than duplicated across three nearly-identical prompts.
fn compose_revision_directive(
    execution: &crate::work::WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    conflict_attempt: Option<&ConflictResolution>,
    ci_attempt: Option<&CiRemediation>,
) -> String {
    let description = match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => task.description.trim().to_owned(),
        _ => String::new(),
    };
    let parent_pr_url = execution.pr_url.as_deref().unwrap_or("(unknown)");
    let pr_number = boss_github::pr_url::pr_number_from_url(parent_pr_url)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    let repo_slug =
        crate::completion::parse_repo_slug(&execution.repo_remote_url).unwrap_or_else(|_| "<owner/repo>".to_owned());
    // A conflict-resolution revision pushes the merge-corrected branch as
    // soon as it COMPILES (the merge-correctness gate); the PR's own CI
    // runs the full test suite post-push. Other revisions keep the
    // build-and-test-before-push gate.
    let is_conflict_resolution = conflict_attempt.is_some();

    let mut out = String::new();
    out.push_str("Expected outcome for this run:\n");
    out.push_str("- This is a **REVISION** task. Your deliverable is an update to an EXISTING pull request — typically a new commit on the PR branch, or a rebase if that is all that is needed. Do NOT open a new PR. Do NOT create a `boss/exec_*` bookmark.\n");
    out.push_str(&format!("- The parent PR is #{pr_number} at {parent_pr_url}.\n"));
    out.push_str(&format!("- What this revision should change: {description}\n"));
    out.push_str(&format!(
        "\n**`gh` requires `--repo` in this workspace:** This repo is `{repo_slug}`. \
         `gh` cannot auto-detect the repo in a jj workspace (there is no `.git` \
         directory at the root — only `.jj/`). Pass `--repo {repo_slug}` on every \
         `gh` command: `gh pr view`, `gh pr checks`, `gh pr diff`, `gh api`, etc.\n"
    ));
    // Issue #804: revision chores (T30–T34 on PR #250) were the worst
    // offenders for pushing red code. Apply the pre-push build gate when
    // the workspace is a Bazel workspace. Conflict-resolution revisions
    // get the merge-correctness variant (build before push; tests run in
    // the PR's CI after the push) so a correct resolution is never
    // stranded behind a slow/flaky full-suite run.
    let prepush_gate = if is_conflict_resolution {
        bazel_conflict_resolution_gate_block(workspace_path)
    } else {
        bazel_prepush_gate_block(workspace_path)
    };
    if let Some(gate) = prepush_gate {
        out.push_str(&gate);
    }
    out.push('\n');
    out.push_str("## Workspace state\n");
    // `pr_number != "?"` is equivalent to `execution.pr_url` being a parseable
    // GitHub PR URL, which is exactly when the engine called `cube workspace goto`
    // to position the workspace at the PR head. Without a parseable URL,
    // the workspace is on main and the worker must position it manually.
    if pr_number != "?" {
        out.push_str("The engine pre-positioned this workspace via `cube workspace goto`, so you are already on a fresh editable commit whose parent is the PR head. Start making your changes directly — no branch discovery or checkout is needed.\n");
        out.push('\n');
        out.push_str(
            "**Fallback** (only if the workspace is NOT already positioned on an editable change atop the PR head):\n",
        );
        out.push_str("```\n");
        out.push_str(&format!("cube workspace goto --pr {pr_number}\n"));
        out.push_str("```\n");
    } else {
        out.push_str(
            "**The engine could not determine the PR number from the pr_url field. \
             You MUST position the workspace manually before making any changes \
             (replace `<n>` with the actual PR number):**\n",
        );
        out.push_str("```\n");
        out.push_str("cube workspace goto --pr <n>\n");
        out.push_str("```\n");
    }
    out.push_str("IMPORTANT: NEVER run `jj edit`, `gh pr checkout`, or `git checkout` in this workspace — fetched remote commits are immutable and those tools do not work correctly in a jj workspace.\n");
    out.push('\n');
    out.push_str("Steps:\n");
    out.push_str("1. Make the requested change.\n");
    out.push_str("2. `jj describe -m \"<short message describing THIS revision's change>\"`\n");
    out.push_str("3. Find the parent bookmark name and advance it to the new commit:\n");
    out.push_str("   ```\n");
    out.push_str("   # Find the parent bookmark (strip the @origin suffix for the branch name):\n");
    out.push_str("   jj log -r 'parents(@)' --no-graph -T 'remote_bookmarks'\n");
    out.push_str("   # Advance the local bookmark:\n");
    out.push_str("   jj bookmark set <parent-branch-name> -r @\n");
    out.push_str("   ```\n");
    out.push_str(
        "4. `jj git push -b <parent-branch-name>`   # NO --allow-new; NO GIT_DIR prefix; the branch already exists.\n",
    );
    out.push_str("5. **Update the PR description** — this is a required step, not optional:\n");
    out.push_str(&format!(
        "   a. Read the current description: `gh pr view {pr_number} -R {repo_slug} --json body -q .body`\n"
    ));
    out.push_str("   b. Compare it carefully against what the PR NOW does after your change. Pay special attention to any section that describes behaviour, scope, or approach that this revision REVERSES, supersedes, or obsoletes — those sections MUST be corrected or removed. A description that tells a reviewer the exact opposite of what the code does is worse than a terse one.\n");
    out.push_str("   c. If any part of the description is now inaccurate, write the corrected body to a temp file and apply it:\n");
    out.push_str(&format!(
        "      `body=$(mktemp) && <write corrected body to $body> && gh pr edit {pr_number} --body-file \"$body\" -R {repo_slug}`\n"
    ));
    out.push_str(
        "      Never pass the body as an inline `--body` argument — the shell evaluates backticks and `$(...)`.\n",
    );
    out.push_str("   d. What to write: rewrite the description so it is accurate and self-contained for reviewers NOW. The main summary must describe the CURRENT state — what the PR does, not what it used to do. Do NOT append a changelog that leaves a contradictory original summary above it; instead correct the summary in place. A brief \"Changes in this revision\" note may follow the corrected summary if it adds context, but it must never contradict or overshadow the corrected summary.\n");
    out.push_str("   e. A revision may skip steps c–d ONLY if it changes ZERO source files (e.g. a PR-description-only fix or a pure markdown/comment edit) AND involves no rebase, merge, or conflict resolution. Rebase and conflict-resolution revisions do NOT qualify for this skip — they touch compiled output and must go through the full description review.\n");
    out.push('\n');
    out.push_str(&format!(
        "6. Confirm the new commit is on the PR: `gh pr view {pr_number} -R {repo_slug}`\n"
    ));
    out.push_str(&format!(
        "7. Print the parent PR URL on its own line as the FINAL thing in your final response: {parent_pr_url}\n"
    ));
    out.push('\n');
    out.push_str("Preserve revision history — each revision is a new commit on the PR branch; never amend, squash, or rename existing commits on the branch.\n");
    out.push('\n');
    let rebase_gate_clause = if is_conflict_resolution {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the merge-correctness build gate. Any rebase, merge, or conflict resolution MUST run the `bazel build` merge-correctness gate (compile the touched/upstream targets, regenerate invalidated lockfiles) before pushing, even when the rebase appeared clean — a rebase merges upstream changes in and the resulting code is new and must compile. The full `bazel test` suite is NOT a precondition for this push; it runs in the PR's CI after you push (see the conflict-resolution gate above).\n"
    } else {
        "Rebase-only exception (VCS only — not a build-gate skip): if the ONLY thing needed to satisfy this revision is a rebase (e.g. rebasing the branch onto updated main) and the rebase produces NO diff whatsoever (zero changed files), it is valid to have NO new commit. Do not manufacture an empty or cosmetic commit. In that case, push the rebased branch and explain in your response that the revision was satisfied by a rebase with no code change. IMPORTANT: this exception covers VCS mechanics only — whether to add a new commit. It does NOT exempt you from the pre-push build gate. Any revision that involves a rebase, merge, or conflict resolution MUST run the full `bazel build` + `bazel test` gate before pushing, even when the rebase appeared clean. A rebase merges upstream changes into your branch — the resulting code is new and must be compiled and tested. This is exactly where compile errors get reintroduced.\n"
    };
    out.push_str(rebase_gate_clause);
    out.push('\n');
    out.push_str("Constraints:\n");
    out.push_str("- Do NOT run `gh pr create` — this revision has no PR of its own.\n");
    out.push_str("- Do NOT create a `boss/exec_*` bookmark — push to the existing parent branch.\n");
    out.push_str("- Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty and this is NOT a rebase-only revision, stop and explain.\n");
    out.push('\n');
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out.push_str(&format!(
        "\nAcceptance criterion: when you believe the work is done, the deliverable is the parent PR URL.\n\
         - Push your changes to the parent branch (see step 4 above). Do NOT open a new PR.\n\
         - Update the PR description per step 5 above — a stale or contradictory description is a defect.\n\
         - Confirm the parent PR shows your new commit with `gh pr view {pr_number} -R {repo_slug}`.\n\
         - Print {parent_pr_url} on its own line as the final thing in your final response so the engine can pick it up.\n\
         - Before pushing, verify your changes are real with `jj diff -r @`. If the diff is empty and no rebase was needed, stop and explain.\n"
    ));
    if let Some(attempt) = conflict_attempt {
        out.push_str(&compose_conflict_resolution_fragment(attempt));
    }
    if let Some(attempt) = ci_attempt {
        out.push_str(&compose_ci_remediation_fragment(attempt));
    }
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
    if let Some(path) = project
        .design_doc_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
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

/// Signal-specific fragment appended to `compose_revision_directive` when the
/// revision was created with `created_via = "merge-conflict:<crz_id>"`.
///
/// Provides the conflict context and diagnosis that the worker needs to
/// resolve the merge conflict — identical in content to the bespoke
/// `compose_conflict_resolution_prompt` except that the branch/push spine
/// is already covered by the shared revision directive, so this fragment
/// covers only the signal-specific parts: the diagnosis block, rebase
/// instructions, stop conditions, and post-resolution PR comment template.
fn compose_conflict_resolution_fragment(attempt: &ConflictResolution) -> String {
    let mut out = String::new();
    out.push_str("\n---\n\n");
    out.push_str(&format!(
        "## Conflict resolution context: PR #{pr_num} against `{base}`\n\n",
        pr_num = attempt.pr_number,
        base = attempt.base_branch,
    ));
    out.push_str(&format!(
        "**Branch**: `{}` based off `{}`\n",
        attempt.head_branch, attempt.base_branch,
    ));
    if let Some(base_sha) = attempt.base_sha_at_trigger.as_deref() {
        out.push_str(&format!(
            "**Base sha at conflict detection**: `{base_sha}` (current `{}` may be ahead)\n",
            attempt.base_branch,
        ));
    }
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));
    out.push_str(
        "This PR was in code review when `main` moved under it. The PR's diff against\n\
         the current `main` does not apply cleanly. Your task in step 3 above is to\n\
         resolve the conflicts — **you are not adding new work to this PR.**\n\n",
    );
    out.push_str("### Rebase steps (replaces step 3)\n\n");
    out.push_str(
        "Run the cube rebase command — it encodes the correct jj recipe automatically \
         and avoids the `@origin` / immutable-heads pitfalls agents commonly hit:\n\n\
         ```\n\
         cube workspace rebase\n\
         ```\n\n\
         This command: fetches the latest integration branch from GitHub, resolves this \
         workspace's boss branch automatically (no branch name argument needed), rebases \
         it onto the repo's configured integration branch with `--ignore-immutable`, and \
         reports a clear signal:\n\n\
         - `REBASED_CLEAN` — no conflicts; the branch has been pushed automatically. Skip to step 5 (update PR description).\n\
         - `REBASED_WITH_CONFLICTS` — conflicts are materialized in the working copy. \
         Inspect with `jj st` and `jj resolve --list`, read the diagnosis below for what \
         was touched on the upstream side, resolve each file, then continue to step 4.\n\n\
         Do NOT hand-roll `jj rebase` manually — the correct flags differ from the bare \
         form and agents reliably get them wrong.\n\n",
    );
    out.push_str(
        "### How to resolve jj conflicts (structural edit — NOT line-range surgery)\n\n\
         jj materializes each conflict as annotated regions directly in the file. \
         Resolve by **editing those regions in place**:\n\n\
         - Open the conflicted file and find the `<<<<<<<` / `>>>>>>>` marker blocks.\n\
         - Each block contains the conflict base and the two sides (`Contents of side #1`, \
         `Contents of side #2`). Decide which content to keep (or merge both), then replace \
         the entire marker block with the resolved content.\n\
         - Alternatively, run `jj resolve <file>` to open a 3-way merge tool (e.g. vimdiff) \
         that handles the structured regions for you.\n\n\
         **Anti-pattern — do NOT do this:** grep for conflict markers, extract specific line \
         ranges, and concatenate them to rebuild the file. That approach silently drops hunks \
         (off-by-one, missed sections) and makes the resolution look like a from-scratch \
         rewrite. Edit the marker regions directly instead.\n\n",
    );
    out.push_str("### Conflict diagnosis (from the engine's pre-spawn pass)\n\n");
    match attempt
        .conflict_diagnosis
        .as_deref()
        .map(serde_json::from_str::<ConflictDiagnosis>)
    {
        Some(Ok(diagnosis)) => out.push_str(&render_diagnosis_markdown(&diagnosis)),
        Some(Err(err)) => {
            out.push_str(&format!(
                "_Engine could not re-parse the diagnosis JSON (error: {err}). The\n\
                 raw blob is on `conflict_resolutions.conflict_diagnosis` if you need it._\n",
            ));
        }
        None => {
            out.push_str(
                "_No engine-collected diagnosis is available for this attempt. Use\n\
                 `jj st` and `jj resolve --list` after the rebase to discover the\n\
                 conflicts directly._\n",
            );
        }
    }
    out.push_str("\n### Stop conditions\n\n");
    out.push_str(
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
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out.push_str("### Post-resolution PR comment template\n\n");
    out.push_str(
        "```\n\
         🤖 boss resolved merge conflicts on this PR after `main` moved.\n\n\
         Resolutions:\n\
         - <per-file resolution summary>\n\n\
         Branch force-pushed; per branch protection, prior approvals have been dismissed.\n\
         Re-review when ready.\n\
         ```\n\n",
    );
    out
}

/// Signal-specific fragment appended to `compose_revision_directive` when the
/// revision was created with `created_via = "ci-fix:<crm_id>"`.
///
/// Provides the CI remediation context (failing checks, log excerpt, playbook)
/// that the worker needs to fix the failing CI — identical in content to the
/// bespoke `compose_ci_remediation_prompt` except that the branch/push spine
/// is already covered by the shared revision directive.
fn compose_ci_remediation_fragment(attempt: &CiRemediation) -> String {
    let is_rebounce = attempt.failure_kind.as_deref() == Some("merge_queue_rebounce");

    let mut out = String::new();
    out.push_str("\n---\n\n");

    if is_rebounce {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — merge-queue FAILED_CHECKS\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
        out.push_str(
            "> **Important**: this is a **merge-queue rebounce**, not a per-PR CI failure.\n\
             > - The PR's own required checks are **green** on its head SHA. Do NOT look at them.\n\
             > - The failure happened on the **synthetic merge commit** GitHub assembled when the PR\n\
             >   entered the queue. See `Synthetic merge SHA` below.\n\
             > - Root cause: something landed on `main` between this PR's CI run and its queue turn\n\
             >   that is semantically incompatible. After fixing, **re-enqueue** the PR.\n\n",
        );
    } else {
        out.push_str(&format!(
            "## CI remediation context: PR #{pr_num} ({kind}) — required checks failing\n\n",
            pr_num = attempt.pr_number,
            kind = attempt.attempt_kind,
        ));
    }

    if !attempt.head_branch.is_empty() {
        out.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    if is_rebounce && let Some(ref sha) = attempt.before_commit_sha {
        out.push_str(&format!("**Synthetic merge SHA** (fetch CI logs from here): `{sha}`\n",));
    }
    out.push_str(&format!("**Head sha at trigger**: `{}`\n", attempt.head_sha_at_trigger,));
    out.push_str(&format!("**Attempt id**: `{}`\n\n", attempt.id));

    out.push_str("### Failing required checks\n\n");
    match render_failed_checks_markdown(&attempt.failed_checks) {
        Some(md) => out.push_str(&md),
        None => out.push_str(
            "_The engine did not record a parseable `failed_checks` blob for this attempt. \
             Read `gh pr checks` to enumerate the failing required checks before deciding the fix._\n",
        ),
    }
    out.push('\n');

    if let Some(bk_cmds) = render_bk_log_commands(&attempt.failed_checks) {
        out.push_str(&bk_cmds);
    }

    if !is_rebounce {
        out.push_str("### If CI is already green (nothing to fix)\n\n");
        out.push_str(&format!(
            "Before assuming there is work to do, check the **current** state of the PR's required \
             checks (`gh pr checks {pr}` / `gh pr view {pr}`). If they are **already passing** — the \
             failure cleared on its own (a flaky check settled, `main` moved, or a stale failure was \
             re-detected) — you do NOT have to invent a fix. Declare it; the engine VALIDATES your \
             claim against live CI before retiring the attempt:\n\n\
             ```\n\
             boss engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             The engine independently re-probes live CI for the PR's current head SHA. If every \
             required check is verified green, the attempt is retired and the parent unblocks — you are \
             **done, stop**. If CI is still red or pending, the command **fails** (non-zero exit) with \
             the live status and the attempt stays open: the failure is real, so continue below.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

    if attempt.attempt_kind == "retrigger" {
        out.push_str("### Action: retrigger the failing build\n\n");
        out.push_str(
            "The engine has pre-classified this failure as infra (every failing check has \
             `conclusion ∈ {STARTUP_FAILURE, CANCELLED}`). No log read or code change is needed.\n\n",
        );
        out.push_str(
            "1. Re-run the failing build via the per-provider CLI (`bk build retry <build-id>` \
             for Buildkite or `gh run rerun <run-id> --failed` for GitHub Actions). The failing \
             check's `target_url` above carries the right id.\n\
             2. Call `boss engine ci mark-retriggered --attempt-id <attempt-id> --new-id <new-build-or-run-id>` \
             so the engine records the new run id and stays out of the budget path. Do NOT call \
             `mark-failed` or push code.\n\
             3. Stop. The merge-poller will observe the re-run's outcome on the next sweep.\n\n",
        );
    } else {
        if is_rebounce {
            out.push_str("### Action: rebase onto current main, then fix the semantic conflict\n\n");
            out.push_str(
                "A merge-queue rebounce almost always means something landed on `main` between \
                 this PR's CI run and its queue turn that is **semantically incompatible**.\n\
                 Fix is: rebase, look at the CI failure on the synthetic merge SHA, add a focused \
                 fix, push, and re-enqueue the PR.\n\n",
            );
        } else {
            out.push_str("### Action: rebase first, then fix\n\n");
            out.push_str(
                "Many CI failures on long-running PRs are caused by `main` moving. The cheapest \
                 experiment is rebasing onto `main` HEAD before changing any code — if CI goes \
                 green after the rebase, no fix-attempt slot is consumed.\n\n",
            );
        }
        out.push_str("**Step 1 — Rebase onto base HEAD and force-push** (replaces step 3 above).\n\n");
        out.push_str(&format!(
            "```\n\
             jj edit {branch}\n\
             jj rebase -d main -b {branch}\n\
             # then push via step 5 of the revision directive\n\
             ```\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        if is_rebounce {
            out.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, call \
                 `boss engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>` and stop. \
                 Then re-enqueue the PR (`gh pr merge --auto --squash`). The engine flips the \
                 attempt to `succeeded` and the budget slot is not consumed.\n\
                 - **If post-rebase CI is still red**, the semantic conflict requires a code fix — \
                 continue to Step 2.\n\n",
            );
        } else {
            out.push_str(
                "Wait for the re-run's required checks to settle (`gh pr checks --watch`). Then:\n\n\
                 - **If post-rebase CI is green**, call \
                 `boss engine ci mark-succeeded-via-rebase --attempt-id <attempt-id>` and stop. The \
                 engine flips the attempt to `succeeded`, sets `consumes_budget = 0`, and decrements \
                 `tasks.ci_attempts_used` so this attempt does not count against the PR's budget.\n\
                 - **If post-rebase CI is still red**, continue to Step 2. The budget slot is now \
                 consumed; this is the fix attempt the engine pre-classified.\n\n",
            );
        }

        out.push_str("**Step 2 — Read the log, classify, fix, push.**\n\n");
        if is_rebounce {
            let sha_hint = attempt.before_commit_sha.as_deref().unwrap_or("<synthetic-merge-sha>");
            out.push_str(&format!(
                "Fetch CI logs from the **synthetic merge SHA `{sha_hint}`**, not the PR head \
                 (whose checks are green). Use the per-provider CLI:\n\n\
                 - Buildkite: `bk job log --pipeline <slug> --build-number <N> <job-uuid>` \
                 (slug and build number are in the check's `target_url`; job UUIDs come from \
                 `bk build view <N> --pipeline <slug>`)\n\
                 - GitHub Actions: `gh run view --log-failed --job <job-id>` (job id from failing check URL)\n\n",
            ));
        } else {
            out.push_str("Engine-collected log excerpt (failing job tail):\n\n");
            match attempt.log_excerpt.as_deref().map(str::trim) {
                Some(tail) if !tail.is_empty() => {
                    out.push_str("```\n");
                    out.push_str(tail);
                    out.push_str("\n```\n\n");
                }
                _ => {
                    out.push_str(
                        "_The engine's pre-spawn log fetch did not produce an excerpt for this attempt. \
                         Use the ready-to-run commands above (`bk job log --pipeline …`) or \
                         `gh run view --log-failed --job <job-id>` (job id from the failing check URL)._\n\n",
                    );
                }
            }
        }
        out.push_str(
            "1. Classify the failure with `boss engine ci classify --attempt-id <attempt-id> --class <tractable|flaky_or_infra|unfixable>`.\n   \
                - `tractable` → there's a clear code change that resolves it. Make it. Push.\n   \
                - `flaky_or_infra` → the failure is environmental. Pivot to the retrigger playbook \
                (re-run the failing build via the provider CLI and call `mark-retriggered`).\n   \
                - `unfixable` → the failure is real and out of scope. Call \
                `boss engine ci mark-failed --attempt-id <attempt-id> --reason <reason>` \
                and stop. Do NOT push.\n",
        );
        out.push_str("2. No `test_command` context is available here; rely on CI to verify the push.\n");
        out.push_str(&format!(
            "3. Push your fix via step 5 of the revision directive (push to the parent branch \
                `{branch}`). The merge-poller will observe the new head sha and re-evaluate CI on \
                the next sweep — when green it flips the attempt to `succeeded` and unblocks the parent.\n\n",
            branch = if attempt.head_branch.is_empty() {
                "<branch>"
            } else {
                attempt.head_branch.as_str()
            },
        ));
        if is_rebounce {
            out.push_str(
                "**Step 3 (after CI is green) — Re-enqueue the PR.**\n\n\
                 The merge queue does **not** auto-retry after a dequeue. After your push produces \
                 green CI, re-add the PR to the merge queue:\n\n\
                 ```\n\
                 gh pr merge --auto --squash  # or --merge / --rebase per repo policy\n\
                 ```\n\n",
            );
        }
    }

    out.push_str("### Stop conditions\n\n");
    out.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (rebase, infra retrigger, or a focused fix).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\
         - **Always pass `-m \"…\"` to `jj describe` / `jj squash`.** The worker \
         environment has no usable `$EDITOR`.\n\n",
    );
    out.push_str(check_bypass_prohibition_text());
    out.push('\n');
    out
}

/// Templated prompt for the `ci_remediation` execution kind, retrigger path
/// only. `fix`-kind CI attempts now dispatch through the revision substrate
/// (`revision_implementation`); only `retrigger` (design Q6: no commit,
/// not revision-shaped) still uses this bespoke execution kind.
fn compose_ci_remediation_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
    attempt: &CiRemediation,
    _test_command: Option<&str>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!(
        "## CI remediation: PR #{pr_num} ({kind}) — required checks failing\n\n",
        pr_num = attempt.pr_number,
        kind = attempt.attempt_kind,
    ));

    prompt.push_str(&format!("**PR**: {}\n", attempt.pr_url));
    if !attempt.head_branch.is_empty() {
        prompt.push_str(&format!("**Branch**: `{}`\n", attempt.head_branch));
    }
    prompt.push_str(&format!("**Head sha at trigger**: `{}`\n", attempt.head_sha_at_trigger,));
    prompt.push_str(&format!("**Workspace**: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("**Attempt id**: `{}`\n", attempt.id));
    prompt.push_str(&format!("**Execution id**: `{}`\n", execution.id));
    if let Some(change) = cube_change_id {
        prompt.push_str(&format!("**Local change**: `{change}`\n"));
    }
    prompt.push_str(&format!("**Work item**: `{}`\n\n", work_item_name(work_item),));

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

    // If the failure already cleared, the worker can declare a
    // validated noop rather than retriggering a build that is no longer
    // red. The engine re-probes live CI before honoring it.
    //
    // Gated `!is_rebounce` to match the sibling revision-fragment brief
    // (`compose_ci_remediation_fragment`): a merge_queue_rebounce
    // failure lives on the synthetic merge commit, so the PR's
    // head-branch checks always read green — surfacing `mark-noop` to a
    // rebounce worker would invite a claim the engine is guaranteed to
    // reject (`handle_mark_ci_remediation_noop` refuses rebounce
    // attempts before it even probes). Rebounce rows normally deliver
    // via a revision rather than this bespoke prompt, but the
    // stranded-rescue path can re-dispatch one here, so guard it.
    let is_rebounce = attempt.failure_kind.as_deref() == Some("merge_queue_rebounce");
    if !is_rebounce {
        prompt.push_str("### If CI is already green (nothing to fix)\n\n");
        prompt.push_str(&format!(
            "Check the **current** required checks first (`gh pr checks {pr}`). If they are already \
             passing, declare it instead of retriggering — the engine validates the claim against live \
             CI before retiring the attempt:\n\n\
             ```\n\
             boss engine ci mark-noop --attempt-id {attempt} --observed-sha <current-head-sha> --reason already-green\n\
             ```\n\n\
             Verified green → attempt retired, parent unblocked, you are done. Still red/pending → the \
             command fails (non-zero) and the attempt stays open; fall through to the retrigger playbook.\n\n",
            pr = attempt.pr_url,
            attempt = attempt.id,
        ));
    }

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

    prompt.push_str("### Stop conditions\n\n");
    prompt.push_str(
        "- **You are not adding scope.** The only allowed change is one that makes the failing \
         required checks pass (infra retrigger only — no code changes).\n\
         - **Do not close the PR yourself.** Closing is the human's call.\n\n",
    );
    prompt.push_str("Respond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
}

/// Build a block of ready-to-run `bk` CLI commands for every Buildkite
/// entry in the `failed_checks` JSON. Returns `None` when the JSON
/// contains no Buildkite entries or the target URLs lack enough
/// information to construct pre-filled commands.
///
/// Emits two commands per failing Buildkite job:
///   `bk build view <N> --pipeline <slug>`  — enumerate all jobs in the build
///   `bk job log --pipeline <slug> --build-number <N> <job-uuid>`
fn render_bk_log_commands(failed_checks_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Entry {
        target_url: String,
        provider: String,
        #[serde(default)]
        provider_job_id: Option<String>,
    }
    let entries: Vec<Entry> = serde_json::from_str(failed_checks_json).ok()?;

    let mut commands = String::new();
    for e in &entries {
        if e.provider != "buildkite" {
            continue;
        }
        let Some(pipeline) = parse_buildkite_pipeline_slug(&e.target_url) else {
            continue;
        };
        let Some(build_num) = parse_buildkite_build_id(&e.target_url) else {
            continue;
        };
        commands.push_str(&format!("bk build view {build_num} --pipeline {pipeline}\n",));
        match e.provider_job_id.as_deref() {
            Some(job_id) => {
                commands.push_str(&format!(
                    "bk job log --pipeline {pipeline} --build-number {build_num} {job_id}\n",
                ));
            }
            None => {
                commands.push_str(&format!(
                    "# (replace <job-uuid> with an id from `bk build view` above)\n\
                     bk job log --pipeline {pipeline} --build-number {build_num} <job-uuid>\n",
                ));
            }
        }
    }

    if commands.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("### Ready-to-run Buildkite log commands\n\n");
    out.push_str(
        "`bk` is the Buildkite CLI. These commands are pre-filled with the \
         pipeline, build number, and job id — no argument guessing required:\n\n",
    );
    out.push_str("```\n");
    out.push_str(&commands);
    out.push_str("```\n\n");
    Some(out)
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

pub(crate) fn work_item_name(work_item: &WorkItem) -> &str {
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
pub(crate) fn work_item_task_kind(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(task.kind.as_str()),
        WorkItem::Product(_) | WorkItem::Project(_) => None,
    }
}

/// Return the `created_via` provenance string for task work items.
/// Returns `None` for products and projects.
fn work_item_created_via(work_item: &WorkItem) -> Option<&str> {
    match work_item {
        WorkItem::Task(task) | WorkItem::Chore(task) => Some(&task.created_via),
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
// Fully tested but not yet wired into the exec runner; keeping here so it
// can be called once the PR-URL extraction step is plumbed in.
#[allow(dead_code)]
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
#[allow(dead_code)] // helper for extract_pr_url_from_text
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

#[allow(dead_code)] // helper for parse_canonical_pr_url
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
    if let Some(pr_url) = task.pr_url.as_deref()
        && !pr_url.trim().is_empty()
    {
        lines.push(format!("  - pr_url: {}", pr_url.trim()));
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod compose_prompt_tests {
    use super::*;
    use crate::work::Task;

    fn base_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_abc123_01")
            .work_item_id("task-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
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
                .kind(TaskKind::Chore)
                .name("Fix the thing")
                .description("Description here.")
                .status(TaskStatus::Todo)
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            !prompt.contains("expected branch name"),
            "expected-branch-name line should be suppressed when resuming a PR:\n{prompt}",
        );
    }

    #[test]
    fn expected_branch_name_present_when_no_pr_url() {
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("jj bookmark create"),
            "acceptance criterion should guide fresh branch creation:\n{prompt}",
        );
        assert!(
            prompt.contains("gh pr create") || prompt.contains("cube pr create"),
            "acceptance criterion should guide opening a new PR:\n{prompt}",
        );
    }

    #[test]
    fn no_recovery_block_when_no_prior_branch() {
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            !prompt.contains("STARTUP RECOVERY"),
            "no recovery block expected when recovery_branch is None:\n{prompt}",
        );
    }

    #[test]
    fn recovery_block_injected_when_prior_branch_provided() {
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .recovery_branch("boss/exec_prior123_09")
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .recovery_branch("boss/exec_prior123_09")
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .recovery_branch("boss/exec_prior123_09")
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .recovery_branch("boss/exec_prior123_09")
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        // "boss/exec_abc123_01" is the new expected branch
        assert!(
            prompt.contains("boss/exec_abc123_01"),
            "recovery block should mention the new expected branch name:\n{prompt}",
        );
    }

    /// Like `base_execution` but pointed at a given repo remote, so the
    /// CI-monitoring directive's org-specific branch can be exercised.
    fn execution_for_remote(remote: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_abc123_01")
            .work_item_id("task-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url(remote.to_string())
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    #[test]
    fn ci_monitoring_directive_present_for_implementation_chore() {
        // Issue #899: the worker must be told not to poll CI forever, and
        // that the engine auto-transitions to Review once CI is effectively
        // green. This general guidance applies regardless of org.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("do not babysit CI"),
            "missing CI-monitoring directive:\n{prompt}",
        );
        assert!(
            prompt.contains("effectively green"),
            "directive should reference the engine's effectively-green definition:\n{prompt}",
        );
    }

    #[test]
    fn ci_monitoring_directive_names_human_gated_checks_for_linkedin_org() {
        // The human-gated check name must be sourced from the engine's
        // REVIEW_SIGNAL_RULES table (via review_signal_checks_for_owner),
        // not re-hardcoded in the prompt — single sourcing is the fix.
        let exec = execution_for_remote("git@github.com:linkedin-multiproduct/some-repo.git");
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&exec)
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("Owner Approval"),
            "directive should name the org's human-gated check:\n{prompt}",
        );
        assert!(
            prompt.contains("linkedin-multiproduct"),
            "directive should name the org:\n{prompt}",
        );
    }

    #[test]
    fn ci_monitoring_directive_omits_human_gated_names_for_plain_org() {
        // A non-LinkedIn org has no review-signal rules; the directive's
        // general guidance stands alone without naming any check.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&execution_for_remote("git@github.com:org/repo.git"))
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("do not babysit CI"),
            "general directive should still be present:\n{prompt}",
        );
        assert!(
            !prompt.contains("Owner Approval"),
            "no human-gated check should be named for a plain org:\n{prompt}",
        );
    }

    #[test]
    fn no_op_directive_present_for_fresh_chore_without_pr() {
        // T1868: a fresh chore_implementation worker (no existing PR) must
        // be told the sanctioned way to terminate when the work is already
        // done — emit NO_CHANGES_NEEDED — instead of only "stop and explain".
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains(crate::no_op_signal::NO_CHANGES_NEEDED_MARKER),
            "fresh-chore prompt must name the NO_CHANGES_NEEDED marker:\n{prompt}",
        );
        assert!(
            prompt.contains("signal a sanctioned no-op"),
            "fresh-chore prompt must carry the no-op completion directive:\n{prompt}",
        );
    }

    #[test]
    fn no_op_directive_absent_when_pr_already_exists() {
        // When a PR already exists (resume / existing-PR flow), an empty diff
        // means "already pushed" and is handled by the push-to-existing path
        // — NOT by closing the task as a no-op. The directive must not appear.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_with_pr("https://github.com/org/repo/pull/7"))
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            !prompt.contains(crate::no_op_signal::NO_CHANGES_NEEDED_MARKER),
            "existing-PR prompt must NOT carry the no-op marker directive:\n{prompt}",
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
                .status(crate::work::ProjectStatus::Active)
                .slug("my-project")
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .build(),
        );
        assert!(work_item_pr_url(&project).is_none());
    }

    #[test]
    fn extract_pr_number_parses_standard_github_url() {
        assert_eq!(extract_pr_number("https://github.com/org/repo/pull/123"), Some(123),);
    }

    #[test]
    fn extract_pr_number_returns_none_for_malformed_url() {
        assert_eq!(extract_pr_number("https://github.com/org/repo"), None);
        assert_eq!(extract_pr_number("not-a-url"), None);
    }

    #[test]
    fn extract_pr_url_from_text_finds_bare_url() {
        let s = "see https://github.com/org/repo/pull/42 for context";
        assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
    }

    #[test]
    fn extract_pr_url_from_text_strips_trailing_punctuation() {
        let s = "follow-up on https://github.com/org/repo/pull/42.";
        assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
    }

    #[test]
    fn extract_pr_url_from_text_strips_subpath() {
        let s = "see https://github.com/org/repo/pull/42/files";
        assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
    }

    #[test]
    fn extract_pr_url_from_text_handles_markdown_link() {
        let s = "[PR](https://github.com/org/repo/pull/7) is in review";
        assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/7"),);
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
        assert_eq!(extract_pr_url_from_text(s), Some("https://github.com/org/repo/pull/42"),);
    }

    #[test]
    fn task_bound_pr_url_prefers_explicit_column() {
        let chore = chore_with_pr("https://github.com/org/repo/pull/99");
        let task = match &chore {
            WorkItem::Chore(t) => t,
            _ => unreachable!(),
        };
        assert_eq!(task_bound_pr_url(task), Some("https://github.com/org/repo/pull/99"),);
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
                task.description = "Ref: https://github.com/linkedin-multiproduct/dev-infra/pull/250".into();
                WorkItem::Chore(task)
            }
            other => other,
        };
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Running)
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
        assert!(
            prompt.contains("FOREGROUND") && prompt.contains("run_in_background"),
            "gate must mandate foreground execution and forbid the background-and-idle anti-pattern (issue #976):\n{prompt}",
        );
    }

    #[test]
    fn bazel_gate_absent_on_non_bazel_workspace() {
        // Empty tempdir — no MODULE.bazel / WORKSPACE marker.
        let ws = tempfile::TempDir::new().unwrap();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/250"))
                .work_item(&chore_without_pr())
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/250"))
                .work_item(&chore_without_pr())
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
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

    // Helpers shared by revision fragment tests.

    fn revision_task_with_created_via(pr_url: Option<&str>, created_via: &str) -> WorkItem {
        let mut task = crate::work::Task::builder()
            .id("task-rev-1")
            .product_id("prod-1")
            .kind(TaskKind::Revision)
            .name("Revision task")
            .description("Fix the merge conflict.")
            .status(TaskStatus::Active)
            .created_at("2026-05-15T00:00:00Z")
            .updated_at("2026-05-15T00:00:00Z")
            .autostart(false)
            .created_via(created_via)
            .build();
        task.pr_url = pr_url.map(|s| s.to_owned());
        WorkItem::Task(task)
    }

    fn sample_conflict_attempt() -> crate::work::ConflictResolution {
        use crate::conflict_diagnosis::{ConflictDiagnosis, ConflictedFile};
        let diag = ConflictDiagnosis {
            schema_version: 1,
            base_sha: "aaa111".into(),
            head_sha: "bbb222".into(),
            files: vec![ConflictedFile {
                path: "src/lib.rs".into(),
                marker_count: Some(1),
                shape: "content".into(),
            }],
            error: None,
        };
        crate::work::ConflictResolution {
            id: "crz_frag_01".into(),
            product_id: "prod-1".into(),
            work_item_id: "task-rev-1".into(),
            pr_url: "https://github.com/org/repo/pull/77".into(),
            pr_number: 77,
            head_branch: "feature/frag".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("aaa111".into()),
            head_sha_before: None,
            head_sha_after: None,
            status: "running".into(),
            failure_reason: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            worker_id: None,
            conflict_diagnosis: Some(serde_json::to_string(&diag).unwrap()),
            created_at: "2026-05-15T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            revision_task_id: Some("task-rev-1".into()),
        }
    }

    fn sample_ci_attempt() -> crate::work::CiRemediation {
        crate::work::CiRemediation {
            id: "crm_frag_01".into(),
            product_id: "prod-1".into(),
            work_item_id: "task-rev-1".into(),
            pr_url: "https://github.com/org/repo/pull/77".into(),
            pr_number: 77,
            head_branch: "feature/frag".into(),
            head_sha_at_trigger: "ccc333".into(),
            head_sha_after: None,
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: r#"[{"name":"ci/test","conclusion":"FAILURE","target_url":"https://buildkite.com/myorg/mypipeline/builds/1329","provider":"buildkite","provider_job_id":"job-uuid-456"}]"#.into(),
            triage_class: None,
            log_excerpt: Some("ERROR: test failed at line 42".into()),
            status: "running".into(),
            failure_reason: None,
            cube_lease_id: None,
            cube_workspace_id: None,
            worker_id: None,
            created_at: "2026-05-15T00:00:00Z".into(),
            started_at: None,
            finished_at: None,
            failure_kind: None,
            before_commit_sha: None,
            revision_task_id: Some("task-rev-1".into()),
        }
    }

    #[test]
    fn revision_directive_with_conflict_provenance_injects_conflict_fragment() {
        let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
        let attempt = sample_conflict_attempt();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .conflict_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        // Must contain the conflict-resolution section header.
        assert!(
            prompt.contains("## Conflict resolution context"),
            "conflict fragment must be injected into revision directive:\n{prompt}",
        );
        // Must embed the attempt id.
        assert!(
            prompt.contains("`crz_frag_01`"),
            "conflict fragment must include the attempt id:\n{prompt}",
        );
        // Must embed the diagnosis file.
        assert!(
            prompt.contains("`src/lib.rs`"),
            "conflict fragment must render the conflict diagnosis:\n{prompt}",
        );
        // Must include the stop conditions.
        assert!(
            prompt.contains("boss engine conflicts mark-failed"),
            "conflict fragment must include the mark-failed stop condition:\n{prompt}",
        );
        // Must still contain the base revision directive spine.
        assert!(
            prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
            "base revision directive must still be present:\n{prompt}",
        );
    }

    #[test]
    fn conflict_revision_uses_merge_correctness_gate_not_full_test_gate() {
        // A conflict-resolution revision must push the merge-corrected
        // branch as soon as it COMPILES (the merge-correctness gate); the
        // full `bazel test` suite is the PR's own CI's job, run after the
        // push. Blocking the push behind the full suite is what stranded
        // correct resolutions unpushed (the loop this fix addresses).
        let ws = bazel_workspace();
        let work_item = revision_task_with_created_via(None, "merge-conflict:crz_frag_01");
        let attempt = sample_conflict_attempt();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                .work_item(&work_item)
                .workspace_path(ws.path())
                .conflict_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("## Pre-push gate for conflict resolution (Bazel workspace)"),
            "conflict revision must get the merge-correctness gate:\n{prompt}",
        );
        assert!(
            prompt.contains("Do NOT block the push on a full `bazel test //...`"),
            "conflict gate must defer the full test suite to CI:\n{prompt}",
        );
        // The generic build-AND-test-before-push gate must NOT be present
        // for conflict revisions — it is what caused the pre-push stall.
        assert!(
            !prompt.contains("## Pre-push build gate (Bazel workspace)"),
            "conflict revision must NOT carry the generic build+test gate:\n{prompt}",
        );
        assert!(
            !prompt.contains("Both `bazel build` and `bazel test` must finish clean"),
            "conflict revision must not require a full test pass before push:\n{prompt}",
        );
        // The rebase clause must reference the merge-correctness gate, not
        // the full build+test gate.
        assert!(
            prompt.contains("The full `bazel test` suite is NOT a precondition for this push"),
            "conflict rebase clause must defer tests to CI:\n{prompt}",
        );
        // Verification is NOT skipped — the merged code must still build.
        assert!(
            prompt.contains("The merged code MUST COMPILE"),
            "conflict gate must still require a clean build:\n{prompt}",
        );
    }

    #[test]
    fn non_conflict_revision_keeps_full_build_and_test_gate() {
        // A plain operator revision (no conflict attempt) keeps the
        // build-AND-test-before-push gate — the merge-correctness rescope
        // is conflict-resolution-specific.
        let ws = bazel_workspace();
        let work_item = revision_task_with_created_via(None, "operator");
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                .work_item(&work_item)
                .workspace_path(ws.path())
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("## Pre-push build gate (Bazel workspace)"),
            "non-conflict revision must keep the generic build+test gate:\n{prompt}",
        );
        assert!(
            prompt.contains("Both `bazel build` and `bazel test` must finish clean"),
            "non-conflict revision must still require a full test pass before push:\n{prompt}",
        );
        assert!(
            !prompt.contains("## Pre-push gate for conflict resolution"),
            "non-conflict revision must NOT get the conflict merge-correctness gate:\n{prompt}",
        );
    }

    #[test]
    fn revision_directive_with_ci_fix_provenance_injects_ci_fragment() {
        let work_item = revision_task_with_created_via(None, "ci-fix:crm_frag_01");
        let attempt = sample_ci_attempt();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .ci_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        // Must contain the CI remediation section header.
        assert!(
            prompt.contains("## CI remediation context"),
            "CI fragment must be injected into revision directive:\n{prompt}",
        );
        // Must embed the attempt id.
        assert!(
            prompt.contains("`crm_frag_01`"),
            "CI fragment must include the attempt id:\n{prompt}",
        );
        // Must embed the failing check name.
        assert!(
            prompt.contains("`ci/test`"),
            "CI fragment must render the failing check list:\n{prompt}",
        );
        // Must embed the log excerpt.
        assert!(
            prompt.contains("ERROR: test failed at line 42"),
            "CI fragment must include the log excerpt:\n{prompt}",
        );
        // Must contain the pre-filled bk commands block.
        assert!(
            prompt.contains("bk build view 1329 --pipeline mypipeline"),
            "CI fragment must include pre-filled bk build view command:\n{prompt}",
        );
        assert!(
            prompt.contains("bk job log --pipeline mypipeline --build-number 1329 job-uuid-456"),
            "CI fragment must include pre-filled bk job log command:\n{prompt}",
        );
        // Must still contain the base revision directive spine.
        assert!(
            prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
            "base revision directive must still be present:\n{prompt}",
        );
    }

    /// A bespoke `ci_remediation`-kind execution routes to
    /// `compose_ci_remediation_prompt` (the retrigger playbook) instead
    /// of the revision directive.
    fn ci_remediation_execution(pr_url: &str) -> WorkExecution {
        WorkExecution::builder()
            .id("exec_cir_01")
            .work_item_id("task-cir-1")
            .kind(ExecutionKind::CiRemediation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .pr_url(pr_url)
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    #[test]
    fn ci_remediation_prompt_offers_mark_noop_for_non_rebounce() {
        // A bespoke (retrigger / stranded-rescue) ci_remediation worker
        // should be told it can declare a validated noop if the failure
        // already cleared — the engine re-probes live CI before honoring
        // it. `sample_ci_attempt` has `failure_kind: None`.
        let attempt = sample_ci_attempt();
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&ci_remediation_execution(&attempt.pr_url))
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .ci_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            prompt.contains("### If CI is already green (nothing to fix)"),
            "non-rebounce ci_remediation prompt must offer the validated mark-noop escape:\n{prompt}",
        );
        assert!(
            prompt.contains("boss engine ci mark-noop --attempt-id"),
            "non-rebounce ci_remediation prompt must include the mark-noop verb:\n{prompt}",
        );
    }

    #[test]
    fn ci_remediation_prompt_omits_mark_noop_for_rebounce() {
        // A merge_queue_rebounce failure lives on the synthetic merge
        // commit, so the PR's head-branch checks always read green. The
        // engine REJECTS a rebounce noop outright
        // (`handle_mark_ci_remediation_noop`), so the brief must NOT
        // surface it — mirroring the `!is_rebounce` gate in the sibling
        // `compose_ci_remediation_fragment`. (A rebounce normally
        // delivers via a revision; the stranded-rescue path can still
        // re-dispatch one through this bespoke prompt.)
        let mut attempt = sample_ci_attempt();
        attempt.failure_kind = Some("merge_queue_rebounce".into());
        attempt.before_commit_sha = Some("mergesha999".into());
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&ci_remediation_execution(&attempt.pr_url))
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .ci_attempt(&attempt)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            !prompt.contains("mark-noop"),
            "rebounce ci_remediation prompt must NOT surface mark-noop (engine rejects it):\n{prompt}",
        );
        // Sanity: we still produced the bespoke ci_remediation prompt.
        assert!(
            prompt.contains("CI remediation: PR #"),
            "expected the bespoke ci_remediation prompt to be generated:\n{prompt}",
        );
    }

    #[test]
    fn revision_directive_without_provenance_has_no_fragment() {
        // Operator-triggered revision: no conflict or CI attempt → no fragment.
        let work_item = revision_task_with_created_via(None, "operator");
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&revision_execution("https://github.com/org/repo/pull/77"))
                .work_item(&work_item)
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .build(),
        );
        assert!(
            !prompt.contains("## Conflict resolution context"),
            "no conflict fragment for operator revision:\n{prompt}",
        );
        assert!(
            !prompt.contains("## CI remediation context"),
            "no CI fragment for operator revision:\n{prompt}",
        );
        assert!(
            prompt.contains("Do NOT create a `boss/exec_*` bookmark"),
            "base revision directive must still be present:\n{prompt}",
        );
    }

    // -----------------------------------------------------------------------
    // editorial-rules block rendering (chore #5)
    // -----------------------------------------------------------------------

    #[test]
    fn editorial_rules_block_always_rendered_with_baked_in_rules() {
        // Default config: block always appears with baked-in rules.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("[editorial-rules]"),
            "editorial-rules block must always be present:\n{prompt}",
        );
        assert!(
            prompt.contains("[/editorial-rules]"),
            "editorial-rules closing tag must be present:\n{prompt}",
        );
        assert!(
            prompt.contains("exec_\u{2026}"),
            "baked-in identifier rule must be present:\n{prompt}",
        );
        assert!(
            prompt.contains("Boss worker"),
            "baked-in phrase rule must be present:\n{prompt}",
        );
    }

    #[test]
    fn editorial_rules_block_default_config_has_no_instructions_section() {
        // Default config: no instructions, no template, no enforcement banner.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            !prompt.contains("Product-specific rules"),
            "default config must not render instructions section:\n{prompt}",
        );
        assert!(
            !prompt.contains("Template policy"),
            "default config must not render template section:\n{prompt}",
        );
        assert!(
            !prompt.contains("Enforcement:"),
            "default config must not render enforcement banner:\n{prompt}",
        );
    }

    #[test]
    fn editorial_rules_block_with_instructions_renders_full_configured_sections() {
        let rules = boss_protocol::EditorialRules {
            instructions: Some("No emoji in PR titles.".to_owned()),
            ..Default::default()
        };
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .editorial_rules(&rules)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("Product-specific rules"),
            "configured product must render instructions section:\n{prompt}",
        );
        assert!(
            prompt.contains("No emoji in PR titles."),
            "configured product must include verbatim instructions:\n{prompt}",
        );
        assert!(
            prompt.contains("Enforcement:"),
            "configured product must render enforcement banner:\n{prompt}",
        );
    }

    #[test]
    fn editorial_rules_block_with_enforce_template_includes_template_text() {
        let tmpl = crate::pr_template::PrTemplate {
            text: "## Summary\n\n## Test plan\n".to_owned(),
            required_headings: vec!["Summary".to_owned(), "Test plan".to_owned()],
            source_path: std::path::PathBuf::from(".github/PULL_REQUEST_TEMPLATE.md"),
        };
        let pr_template_set = crate::pr_template::PrTemplateSet {
            default_template: Some(tmpl),
            named_templates: std::collections::HashMap::new(),
        };
        let rules = boss_protocol::EditorialRules {
            template_policy: boss_protocol::TemplatePolicy::Enforce,
            ..Default::default()
        };
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .editorial_rules(&rules)
                .pr_template_set(&pr_template_set)
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("Template policy: Enforce"),
            "enforce policy must appear in prompt:\n{prompt}",
        );
        assert!(
            prompt.contains("## Summary"),
            "template content must be rendered verbatim:\n{prompt}",
        );
        assert!(
            prompt.contains("## Test plan"),
            "template content must be rendered verbatim:\n{prompt}",
        );
        assert!(
            prompt.contains("Enforcement:"),
            "enforcement banner must be present for configured product:\n{prompt}",
        );
    }

    #[test]
    fn editorial_rules_block_appears_before_per_kind_directive() {
        // [editorial-rules] must appear before "Expected outcome for this run:"
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        let editorial_pos = prompt.find("[editorial-rules]").expect("editorial-rules block missing");
        let directive_pos = prompt
            .find("Expected outcome for this run:")
            .expect("per-kind directive missing");
        assert!(
            editorial_pos < directive_pos,
            "editorial-rules block must appear before the per-kind directive:\n{prompt}",
        );
    }

    #[test]
    fn editorial_rules_block_advise_template_policy_rendered() {
        let rules = boss_protocol::EditorialRules {
            template_policy: boss_protocol::TemplatePolicy::Advise,
            ..Default::default()
        };
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .editorial_rules(&rules)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("Template policy: Advise"),
            "advise policy must appear in prompt:\n{prompt}",
        );
        assert!(
            prompt.contains("Enforcement:"),
            "enforcement banner must be present when template policy is set:\n{prompt}",
        );
    }

    // -----------------------------------------------------------------------
    // editorial_controls feature flag (kill switch, default off)
    // -----------------------------------------------------------------------

    #[test]
    fn editorial_controls_flag_off_omits_block() {
        // With editorial_enabled = false, no [editorial-rules] block in the prompt.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(false)
                .build(),
        );
        assert!(
            !prompt.contains("[editorial-rules]"),
            "editorial-rules block must be absent when flag is off:\n{prompt}",
        );
        assert!(
            !prompt.contains("[/editorial-rules]"),
            "editorial-rules closing tag must be absent when flag is off:\n{prompt}",
        );
        // Prompt must still be a valid worker prompt (has execution context).
        assert!(
            prompt.contains("execution id"),
            "prompt must still contain execution context when editorial is off:\n{prompt}",
        );
    }

    #[test]
    fn editorial_controls_flag_off_omits_block_even_with_configured_rules() {
        // Rules configured on the product are also suppressed when the flag is off.
        let rules = boss_protocol::EditorialRules {
            instructions: Some("No emoji in titles.".to_owned()),
            ..Default::default()
        };
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .editorial_rules(&rules)
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(false)
                .build(),
        );
        assert!(
            !prompt.contains("[editorial-rules]"),
            "editorial-rules block must be absent when flag is off (even with rules configured):\n{prompt}",
        );
    }

    #[test]
    fn editorial_controls_flag_on_preserves_existing_behavior() {
        // With editorial_enabled = true, the [editorial-rules] block must be present
        // and contain the baked-in rules — identical to the original behavior.
        let prompt = compose_execution_prompt(
            ExecutionPromptParams::builder()
                .execution(&base_execution())
                .work_item(&chore_without_pr())
                .workspace_path(std::path::Path::new("/tmp/workspace"))
                .pr_template_set(&crate::pr_template::PrTemplateSet::default())
                .editorial_enabled(true)
                .build(),
        );
        assert!(
            prompt.contains("[editorial-rules]"),
            "editorial-rules block must be present when flag is on:\n{prompt}",
        );
        assert!(
            prompt.contains("[/editorial-rules]"),
            "editorial-rules closing tag must be present when flag is on:\n{prompt}",
        );
        assert!(
            prompt.contains("exec_\u{2026}"),
            "baked-in identifier rule must be present when flag is on:\n{prompt}",
        );
    }
}

#[cfg(test)]
mod compose_worker_spawn_tests {
    //! Targeted tests for `compose_worker_spawn` covering the `pr_review`
    //! branch: branch selection (PrReview vs. other kinds), the no-pr-url
    //! fallback to the generic implementer prompt, and the URL-only reviewer
    //! prompt rendered when the PR metadata fetch fails.
    use super::*;
    use crate::work::Task;
    use tempfile::TempDir;

    fn pr_review_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_rev123_01")
            .work_item_id("task-pr-1")
            .kind(ExecutionKind::PrReview)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn chore_execution() -> WorkExecution {
        WorkExecution::builder()
            .id("exec_chore123_01")
            .work_item_id("task-chore-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .repo_remote_url("git@github.com:org/repo.git")
            .workspace_path("/tmp/workspace")
            .created_at("2026-05-15T00:00:00Z")
            .build()
    }

    fn task_without_pr(task_id: &str) -> WorkItem {
        WorkItem::Chore(
            Task::builder()
                .id(task_id)
                .product_id("prod-1")
                .kind(TaskKind::Chore)
                .name("Add a new feature")
                .description("Feature description.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-15T00:00:00Z")
                .updated_at("2026-05-15T00:00:00Z")
                .autostart(false)
                .build(),
        )
    }

    fn task_with_pr(task_id: &str, pr_url: &str) -> WorkItem {
        match task_without_pr(task_id) {
            WorkItem::Chore(mut task) => {
                task.pr_url = Some(pr_url.into());
                WorkItem::Chore(task)
            }
            other => other,
        }
    }

    fn open_memory_db() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    /// When a `pr_review` execution's producing task has no `pr_url`, the
    /// branch falls back to the generic implementer prompt rather than
    /// rendering a reviewer prompt with no target PR.
    #[tokio::test]
    async fn pr_review_no_pr_url_falls_back_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let work_item = task_without_pr("task-pr-1");

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            false,
            0,
        )
        .await;

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "pr_review with no pr_url must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_rev123_01"),
            "fallback generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
    }

    /// When a `pr_review` execution has a `pr_url`, `compose_worker_spawn`
    /// calls `render_reviewer_initial_prompt` even when the upstream
    /// `fetch_pr_review_context` fails (no real `gh` in tests) — the
    /// URL-only reviewer prompt is still correctly formatted.
    #[tokio::test]
    async fn pr_review_with_pr_url_renders_reviewer_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/42";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            false,
            0,
        )
        .await;

        assert!(
            composed.prompt_text.contains("# PR review"),
            "pr_review with pr_url must render the reviewer prompt header:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("independent PR reviewer"),
            "reviewer prompt must identify the agent role:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains(pr_url),
            "reviewer prompt must include the PR URL:\n{}",
            composed.prompt_text,
        );
    }

    /// A non-`pr_review` execution kind (e.g. `ChoreImplementation`) must not
    /// enter the `pr_review` branch at all and must produce the generic
    /// implementer prompt.
    #[tokio::test]
    async fn non_pr_review_execution_routes_to_generic_prompt() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = chore_execution();
        let work_item = task_without_pr("task-chore-1");

        let composed = compose_worker_spawn(
            &db,
            "worker-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            false,
            0,
        )
        .await;

        assert!(
            !composed.prompt_text.contains("# PR review"),
            "non-pr_review execution must not render the reviewer prompt:\n{}",
            composed.prompt_text,
        );
        assert!(
            !composed.prompt_text.contains("independent PR reviewer"),
            "non-pr_review execution must not contain reviewer role text:\n{}",
            composed.prompt_text,
        );
        assert!(
            composed.prompt_text.contains("exec_chore123_01"),
            "generic prompt must contain the execution id:\n{}",
            composed.prompt_text,
        );
    }

    /// The reviewer prompt must not include implementer-only directives like
    /// "expected branch name" — reviewers must not commit or push anything.
    #[tokio::test]
    async fn pr_review_prompt_omits_branch_push_directives() {
        let workspace = TempDir::new().unwrap();
        let db = open_memory_db();
        let execution = pr_review_execution();
        let pr_url = "https://github.com/org/repo/pull/99";
        let work_item = task_with_pr("task-pr-1", pr_url);

        let composed = compose_worker_spawn(
            &db,
            "review-1",
            &execution,
            &work_item,
            workspace.path(),
            None,
            false,
            0,
        )
        .await;

        assert!(
            !composed.prompt_text.contains("expected branch name"),
            "reviewer prompt must not include the expected branch name directive:\n{}",
            composed.prompt_text,
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
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::protocol::{
        EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput, SpawnWorkerPaneResult,
    };
    use crate::work::{
        CreateChoreInput, CreateExecutionInput, CreateProductInput, CreateProjectInput, CreateTaskInput, EffortLevel,
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
        /// Run ids passed to `reap_worker_pane` — lets the mid-spawn
        /// cancel test assert the runner reaped the just-spawned pane.
        reaped: StdMutex<Vec<String>>,
    }

    impl CapturingSpawner {
        fn new() -> Self {
            Self {
                registry: WorkerRegistry::new(),
                live_states: LiveWorkerStateRegistry::new(),
                last: StdMutex::new(None),
                reaped: StdMutex::new(Vec::new()),
            }
        }

        fn spawn_input(&self) -> SpawnWorkerPaneInput {
            self.last
                .lock()
                .unwrap()
                .clone()
                .expect("expected SpawnWorkerPane to be sent")
        }

        fn reaped_run_ids(&self) -> Vec<String> {
            self.reaped.lock().unwrap().clone()
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
                        result: Ok(SpawnWorkerPaneResult { slot_id, shell_pid: 0 }),
                    })
                }
                other => panic!("unexpected request kind: {other:?}"),
            }
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        async fn reap_worker_pane(&self, run_id: &str) {
            self.reaped.lock().unwrap().push(run_id.to_owned());
            // Mirror production teardown enough for the test: drop the
            // slot mapping so a follow-up release is a no-op.
            let _ = self.registry.take_slot_for_run(run_id);
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
        }
    }

    fn sample_execution(workspace_path: &Path) -> WorkExecution {
        WorkExecution::builder()
            .id("exec-test-1")
            .work_item_id("task-1")
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
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
                .kind(TaskKind::Chore)
                .name("Improve top header (agent card) styling")
                .description("The gray header at the top is too cramped.")
                .status(TaskStatus::Todo)
                .created_at("2026-05-06T20:00:00Z")
                .updated_at("2026-05-06T20:00:00Z")
                .build(),
        )
    }

    /// Build a runner already bound to a `CapturingSpawner` and drive a
    /// run_execution against `workspace`. Returns the spawner so tests
    /// can inspect the captured request.
    ///
    /// `boss_event_path`: when `Some`, injects a known absolute path for
    /// the boss-event binary so the test is independent of host
    /// filesystem layout / env vars. Pass `None` for tests that don't
    /// inspect the hook command.
    async fn run_once(workspace: &TempDir, boss_event_path: Option<&Path>) -> Result<Arc<CapturingSpawner>> {
        // We need a Weak<dyn WorkerSpawner> the runner can upgrade.
        // Box-leak the Arc so it lives for the test's duration; the
        // tempdir guards the workspace lifetime.
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;

        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);
        if let Some(path) = boss_event_path {
            runner.set_boss_event_path(path.to_path_buf());
        }

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
        let _spawner = run_once(&workspace, None).await.unwrap();

        let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
        assert!(prompt_path.exists(), "expected {} to exist", prompt_path.display());
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
        let _spawner = run_once(&workspace, None).await.unwrap();
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        assert!(
            prompt.contains("the deliverable is a PR URL"),
            "implementation prompt must state the PR-URL acceptance criterion: {prompt}",
        );
        assert!(
            prompt.contains("on its own line"),
            "implementation prompt must tell the worker to print the URL on its own line: {prompt}",
        );
        assert!(
            prompt.contains("gh pr create") || prompt.contains("gh pr view") || prompt.contains("cube pr create"),
            "implementation prompt must mention gh pr commands or cube pr create: {prompt}",
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
        let _spawner = run_once(&workspace, None).await.unwrap();
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
        let expected_branch =
            crate::completion::expected_branch_name("exec-test-1", &boss_protocol::BranchNaming::BossExecPrefix, None);
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
        let spawner = run_once(&workspace, None).await.unwrap();
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
        // The first shell line re-prepends BOSS_BIN_DIR to PATH (so the
        // bundled `cube`/`boss`/`bossctl` win over any `~/bin` repobin
        // shim the login-shell init re-prepends), then unsets the API key
        // and invokes claude. See the comment at the construction site.
        assert!(
            input.initial_input.starts_with(
                "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; claude"
            ),
            "expected initial_input to re-prepend BOSS_BIN_DIR, unset ANTHROPIC_API_KEY, and invoke claude, got: {:?}",
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
            work_db.set_product_default_model(&product.id, Some(model)).unwrap();
        }
        let mut chore_input = chore_input;
        chore_input.product_id = product.id.clone();
        let chore = work_db.create_chore(chore_input).unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
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
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Untagged chore")
            .description("plain row, no effort/model")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        // The worker settings file lives outside the workspace; the
        // engine points claude at it with `--settings '<abs-path>'`,
        // positioned before the positional prompt arg.
        let settings_path = crate::worker_setup::worker_settings_path(workspace.path());
        assert_eq!(
            input.initial_input,
            format!(
                "[ -n \"$BOSS_BIN_DIR\" ] && export PATH=\"$BOSS_BIN_DIR:$PATH\"; unset ANTHROPIC_API_KEY; claude --model {} --permission-mode auto --settings '{}' \"$(cat .claude/initial-prompt.txt)\"\n",
                crate::effort::ENGINE_DEFAULT_MODEL,
                settings_path.display(),
            ),
            "untagged row should re-prepend BOSS_BIN_DIR to PATH, then spawn with the engine default model, --permission-mode auto (Opus), --settings <worker file>, and no --effort",
        );

        // No addendum prepended — the existing implementation framing
        // must be the first thing the worker sees.
        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
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
    /// `trivial` row dispatches with `--model sonnet --effort low`
    /// and no prompt addendum. Per #746 ("don't use haiku") the model
    /// floor is Sonnet, not Haiku, even at the trivial tier — only the
    /// effort value stays `low`. See
    /// [`crate::effort::default_model_for_level`].
    #[tokio::test]
    async fn trivial_row_spawn_uses_sonnet_at_low_effort() {
        let workspace = TempDir::new().unwrap();
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Apply resize-cursor fix to nav divider")
            .description("one-line CSS tweak")
            .effort_level(EffortLevel::Trivial)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model sonnet"),
            "trivial row must spawn Sonnet (#746: never Haiku), got: {:?}",
            input.initial_input,
        );
        assert!(
            !input.initial_input.contains("--model haiku"),
            "trivial row must NOT spawn Haiku (#746), got: {:?}",
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
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
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Add created_via provenance to chore/task creates")
            .description("multi-file edit with judgement calls")
            .effort_level(EffortLevel::Medium)
            .model_override("opus")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
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
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Investigate isolated test instance")
            .description("multi-subsystem investigation")
            .effort_level(EffortLevel::Large)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
        let input = spawner.spawn_input();

        assert!(
            input.initial_input.contains("--model opus"),
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
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
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Untagged on Sonnet-defaulted product")
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, Some("claude-sonnet-4-6"))
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Trivial chore")
                    .effort_level(EffortLevel::Trivial)
                    .build(),
            )
            .unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
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
        // #746: trivial floors to Sonnet, never Haiku.
        assert_eq!(spawn.model, "sonnet");
        assert_eq!(spawn.prompt_addendum, None);
    }

    /// Regression for T1647: `PaneSpawnRunner::run_execution` must return
    /// `ReviewerPaneAlive` (not `WaitingHuman`) for `PrReview` executions so
    /// the execution stays in `running` while the reviewer pane is alive.
    ///
    /// This pins the runner.rs change at the `PaneSpawnRunner` level.
    /// Reverting `run_execution` back to always returning `WaitingHuman`
    /// would cause this test to fail even if the badge-SQL test in t01.rs
    /// still passes.
    #[tokio::test]
    async fn pr_review_execution_yields_reviewer_pane_alive() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Some chore being reviewed")
                    .autostart(false)
                    .build(),
            )
            .unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg.clone(), work_db.clone(), flags.clone());
        runner.set_server_state(weak.clone());

        // Build a PrReview execution; no pr_url on the chore is fine —
        // the runner falls back to the generic prompt, which is irrelevant
        // to the wait_state assertion.
        let mut pr_review_exec = sample_execution(workspace.path());
        pr_review_exec.kind = ExecutionKind::PrReview;
        pr_review_exec.work_item_id = chore.id.clone();

        let outcome = runner
            .run_execution(
                "review-1",
                &pr_review_exec,
                &WorkItem::Chore(chore.clone()),
                workspace.path(),
                Some("change-pr-review"),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome.wait_state,
            RunWaitState::ReviewerPaneAlive,
            "PaneSpawnRunner must return ReviewerPaneAlive for PrReview executions so the \
             execution stays in running (not waiting_human) while the reviewer pane is alive"
        );

        // Verify that a non-PrReview kind still yields WaitingHuman.
        let runner2 = PaneSpawnRunner::new(cfg, work_db, flags);
        runner2.set_server_state(weak);
        let mut chore_exec = sample_execution(workspace.path());
        chore_exec.kind = ExecutionKind::ChoreImplementation;
        chore_exec.work_item_id = chore.id.clone();

        let outcome2 = runner2
            .run_execution(
                "worker-1",
                &chore_exec,
                &WorkItem::Chore(chore),
                workspace.path(),
                Some("change-chore"),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome2.wait_state,
            RunWaitState::WaitingHuman,
            "PaneSpawnRunner must return WaitingHuman for non-PrReview executions"
        );
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
        let chore_input = CreateChoreInput::builder()
            .product_id(String::new())
            .name("Any chore")
            .effort_level(EffortLevel::Large)
            .build();
        let (spawner, _chore) = run_once_with_chore(&workspace, chore_input, None).await.unwrap();
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
        let spawner = run_once(&workspace, None).await.unwrap();
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
            input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_LEASE_ID"),
            "expected BOSS_LEASE_ID to be set"
        );
        assert!(
            input.env.iter().any(|EnvVar { key, .. }| key == "BOSS_EVENTS_SOCKET"),
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .worker_pool_size(8)
                .automation_pool_size(3)
                .build(),
            None,
        ));
        let work_db = Arc::new(WorkDb::open(workspace.path().join("state.db")).unwrap());
        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
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
        let spawner = run_once(&workspace, None).await.unwrap();

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

    /// T981 regression — the mid-spawn cancel reconciliation. When the
    /// execution row is cancelled while the `SpawnWorkerPane` round-trip
    /// is in flight, `run_execution` must, on return, (i) reap the
    /// just-spawned pane (the pid is now known, so the reap is no longer
    /// a no-op) and (ii) report `CancelledDuringSpawn` so the coordinator
    /// releases the cube lease the cancel path deliberately left held.
    /// Without this the worker survives unreaped in a workspace the
    /// engine believes is free, which is what produced the duplicate
    /// dispatch into a shared workspace.
    #[tokio::test]
    async fn run_execution_reaps_and_signals_when_cancelled_mid_spawn() {
        let workspace = TempDir::new().unwrap();
        let spawner: Arc<CapturingSpawner> = Arc::new(CapturingSpawner::new());
        let weak: Weak<dyn crate::spawn_flow::WorkerSpawner> =
            Arc::downgrade(&spawner) as Weak<dyn crate::spawn_flow::WorkerSpawner>;
        let cfg = Arc::new(crate::config::RuntimeConfig::from_parts(
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Sort struct definitions")
                    .build(),
            )
            .unwrap();
        let ready = work_db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        // Start the run (ready → running, lease attached) — this is the
        // exact state the row is in when the spawn round-trip is in
        // flight — then cancel it, mirroring a kanban drag-to-Backlog
        // landing inside the spawn window.
        let (execution, _run) = work_db
            .start_execution_run(
                &ready.id,
                "worker-1",
                "foo",
                "lease-1",
                "foo-agent-001",
                workspace.path().to_str().unwrap(),
            )
            .unwrap();
        assert!(work_db.cancel_running_execution(&execution.id).unwrap());

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db.clone(), flags);
        runner.set_server_state(weak);

        let chore_item = work_db.get_work_item(&chore.id).unwrap();
        let outcome = runner
            .run_execution("worker-1", &execution, &chore_item, workspace.path(), Some("change-1"))
            .await
            .unwrap();

        assert_eq!(
            outcome.wait_state,
            RunWaitState::CancelledDuringSpawn,
            "a cancel that races the spawn window must yield CancelledDuringSpawn",
        );
        assert!(
            outcome.slot_id.is_none(),
            "the pane was reaped, so the coordinator must not keep the pool slot claimed",
        );
        assert_eq!(
            spawner.reaped_run_ids().as_slice(),
            [execution.id.as_str()],
            "the runner must reap the just-spawned pane for the cancelled execution",
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
                description: Some("Instrument the auto-dispatcher so every spawn decision is traceable.".to_owned()),
                goal: Some("Operators can answer 'why did this task spawn now' from logs alone.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();
        let task = work_db
            .create_task(
                CreateTaskInput::builder()
                    .product_id(product.id.clone())
                    .project_id(project.id.clone())
                    .name("Tag dispatch logs with execution kind")
                    .build(),
            )
            .unwrap();

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::TaskImplementation;
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
                    "Surface every running worker's live state on the kanban without polling.".to_owned(),
                ),
                goal: Some("Operators can see what every active worker is doing without opening panes.".to_owned()),
                autostart: false,
                no_design_task: false,
            })
            .unwrap();

        // Find the design task `create_project` auto-filed for this
        // project. It sorts ordinal-0 with `kind = 'design'`.
        let design_task = work_db
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("create_project should auto-file a kind='design' task");

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::ProjectDesign;
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

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
            prompt.contains("Operators can see what every active worker is doing without opening panes."),
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
            crate::config::WorkConfig::builder()
                .cwd(workspace.path().to_path_buf())
                .db_path(workspace.path().join("state.db"))
                .build(),
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
            .list_tasks(&product.id, Some(&project.id), None, false)
            .unwrap()
            .into_iter()
            .find(|t| t.kind == TaskKind::Design)
            .expect("create_project should auto-file a kind='design' task");

        let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
            workspace.path().join("feature-flags.toml"),
        ));
        let runner = PaneSpawnRunner::new(cfg, work_db, flags);
        runner.set_server_state(weak);

        let mut execution = sample_execution(workspace.path());
        execution.kind = ExecutionKind::ProjectDesign;
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

        let prompt = std::fs::read_to_string(workspace.path().join(".claude").join("initial-prompt.txt")).unwrap();

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
        // Inject a fake boss-event at a known absolute temp path so this
        // test is deterministic on every agent — no host PATH lookup, no
        // BOSS_EVENT_BIN env var, no runfiles, no bazel-bin dependency.
        let fake_bin_dir = TempDir::new().unwrap();
        let fake_boss_event = fake_bin_dir.path().join("boss-event");
        std::fs::write(&fake_boss_event, b"").unwrap();

        let workspace = TempDir::new().unwrap();
        let _spawner = run_once(&workspace, Some(&fake_boss_event)).await.unwrap();

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
        let expected_path = fake_boss_event.to_str().unwrap();
        assert!(
            settings.contains(expected_path),
            "expected absolute boss-event path {} in settings file, got: {}",
            expected_path,
            settings,
        );
        assert!(
            !settings.contains("'boss-event'") && !settings.contains("\"boss-event\""),
            "settings file must not invoke `boss-event` as a bare name, got: {}",
            settings,
        );
    }

    /// `BOSS_EVENT_BIN` short-circuits everything else.
    #[test]
    fn resolve_boss_event_prefers_env_override() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let override_path = PathBuf::from("/opt/whatever/boss-event");
        let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path), None, None);
        assert_eq!(resolved, Some(override_path));
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
        assert_eq!(resolved, Some(bundle_shim));
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
        assert_eq!(resolved, Some(shim));
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
        assert_eq!(resolved, Some(shim));
    }

    /// When nothing resolves the function returns `None` — the caller
    /// (`boss_event_binary`) turns this into a hard panic rather than
    /// silently baking a bare `boss-event` into hook commands (which
    /// causes `command not found` in the worker's sanitized PATH).
    #[test]
    fn resolve_boss_event_returns_none_when_nothing_resolves() {
        let dir = TempDir::new().unwrap();
        let engine = dir.path().join("engine");
        std::fs::write(&engine, b"").unwrap();
        let resolved = resolve_boss_event_binary(&engine, None, None, None, None);
        assert_eq!(resolved, None);
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
        assert_eq!(resolved, Some(stable_shim));
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
