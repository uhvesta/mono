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

/// Engine asks the app to host a worker pane in a specific slot.
///
/// The engine is the source of truth for which slot a worker lands
/// in: it picks the slot via [`crate::WorkerPool::claim_worker`] and
/// passes the result here as `slot_id`. The app's job is to honor
/// that slot — no fallback / re-allocation. If the slot is already
/// occupied (engine and app disagree), the app returns
/// [`EngineToAppError::SlotBusy`] rather than silently picking a
/// different slot, which would re-introduce the dual-allocator bug
/// the engine-owns-slots refactor exists to fix.
///
/// Naming: `worker-{N}` (engine side) and slot `N` (app side, also
/// 1-indexed) refer to the same physical pane. There is one and
/// only one numbering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpawnWorkerPaneInput {
    pub run_id: String,
    pub workspace_path: String,
    /// 1-indexed slot the engine has claimed for this worker. The
    /// app must host the pane in this exact slot or fail with
    /// [`EngineToAppError::SlotBusy`] / `UnknownSlot`.
    pub slot_id: u8,
    /// Text written into the pty after the shell starts. Typically
    /// `"claude\n"` so the shell types `claude` and runs the worker.
    pub initial_input: String,
    pub env: Vec<EnvVar>,
    /// Short lowercase present-continuous verb phrase describing
    /// what the worker is doing (e.g. `"fixing the fencer scraper"`).
    /// The app renders this under the worker's display name as a
    /// natural-language sentence: `"Riker is fixing the fencer
    /// scraper"`. The full run id is still surfaced as a tooltip for
    /// traceability. Present only when the engine successfully called
    /// Claude to generate a proper gerund phrase (ANTHROPIC_API_KEY
    /// was available and the call succeeded). When absent, the app
    /// uses `task_title` for the fallback format `"Riker: <task>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Raw work-item title (the task's `name` column), passed for
    /// display when `summary` is absent (no API key or generation
    /// failed). The app renders this as `"<AgentName>: <task_title>"`
    /// — no gerund connector — so the pane header still identifies
    /// the task without looking grammatically broken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_title: Option<String>,
}

/// App's reply when allocation succeeds. The slot is dictated by
/// the engine in [`SpawnWorkerPaneInput::slot_id`]; the app echoes
/// it back here purely as a confirmation aid.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnWorkerPaneResult {
    /// Confirmation echo of [`SpawnWorkerPaneInput::slot_id`]. Engine
    /// callers can debug-assert equality, but should otherwise treat
    /// the slot they sent as authoritative.
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

/// Engine asks the app to write text into a worker pane's pty as if
/// it were typed by the user. Used for probe-injection on `Stop`
/// boundaries and for `bossctl agents send`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendToPaneInput {
    pub slot_id: u8,
    pub text: String,
}

/// App's reply when text injection succeeds. Empty for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendToPaneResult {}

/// Engine asks the app to bring a worker pane to the front: select
/// the pane in the Workers grid, focus its surface so keystrokes go
/// to that pty, and raise the app window to the front of the
/// window-server stack. Used by `bossctl agents focus`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusWorkerPaneInput {
    pub slot_id: u8,
}

/// App's reply when focus succeeds. Empty for now; reserved for
/// future fields (e.g., whether the window was already key).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FocusWorkerPaneResult {}

/// Engine asks the app to deliver an Esc / interrupt key event to a
/// worker pane's pty — equivalent to the human pressing Esc while
/// the pane has keyboard focus. Used by `bossctl agents interrupt`
/// to cancel a worker's in-flight turn without terminating the run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptWorkerPaneInput {
    pub slot_id: u8,
}

/// App's reply when interrupt delivery succeeds. Empty for now.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterruptWorkerPaneResult {}

/// Engine asks the app to scroll the kanban to a specific work item
/// and play a short transient highlight. `work_item_id` is the
/// resolved canonical id (`task_…`/`proj_…`). `product_id` is
/// included so the app can switch to the right product board even
/// when that product is not currently loaded.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealWorkItemInput {
    pub work_item_id: String,
    pub product_id: String,
}

/// App's reply when the reveal animation has been triggered.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RevealWorkItemResult {}

