use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;

use crate::config::RuntimeConfig;
use crate::coordinator::slot_id_from_worker_id;
use crate::pane_summary;
use crate::spawn_flow::{StartWorkerInput, start_worker};
use crate::work::{Project, Task, WorkDb, WorkExecution, WorkItem};
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
        resolve_boss_event_binary(&engine_path, workspace.as_deref(), env_override.as_deref())
    }
}

/// Pure resolver for the absolute path of the `boss-event` shim
/// that the worker pane invokes from `settings.json`. Pulled out
/// as a free function so tests can pass synthetic `engine_path` /
/// `workspace_dir` / env values without monkey-patching globals.
///
/// Resolution order:
///   1. `BOSS_EVENT_BIN` env override (caller-controlled).
///   2. Bazel runfiles next to the engine binary
///      (`<engine_path>.runfiles/_main/tools/boss/event-shim/boss-event`).
///      Requires the engine `rust_binary` to declare a `data` dep
///      on `//tools/boss/event-shim:boss-event` — without it bazel
///      doesn't include the shim in the engine's runfiles.
///   3. Workspace `bazel-bin` symlink
///      (`<workspace>/bazel-bin/tools/boss/event-shim/boss-event`)
///      when `BUILD_WORKSPACE_DIRECTORY` is set (i.e., the engine
///      was launched via `bazel run` from a checkout).
///   4. Cargo / hand-built sibling: `<engine_dir>/boss-event`.
///   5. Bare name `boss-event` — only useful if the worker's PATH
///      happens to include it (today it doesn't, on purpose).
pub(crate) fn resolve_boss_event_binary(
    engine_path: &Path,
    workspace_dir: Option<&Path>,
    env_override: Option<&Path>,
) -> PathBuf {
    if let Some(override_path) = env_override {
        return override_path.to_path_buf();
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
        // For project_design executions the work item is the
        // project's synthetic design task. The task itself carries
        // little context (name = "Design"), so look up the parent
        // project to enrich the prompt with the project's name,
        // description, and goal — that's the information the worker
        // actually needs to produce a useful design pass.
        let parent_project = match work_item {
            WorkItem::Task(task) if task.kind == "design" => {
                if let Some(project_id) = task.project_id.as_deref() {
                    self.work_db.get_project(project_id).ok()
                } else {
                    None
                }
            }
            _ => None,
        };
        let prompt_text = compose_execution_prompt(
            execution,
            work_item,
            parent_project.as_ref(),
            workspace_path,
            cube_change_id,
        );
        let prompt_path = workspace_path.join(".claude").join("initial-prompt.txt");
        if let Some(parent) = prompt_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&prompt_path, &prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;
        let initial_input = "claude \"$(cat .claude/initial-prompt.txt)\"\n".to_owned();

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
        let title_summary =
            pane_summary::get_or_generate(&self.work_db, api_key.as_deref(), work_item).await;

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
        })
    }
}

fn compose_execution_prompt(
    execution: &WorkExecution,
    work_item: &WorkItem,
    parent_project: Option<&Project>,
    workspace_path: &Path,
    cube_change_id: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are a reusable Boss worker running one execution inside a dedicated repo workspace.\n",
    );
    prompt.push_str("The current session cwd is already set to that workspace.\n");
    prompt.push_str("Do the work directly in the repository checkout before ending this run.\n");
    prompt.push_str("Avoid asking the human for permission during this pass; when you need review or direction, stop and summarize it clearly.\n\n");
    prompt.push_str("Execution context:\n");
    prompt.push_str(&format!("- execution id: `{}`\n", execution.id));
    prompt.push_str(&format!("- execution kind: `{}`\n", execution.kind));
    prompt.push_str(&format!("- workspace: `{}`\n", workspace_path.display()));
    prompt.push_str(&format!("- work item: `{}`\n", work_item_name(work_item)));
    if let Some(cube_change_id) = cube_change_id {
        prompt.push_str(&format!("- local change: `{}`\n", cube_change_id));
    }
    // For project_design executions the work item is now the
    // synthetic `kind = 'design'` task, which is intentionally
    // sparse (`name = "Design"`, no description). The interesting
    // context — what is the project actually for? — lives on the
    // parent project. Surface it inline so the worker has the
    // project's name/goal/description to anchor the design pass.
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
    prompt.push_str(match execution.kind.as_str() {
        "project_design" => {
            "Expected outcome for this run:\n- inspect the repository and relevant context,\n- draft or update a repo-backed design artifact,\n- identify likely follow-up tasks or phases,\n- stop once the design pass is in a state a human can review.\n"
        }
        "task_implementation" | "chore_implementation" => {
            "Expected outcome for this run:\n- implement the requested change in the workspace,\n- run relevant local validation when practical,\n- stop once the work is ready for a human to review or redirect.\n"
        }
        _ => {
            "Expected outcome for this run:\n- make concrete progress on the assigned work,\n- leave the workspace in a reviewable state,\n- stop with a concise review summary.\n"
        }
    });
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
        prompt.push_str(
            "\nAcceptance criterion: when you believe the work is done, the deliverable is a PR URL.\n\
             - Push your branch (`jj git push -b <bookmark>`) and open a PR with `gh pr create` if one does not already exist for this branch.\n\
             - If a PR already exists for this branch (e.g. you are resuming work or addressing review comments), push your new commits to update it instead of opening a duplicate. Check with `gh pr view` from inside the workspace.\n\
             - Print the PR URL on its own line as the final thing in your final response so the engine can pick it up automatically.\n",
        );
    }
    prompt.push_str("\nRespond with concise markdown using exactly these sections:\n");
    prompt.push_str("## Summary\n## Validation\n## Open Questions\n");
    prompt
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
    use crate::work::{Task, WorkExecution, WorkItem};
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
        let resolved = resolve_boss_event_binary(&engine, None, Some(&override_path));
        assert_eq!(resolved, override_path);
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

        let resolved = resolve_boss_event_binary(&engine, None, None);
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

        let resolved = resolve_boss_event_binary(&engine, Some(&workspace), None);
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
        let resolved = resolve_boss_event_binary(&engine, None, None);
        assert_eq!(resolved, PathBuf::from("boss-event"));
    }
}
