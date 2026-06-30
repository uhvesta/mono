//! Agent-driver abstraction: the capability-oriented interface between Boss
//! and the coding-agent CLI it drives.
//!
//! See `tools/boss/docs/designs/agent-driver-abstraction-*.md` for the full
//! design (§Chosen approach, §Capabilities, §The absence-policy model).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use boss_protocol::{NormalizeError, TaskKind, WorkerEvent};

/// A named capability Boss needs from an agent driver.
///
/// A driver declares, per capability, that it provides that capability; for
/// any capability not declared the absence disposition applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Build the command/plan that starts a worker against a workspace with a prompt.
    Spawn,
    /// Materialise per-session files (prompt, agent-rules, gitignore) and
    /// suppress the backend's first-run workspace-trust prompt.
    WorkspaceProvisioning,
    /// Apply Boss's abstract permission policy: autonomous-honour-denies,
    /// reviewer read-only, and the structural deny set (bossctl/state-dir/rm/sudo).
    PermissionPolicy,
    /// Resolve effort+override against the driver's model menu; classify model
    /// families for the autonomy-default branch.
    ModelAndEffortMenu,
    /// Produce a `WorkerEvent` stream driving the activity machine (fidelity
    /// tiers: rich / coarse / minimal).
    ProgressObservation,
    /// Intercept-and-rewrite-or-deny a tool call *before* it runs (editorial
    /// PreToolUse hooks, path guard, revision-PR guard).
    ToolUseInterception,
    /// A "turn ended" signal triggering completion detection and probe injection.
    TurnBoundary,
    /// Receive the worker's structured results (PR URL, ReviewResult, triage,
    /// FOLLOWUPS) via file-based primary contract (T1414).
    StructuredOutput,
    /// A redactable, role-structured view of the run for summarisation and
    /// post-hoc extraction.
    TranscriptAccess,
    /// probe / interrupt / stop / reap / classify-error.
    ControlVerbs,
    /// Inject MCP servers and tool definitions (unused in v1 for any driver;
    /// named seam for future use).
    ToolProvisioning,
    /// Driver supplies the agent-rules filename, hook-enforcement wording, and
    /// the final-output convention; the body is shared.
    PromptComposition,
}

impl Capability {
    /// Default absence disposition: what Boss does when a driver does not
    /// declare this capability. Per-kind escalation via [`KindRequirements`]
    /// can upgrade Degrade/Synthesize to Refuse.
    pub fn default_absence_disposition(self) -> AbsenceDisposition {
        match self {
            Self::Spawn => AbsenceDisposition::Refuse,
            Self::WorkspaceProvisioning => AbsenceDisposition::Refuse,
            Self::PermissionPolicy => AbsenceDisposition::Refuse,
            Self::ModelAndEffortMenu => AbsenceDisposition::Degrade,
            Self::ProgressObservation => AbsenceDisposition::Synthesize,
            Self::ToolUseInterception => AbsenceDisposition::Degrade,
            Self::TurnBoundary => AbsenceDisposition::Synthesize,
            Self::StructuredOutput => AbsenceDisposition::Degrade,
            Self::TranscriptAccess => AbsenceDisposition::Degrade,
            Self::ControlVerbs => AbsenceDisposition::Degrade,
            Self::ToolProvisioning => AbsenceDisposition::Degrade,
            Self::PromptComposition => AbsenceDisposition::Refuse,
        }
    }
}

/// What Boss does when a driver does not declare a required capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbsenceDisposition {
    /// Boss manufactures the signal from a lower-fidelity channel the driver
    /// does provide (e.g. ProgressObservation from JSON stdout).
    Synthesize,
    /// Boss runs with reduced fidelity and records that it did (e.g. 5-value
    /// effort collapsing to 3-value, post-hoc editorial instead of pre-tool).
    Degrade,
    /// Boss refuses to dispatch this work item on this driver, failing at the
    /// dispatch gate with an actionable error before any pane spawns.
    Refuse,
}

/// The capabilities a driver declares, plus optional per-capability
/// absence-disposition overrides.
#[derive(Debug, Clone)]
pub struct CapabilitySet {
    provided: HashSet<Capability>,
    /// Overrides the default absence disposition for specific capabilities this
    /// driver does NOT provide (e.g. to express Refuse instead of the default
    /// Degrade for ToolUseInterception on an editorial-required driver).
    absence_overrides: HashMap<Capability, AbsenceDisposition>,
}

