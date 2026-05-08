//! End-to-end worker spawn helper.
//!
//! Combines the pieces that need to fire when the engine starts a
//! pane-hosted worker for a run:
//!
//! 1. Render and write `<workspace>/.claude/CLAUDE.md` and
//!    `<workspace>/.claude/settings.json` from the templates in
//!    [`crate::worker_setup`].
//! 2. Send `SpawnWorkerPane` to the registered app session via the
//!    engine→app dispatch on `ServerState`.
//! 3. Register the returned shell pid in the
//!    [`crate::worker_registry::WorkerRegistry`] so subsequent hook
//!    events from the boss-event shim can be correlated back to the
//!    run via the registry's ancestor walk.
//!
//! This module is just the helper; the pane-driven runner that drives
//! it lives in `runner::PaneSpawnRunner`.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use boss_protocol::WorkItemBinding;
use thiserror::Error;
use tokio::time::Duration;

use crate::live_worker_state::{DEFAULT_LAUNCH_MODEL, LiveWorkerStateRegistry};
use crate::protocol::{
    EngineToAppError, EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput,
    SpawnWorkerPaneResult,
};
use crate::worker_registry::WorkerRegistry;
use crate::worker_setup::{WorkerSetupInput, WrittenFiles, write_workspace_files};

/// Sanitized PATH for worker panes. Excludes `~/bin` (where the
/// `bossctl` symlink lives in this user's setup) and any other
/// per-user bin dir, so a worker that tries to invoke `bossctl`
/// directly fails with a PATH miss. Per `v2-design-risks.md` R3.
///
/// Order: Homebrew first (modern Apple-silicon paths), then the
/// system bins. `/usr/local/bin` is included for legacy x86 brew
/// installs but Apple-silicon machines ignore it.
const WORKER_SANITIZED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";

/// Env keys allowed to flow from the runner's `extra_env` into the
/// worker pane. Anything outside this set is dropped with a warning;
/// the goal is to prevent ambient env (e.g., a stray
/// `BOSS_CONTROL_SOCKET` left over from an interactive run, or
/// arbitrary tokens carried from the user's shell) from reaching
/// workers. Standard env (HOME, USER, SHELL, TERM, LANG, locale)
/// inherits naturally from the app process and is not in this list
/// because we never set it explicitly here.
const WORKER_EXTRA_ENV_ALLOWLIST: &[&str] = &[
    "BOSS_TASK_ID",
    "CUBE_LEASE_ID",
    "CUBE_REPO",
];

/// Value forced into `EDITOR`/`VISUAL`/`GIT_EDITOR`/`JJ_EDITOR` for
/// worker panes. `false` exits non-zero immediately, so any tool that
/// falls through to `$EDITOR` aborts loudly instead of silently popping
/// the user's vim/VS Code window. The CLAUDE.md template tells workers
/// to always pass `-m` inline; this is the safety net that turns a
/// forgotten `-m` into a fast, recoverable error.
const WORKER_EDITOR_NOOP: &str = "false";

#[derive(Debug, Clone)]
pub struct StartWorkerInput {
    pub run_id: String,
    pub lease_id: String,
    /// Slot the engine has already claimed for this worker (1-indexed,
    /// matches the app's WorkersWorkspaceModel slot numbering). The
    /// engine is the source of truth for slot allocation; the app's
    /// job is to host the pane in this exact slot or fail with
    /// `EngineToAppError::SlotBusy`.
    pub slot_id: u8,
    pub workspace_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub boss_event_path: PathBuf,
    pub initial_input: String,
    /// Extra env vars to thread to the worker on top of the ones the
    /// settings.json template injects (`BOSS_EVENTS_SOCKET`,
    /// `BOSS_LEASE_ID`).
    pub extra_env: Vec<(String, String)>,
    /// Optional 2–4 word summary to display in the pane titlebar in
    /// place of the run id. The app keeps the run id available as a
    /// tooltip; this field is purely visual. `None` means the
    /// engine had no summary to offer (e.g., generation failed) —
    /// the app falls back to showing the run id.
    pub title_summary: Option<String>,
    /// Work-item linkage stamped onto the resulting `LiveWorkerState`
    /// so `bossctl agents list` / `agents status` can resolve "the
    /// worker on chore X" without prompting for a slot. `None` from
    /// callers that don't have a work item (tests).
    pub work_item_binding: Option<WorkItemBinding>,
}

#[derive(Debug)]
pub struct StartedWorker {
    pub slot_id: u8,
    pub shell_pid: i32,
    pub written_files: WrittenFiles,
}