/// What the engine is asking the app to do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppRequest {
    SpawnWorkerPane(SpawnWorkerPaneInput),
    ReleaseWorkerPane(ReleaseWorkerPaneInput),
    SendToPane(SendToPaneInput),
    FocusWorkerPane(FocusWorkerPaneInput),
    InterruptWorkerPane(InterruptWorkerPaneInput),
    RevealWorkItem(RevealWorkItemInput),
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
    SendToPane {
        result: Result<SendToPaneResult, EngineToAppError>,
    },
    FocusWorkerPane {
        result: Result<FocusWorkerPaneResult, EngineToAppError>,
    },
    InterruptWorkerPane {
        result: Result<InterruptWorkerPaneResult, EngineToAppError>,
    },
    RevealWorkItem {
        result: Result<RevealWorkItemResult, EngineToAppError>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EngineToAppError {
    /// All 8 worker slots are in use. Retained for completeness;
    /// since the engine now picks the slot, this is effectively
    /// unreachable from `SpawnWorkerPane` (the engine returns
    /// before the request is even sent when the pool is full).
    NoAvailableSlot,
    /// `ReleaseWorkerPane` / `SendToPane` / `FocusWorkerPane` /
    /// `InterruptWorkerPane` referred to a slot the app does not
    /// recognise — already released, never allocated, or stale after
    /// an app restart.
    UnknownSlot,
    /// `SpawnWorkerPane` requested a slot the app considers already
    /// in use (a session is hosted there). Surfaces engine↔app
    /// disagreement instead of silently re-allocating; the engine
    /// must reconcile rather than retry blindly.
    SlotBusy,
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
            slot_id: 3,
            initial_input: "claude\n".into(),
            env: vec![EnvVar {
                key: "BOSS_LEASE_ID".into(),
                value: "lease-uuid".into(),
            }],
            summary: Some("fixing the fencer scraper".into()),
            task_title: None,
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("\"slot_id\":3"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn spawn_request_without_summary_round_trips_and_omits_field() {
        let original = EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
            run_id: "run-1".into(),
            workspace_path: "/tmp/ws".into(),
            slot_id: 1,
            initial_input: "claude\n".into(),
            env: vec![],
            summary: None,
            task_title: None,
        });
        let json = serde_json::to_string(&original).unwrap();
        // None should not serialize `summary` or `task_title`; they
        // must be omitted so apps that predate the field continue to parse.
        assert!(!json.contains("summary"));
        assert!(!json.contains("task_title"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn spawn_request_with_task_title_round_trips() {
        let original = EngineToAppRequest::SpawnWorkerPane(SpawnWorkerPaneInput {
            run_id: "run-2".into(),
            workspace_path: "/tmp/ws".into(),
            slot_id: 2,
            initial_input: "claude\n".into(),
            env: vec![],
            summary: None,
            task_title: Some("kanban: revision cards render broken".into()),
        });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("task_title"));
        assert!(!json.contains("\"summary\""));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn slot_busy_error_round_trips() {
        let original = EngineToAppResponse::SpawnWorkerPane {
            result: Err(EngineToAppError::SlotBusy),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("slot_busy"));
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
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

    #[test]
    fn focus_request_round_trips() {
        let original = EngineToAppRequest::FocusWorkerPane(FocusWorkerPaneInput { slot_id: 4 });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("focus_worker_pane"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn focus_response_ok_round_trips() {
        let original = EngineToAppResponse::FocusWorkerPane {
            result: Ok(FocusWorkerPaneResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn focus_response_err_round_trips() {
        let original = EngineToAppResponse::FocusWorkerPane {
            result: Err(EngineToAppError::UnknownSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_request_round_trips() {
        let original = EngineToAppRequest::InterruptWorkerPane(InterruptWorkerPaneInput { slot_id: 7 });
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("interrupt_worker_pane"));
        let parsed: EngineToAppRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_response_ok_round_trips() {
        let original = EngineToAppResponse::InterruptWorkerPane {
            result: Ok(InterruptWorkerPaneResult {}),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn interrupt_response_err_round_trips() {
        let original = EngineToAppResponse::InterruptWorkerPane {
            result: Err(EngineToAppError::UnknownSlot),
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EngineToAppResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }
}