impl CapabilitySet {
    pub fn new(provided: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            provided: provided.into_iter().collect(),
            absence_overrides: HashMap::new(),
        }
    }

    /// Override the absence disposition for a capability this driver does not
    /// provide. Chainable builder method.
    pub fn with_absence_override(mut self, cap: Capability, disposition: AbsenceDisposition) -> Self {
        self.absence_overrides.insert(cap, disposition);
        self
    }

    pub fn provides(&self, cap: Capability) -> bool {
        self.provided.contains(&cap)
    }

    /// Absence disposition for a capability this driver does NOT provide,
    /// combining the driver-level override with the global default.
    pub fn absence_disposition(&self, cap: Capability) -> AbsenceDisposition {
        self.absence_overrides
            .get(&cap)
            .copied()
            .unwrap_or_else(|| cap.default_absence_disposition())
    }
}

/// Static data-half of a driver: binary, file layout, display labels.
/// The behavioural half is the `AgentDriver` trait methods.
#[derive(bon::Builder, Debug, Clone)]
#[builder(on(String, into))]
pub struct DriverDescriptor {
    /// Canonical slug in `tasks.driver` and CLI `--driver` flag
    /// (e.g. `"claude"`, `"copilot"`, `"codex"`).
    pub name: &'static str,
    /// Human-readable label for UI and logs (e.g. `"Claude Code"`).
    pub label: &'static str,
    /// Binary name to invoke (e.g. `"claude"`, `"copilot"`).
    pub binary: &'static str,
    /// Per-session config directory relative to the workspace root
    /// (e.g. `".claude"`, `".copilot"`).
    pub config_dir: &'static str,
    /// Filename for the agent-rules file inside `config_dir`
    /// (e.g. `"CLAUDE.md"`, `"AGENTS.md"`).
    pub agent_rules_filename: &'static str,
    /// Filename for the initial prompt inside `config_dir`
    /// (e.g. `"initial-prompt.txt"`).
    pub initial_prompt_filename: &'static str,
}

/// Per-[`TaskKind`] capability escalations. A kind can mark specific
/// capabilities as *required-strict*, forcing [`AbsenceDisposition::Refuse`]
/// on absence even when the capability's default is Degrade or Synthesize.
///
/// Example: `TaskKind::Design` marks `StructuredOutput` and
/// `ToolUseInterception` required-strict so a driver lacking them is refused
/// for design tasks without a bespoke per-kind block.
pub struct KindRequirements {
    required_strict: HashSet<Capability>,
}

impl KindRequirements {
    /// Required-strict capability set for a given task kind.
    /// Empty means no escalations beyond per-capability defaults.
    pub fn for_kind(kind: TaskKind) -> Self {
        let required_strict = match kind {
            TaskKind::Design => [Capability::StructuredOutput, Capability::ToolUseInterception]
                .into_iter()
                .collect(),
            _ => HashSet::new(),
        };
        Self { required_strict }
    }

    pub fn is_required_strict(&self, cap: Capability) -> bool {
        self.required_strict.contains(&cap)
    }

    /// Resolved absence disposition for `cap` when dispatching a work item of
    /// this kind to a driver with the given `CapabilitySet`.
    ///
    /// - `None` — driver provides the capability (no absence to resolve).
    /// - `Some(Refuse)` — absent and required-strict for this kind.
    /// - `Some(_)` — absent, not required-strict; driver's default applies.
    pub fn resolve_absence_disposition(
        &self,
        cap: Capability,
        driver_caps: &CapabilitySet,
    ) -> Option<AbsenceDisposition> {
        if driver_caps.provides(cap) {
            return None;
        }
        if self.is_required_strict(cap) {
            return Some(AbsenceDisposition::Refuse);
        }
        Some(driver_caps.absence_disposition(cap))
    }
}

/// Abstract classification of a worker error for recovery decisions.
/// Each driver translates its backend-specific error strings into this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerErrorClass {
    /// Retryable infrastructure error — auto-resume is appropriate.
    Transient,
    /// Non-retryable — retrying would reproduce the failure.
    Permanent,
    /// Recognised as an error but not confidently bucketed; treat as Permanent.
    Indeterminate,
}