#[derive(Debug, Error)]
pub enum StartWorkerError {
    #[error("writing worker config: {0}")]
    WriteFiles(std::io::Error),
    #[error("sending SpawnWorkerPane to app: {0}")]
    Send(#[from] crate::app::SendToAppError),
    #[error("app reported spawn error: {0:?}")]
    AppError(EngineToAppError),
    #[error("app responded with unexpected response variant")]
    ResponseKindMismatch,
}

/// Public API for callers that want to wire pane-spawning into the
/// coordinator (or a test). The trait is implemented by
/// [`crate::app::ServerState`]; users should typically call through
/// `ServerState` directly, but the trait makes the dependency
/// explicit and lets stub implementations stand in for unit tests.
#[async_trait::async_trait]
pub trait WorkerSpawner: Send + Sync {
    async fn send_to_app_request(
        &self,
        request: EngineToAppRequest,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, crate::app::SendToAppError>;

    fn worker_registry(&self) -> &WorkerRegistry;

    /// Engine's live per-slot state registry. Implementations return
    /// `None` from in-process tests that don't care about the live
    /// state surface; the spawn flow then skips the registration
    /// step. Production `ServerState` always returns `Some`.
    fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
        None
    }

    /// Hook called after `LiveWorkerStateRegistry` is updated so the
    /// caller can broadcast the snapshot on the worker live-state
    /// topic. Default no-op for tests.
    async fn publish_live_worker_states(&self) {}
}

/// Render the worker-config files, ask the app to spawn a pane,
/// register the resulting shell pid for hook-event correlation, and
/// return the slot id + pid for the caller to record.
pub async fn start_worker<S: WorkerSpawner + ?Sized>(
    spawner: &S,
    input: StartWorkerInput,
    spawn_timeout: StdDuration,
) -> Result<StartedWorker, StartWorkerError> {
    // 1. Write CLAUDE.md and settings.json into the workspace.
    let setup = WorkerSetupInput {
        run_id: input.run_id.clone(),
        lease_id: input.lease_id.clone(),
        workspace_path: input.workspace_path.clone(),
        events_socket_path: input.events_socket_path.clone(),
        boss_event_path: input.boss_event_path.clone(),
    };
    let written = write_workspace_files(&setup).map_err(StartWorkerError::WriteFiles)?;

    // 2. Build the SpawnWorkerPane request. Workers get a strict env
    //    allowlist (per `v2-design-risks.md` R3): a sanitized PATH
    //    (no `bossctl`), the engine-injected `BOSS_EVENTS_SOCKET` and
    //    `BOSS_LEASE_ID`, and any caller-provided `extra_env` keys
    //    that survive the allowlist filter. Anything else is dropped.
    //
    //    Editor env vars are forced to `false` so a worker that runs
    //    `git commit` / `jj describe` without `-m` doesn't pop the
    //    user's vim/VS Code window — the command exits non-zero and
    //    the worker corrects course by passing `-m` inline. The
    //    matching CLAUDE.md guidance tells the worker the rule; this
    //    is the belt that catches it when the suspenders slip.
    let mut env = vec![
        EnvVar {
            key: "PATH".into(),
            value: WORKER_SANITIZED_PATH.into(),
        },
        EnvVar {
            key: "BOSS_EVENTS_SOCKET".into(),
            value: input.events_socket_path.display().to_string(),
        },
        EnvVar {
            key: "BOSS_LEASE_ID".into(),
            value: input.lease_id.clone(),
        },
        EnvVar {
            // Read by `boss-event` and embedded in every hook payload
            // as `_boss_run_id`. The engine uses this to correlate
            // hook events to runs without depending on a working
            // shell-pid lookup. `proc_listpids` in the app side is
            // still a TODO, and without it `WorkerRegistry`'s pid
            // map stays empty, `lookup_with_ancestor_walk` returns
            // None, and `dispatch_live_worker_state` silently skips
            // every event — that's the bug that pinned every worker's
            // activity at `Spawning` regardless of what the worker
            // was actually doing.
            key: "BOSS_RUN_ID".into(),
            value: input.run_id.clone(),
        },
        EnvVar {
            key: "EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "VISUAL".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "GIT_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
        EnvVar {
            key: "JJ_EDITOR".into(),
            value: WORKER_EDITOR_NOOP.into(),
        },
    ];
    for (k, v) in input.extra_env {
        if WORKER_EXTRA_ENV_ALLOWLIST.contains(&k.as_str()) {
            env.push(EnvVar { key: k, value: v });
        } else {
            tracing::warn!(
                key = %k,
                "spawn_flow: dropping non-allowlisted env key from worker spawn",
            );
        }
    }

    let claimed_slot = input.slot_id;
    let response = spawner
        .send_to_app_request(
            EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
                run_id: input.run_id.clone(),
                workspace_path: input.workspace_path.display().to_string(),
                slot_id: claimed_slot,
                initial_input: input.initial_input,
                env,
                summary: input.title_summary,
            }),
            Duration::from_secs(spawn_timeout.as_secs()),
        )
        .await?;

