//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn` capability is live; remaining behavioural methods
//! are `unimplemented!()` pending their per-capability extraction tasks
//! (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use boss_protocol::{EffortLevel, NormalizeError, WorkerEvent, normalize_hook_event};

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, ModelMenu, ProgressFidelity, ProgressObservationConfig,
    ProgressObservationWiring, ToolUseInterceptionConfig, ToolUseInterceptionWiring, WorkerErrorClass,
};

// ---------------------------------------------------------------------------
// Claude model / effort menu (design §1.4 / §Mix-and-match)
// ---------------------------------------------------------------------------
//
// These are the per-driver table functions referenced from CLAUDE_DESCRIPTOR.model_menu.
// The same tables lived in `effort.rs` as global functions prior to this move.
// All callers now route through the driver's ModelMenu rather than calling
// these functions directly.

fn claude_effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    })
}

/// Default model slug for a given effort level.
///
/// Family aliases (`"sonnet"`, `"opus"`) are used so the engine auto-tracks the
/// latest snapshot per family without requiring a code change on each model release.
///
/// `Trivial` maps to `sonnet`, NOT `haiku`. Per issue #746 ("don't use haiku")
/// Boss must never dispatch a worker on Haiku: on the user's work machine Haiku
/// supports neither auto mode nor `--dangerously-skip-permissions`, so it prompts
/// for every edit. Trivial work still runs at `--effort low`; only the model floor
/// is raised to Sonnet. Do not lower it back to Haiku.
fn claude_default_model_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::Trivial | EffortLevel::Small | EffortLevel::Medium => "sonnet",
        EffortLevel::Large | EffortLevel::Max => "opus",
    }
}

/// Optional per-level worker-prompt addendum prepended to `.claude/initial-prompt.txt`.
/// `None` for levels where the existing task-implementation framing is already correct.
fn claude_prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(
            "Begin with a written plan. Identify the files you expect to touch and the \
             order you'll touch them in. Confirm the approach against the work item's \
             description before writing code.",
        ),
    }
}

/// Returns `true` iff the model slug belongs to the Opus or Fable tier (both require
/// `--permission-mode auto` instead of `--dangerously-skip-permissions`).
/// Matching is case-insensitive substring search.
///
/// Note: Fable (`claude-fable-5`) has been suspended but existing rows may carry it
/// as a `model_override`; they still receive `--permission-mode auto`.
fn claude_model_requires_auto_permissions(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("opus") || lower.contains("fable")
}

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        engine_default: "opus",
        effort_value_for_level: claude_effort_value_for_level,
        default_model_for_level: claude_default_model_for_level,
        prompt_addendum_for_level: claude_prompt_addendum_for_level,
        model_requires_auto_permissions: claude_model_requires_auto_permissions,
    },
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

/// Inline Python decision hook that blocks all workers from launching Boss
/// itself — the macOS app or its bundled engine. Always applied (matcher `Bash`,
/// inspects the command string). Blocks `open -a Boss`, `open … Boss.app`,
/// `Boss.app/Contents/MacOS/Boss`, the bundled engine binary, `bazel run` of the
/// app-macos or engine targets, and `swift run`. Does NOT block `bazel test` or
/// `bazel build`.
const BOSS_LAUNCH_GUARD_COMMAND: &str = concat!(
    "python3 -c \"",
    "import json,sys,re; ",
    "inp=json.load(sys.stdin); ",
    "cmd=inp.get('tool_input',{}).get('command',''); ",
    r#"m=re.search(r'(\bopen\b[^\n]*(Boss\.app|-a\s+Boss\b|-b\s+dev\.spinyfin\.bossmacapp))|Boss\.app/Contents/MacOS/Boss|Boss\.app/Contents/Resources/bin/engine|((bazel|bazelisk)\s+run\b[^\n]*tools/boss/(app-macos|engine))|(\bswift\s+run\b)',cmd); "#,
    "msg='Workers must not launch or run Boss itself. This command would start the Boss app or its bundled engine, which attaches to the operator live engine on /tmp/boss-engine.sock, collides with the running engine, and triggers OS permission prompts. Building and unit tests are fine (bazel build, bazel test); launching/running the app or engine is not. Runtime and UI verification are the coordinator job.'; ",
    "print(json.dumps({'decision':'block','reason':msg}) if m else json.dumps({'decision':'approve'})); ",
    "\""
);

