//! Typed events emitted by a claude worker via its hook payloads.
//!
//! Claude's hooks (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse,
//! Stop, Notification, SessionEnd) deliver JSON payloads to the
//! `boss-event` shim, which forwards them over the engine events socket.
//! [`normalize_hook_event`] converts a raw payload into a typed
//! [`WorkerEvent`] that downstream engine code can match on.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerEvent {
    SessionStart {
        session_id: String,
        source: SessionStartSource,
    },
    UserPromptSubmit {
        session_id: String,
        prompt: String,
    },
    PreToolUse {
        session_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
    },
    PostToolUse {
        session_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
        tool_response: serde_json::Value,
    },
    Stop {
        session_id: String,
        stop_hook_active: bool,
        stop_reason: StopReason,
    },
    Notification {
        session_id: String,
        message: String,
    },
    SessionEnd {
        session_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Compact,
    Other,
}

/// Why a worker turn ended. The hook payload alone only tells us
/// `stop_hook_active`; richer reasons (`AwaitingInput`, `Interrupted`)
/// are derived by the events-socket sequencer (Phase 6c) using
/// surrounding context — `Notification` immediately before `Stop`
/// implies the worker is awaiting a permission prompt, etc. The
/// normalizer here always returns [`StopReason::Completed`]; the
/// sequencer overwrites as needed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    AwaitingInput,
    Interrupted,
    Other,
}

#[derive(Debug, Error)]
pub enum NormalizeError {
    #[error("missing or non-string field: {0}")]
    MissingField(&'static str),
    #[error("unknown hook_event_name: {0}")]
    UnknownEvent(String),
    #[error("malformed payload: {0}")]
    Malformed(String),
}

pub fn normalize_hook_event(raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
    let obj = raw
        .as_object()
        .ok_or_else(|| NormalizeError::Malformed("expected JSON object".into()))?;

    let session_id = obj
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or(NormalizeError::MissingField("session_id"))?;

    let event_name = obj
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .ok_or(NormalizeError::MissingField("hook_event_name"))?;

    Ok(match event_name {
        "SessionStart" => WorkerEvent::SessionStart {
            session_id,
            source: parse_session_start_source(obj.get("source").and_then(|v| v.as_str())),
        },
        "UserPromptSubmit" => WorkerEvent::UserPromptSubmit {
            session_id,
            prompt: string_or_empty(obj.get("prompt")),
        },
        "PreToolUse" => WorkerEvent::PreToolUse {
            session_id,
            tool_name: string_or_empty(obj.get("tool_name")),
            tool_input: obj
                .get("tool_input")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        "PostToolUse" => WorkerEvent::PostToolUse {
            session_id,
            tool_name: string_or_empty(obj.get("tool_name")),
            tool_input: obj
                .get("tool_input")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            tool_response: obj
                .get("tool_response")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        },
        "Stop" => WorkerEvent::Stop {
            session_id,
            stop_hook_active: obj
                .get("stop_hook_active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            stop_reason: StopReason::Completed,
        },
        "Notification" => WorkerEvent::Notification {
            session_id,
            message: string_or_empty(obj.get("message")),
        },
        "SessionEnd" => WorkerEvent::SessionEnd {
            session_id,
            reason: string_or_empty(obj.get("reason")),
        },
        other => return Err(NormalizeError::UnknownEvent(other.to_owned())),
    })
}

fn parse_session_start_source(source: Option<&str>) -> SessionStartSource {
    match source {
        Some("startup") => SessionStartSource::Startup,
        Some("resume") => SessionStartSource::Resume,
        Some("compact") => SessionStartSource::Compact,
        _ => SessionStartSource::Other,
    }
}

fn string_or_empty(value: Option<&serde_json::Value>) -> String {
    value
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_start_startup() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "SessionStart",
            "source": "startup",
        });
        assert_eq!(
            normalize_hook_event(&raw).unwrap(),
            WorkerEvent::SessionStart {
                session_id: "sess-1".into(),
                source: SessionStartSource::Startup,
            }
        );
    }