    let SpawnWorkerPaneResult { slot_id, shell_pid } = match response {
        EngineToAppResponse::SpawnWorkerPane { result } => match result {
            Ok(value) => value,
            Err(err) => return Err(StartWorkerError::AppError(err)),
        },
        EngineToAppResponse::ReleaseWorkerPane { .. }
        | EngineToAppResponse::SendToPane { .. }
        | EngineToAppResponse::FocusWorkerPane { .. }
        | EngineToAppResponse::InterruptWorkerPane { .. } => {
            return Err(StartWorkerError::ResponseKindMismatch);
        }
    };

    // The engine dictates the slot; the app's response slot is just a
    // confirmation echo. A mismatch means the app picked a different
    // slot than we asked for, which would re-introduce the dual
    // allocator the engine-owns-slots refactor exists to remove.
    debug_assert_eq!(
        slot_id, claimed_slot,
        "app honored a different slot ({slot_id}) than the engine claimed ({claimed_slot})"
    );

    // 3. Register the shell pid against the run id so the events
    //    socket can correlate hook events from descendants of the
    //    spawned shell back to this run, and remember the slot id so
    //    follow-up `SendToPane` requests (e.g., probe injection) can
    //    route by run id.
    spawner
        .worker_registry()
        .register_run_slot(input.run_id.clone(), slot_id);
    if shell_pid > 0 {
        spawner
            .worker_registry()
            .register(shell_pid, input.run_id.clone());
    } else {
        tracing::warn!(
            slot_id,
            "spawn returned shell_pid 0; hook-event correlation will fail until a real pid is wired (TODO: proc_listpids in app)",
        );
    }

    // 4. Stamp the initial LiveWorkerState so bossctl/UI immediately
    //    see "Spawning" with the launch-default model — no more
    //    "Claude Unknown" while we wait for SessionStart to fire.
    if let Some(live_states) = spawner.live_worker_state_registry() {
        live_states.register_spawn(
            slot_id,
            input.run_id,
            DEFAULT_LAUNCH_MODEL,
            shell_pid,
            input.work_item_binding,
        );
        spawner.publish_live_worker_states().await;
    }

    Ok(StartedWorker {
        slot_id,
        shell_pid,
        written_files: written,
    })
}