/// Inline Python decision hook that blocks all Standard workers from pushing
/// branches or opening PRs via bare VCS commands (`gh pr create`, `jj git push`,
/// `git push`). Uses `shlex.split()` so push/PR-creation phrases inside quoted
/// arguments do NOT trigger the block.
///
/// Applies to ALL `WorkerKind::Standard` workers (local and remote). The
/// revision-specific guard ([`REVISION_PR_GUARD_COMMAND`]) stacks on top for
/// revision workers and adds additional blocks.
pub(crate) const PR_REDIRECT_GUARD_COMMAND: &str = concat!(
    "python3 -c \"\n",
    "import json,os,sys,re,shlex\n",
    "inp=json.load(sys.stdin)\n",
    "cmd=inp.get('tool_input',{}).get('command','')\n",
    "DELIMS={'&&','||',';','|','&'}\n",
    "try:\n",
    "    toks=shlex.split(cmd,posix=True)\n",
    "except Exception:\n",
    "    toks=cmd.split()\n",
    "groups=[]\n",
    "cur=[]\n",
    "for t in toks:\n",
    "    if t in DELIMS:\n",
    "        if cur:\n",
    "            groups.append(cur[:])\n",
    "        cur=[]\n",
    "    else:\n",
    "        cur.append(t)\n",
    "if cur:\n",
    "    groups.append(cur)\n",
    "matched=None\n",
    "for g in groups:\n",
    "    i=0\n",
    "    while i<len(g) and re.match(r'^[A-Za-z_][A-Za-z0-9_]*=',g[i]):\n",
    "        i+=1\n",
    "    rest=g[i:]\n",
    "    prog=os.path.basename(rest[0]) if rest else ''\n",
    "    if len(rest)>=3 and prog=='gh' and rest[1]=='pr' and rest[2]=='create':\n",
    "        matched='gh pr create'\n",
    "        break\n",
    "    if len(rest)>=3 and prog=='jj' and rest[1]=='git' and rest[2]=='push':\n",
    "        matched='jj git push'\n",
    "        break\n",
    "    if len(rest)>=2 and prog=='git' and rest[1]=='push':\n",
    "        matched='git push'\n",
    "        break\n",
    "if matched:\n",
    "    msg='Workers must not push branches or open PRs with bare VCS commands (blocked: '+matched+'). Use cube instead: cube pr create --branch <branch> (new PR: pushes the branch and opens the PR in one step, jj-aware, no GIT_DIR) or cube pr update --branch <branch> (existing PR: pushes new commits to it). Never use jj git push, git push, or gh pr create directly.'\n",
    "    print(json.dumps({'decision':'block','reason':msg}))\n",
    "else:\n",
    "    print(json.dumps({'decision':'approve'}))\n",
    "\""
);