/// Fidelity tier of the [`WorkerEvent`] stream a driver's
/// [`Capability::ProgressObservation`] produces (design §Capabilities).
///
/// The activity machine downstream consumes the same `WorkerEvent` type at
/// every tier; the tier records how much resolution the driver's event source
/// actually carries, so degrade decisions (and the staleness sweep) can
/// account for a driver that observes less than Claude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressFidelity {
    /// Per-tool events plus lifecycle. Claude provides this from its hook
    /// stream — `PreToolUse`/`PostToolUse` give per-tool granularity.
    Rich,
    /// Turn + lifecycle boundaries only, with no per-tool granularity.
    Coarse,
    /// Process alive/exited only — no in-run signal.
    Minimal,
}

/// Inputs the rich-tier ProgressObservation wiring needs to point a worker's
/// event source at the engine: the events-socket endpoint, the run/lease
/// identity tags, the worker's workspace, and the event-forwarder binary the
/// worker invokes (the `boss-event` shim for Claude).
#[derive(Debug, Clone)]
pub struct ProgressObservationConfig {
    /// Engine events-socket path the forwarder connects to.
    pub events_socket_path: PathBuf,
    /// Cube lease id, surfaced to the forwarder via `BOSS_LEASE_ID`.
    pub lease_id: String,
    /// Run id, inline-prefixed as `BOSS_RUN_ID` so the forwarder can splice
    /// `_boss_run_id` into every payload for run correlation.
    pub run_id: String,
    /// Worker workspace, where the forwarder buffers events when the engine
    /// is unreachable (`BOSS_WORKSPACE`).
    pub workspace_path: PathBuf,
    /// Absolute path to the event-forwarder binary (the `boss-event` shim).
    pub forwarder_binary: PathBuf,
}

/// A driver's event-source wiring for [`Capability::ProgressObservation`].
///
/// For the Claude driver, `hooks` is the settings-file `hooks` map that routes
/// every lifecycle + tool hook event to the `boss-event` shim, which forwards
/// each payload to the engine events socket; the spawn flow merges this
/// fragment into the worker settings file. A driver whose event source is
/// configured at spawn time instead (e.g. a CLI `--output-format json`
/// stream) returns an empty `hooks` map and wires its observation through
/// [`AgentDriver::spawn_invocation`].
#[derive(Debug, Clone, Default)]
pub struct ProgressObservationWiring {
    /// Hook-event name → array of hook entries. Claude wires all seven
    /// lifecycle events to the forwarder; the caller may extend the
    /// `PreToolUse` entry with interception guards (a separate capability).
    pub hooks: serde_json::Map<String, serde_json::Value>,
}

/// An agent driver: the abstraction layer between Boss and a coding-agent CLI.
///
/// A driver declares its [`CapabilitySet`] and implements the behavioural
/// methods for each capability it claims. Boss queries the declaration at
/// dispatch time and applies absence policies for undeclared capabilities.
///
/// Held as `Box<dyn AgentDriver>` or `Arc<dyn AgentDriver>` by the resolver;
/// all methods are object-safe.
#[async_trait]
pub trait AgentDriver: Send + Sync {
    // ── Static half (data-descriptor) ──────────────────────────────────────

    fn descriptor(&self) -> &DriverDescriptor;
    fn capabilities(&self) -> CapabilitySet;

    // ── Spawn capability ────────────────────────────────────────────────────

    /// Build the worker invocation string written into the pane as the
    /// spawn command. Replaces [`crate::effort::SpawnConfig::claude_invocation`]
    /// for the Claude driver.
    fn spawn_invocation(
        &self,
        model: &str,
        effort: Option<&str>,
        settings_path: Option<&Path>,
        non_opus_auto_mode: bool,
    ) -> String;

    // ── WorkspaceProvisioning capability ────────────────────────────────────

    /// Write per-session workspace files (prompt file, agent-rules, gitignore)
    /// and suppress the backend's first-run trust prompt.
    async fn provision_workspace(&self, workspace: &Path, prompt_text: &str, run_id: &str) -> anyhow::Result<()>;

    // ── PermissionPolicy capability ─────────────────────────────────────────

    /// Write the driver's permission/hooks config to `dest_dir` and return the
    /// path to the settings file (passed as `--settings` or equivalent to the
    /// worker CLI).
    async fn write_permission_config(&self, dest_dir: &Path) -> anyhow::Result<PathBuf>;

