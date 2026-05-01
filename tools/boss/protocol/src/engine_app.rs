//! Wire types for the engine ↔ app pane RPC layered on the frontend
//! Unix socket. See `tools/boss/docs/designs/engine-app-rpc.md` for
//! the full design (transport choice, trust model, lifecycle).
//!
//! These types appear inside [`FrontendRequest::EngineResponse`] and
//! [`FrontendEvent::EngineRequest`] envelopes. They have no engine or
//! app implementation in this module — separate engine-side dispatch
//! and app-side pane-allocator code consume them.

use serde::{Deserialize, Serialize};

/// One env-var entry to set on the worker process. The shim and
/// `claude` running inside the libghostty pane inherit these. Used to
/// thread `BOSS_EVENTS_SOCKET`, `BOSS_LEASE_ID`, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
}

/// Engine asks the app to allocate a worker pane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpawnWorkerPaneInput {
    pub run_id: String,
    pub workspace_path: String,
    /// Text written into the pty after the shell starts. Typically
    /// `"claude\n"` so the shell types `claude` and runs the worker.
    pub initial_input: String,
    pub env: Vec<EnvVar>,
}

/// App's reply when allocation succeeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnWorkerPaneResult {
    /// One of 1..=8.
    pub slot_id: u8,
    /// Pid of the shell the surface spawned. The actual `claude`
    /// process will be a descendant of this pid; the engine registers
    /// this pid in `WorkerRegistry` and relies on the ancestor walk
    /// to correlate hook events from the shim back to the run.
    pub shell_pid: i32,
}

/// Engine asks the app to release a previously allocated pane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseWorkerPaneInput {
    pub slot_id: u8,
    /// SIGTERM, then SIGKILL after this many seconds. `0` means no
    /// grace — go straight to SIGKILL.
    pub kill_grace_seconds: u32,
}

/// App's reply when release succeeds. Empty for now; reserved for
/// future fields (e.g., final shell exit status).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseWorkerPaneResult {}

/// What the engine is asking the app to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppRequest {
    SpawnWorkerPane(SpawnWorkerPaneInput),
    ReleaseWorkerPane(ReleaseWorkerPaneInput),
}

/// App's reply, paired with the `request_id` from the originating
/// [`crate::FrontendEvent::EngineRequest`].
///
/// The result is `Ok(...)` on success and `Err(EngineToAppError)` on
/// any failure the app can surface. Engine-side timeouts and
/// app-disconnect failures are synthesised by the engine itself; they
/// don't travel on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppResponse {
    SpawnWorkerPane {
        result: Result<SpawnWorkerPaneResult, EngineToAppError>,
    },
    ReleaseWorkerPane {
        result: Result<ReleaseWorkerPaneResult, EngineToAppError>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppError {
    /// All 8 worker slots are in use.
    NoAvailableSlot,
    /// `ReleaseWorkerPane` referred to a slot the app does not
    /// recognise — already released, never allocated, or stale after
    /// an app restart.
    UnknownSlot,
    /// App lost its connection to the engine before responding. The
    /// engine synthesises this on the caller's side; the app never
    /// sends it on the wire.
    AppDisconnected,
    /// Engine-side timeout. Synthesised by the engine.
    Timeout,
    /// App-side failure with detail.
    Internal { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_request_round_trips_through_serde() {
        let original = EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
            run_id: "run-1".into(),
            workspace_path: "/tmp/ws".into(),
            initial_input: "claude\n".into(),
            env: vec![EnvVar {
                key: "BOSS_LEASE_ID".into(),
                value: "lease-uuid".into(),
            }],
        });
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn release_request_round_trips() {
        let original = EngineToAppRequest::ReleaseWorkerPane(ReleaseWorkerPaneInput {
            slot_id: 3,
            kill_grace_seconds: 5,
        });
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn spawn_response_ok_round_trips() {
        let original = EngineToAppResponse::SpawnWorkerPane {
            result: Ok(SpawnWorkerPaneResult {
                slot_id: 1,
                shell_pid: 12345,
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn spawn_response_err_round_trips() {
        let original = EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::NoAvailableSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn internal_error_carries_message() {
        let err = EngineToAppError::Internal {
            message: "surface init failed".into(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("surface init failed"));
        let parsed: EngineToAppError = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, err);
    }

    #[test]
    fn release_response_round_trips() {
        let original = EngineToAppResponse::ReleaseWorkerPane {
            result: Ok(ReleaseWorkerPaneResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }
}
