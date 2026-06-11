//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn` capability is live; remaining behavioural methods
//! are `unimplemented!()` pending their per-capability extraction tasks
//! (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{AgentDriver, Capability, CapabilitySet, DriverDescriptor, WorkerErrorClass};

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
};

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
}
