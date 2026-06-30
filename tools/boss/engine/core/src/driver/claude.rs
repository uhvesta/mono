//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn` capability is live; remaining behavioural methods
//! are `unimplemented!()` pending their per-capability extraction tasks
//! (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use boss_protocol::{NormalizeError, WorkerEvent, normalize_hook_event};

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, ProgressFidelity, ProgressObservationConfig,
    ProgressObservationWiring, WorkerErrorClass,
};

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
};

/// The seven Claude hook events wired to the `boss-event` forwarder for
/// rich-tier ProgressObservation, in lifecycle order. Output key order is
/// independent of this list (the settings file serialises a sorted map); the
/// order here is purely for readers.
const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "Notification",
    "SessionEnd",
];

/// Single-quote a shell argument, escaping internal quotes, matching the
/// POSIX `sh` that spawns Claude's hook commands.
///
/// Deliberately a small local copy of `worker_setup::shell_escape`: building
/// the worker invocation and hook commands is the driver's job, and keeping
/// this here avoids a back-edge from the driver into `worker_setup` during the
/// incremental capability extraction. Both consolidate into a shared util once
/// `worker_setup`'s remaining command-building (path guard, settings healing)
/// also moves behind the driver.
fn shell_escape(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

/// Reference implementation of [`AgentDriver`] for Claude Code.
///
/// Declares all capabilities (Claude is the full-fidelity reference driver).
/// Behavioural methods delegate to existing engine code and will be extracted
/// from [`crate::effort`], [`crate::worker_setup`], [`crate::runner`], and
/// [`crate::transient_error`] in subsequent tasks.
pub struct ClaudeDriver;

#[async_trait]
impl AgentDriver for ClaudeDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &CLAUDE_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Claude provides all capabilities. ToolProvisioning is declared
        // provided even though it is unused in v1 — the driver could in
        // principle inject MCP servers; it currently does not.
        CapabilitySet::new([
            Capability::Spawn,
            Capability::WorkspaceProvisioning,
            Capability::PermissionPolicy,
            Capability::ModelAndEffortMenu,
            Capability::ProgressObservation,
            Capability::ToolUseInterception,
            Capability::TurnBoundary,
            Capability::StructuredOutput,
            Capability::TranscriptAccess,
            Capability::ControlVerbs,
            Capability::ToolProvisioning,
            Capability::PromptComposition,
        ])
    }

    fn spawn_invocation(
        &self,
        model: &str,
        effort: Option<&str>,
        settings_path: Option<&Path>,
        non_opus_auto_mode: bool,
    ) -> String {
        let mut cmd = format!("claude --model {model}");
        if let Some(e) = effort {
            cmd.push_str(" --effort ");
            cmd.push_str(e);
        }
        if crate::effort::model_requires_auto_permissions(model) || non_opus_auto_mode {
            cmd.push_str(" --permission-mode auto");
        } else {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if let Some(settings) = settings_path {
            // Single-quote the path so a `$TMPDIR` with spaces survives
            // the pane's shell. Worker settings paths never contain a
            // single quote, so naive single-quoting is sufficient.
            cmd.push_str(&format!(" --settings '{}'", settings.display()));
        }
        cmd.push_str(" \"$(cat .claude/initial-prompt.txt)\"\n");
        cmd
    }

    async fn provision_workspace(&self, _workspace: &Path, _prompt_text: &str, _run_id: &str) -> anyhow::Result<()> {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::write_workspace_files
        unimplemented!("extracted in the WorkspaceProvisioning task")
    }

    async fn write_permission_config(&self, _dest_dir: &Path) -> anyhow::Result<PathBuf> {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::render_settings_json
        unimplemented!("extracted in the PermissionPolicy task")
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Claude's hooks deliver per-tool PreToolUse/PostToolUse events — the
        // richest tier (design §Capabilities, ProgressObservation).
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressObservationWiring {
        // Inline-prefix every env var the `boss-event` shim needs. `BOSS_RUN_ID`
        // is load-bearing: without it the shim can't splice `_boss_run_id` and
        // the engine drops the event, pinning the worker at `Spawning`.
        // `BOSS_WORKSPACE` tells the shim where to buffer events when the
        // engine is unreachable. Setting them here (rather than relying on env
        // inheritance from the worker pane through Claude into the hook
        // subprocess) guarantees the shim sees them regardless of how Claude
        // propagates env.
        let command = format!(
            "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} BOSS_WORKSPACE={workspace} {shim}",
            socket = shell_escape(&config.events_socket_path.display().to_string()),
            lease = shell_escape(&config.lease_id),
            run_id = shell_escape(&config.run_id),
            workspace = shell_escape(&config.workspace_path.display().to_string()),
            shim = shell_escape(&config.forwarder_binary.display().to_string()),
        );

        // Every hook event fires this same forwarder hook (matcher `*`). The
        // caller may extend the `PreToolUse` array with interception guards —
        // a separate capability — without disturbing the forwarder, which
        // stays the first entry.
        let forward_hook = serde_json::json!({
            "matcher": "*",
            "hooks": [
                {
                    "type": "command",
                    "command": command,
                }
            ],
        });

        let mut hooks = serde_json::Map::new();
        for event in CLAUDE_HOOK_EVENTS {
            hooks.insert((*event).to_owned(), serde_json::json!([forward_hook.clone()]));
        }
        ProgressObservationWiring { hooks }
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        normalize_hook_event(raw)
    }

    fn agent_rules_preamble(&self) -> &'static str {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::render_claude_md
        unimplemented!("extracted in the PromptComposition task")
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // TODO(@brianduff,2026-12-31): extract from transient_error::classify_claude_error
        unimplemented!("extracted in the ControlVerbs task")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Capability;

    #[test]
    fn claude_driver_provides_all_capabilities() {
        let driver = ClaudeDriver;
        let caps = driver.capabilities();

        for cap in [
            Capability::Spawn,
            Capability::WorkspaceProvisioning,
            Capability::PermissionPolicy,
            Capability::ModelAndEffortMenu,
            Capability::ProgressObservation,
            Capability::ToolUseInterception,
            Capability::TurnBoundary,
            Capability::StructuredOutput,
            Capability::TranscriptAccess,
            Capability::ControlVerbs,
            Capability::ToolProvisioning,
            Capability::PromptComposition,
        ] {
            assert!(caps.provides(cap), "ClaudeDriver must provide {cap:?}",);
        }
    }

    #[test]
    fn claude_descriptor_slug_is_claude() {
        let driver = ClaudeDriver;
        assert_eq!(driver.descriptor().name, "claude");
        assert_eq!(driver.descriptor().config_dir, ".claude");
        assert_eq!(driver.descriptor().agent_rules_filename, "CLAUDE.md");
        assert_eq!(driver.descriptor().binary, "claude");
    }

    fn sample_config() -> ProgressObservationConfig {
        ProgressObservationConfig {
            events_socket_path: PathBuf::from("/Users/x/Library/Application Support/Boss/events.sock"),
            lease_id: "lease-uuid-abc".into(),
            run_id: "run-sample".into(),
            workspace_path: PathBuf::from("/ws/mono-agent-007"),
            forwarder_binary: PathBuf::from("/Users/x/Library/Application Support/Boss/bin/boss-event"),
        }
    }

    #[test]
    fn claude_progress_fidelity_is_rich() {
        assert_eq!(ClaudeDriver.progress_fidelity(), ProgressFidelity::Rich);
    }

    #[test]
    fn observation_wiring_covers_all_seven_lifecycle_events() {
        let wiring = ClaudeDriver.progress_observation_wiring(&sample_config());
        for name in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "Notification",
            "SessionEnd",
        ] {
            let entries = wiring.hooks[name].as_array().unwrap();
            // Exactly the forwarder hook; interception guards are layered on
            // by the caller, not by the ProgressObservation producer.
            assert_eq!(entries.len(), 1, "{name} should wire only the forwarder");
            assert_eq!(entries[0]["matcher"], "*");
        }
    }

    #[test]
    fn observation_wiring_threads_socket_lease_run_and_workspace_into_command() {
        let wiring = ClaudeDriver.progress_observation_wiring(&sample_config());
        let command = wiring.hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        // Single-quote escaping must survive the space in "Application Support".
        assert!(command.contains("BOSS_EVENTS_SOCKET='/Users/x/Library/Application Support/Boss/events.sock'"));
        assert!(command.contains("BOSS_LEASE_ID='lease-uuid-abc'"));
        assert!(command.contains("BOSS_RUN_ID='run-sample'"));
        assert!(command.contains("BOSS_WORKSPACE='/ws/mono-agent-007'"));
        assert!(command.starts_with("BOSS_EVENTS_SOCKET="));
        assert!(command.trim_end().ends_with("/boss-event'"));
    }

    #[test]
    fn normalize_progress_event_decodes_a_stop_hook() {
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
        });
        let event = ClaudeDriver.normalize_progress_event(&raw).unwrap();
        assert!(matches!(event, WorkerEvent::Stop { .. }));
    }

    #[test]
    fn normalize_progress_event_surfaces_unknown_hook_error() {
        let raw = serde_json::json!({
            "session_id": "sess-1",
            "hook_event_name": "WeirdNewHook",
        });
        assert!(ClaudeDriver.normalize_progress_event(&raw).is_err());
    }
}