/// Inline Python decision hook that guards revision tasks from opening new PRs.
/// Uses `shlex.split()` to tokenise the Bash command so PR-creation phrases
/// inside quoted arguments do NOT trigger the block. Blocks `gh pr create`,
/// `cube pr create`, and the deprecated `cube pr ensure`; allows `cube pr update`.
pub(crate) const REVISION_PR_GUARD_COMMAND: &str = concat!(
    "python3 -c \"\n",
    "import json,sys,re,shlex\n",
    "inp=json.load(sys.stdin)\n",
    "cmd=inp.get('tool_input',{}).get('command','')\n",
    "DELIMS={'&&','||',';','|','&'}\n",
    "try:\n",
    "    toks=shlex.split(cmd,posix=True)\n",
    "except Exception:\n",
    "    toks=cmd.split()\n",
    "groups=[]\n",
    "cur=[]\n",
    "for t in toks:\n",
    "    if t in DELIMS:\n",
    "        if cur:\n",
    "            groups.append(cur[:])\n",
    "        cur=[]\n",
    "    else:\n",
    "        cur.append(t)\n",
    "if cur:\n",
    "    groups.append(cur)\n",
    "def branch_of(g):\n",
    "    for j,t in enumerate(g):\n",
    "        if t in ('--branch','--head') and j+1<len(g):\n",
    "            return g[j+1]\n",
    "        if t.startswith('--branch=') or t.startswith('--head='):\n",
    "            return t.split('=',1)[1]\n",
    "    return None\n",
    "matched=None\n",
    "br=None\n",
    "for g in groups:\n",
    "    i=0\n",
    "    while i<len(g) and re.match(r'^[A-Za-z_][A-Za-z0-9_]*=',g[i]):\n",
    "        i+=1\n",
    "    rest=g[i:]\n",
    "    if len(rest)>=3 and rest[0]=='gh' and rest[1]=='pr' and rest[2]=='create':\n",
    "        matched='gh pr create'\n",
    "        br=branch_of(rest)\n",
    "        break\n",
    "    if len(rest)>=3 and rest[0]=='cube' and rest[1]=='pr' and rest[2] in ('create','ensure'):\n",
    "        matched='cube pr '+rest[2]\n",
    "        br=branch_of(rest)\n",
    "        break\n",
    "if matched:\n",
    "    sug='cube pr update --branch '+br if br else 'cube pr update --branch <your-pr-bookmark>'\n",
    "    msg='Revision tasks push commits to the existing parent PR; they must not open a new PR (matched command: '+matched+'). Push your commits to the existing PR with: '+sug\n",
    "    print(json.dumps({'decision':'block','reason':msg}))\n",
    "else:\n",
    "    print(json.dumps({'decision':'approve'}))\n",
    "\""
);

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
        if claude_model_requires_auto_permissions(model) || non_opus_auto_mode {
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

    fn tool_use_interception_wiring(&self, config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        let mut hooks: Vec<serde_json::Value> = Vec::new();

        // 1. Path guard (data-dir sandbox). Canonicalises every candidate path
        //    and blocks any tool call that resolves inside the Boss data dir.
        //    Matcher `*` covers all tools. Local workers only — the script is
        //    never shipped to remote hosts.
        if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
            let guard_command = format!(
                "BOSS_DATA_DIR={dir} python3 {script}",
                dir = shell_escape(&data_dir.display().to_string()),
                script = shell_escape(&guard_script.display().to_string()),
            );
            hooks.push(serde_json::json!({
                "matcher": "*",
                "hooks": [{"type": "command", "command": guard_command}],
            }));
        }

        // 2. Boss-launch guard (always on, all workers). Blocks the worker from
        //    starting the Boss macOS app or its bundled engine binary.
        hooks.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": BOSS_LAUNCH_GUARD_COMMAND}],
        }));

        // 3. PR redirect guard (Standard workers only, local AND remote). Blocks
        //    bare VCS push and `gh pr create`; redirects to cube pr create/update.
        //    Reviewer and triage workers skip this — their deny rules already block
        //    push operations.
        if config.is_standard_worker {
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": PR_REDIRECT_GUARD_COMMAND}],
            }));
        }

        // 4. Checkleft push guard (local Standard workers only). Blocks jj/git push
        //    when the repo's checkleft reports errors. Remote workers skip it — the
        //    script is materialised locally and never shipped.
        if config.is_standard_worker {
            if let Some(checkleft_script) = &config.checkleft_guard_script {
                let guard_command = format!(
                    "python3 {script}",
                    script = shell_escape(&checkleft_script.display().to_string()),
                );
                hooks.push(serde_json::json!({
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": guard_command}],
                }));
            }
        }

        // 5. Revision PR guard. Blocks PR creation (`gh pr create`, `cube pr
        //    create`, `cube pr ensure`) for revision workers, which must push
        //    commits to the existing parent PR, never open a new one.
        if config.is_revision {
            hooks.push(serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": REVISION_PR_GUARD_COMMAND}],
            }));
        }

        ToolUseInterceptionWiring {
            pre_tool_use_hooks: hooks,
        }
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

    fn local_standard_config() -> ToolUseInterceptionConfig {
        ToolUseInterceptionConfig {
            data_dir: Some(PathBuf::from("/Library/Application Support/Boss")),
            path_guard_script: Some(PathBuf::from("/tmp/boss-settings/boss-path-guard.py")),
            checkleft_guard_script: Some(PathBuf::from("/tmp/boss-settings/boss-checkleft-push-guard.py")),
            is_revision: false,
            is_standard_worker: true,
        }
    }

    fn remote_standard_config() -> ToolUseInterceptionConfig {
        ToolUseInterceptionConfig {
            data_dir: None,
            path_guard_script: None,
            checkleft_guard_script: None,
            is_revision: false,
            is_standard_worker: true,
        }
    }

    #[test]
    fn local_standard_worker_gets_all_five_guards() {
        let wiring = ClaudeDriver.tool_use_interception_wiring(&local_standard_config());
        // path guard + boss-launch guard + PR redirect guard + checkleft guard = 4
        // (no revision guard since is_revision: false)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            4,
            "local standard non-revision worker must get exactly 4 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
    }

    #[test]
    fn local_revision_worker_gets_all_five_guards() {
        let mut config = local_standard_config();
        config.is_revision = true;
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        // path guard + boss-launch guard + PR redirect guard + checkleft guard + revision guard = 5
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            5,
            "local standard revision worker must get exactly 5 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            cmds.iter().any(|c| c.contains("ensure")),
            "revision guard must block cube pr ensure: {cmds:?}",
        );
    }

    #[test]
    fn remote_worker_skips_path_guard_and_checkleft() {
        let wiring = ClaudeDriver.tool_use_interception_wiring(&remote_standard_config());
        // boss-launch guard + PR redirect guard = 2 (no path guard, no checkleft)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            2,
            "remote standard worker must get exactly 2 guards (boss-launch + PR redirect): {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            !cmds.iter().any(|c| c.contains("BOSS_DATA_DIR")),
            "remote worker must not have the path guard: {cmds:?}",
        );
        assert!(
            !cmds.iter().any(|c| c.contains("checkleft")),
            "remote worker must not have the checkleft guard: {cmds:?}",
        );
    }

    #[test]
    fn path_guard_command_names_data_dir_and_script() {
        let config = local_standard_config();
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        let path_guard = wiring
            .pre_tool_use_hooks
            .iter()
            .find(|e| {
                e["hooks"][0]["command"]
                    .as_str()
                    .unwrap_or("")
                    .contains("BOSS_DATA_DIR")
            })
            .expect("path guard must be present for local workers");
        let cmd = path_guard["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("BOSS_DATA_DIR="), "must set BOSS_DATA_DIR: {cmd}");
        assert!(cmd.contains("boss-path-guard.py"), "must reference script: {cmd}");
        assert_eq!(path_guard["matcher"], "*", "path guard matcher must be '*'");
    }

    #[test]
    fn boss_launch_guard_is_always_present() {
        for config in [local_standard_config(), remote_standard_config()] {
            let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
            assert!(
                wiring.pre_tool_use_hooks.iter().any(|e| {
                    e["hooks"][0]["command"]
                        .as_str()
                        .unwrap_or("")
                        .contains("Workers must not launch or run Boss itself")
                }),
                "boss-launch guard must be present in every config",
            );
        }
    }

    #[test]
    fn reviewer_worker_skips_pr_redirect_and_checkleft() {
        let config = ToolUseInterceptionConfig {
            data_dir: Some(PathBuf::from("/Library/Boss")),
            path_guard_script: Some(PathBuf::from("/tmp/boss-path-guard.py")),
            checkleft_guard_script: Some(PathBuf::from("/tmp/boss-checkleft-push-guard.py")),
            is_revision: false,
            is_standard_worker: false,
        };
        let wiring = ClaudeDriver.tool_use_interception_wiring(&config);
        // path guard + boss-launch guard = 2 (no PR redirect, no checkleft)
        assert_eq!(
            wiring.pre_tool_use_hooks.len(),
            2,
            "non-standard (reviewer/triage) worker must get exactly 2 guards: {:?}",
            wiring.pre_tool_use_hooks,
        );
        let cmds: Vec<&str> = wiring
            .pre_tool_use_hooks
            .iter()
            .filter_map(|e| e["hooks"][0]["command"].as_str())
            .collect();
        assert!(
            !cmds.iter().any(|c| c.contains("jj git push")),
            "non-standard worker must not have the PR redirect guard: {cmds:?}",
        );
    }
}