/// Stub helper used by [`Path`] callers that want a default events
/// socket path; mirrors the resolver in `app.rs::default_events_socket_path`.
/// Kept here so callers outside `app.rs` (tests) don't need to depend
/// on `app.rs` private internals.
#[allow(dead_code)]
pub fn default_events_socket_path() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("BOSS_EVENTS_SOCKET") {
        return Some(override_path.into());
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/Application Support/Boss/events.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SendToAppError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    struct StubSpawner {
        registry: WorkerRegistry,
        spawn_calls: Arc<AtomicUsize>,
        canned_response: Result<EngineToAppResponse, SendToAppError>,
        last_request: std::sync::Mutex<Option<EngineToAppRequest>>,
    }

    #[async_trait::async_trait]
    impl WorkerSpawner for StubSpawner {
        async fn send_to_app_request(
            &self,
            request: EngineToAppRequest,
            _timeout: Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_request.lock().unwrap() = Some(request);
            self.canned_response.clone().map_err(|e| match e {
                SendToAppError::NotRegistered => SendToAppError::NotRegistered,
                SendToAppError::AppDisconnected => SendToAppError::AppDisconnected,
                SendToAppError::Timeout => SendToAppError::Timeout,
                SendToAppError::ResponseKindMismatch(s) => SendToAppError::ResponseKindMismatch(s),
            })
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }
    }

    impl StubSpawner {
        fn last_spawn_env(&self) -> Vec<(String, String)> {
            match self.last_request.lock().unwrap().clone() {
                Some(EngineToAppRequest::SpawnWorkerPane(input)) => input
                    .env
                    .into_iter()
                    .map(|EnvVar { key, value }| (key, value))
                    .collect(),
                _ => panic!("last request was not SpawnWorkerPane"),
            }
        }
    }

    fn sample_input(workspace: &TempDir) -> StartWorkerInput {
        StartWorkerInput {
            run_id: "run-test".into(),
            lease_id: "lease-test".into(),
            slot_id: 3,
            workspace_path: workspace.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            boss_event_path: PathBuf::from("/tmp/boss-event"),
            initial_input: "claude\n".into(),
            extra_env: vec![],
            title_summary: None,
            work_item_binding: None,
        }
    }

    #[tokio::test]
    async fn happy_path_writes_files_sends_request_and_registers_pid() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 42_111,
                }),
            }),
        };

        let started = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(started.slot_id, 3);
        assert_eq!(started.shell_pid, 42_111);
        assert!(started.written_files.claude_md_path.exists());
        assert!(started.written_files.settings_path.exists());
        assert!(started.written_files.gitignore_path.exists());
        assert_eq!(registry.lookup(42_111).as_deref(), Some("run-test"));
    }

    #[tokio::test]
    async fn shell_pid_zero_skips_registration_with_warning() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 0,
                }),
            }),
        };

        let started = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(started.shell_pid, 0);
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn app_error_propagates() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Err(EngineToAppError::NoAvailableSlot),
            }),
        };

        let result = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await;
        assert!(matches!(
            result,
            Err(StartWorkerError::AppError(EngineToAppError::NoAvailableSlot))
        ));
        assert!(registry.is_empty());
    }

    /// Engine-owns-slots invariant: the slot the runner claimed for
    /// this worker (set on `StartWorkerInput.slot_id`) must reach
    /// the app verbatim on `SpawnWorkerPaneInput.slot_id`. A drop
    /// here would re-allow the app's old firstIndex(where:) heuristic
    /// to silently override the engine's pick.
    #[tokio::test]
    async fn spawn_request_carries_engine_claimed_slot_id() {
        let workspace = TempDir::new().unwrap();
        let spawner = StubSpawner {
            registry: WorkerRegistry::new(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 7,
                    shell_pid: 99,
                }),
            }),
        };
        let mut input = sample_input(&workspace);
        input.slot_id = 7;

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        let last = spawner.last_request.lock().unwrap().clone().unwrap();
        match last {
            EngineToAppRequest::SpawnWorkerPane(req) => {
                assert_eq!(req.slot_id, 7);
            }
            other => panic!("expected SpawnWorkerPane request, got {other:?}"),
        }
    }

    /// If the app and the engine disagree about which slot is free
    /// (engine asks for slot N; app already hosts a session there),
    /// the app returns `SlotBusy` and the spawn flow surfaces it as
    /// `StartWorkerError::AppError(SlotBusy)` without registering a
    /// pid for the run. The coordinator can then handle the
    /// disagreement explicitly instead of the app silently picking a
    /// different slot.
    #[tokio::test]
    async fn slot_busy_error_propagates_without_registering_pid() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Err(EngineToAppError::SlotBusy),
            }),
        };

        let result = start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await;
        assert!(
            matches!(
                result,
                Err(StartWorkerError::AppError(EngineToAppError::SlotBusy))
            ),
            "expected SlotBusy app error, got {result:?}",
        );
        assert!(
            registry.is_empty(),
            "registry must be empty when the app rejects the spawn — no pid to track",
        );
    }

    #[tokio::test]
    async fn write_failure_does_not_send_request() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawner = StubSpawner {
            registry,
            spawn_calls: spawn_calls.clone(),
            last_request: std::sync::Mutex::new(None),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 1,
                    shell_pid: 1,
                }),
            }),
        };

        // Point at a path that's a regular file, not a directory, so
        // create_dir_all fails inside write_workspace_files.
        let blocked = workspace.path().join("blocked");
        std::fs::write(&blocked, b"i am a file").unwrap();
        let mut input = sample_input(&workspace);
        input.workspace_path = blocked;

        let result = start_worker(&spawner, input, StdDuration::from_secs(1)).await;
        assert!(matches!(result, Err(StartWorkerError::WriteFiles(_))));
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
    }

    fn ok_spawner_capturing() -> StubSpawner {
        StubSpawner {
            registry: WorkerRegistry::new(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            last_request: std::sync::Mutex::new(None),
            // Echo whatever sample_input claims (slot 3) so the
            // engine-side debug_assert that the app honored the
            // claimed slot doesn't fire in tests.
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 3,
                    shell_pid: 1,
                }),
            }),
        }
    }

    #[tokio::test]
    async fn env_includes_sanitized_path_and_engine_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        let env = spawner.last_spawn_env();
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .expect("PATH should always be set on worker spawn")
            .1
            .clone();
        assert_eq!(path, WORKER_SANITIZED_PATH);
        assert!(!path.contains("/Users/"), "sanitized PATH must not contain user bin dir");
        assert!(!path.contains(".cargo"), "sanitized PATH must not contain cargo bin");

        assert_eq!(
            env.iter().find(|(k, _)| k == "BOSS_EVENTS_SOCKET").map(|(_, v)| v.as_str()),
            Some("/tmp/events.sock"),
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "BOSS_LEASE_ID").map(|(_, v)| v.as_str()),
            Some("lease-test"),
        );
    }

    #[tokio::test]
    async fn env_forces_editor_vars_to_noop() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        let env = spawner.last_spawn_env();
        // Every editor-resolution env that git/jj/$EDITOR-aware tools
        // consult must be forced to a non-zero-exit no-op. `false`
        // causes the editor invocation to fail fast so the worker
        // notices and re-runs with `-m`.
        for key in ["EDITOR", "VISUAL", "GIT_EDITOR", "JJ_EDITOR"] {
            let value = env
                .iter()
                .find(|(k, _)| k == key)
                .unwrap_or_else(|| panic!("expected env key {key} on worker spawn"))
                .1
                .as_str();
            assert_eq!(
                value, WORKER_EDITOR_NOOP,
                "{key} should be forced to {WORKER_EDITOR_NOOP}, got {value}",
            );
        }
    }

    #[tokio::test]
    async fn extra_env_allowlist_keeps_known_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        let mut input = sample_input(&workspace);
        input.extra_env = vec![
            ("BOSS_TASK_ID".into(), "T-42".into()),
            ("CUBE_LEASE_ID".into(), "lease-cube".into()),
            ("CUBE_REPO".into(), "mono".into()),
        ];

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        let env = spawner.last_spawn_env();
        assert_eq!(
            env.iter().find(|(k, _)| k == "BOSS_TASK_ID").map(|(_, v)| v.as_str()),
            Some("T-42"),
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "CUBE_LEASE_ID").map(|(_, v)| v.as_str()),
            Some("lease-cube"),
        );
        assert_eq!(
            env.iter().find(|(k, _)| k == "CUBE_REPO").map(|(_, v)| v.as_str()),
            Some("mono"),
        );
    }

    #[tokio::test]
    async fn title_summary_is_forwarded_to_spawn_request() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        let mut input = sample_input(&workspace);
        input.title_summary = Some("Pane Titlebar Summary".to_owned());

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        match spawner.last_request.lock().unwrap().clone() {
            Some(EngineToAppRequest::SpawnWorkerPane(input)) => {
                assert_eq!(input.summary.as_deref(), Some("Pane Titlebar Summary"));
            }
            other => panic!("expected SpawnWorkerPane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_title_summary_does_not_attach_one() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        start_worker(&spawner, sample_input(&workspace), StdDuration::from_secs(1))
            .await
            .unwrap();

        match spawner.last_request.lock().unwrap().clone() {
            Some(EngineToAppRequest::SpawnWorkerPane(input)) => {
                assert!(input.summary.is_none());
            }
            other => panic!("expected SpawnWorkerPane, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn extra_env_drops_non_allowlisted_keys() {
        let workspace = TempDir::new().unwrap();
        let spawner = ok_spawner_capturing();

        let mut input = sample_input(&workspace);
        // Mix of clearly-dangerous keys and a fake one to confirm
        // both get filtered. `BOSS_CONTROL_SOCKET` is the canonical
        // example: even if some upstream caller tried to set it, the
        // worker must never see it.
        input.extra_env = vec![
            ("BOSS_CONTROL_SOCKET".into(), "/tmp/should-not-leak".into()),
            ("AWS_SESSION_TOKEN".into(), "secret".into()),
            ("RANDOM_KEY".into(), "v".into()),
            ("BOSS_TASK_ID".into(), "T-keep".into()),
        ];

        start_worker(&spawner, input, StdDuration::from_secs(1)).await.unwrap();

        let env = spawner.last_spawn_env();
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"BOSS_CONTROL_SOCKET"));
        assert!(!keys.contains(&"AWS_SESSION_TOKEN"));
        assert!(!keys.contains(&"RANDOM_KEY"));
        // Allowlisted key still made it through.
        assert!(keys.contains(&"BOSS_TASK_ID"));
    }
}