    #[test]
    fn session_start_resume() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "SessionStart",
            "source": "resume",
        });
        let WorkerEvent::SessionStart { source, .. } = normalize_hook_event(&raw).unwrap() else {
            panic!("expected SessionStart");
        };
        assert_eq!(source, SessionStartSource::Resume);
    }

    #[test]
    fn session_start_unknown_source_defaults_to_other() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "SessionStart",
            "source": "weird-future-source",
        });
        let WorkerEvent::SessionStart { source, .. } = normalize_hook_event(&raw).unwrap() else {
            panic!("expected SessionStart");
        };
        assert_eq!(source, SessionStartSource::Other);
    }

    #[test]
    fn user_prompt_submit() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "ship phase 6d",
        });
        assert_eq!(
            normalize_hook_event(&raw).unwrap(),
            WorkerEvent::UserPromptSubmit {
                session_id: "sess-1".into(),
                prompt: "ship phase 6d".into(),
            }
        );
    }

    #[test]
    fn pre_tool_use_preserves_tool_input() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "ls", "timeout": 5000 },
        });
        let WorkerEvent::PreToolUse {
            tool_name,
            tool_input,
            ..
        } = normalize_hook_event(&raw).unwrap()
        else {
            panic!("expected PreToolUse");
        };
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_input["command"], "ls");
        assert_eq!(tool_input["timeout"], 5000);
    }

    #[test]
    fn post_tool_use_preserves_input_and_response() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "tool_input": { "file_path": "/tmp/x" },
            "tool_response": { "content": "hello" },
        });
        let WorkerEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            ..
        } = normalize_hook_event(&raw).unwrap()
        else {
            panic!("expected PostToolUse");
        };
        assert_eq!(tool_name, "Read");
        assert_eq!(tool_input["file_path"], "/tmp/x");
        assert_eq!(tool_response["content"], "hello");
    }

    #[test]
    fn stop_default_reason_is_completed() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
        });
        assert_eq!(
            normalize_hook_event(&raw).unwrap(),
            WorkerEvent::Stop {
                session_id: "sess-1".into(),
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            }
        );
    }

    #[test]
    fn stop_hook_active_passes_through() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "Stop",
            "stop_hook_active": true,
        });
        let WorkerEvent::Stop {
            stop_hook_active, ..
        } = normalize_hook_event(&raw).unwrap()
        else {
            panic!("expected Stop");
        };
        assert!(stop_hook_active);
    }

    #[test]
    fn notification_carries_message() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "Notification",
            "message": "Claude needs your permission",
        });
        assert_eq!(
            normalize_hook_event(&raw).unwrap(),
            WorkerEvent::Notification {
                session_id: "sess-1".into(),
                message: "Claude needs your permission".into(),
            }
        );
    }

    #[test]
    fn session_end_carries_reason() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "SessionEnd",
            "reason": "exit",
        });
        assert_eq!(
            normalize_hook_event(&raw).unwrap(),
            WorkerEvent::SessionEnd {
                session_id: "sess-1".into(),
                reason: "exit".into(),
            }
        );
    }

    #[test]
    fn missing_session_id_errors() {
        let raw = json!({ "hook_event_name": "Stop" });
        assert!(matches!(
            normalize_hook_event(&raw),
            Err(NormalizeError::MissingField("session_id"))
        ));
    }

    #[test]
    fn missing_hook_event_name_errors() {
        let raw = json!({ "session_id": "sess-1" });
        assert!(matches!(
            normalize_hook_event(&raw),
            Err(NormalizeError::MissingField("hook_event_name"))
        ));
    }

    #[test]
    fn unknown_event_errors() {
        let raw = json!({
            "session_id": "sess-1",
            "hook_event_name": "WeirdNewHook",
        });
        assert!(matches!(
            normalize_hook_event(&raw),
            Err(NormalizeError::UnknownEvent(name)) if name == "WeirdNewHook"
        ));
    }

    #[test]
    fn non_object_payload_errors() {
        let raw = json!("not an object");
        assert!(matches!(
            normalize_hook_event(&raw),
            Err(NormalizeError::Malformed(_))
        ));
    }

    #[test]
    fn worker_event_round_trips_through_json() {
        let original = WorkerEvent::Stop {
            session_id: "sess-1".into(),
            stop_hook_active: false,
            stop_reason: StopReason::AwaitingInput,
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let parsed: WorkerEvent = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, original);
    }
}
