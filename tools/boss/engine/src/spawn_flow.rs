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
//! This module is just the helper. Replacing `AcpExecutionRunner` with
//! a pane-driven runner that calls into it is left as a follow-up —
//! the run-lifecycle question (*when does a pane-driven run end?*)
//! needs more design before it ships.

use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use thiserror::Error;
use tokio::time::Duration;

use crate::protocol::{
    EngineToAppError, EngineToAppRequest, EngineToAppResponse, EnvVar, SpawnWorkerPaneInput,
    SpawnWorkerPaneResult,
};
use crate::worker_registry::WorkerRegistry;
use crate::worker_setup::{WorkerSetupInput, WrittenFiles, write_workspace_files};

#[derive(Debug, Clone)]
pub struct StartWorkerInput {
    pub run_id: String,
    pub lease_id: String,
    pub workspace_path: PathBuf,
    pub events_socket_path: PathBuf,
    pub boss_event_path: PathBuf,
    pub initial_input: String,
    /// Extra env vars to thread to the worker on top of the ones the
    /// settings.json template injects (`BOSS_EVENTS_SOCKET`,
    /// `BOSS_LEASE_ID`).
    pub extra_env: Vec<(String, String)>,
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
        lease_id: input.lease_id.clone(),
        workspace_path: input.workspace_path.clone(),
        events_socket_path: input.events_socket_path.clone(),
        boss_event_path: input.boss_event_path.clone(),
    };
    let written = write_workspace_files(&setup).map_err(StartWorkerError::WriteFiles)?;

    // 2. Build the SpawnWorkerPane request, including standard env
    //    vars plus any caller-provided extras.
    let mut env = vec![
        EnvVar {
            key: "BOSS_EVENTS_SOCKET".into(),
            value: input.events_socket_path.display().to_string(),
        },
        EnvVar {
            key: "BOSS_LEASE_ID".into(),
            value: input.lease_id.clone(),
        },
    ];
    for (k, v) in input.extra_env {
        env.push(EnvVar { key: k, value: v });
    }

    let response = spawner
        .send_to_app_request(
            EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
                run_id: input.run_id.clone(),
                workspace_path: input.workspace_path.display().to_string(),
                initial_input: input.initial_input,
                env,
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
        | EngineToAppResponse::SendToPane { .. } => {
            return Err(StartWorkerError::ResponseKindMismatch);
        }
    };

    // 3. Register the shell pid against the run id so the events
    //    socket can correlate hook events from descendants of the
    //    spawned shell back to this run, and remember the slot id so
    //    follow-up `SendToPane` requests (e.g., probe injection) can
    //    route by run id.
    spawner
        .worker_registry()
        .register_run_slot(input.run_id.clone(), slot_id);
    if shell_pid > 0 {
        spawner.worker_registry().register(shell_pid, input.run_id);
    } else {
        tracing::warn!(
            slot_id,
            "spawn returned shell_pid 0; hook-event correlation will fail until a real pid is wired (TODO: proc_listpids in app)",
        );
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
    }

    #[async_trait::async_trait]
    impl WorkerSpawner for StubSpawner {
        async fn send_to_app_request(
            &self,
            _request: EngineToAppRequest,
            _timeout: Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
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

    fn sample_input(workspace: &TempDir) -> StartWorkerInput {
        StartWorkerInput {
            run_id: "run-test".into(),
            lease_id: "lease-test".into(),
            workspace_path: workspace.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            boss_event_path: PathBuf::from("/tmp/boss-event"),
            initial_input: "claude\n".into(),
            extra_env: vec![],
        }
    }

    #[tokio::test]
    async fn happy_path_writes_files_sends_request_and_registers_pid() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
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
        assert_eq!(registry.lookup(42_111).as_deref(), Some("run-test"));
    }

    #[tokio::test]
    async fn shell_pid_zero_skips_registration_with_warning() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawner = StubSpawner {
            registry: registry.clone(),
            spawn_calls: Arc::new(AtomicUsize::new(0)),
            canned_response: Ok(EngineToAppResponse::SpawnWorkerPane {
                result: Ok(SpawnWorkerPaneResult {
                    slot_id: 1,
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

    #[tokio::test]
    async fn write_failure_does_not_send_request() {
        let workspace = TempDir::new().unwrap();
        let registry = WorkerRegistry::new();
        let spawn_calls = Arc::new(AtomicUsize::new(0));
        let spawner = StubSpawner {
            registry,
            spawn_calls: spawn_calls.clone(),
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
}