    // ── ProgressObservation capability ──────────────────────────────────────

    /// Fidelity tier of the [`WorkerEvent`] stream this driver produces.
    /// Claude declares [`ProgressFidelity::Rich`] (per-tool hook events).
    fn progress_fidelity(&self) -> ProgressFidelity;

    /// Build the driver's event-source wiring so the worker emits a lifecycle
    /// + tool-use stream the engine decodes into [`WorkerEvent`]s. For the
    /// Claude driver this is the `hooks` block routing every hook event to the
    /// `boss-event` shim; the spawn flow merges it into the worker settings.
    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressObservationWiring;

    /// Decode one raw event-source payload into a typed [`WorkerEvent`] that
    /// drives the (driver-agnostic) activity machine. For the Claude driver
    /// the raw payload is a hook JSON object; this delegates to
    /// [`boss_protocol::normalize_hook_event`].
    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError>;

    // ── PromptComposition capability ────────────────────────────────────────

    /// Driver-specific preamble injected at the top of the agent-rules file,
    /// naming the hook mechanism and the `.claude/`-style gitignore contract.
    fn agent_rules_preamble(&self) -> &'static str;

    // ── ControlVerbs capability ─────────────────────────────────────────────

    /// Classify a raw error string from the worker's output for
    /// transient-recovery decisions.
    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass;
}

pub mod claude;
pub use claude::ClaudeDriver;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_strict_capabilities_refuse_absent_driver() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let no_caps = CapabilitySet::new([]);

        assert_eq!(
            reqs.resolve_absence_disposition(Capability::StructuredOutput, &no_caps),
            Some(AbsenceDisposition::Refuse),
        );
        assert_eq!(
            reqs.resolve_absence_disposition(Capability::ToolUseInterception, &no_caps),
            Some(AbsenceDisposition::Refuse),
        );
    }

    #[test]
    fn non_strict_capability_uses_default_disposition() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let no_caps = CapabilitySet::new([]);

        // ModelAndEffortMenu is not required-strict for Design; default is Degrade.
        assert_eq!(
            reqs.resolve_absence_disposition(Capability::ModelAndEffortMenu, &no_caps),
            Some(AbsenceDisposition::Degrade),
        );
    }

    #[test]
    fn provided_capability_resolves_to_none() {
        let reqs = KindRequirements::for_kind(TaskKind::Design);
        let all_caps = CapabilitySet::new([Capability::StructuredOutput, Capability::ToolUseInterception]);

        assert_eq!(
            reqs.resolve_absence_disposition(Capability::StructuredOutput, &all_caps),
            None,
        );
    }

    #[test]
    fn absence_override_takes_precedence_over_default() {
        let caps =
            CapabilitySet::new([]).with_absence_override(Capability::ToolUseInterception, AbsenceDisposition::Refuse);

        // Default for ToolUseInterception is Degrade; override makes it Refuse.
        assert_eq!(
            caps.absence_disposition(Capability::ToolUseInterception),
            AbsenceDisposition::Refuse,
        );
    }

    #[test]
    fn task_kind_has_no_strict_requirements_by_default() {
        for kind in [
            TaskKind::Chore,
            TaskKind::Investigation,
            TaskKind::ProjectTask,
            TaskKind::Revision,
            TaskKind::Task,
        ] {
            let reqs = KindRequirements::for_kind(kind.clone());
            assert!(
                !reqs.is_required_strict(Capability::StructuredOutput),
                "{kind:?} should not require-strict StructuredOutput",
            );
            assert!(
                !reqs.is_required_strict(Capability::ToolUseInterception),
                "{kind:?} should not require-strict ToolUseInterception",
            );
        }
    }

    #[test]
    fn spawn_and_prompt_composition_refuse_when_absent() {
        assert_eq!(
            Capability::Spawn.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::PromptComposition.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::WorkspaceProvisioning.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
        assert_eq!(
            Capability::PermissionPolicy.default_absence_disposition(),
            AbsenceDisposition::Refuse,
        );
    }

    #[test]
    fn progress_and_turn_boundary_synthesize_when_absent() {
        assert_eq!(
            Capability::ProgressObservation.default_absence_disposition(),
            AbsenceDisposition::Synthesize,
        );
        assert_eq!(
            Capability::TurnBoundary.default_absence_disposition(),
            AbsenceDisposition::Synthesize,
        );
    }
}
