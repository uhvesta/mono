use super::*;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serializes tests that touch the *shared* worker-settings dir
/// (`worker_settings_dir()`, a fixed `$TMPDIR` path). `write_workspace_files`
/// truncate-writes the global `boss-path-guard.py` there; a concurrent
/// reader of that same file otherwise observes a half-written (empty)
/// script. The path isn't per-test overridable, so a lock is the
/// minimal isolation. Recovers from poisoning so one failing test
/// doesn't cascade.
static SHARED_SETTINGS_DIR_LOCK: Mutex<()> = Mutex::new(());

fn lock_shared_settings_dir() -> std::sync::MutexGuard<'static, ()> {
    SHARED_SETTINGS_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII guard that points `$HOME` at a throwaway temp dir for the
/// duration of a test. `write_workspace_files` now calls
/// `pre_trust_workspace`, which writes `~/.claude.json`; without this
/// redirection a test run would pollute the developer's real
/// `~/.claude.json` with stale temp-dir project entries. `$HOME` is
/// process-global, so hold this only while `lock_shared_settings_dir()`
/// is held (every `write_workspace_files` test does). Restores the
/// prior `$HOME` on drop.
struct HomeGuard {
    _home: TempDir,
    original: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let original = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
        }
        Self { _home: home, original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

fn sample_input() -> WorkerSetupInput {
    WorkerSetupInput {
        run_id: "run-sample".into(),
        lease_id: "lease-uuid-abc".into(),
        workspace_path: PathBuf::from("/Users/brianduff/Documents/dev/workspaces/mono-agent-007"),
        events_socket_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/events.sock"),
        boss_event_path: PathBuf::from("/Users/brianduff/Library/Application Support/Boss/bin/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    }
}

#[test]
fn remote_settings_drop_data_dir_sandbox_but_keep_hooks_and_static_denies() {
    // A remote worker's events socket is the forwarded /tmp socket and
    // its shim is resolved by name on the remote PATH.
    let input = WorkerSetupInput {
        run_id: "exec_remote_1".into(),
        lease_id: "lease-remote".into(),
        workspace_path: PathBuf::from("/Users/zak/Documents/dev/workspaces/mono-agent-003"),
        events_socket_path: PathBuf::from("/tmp/boss-events-exec_remote_1.sock"),
        boss_event_path: PathBuf::from("boss-event"),
        draft_pr_mode: false,
        execution_kind: "task_implementation".into(),
        task_kind: Some("task".into()),
        worker_kind: WorkerKind::Standard,
    };
    let parsed: serde_json::Value = serde_json::from_str(&render_remote_settings_json(&input)).unwrap();

    // All seven boss-event hook events are still wired.
    let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
    for name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(name), "missing hook: {name}");
    }

    // The boss-event command points at the FORWARDED socket + remote
    // shim resolved by name.
    let stop_cmd = hooks["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(stop_cmd.contains("/tmp/boss-events-exec_remote_1.sock"));
    assert!(stop_cmd.contains("BOSS_RUN_ID='exec_remote_1'"));
    assert!(stop_cmd.trim_end().ends_with("'boss-event'"));

    // No engine-data-dir sandbox: the deny list must NOT fence the
    // worker off the forwarded socket's parent (/tmp), and there is
    // no python path-guard hook (the script is never shipped remote).
    let deny = parsed["permissions"]["deny"].as_array().unwrap();
    assert!(
        !deny.iter().any(|r| {
            let s = r.as_str().unwrap();
            s.starts_with("Read(/tmp") || s.starts_with("Write(/tmp") || s.starts_with("Edit(/tmp")
        }),
        "remote settings must not fence the worker off /tmp: {deny:?}"
    );
    let pre = serde_json::to_string(&hooks["PreToolUse"]).unwrap();
    assert!(
        !pre.contains("boss-path-guard.py") && !pre.contains("BOSS_DATA_DIR="),
        "remote settings must not install the data-dir path-guard hook"
    );

    // The static guards survive: bossctl deny + the boss-launch guard.
    assert!(deny.iter().any(|r| r.as_str() == Some("Bash(bossctl)")));
    assert!(
        pre.contains("Workers must not launch or run Boss itself"),
        "boss-launch guard must remain on remote workers"
    );

    // Sanity: the LOCAL renderer DOES install the path guard, proving
    // the remote variant is the one dropping it.
    assert!(render_settings_json(&input).contains("boss-path-guard.py"));
}

#[test]
fn claude_md_mentions_workspace_and_lease() {
    let input = sample_input();
    let rendered = render_claude_md(&input);
    assert!(rendered.contains(input.workspace_path.to_str().unwrap()));
    assert!(rendered.contains(&input.lease_id));
    assert!(rendered.contains("`jj`"));
    assert!(rendered.contains("PR"));
}

#[test]
fn claude_md_warns_origin_is_a_local_mirror() {
    // Workers must be told that in a cube workspace `origin` is a local
    // mirror, not GitHub, and that pushes must be confirmed against
    // GitHub's head sha — never against the remote they pushed to.
    let input = sample_input();
    let rendered = render_claude_md(&input);
    assert!(rendered.contains("LOCAL MIRROR"));
    assert!(rendered.contains("ls-remote"));
    assert!(rendered.contains(".head.sha") || rendered.contains(".commit.sha"));
}

#[test]
fn claude_md_forbids_editor_fallthrough_for_commit_messages() {
    let input = sample_input();
    let rendered = render_claude_md(&input);
    // The rule must explicitly call out `-m` and the editor
    // fallthrough so a worker that grepped only for "commit" still
    // hits the guidance.
    assert!(rendered.contains("-m"));
    assert!(rendered.contains("$EDITOR"));
    assert!(rendered.contains("jj describe"));
    assert!(rendered.contains("git commit"));
}

#[test]
fn settings_json_is_valid_json_with_all_seven_hooks() {
    let input = sample_input();
    let rendered = render_settings_json(&input);
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
    for name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(hooks.contains_key(name), "missing hook: {name}");
        let entries = hooks.get(name).unwrap().as_array().unwrap();
        // The boss-event shim is always the first entry for every
        // hook event. `PreToolUse` carries extra entries (the
        // deterministic path guard, the always-on boss-launch guard,
        // plus a revision-only guard); the other six events are wired
        // exactly once.
        assert!(!entries.is_empty(), "{name} has no hook entries");
        assert_eq!(entries[0]["matcher"], "*");
        if name != "PreToolUse" {
            assert_eq!(entries.len(), 1, "{name} should have exactly one hook entry");
        }
    }
}

#[test]
fn settings_json_threads_socket_lease_and_shim_into_command() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    assert!(command.contains("events.sock"));
    assert!(command.contains("lease-uuid-abc"));
    assert!(command.contains("boss-event"));
    assert!(command.starts_with("BOSS_EVENTS_SOCKET="));
}

#[test]
fn settings_json_inlines_workspace_into_every_hook_command() {
    // The shim writes its on-disk event buffer relative to
    // `BOSS_WORKSPACE` when the engine socket is unreachable. The
    // hook command must inline-prefix this env var so the buffer
    // lives in the lease's workspace regardless of cwd.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let workspace_str = input.workspace_path.display().to_string();
    for hook_name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        let command = parsed["hooks"][hook_name][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("missing command for {hook_name}"));
        assert!(
            command.contains(&format!("BOSS_WORKSPACE='{workspace_str}'")),
            "{hook_name} command missing BOSS_WORKSPACE=<workspace>: {command}",
        );
    }
}

#[test]
fn settings_json_inlines_run_id_into_every_hook_command() {
    // BOSS_RUN_ID must be inline-prefixed on every hook command so
    // the `boss-event` shim can splice `_boss_run_id` into the
    // payload regardless of whether claude propagates env from the
    // worker pane to its hook subprocess. Without this, the engine
    // can't correlate hook events to runs and the live worker
    // state stays pinned at `Spawning` for the worker's lifetime.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    for hook_name in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        let command = parsed["hooks"][hook_name][0]["hooks"][0]["command"]
            .as_str()
            .unwrap_or_else(|| panic!("missing command for {hook_name}"));
        assert!(
            command.contains("BOSS_RUN_ID='run-sample'"),
            "{hook_name} command missing BOSS_RUN_ID=<run_id>: {command}",
        );
    }
}

#[test]
fn settings_json_denies_boss_state_dir_reads_writes_and_edits() {
    // The acceptance criterion for the worker-sandboxing change:
    // a worker spawned by the engine cannot, via Read / Edit /
    // Write, touch any file under the Boss state dir. The deny
    // list must name the dir and the `**` subtree for each tool
    // so a `Read("…/Boss")` ls and a `Read("…/Boss/state.db")`
    // both deny.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let deny = parsed["permissions"]["deny"].as_array().expect("deny array present");
    let deny_set: Vec<&str> = deny.iter().filter_map(|v| v.as_str()).collect();
    let boss_dir = "/Users/brianduff/Library/Application Support/Boss";
    for tool in ["Read", "Edit", "Write"] {
        let bare = format!("{tool}({boss_dir})");
        let glob = format!("{tool}({boss_dir}/**)");
        assert!(
            deny_set.iter().any(|r| *r == bare),
            "expected deny rule {bare} in {deny_set:?}",
        );
        assert!(
            deny_set.iter().any(|r| *r == glob),
            "expected deny rule {glob} in {deny_set:?}",
        );
    }
}

#[test]
fn settings_json_denies_bossctl_and_engine_lifecycle_verbs() {
    // bossctl is coordinator-only; `boss engine start|stop` reach
    // into engine process state. The rest of the `boss` surface
    // talks to the engine over its IPC socket and is fine.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in [
        "Bash(bossctl)",
        "Bash(bossctl:*)",
        "Bash(boss engine start)",
        "Bash(boss engine start:*)",
        "Bash(boss engine stop)",
        "Bash(boss engine stop:*)",
    ] {
        assert!(deny.contains(&rule), "expected deny rule {rule} in {deny:?}",);
    }
}

#[test]
fn reviewer_kind_adds_write_and_push_deny_rules_standard_does_not() {
    // Standard workers must not carry the reviewer deny rules — that
    // would break every implementation worker.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
    let std_deny: Vec<&str> = std_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in reviewer_deny_rules(&std_input.workspace_path) {
        assert!(
            !std_deny.contains(&rule.as_str()),
            "standard worker must NOT carry reviewer deny rule: {rule}",
        );
    }

    // Reviewer workers must carry every rule from reviewer_deny_rules().
    let mut rev_input = sample_input();
    rev_input.worker_kind = WorkerKind::Reviewer;
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input)).unwrap();
    let rev_deny: Vec<&str> = rev_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in reviewer_deny_rules(&rev_input.workspace_path) {
        assert!(
            rev_deny.contains(&rule.as_str()),
            "reviewer worker must carry deny rule: {rule} (got {rev_deny:?})",
        );
    }
    // Spot-check the most critical publish rules.
    for critical in [
        "Bash(jj git push:*)",
        "Bash(gh pr create:*)",
        "Bash(gh pr comment:*)",
        "Bash(cube pr:*)",
    ] {
        assert!(
            rev_deny.contains(&critical),
            "reviewer must deny {critical} (got {rev_deny:?})",
        );
    }
    // The reviewer's file-write deny is scoped to the worker-workspaces root
    // (NOT a blanket `**`) so it can still write its one out-of-tree
    // structured-output artifact, while sibling workspaces stay protected.
    let fence = rev_input
        .workspace_path
        .parent()
        .unwrap_or(&rev_input.workspace_path)
        .display();
    for critical in [format!("Edit({fence}/**)"), format!("Write({fence}/**)")] {
        assert!(
            rev_deny.contains(&critical.as_str()),
            "reviewer must deny workspaces-root-scoped {critical} (got {rev_deny:?})",
        );
    }
    // And it must NOT carry the blanket file-write denies — that would block
    // the artifact write outside the checkout.
    for blanket in ["Edit(**)", "Write(**)"] {
        assert!(
            !rev_deny.contains(&blanket),
            "reviewer must NOT carry blanket {blanket} (got {rev_deny:?})",
        );
    }
}

#[test]
fn reviewer_settings_json_has_fast_mode_standard_does_not() {
    // Reviewer workers are latency-sensitive: fastMode must be true.
    let mut rev_input = sample_input();
    rev_input.worker_kind = WorkerKind::Reviewer;
    let rev_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&rev_input)).unwrap();
    assert_eq!(
        rev_parsed["fastMode"],
        serde_json::json!(true),
        "reviewer settings.json must have fastMode:true",
    );

    // Standard workers must NOT have fastMode set at all.
    let std_input = sample_input();
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
    assert!(
        std_parsed.get("fastMode").is_none() || std_parsed["fastMode"] == serde_json::json!(null),
        "standard worker settings.json must not carry fastMode (got {:?})",
        std_parsed.get("fastMode"),
    );
}

#[test]
fn triage_kind_adds_no_publish_deny_rules_standard_does_not() {
    // Triage workers must carry the read-only / no-publish denylist (they
    // investigate and emit a marker; they must not edit, push, or open a
    // PR). Standard implementation workers must NOT carry it.
    let std_input = sample_input(); // worker_kind: Standard
    let std_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&std_input)).unwrap();
    let std_deny: Vec<&str> = std_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in triage_deny_rules() {
        assert!(
            !std_deny.contains(&rule.as_str()),
            "standard worker must NOT carry triage deny rule: {rule}",
        );
    }

    let mut triage_input = sample_input();
    triage_input.worker_kind = WorkerKind::Triage;
    let triage_parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&triage_input)).unwrap();
    let triage_deny: Vec<&str> = triage_parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for critical in [
        "Edit(**)",
        "Write(**)",
        "Bash(jj git push:*)",
        "Bash(git push:*)",
        "Bash(gh pr create:*)",
        "Bash(cube pr:*)",
    ] {
        assert!(
            triage_deny.contains(&critical),
            "triage worker must deny {critical} (got {triage_deny:?})",
        );
    }
    // `boss task create` is the triage worker's sole write action and must
    // NOT be denied (none of the no-publish rules touch it).
    assert!(
        !triage_deny.iter().any(|r| r.contains("task create")),
        "triage worker must be able to run `boss task create` (got {triage_deny:?})",
    );
}

#[test]
fn triage_kind_renders_triage_claude_md_without_pr_mandate() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Triage;
    let rendered = render_claude_md(&input);
    // Routed to the triage CLAUDE.md: restates the marker contract.
    assert!(
        rendered.contains("automation: task") && rendered.contains("automation: skip"),
        "triage worker CLAUDE.md must restate the decision-marker contract",
    );
    // Must NOT carry the Standard implementation PR-delivery mandate — that
    // conflict is what leaves triage runs ending without a decision marker.
    assert!(
        !rendered.contains("Pull requests are the deliverable"),
        "triage CLAUDE.md must not include the standard PR-required reminder",
    );
    assert!(
        !rendered.contains("PR creation is your terminal act"),
        "triage CLAUDE.md must not state PR creation is the terminal act",
    );
    // Lease id surfaced, workspace path not hardcoded.
    assert!(rendered.contains(&input.lease_id));
    assert!(
        !rendered.contains(input.workspace_path.to_str().unwrap()),
        "triage CLAUDE.md must not hardcode the workspace path",
    );
}

#[test]
fn settings_json_does_not_deny_workspace_paths() {
    // Defensive: a buggy deny rule that accidentally fences off
    // `~/Documents/dev/workspaces/…` would break every worker
    // (their lease lives there). Verify no deny rule names the
    // workspace root.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let deny: Vec<&str> = parsed["permissions"]["deny"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for rule in &deny {
        assert!(
            !rule.contains("workspaces"),
            "deny rule must not target the workspaces dir: {rule}",
        );
    }
}

#[test]
fn claude_md_warns_against_touching_boss_state_dir() {
    // A worker that misses the harness-level deny rule (e.g. a
    // future claude-code release changes the rule format) needs
    // a soft soft-rule in the CLAUDE.md system prompt to know
    // it's off-limits. Belt-and-suspenders.
    let input = sample_input();
    let rendered = render_claude_md(&input);
    assert!(
        rendered.contains("Library/Application Support/Boss"),
        "CLAUDE.md must call out the Boss state dir explicitly",
    );
    assert!(
        rendered.contains("bossctl"),
        "CLAUDE.md must explicitly identify bossctl as coordinator-only",
    );
}

#[test]
fn settings_json_pins_permissions_default_mode_to_auto() {
    // Workers must spawn in claude's "auto mode" so the soft
    // do-not-ask-the-human-for-permission instruction in the
    // system prompt is enforced at the harness level — without
    // this, a worker whose user has a global `default`
    // permission mode hangs on the first tool call and the
    // execution stalls until a human clicks yes. `auto` (not
    // `bypassPermissions`) is the intended shape: it runs
    // autonomously while still honoring the user's permission
    // allow/deny rules, which the environment policy requires.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    assert_eq!(
        parsed["permissions"]["defaultMode"],
        serde_json::Value::String("auto".into()),
        "expected permissions.defaultMode == 'auto', got: {parsed}",
    );
}

#[test]
fn shell_escape_quotes_paths_with_spaces() {
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
    // Application Support has a space — must round-trip through
    // single-quote escaping.
    assert!(command.contains("'/Users/brianduff/Library/Application Support/Boss/events.sock'"));
}

#[test]
fn shell_escape_single_quote_uses_outer_close_inner_open_pattern() {
    // Ensure paths containing single-quotes can't break out of the
    // quoting envelope. Standard POSIX trick: ' is closed, then
    // \' is appended literally, then ' reopens the quote.
    let escaped = shell_escape("a'b");
    assert_eq!(escaped, r#"'a'\''b'"#);
}

#[test]
fn write_workspace_files_creates_claude_dir_and_writes_all_files() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-1".into(),
        lease_id: "test-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    let written = write_workspace_files(&input).unwrap();

    assert!(written.claude_md_path.exists());
    assert!(written.settings_path.exists());
    assert!(written.gitignore_path.exists());
    assert_eq!(written.claude_md_path, dir.path().join(".claude").join("CLAUDE.md"));
    assert_eq!(written.gitignore_path, dir.path().join(".claude").join(".gitignore"));

    let claude_md_contents = std::fs::read_to_string(&written.claude_md_path).unwrap();
    assert!(claude_md_contents.contains("test-lease"));

    // The settings file must be valid JSON on disk.
    let settings_contents = std::fs::read_to_string(&written.settings_path).unwrap();
    let _: serde_json::Value = serde_json::from_str(&settings_contents).unwrap();

    // Regression guard for the clobbered-`.claude/settings.json`
    // bug: the engine must NEVER drop a settings file into the
    // workspace tree (where `jj`/`git` could ship it). Neither the
    // shared `settings.json` nor the local-override
    // `settings.local.json` may exist under `.claude/`, and the
    // settings file it does write must live outside the workspace.
    let claude_dir = dir.path().join(".claude");
    assert!(
        !claude_dir.join("settings.json").exists(),
        "engine must not write .claude/settings.json into the workspace",
    );
    assert!(
        !claude_dir.join("settings.local.json").exists(),
        "engine must not write .claude/settings.local.json into the workspace",
    );
    assert!(
        !written.settings_path.starts_with(dir.path()),
        "worker settings file must live outside the workspace tree, got: {}",
        written.settings_path.display(),
    );

    // The .gitignore must use the catch-all `*` pattern so every
    // engine-injected file in `.claude/` (including dotfiles and
    // `.gitignore` itself) is hidden from `jj status` / `git status`.
    let gitignore_contents = std::fs::read_to_string(&written.gitignore_path).unwrap();
    assert_eq!(gitignore_contents, "*\n");
}

#[test]
fn write_workspace_files_pre_trusts_workspace_in_claude_json() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-trust".into(),
        lease_id: "lease-trust".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input).unwrap();

    // The redirected HOME now has a ~/.claude.json marking this
    // workspace as trusted, so the worker's claude session skips the
    // folder-trust dialog.
    let config_path = claude_global_config_path().unwrap();
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let key = dir.path().display().to_string();
    assert_eq!(
        config["projects"][&key]["hasTrustDialogAccepted"],
        serde_json::Value::Bool(true),
    );
}

#[test]
fn pre_trust_creates_config_when_absent() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".claude.json");
    let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-001");

    pre_trust_workspace_in(&config, &workspace).unwrap();

    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let key = workspace.display().to_string();
    assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
    // The onboarding counter is seeded so onboarding doesn't re-prompt.
    assert_eq!(value["projects"][&key]["projectOnboardingSeenCount"], 0);
}

#[test]
fn pre_trust_preserves_other_projects_and_top_level_keys() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".claude.json");
    // A realistic config: a top-level key plus another project with
    // its own state. Pre-trust must leave both untouched.
    let existing = serde_json::json!({
        "numStartups": 42,
        "projects": {
            "/some/other/project": {
                "hasTrustDialogAccepted": true,
                "lastCost": 1.23,
            },
        },
    });
    std::fs::write(&config, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

    let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-002");
    pre_trust_workspace_in(&config, &workspace).unwrap();

    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    // Top-level key and the pre-existing project survive verbatim.
    assert_eq!(value["numStartups"], 42);
    assert_eq!(value["projects"]["/some/other/project"]["lastCost"], 1.23);
    assert_eq!(value["projects"]["/some/other/project"]["hasTrustDialogAccepted"], true);
    // The new workspace is now trusted.
    let key = workspace.display().to_string();
    assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
}

#[test]
fn pre_trust_is_a_noop_when_already_trusted() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".claude.json");
    let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-003");
    let key = workspace.display().to_string();
    // Existing entry already trusted, with an extra field a live
    // claude session would have written.
    let existing = serde_json::json!({
        "projects": {
            &key: { "hasTrustDialogAccepted": true, "lastSessionId": "abc" },
        },
    });
    let serialized = serde_json::to_string_pretty(&existing).unwrap();
    std::fs::write(&config, &serialized).unwrap();

    pre_trust_workspace_in(&config, &workspace).unwrap();

    // The file is left byte-for-byte unchanged: no rewrite of the
    // shared config when the workspace is already trusted.
    assert_eq!(std::fs::read_to_string(&config).unwrap(), serialized);
}

#[test]
fn pre_trust_leaves_corrupt_config_untouched() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".claude.json");
    let garbage = "{ this is not valid json";
    std::fs::write(&config, garbage).unwrap();

    let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-004");
    let err = pre_trust_workspace_in(&config, &workspace).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    // The corrupt file must NOT be clobbered — we'd rather skip
    // pre-trust than destroy the user's config.
    assert_eq!(std::fs::read_to_string(&config).unwrap(), garbage);
}

#[test]
fn pre_trust_treats_empty_config_as_fresh() {
    let dir = TempDir::new().unwrap();
    let config = dir.path().join(".claude.json");
    std::fs::write(&config, "   \n").unwrap();

    let workspace = PathBuf::from("/Users/x/.local/share/cube/workspaces/mono-agent-005");
    pre_trust_workspace_in(&config, &workspace).unwrap();

    let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    let key = workspace.display().to_string();
    assert_eq!(value["projects"][&key]["hasTrustDialogAccepted"], true);
}

/// A leaked settings file holding a `boss-event` Stop hook with a
/// stale `BOSS_RUN_ID` (as written by a pre-fix engine build into a
/// reused workspace). Mirrors the real on-disk shape.
fn leaked_settings_json(run_id: &str) -> String {
    serde_json::json!({
        "permissions": { "defaultMode": "auto", "deny": ["Bash(bossctl)"] },
        "hooks": {
            "SessionStart": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": format!("BOSS_LEASE_ID='l' BOSS_RUN_ID='{run_id}' /Applications/Boss.app/Contents/Resources/bin/boss-event"),
                }],
            }],
            "Stop": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": format!("BOSS_LEASE_ID='l' BOSS_RUN_ID='{run_id}' /Applications/Boss.app/Contents/Resources/bin/boss-event"),
                }],
            }],
        },
    })
    .to_string()
}

#[test]
fn purge_leaked_worker_hooks_strips_stale_boss_hooks_but_keeps_other_content() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    std::fs::write(&settings, leaked_settings_json("exec_stale_99")).unwrap();

    purge_leaked_worker_hooks(dir.path());

    // The leaked Stop hook (and every other boss hook) is gone, so
    // a worker session can no longer fire a second Stop with the
    // stale run id. Non-hook content (the repo-style deny rules)
    // survives.
    let after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert!(
        !std::fs::read_to_string(&settings).unwrap().contains("BOSS_RUN_ID"),
        "no leaked BOSS_RUN_ID hook may remain",
    );
    assert!(
        after.get("hooks").is_none(),
        "all hooks were boss hooks, so the now-empty hooks key is dropped",
    );
    assert_eq!(
        after["permissions"]["deny"][0], "Bash(bossctl)",
        "non-hook content must be preserved",
    );
}

#[test]
fn purge_leaked_worker_hooks_removes_pure_engine_file() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // A settings file that is *only* leaked boss hooks (no other
    // keys) is removed entirely, restoring the no-settings-in-tree
    // invariant.
    let local = claude_dir.join("settings.local.json");
    let only_hooks = serde_json::json!({
        "hooks": {
            "Stop": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "BOSS_RUN_ID='exec_old' /bin/boss-event",
                }],
            }],
        },
    });
    std::fs::write(&local, only_hooks.to_string()).unwrap();

    purge_leaked_worker_hooks(dir.path());

    assert!(
        !local.exists(),
        "a settings file with only leaked boss hooks must be removed",
    );
}

#[test]
fn purge_leaked_worker_hooks_leaves_clean_repo_settings_untouched() {
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // A legitimately repo-tracked settings.json (no boss hooks) must
    // survive byte-for-byte: the cheap signature pre-check means we
    // never even parse it.
    let settings = claude_dir.join("settings.json");
    let clean = "{\n  \"hooks\": {\n    \"Stop\": [ { \"matcher\": \"*\", \"hooks\": [ { \"type\": \"command\", \"command\": \"echo hi\" } ] } ]\n  }\n}\n";
    std::fs::write(&settings, clean).unwrap();

    purge_leaked_worker_hooks(dir.path());

    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        clean,
        "a clean repo settings.json with no BOSS_RUN_ID hook must be untouched byte-for-byte",
    );
}

#[test]
fn purge_leaked_worker_hooks_is_noop_when_absent() {
    let dir = TempDir::new().unwrap();
    // No .claude/ dir at all — must not panic or create anything.
    purge_leaked_worker_hooks(dir.path());
    assert!(!dir.path().join(".claude").join("settings.json").exists());
}

#[test]
fn write_workspace_files_purges_leaked_in_tree_settings() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    // Simulate a warm-cached workspace carrying a stale settings.json
    // from a prior execution.
    std::fs::write(claude_dir.join("settings.json"), leaked_settings_json("exec_prev_run")).unwrap();

    let input = WorkerSetupInput {
        run_id: "exec_current".into(),
        lease_id: "test-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input).unwrap();

    let settings = claude_dir.join("settings.json");
    // The leaked prior run's hook must be gone after setup; only
    // the engine's out-of-tree `--settings` file carries hooks now.
    if settings.exists() {
        assert!(
            !std::fs::read_to_string(&settings).unwrap().contains("BOSS_RUN_ID"),
            "write_workspace_files must purge the stale in-tree boss hook",
        );
    }
}

#[test]
fn claude_md_warns_against_force_tracking_dot_claude() {
    let input = sample_input();
    let rendered = render_claude_md(&input);
    // The CLAUDE.md must remind workers not to override the
    // engine's gitignore — otherwise a worker that runs into a
    // status surprise might `jj file track` the engine plumbing
    // back into its PR, undoing the fix.
    assert!(rendered.contains(".claude/"));
    assert!(rendered.contains("force") || rendered.contains("track"));
}

#[test]
fn claude_md_pr_section_is_front_and_centre() {
    // The PR rule moved out from after Boundaries and now sits
    // immediately after the intro. If a future edit buries it
    // again, this test will fail and the writer can move it back.
    let input = sample_input();
    let rendered = render_claude_md(&input);
    let pr_offset = rendered
        .find("Pull requests are the deliverable")
        .expect("expected the strengthened PR heading to be present");
    let workspace_offset = rendered
        .find("## Your workspace")
        .expect("expected the workspace heading to be present");
    assert!(
        pr_offset < workspace_offset,
        "PR section must come before `## Your workspace`",
    );
    // Resuming-work guidance must mention how to detect an
    // existing PR rather than just letting the worker open a duplicate.
    assert!(rendered.contains("gh pr list --head"));
    assert!(rendered.contains("not complete until a PR exists"));
    assert!(rendered.contains("PR URL on its own line"));
    // Empty-diff guard: the worker must verify the diff is non-empty
    // before pushing so the engine's empty-diff probe is never needed.
    assert!(
        rendered.contains("jj diff -r @"),
        "CLAUDE.md must remind workers to verify the diff before pushing",
    );
}

#[test]
fn write_workspace_files_overwrites_existing_files() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let claude_dir = dir.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("CLAUDE.md"), "stale content").unwrap();

    let input = WorkerSetupInput {
        run_id: "run-overwrite".into(),
        lease_id: "new-lease".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };

    write_workspace_files(&input).unwrap();
    let contents = std::fs::read_to_string(claude_dir.join("CLAUDE.md")).unwrap();
    assert!(contents.contains("new-lease"));
    assert!(!contents.contains("stale content"));
}

#[test]
fn claude_dir_for_appends_dot_claude() {
    let dir = claude_dir_for(Path::new("/some/workspace"));
    assert_eq!(dir, PathBuf::from("/some/workspace/.claude"));
}

#[test]
fn claude_md_has_cube_pr_ensure_section() {
    let input = sample_input();
    let rendered = render_claude_md(&input);
    assert!(
        rendered.contains("Creating a PR from a jj workspace"),
        "expected a 'Creating a PR from a jj workspace' section",
    );
    assert!(
        rendered.contains("cube pr ensure"),
        "expected cube pr ensure to be the canonical PR creation command",
    );
    assert!(rendered.contains("--branch"), "expected --branch flag guidance",);
    assert!(
        rendered.contains("jj bookmark create"),
        "expected canonical bookmark creation command",
    );
}

#[test]
fn claude_md_explains_no_git_at_workspace_root() {
    // Workers must know why bare `gh` calls fail before reaching for the fix.
    let input = sample_input();
    let rendered = render_claude_md(&input);
    assert!(
        rendered.contains("fatal: not a git repository") || rendered.contains("no `.git/`"),
        "expected an explanation of why bare gh fails in a jj workspace",
    );
}

#[test]
fn claude_md_draft_directive_present_when_enabled() {
    let mut input = sample_input();
    input.draft_pr_mode = true;
    let rendered = render_claude_md(&input);
    assert!(
        rendered.contains("--draft"),
        "CLAUDE.md must include --draft directive when draft_pr_mode is true",
    );
    assert!(
        rendered.contains("cube pr ensure"),
        "draft directive must reference cube pr ensure",
    );
}

#[test]
fn claude_md_draft_directive_absent_when_disabled() {
    let input = sample_input(); // draft_pr_mode: false
    let rendered = render_claude_md(&input);
    assert!(
        !rendered.contains("--draft"),
        "CLAUDE.md must NOT include --draft directive when draft_pr_mode is false",
    );
}

#[test]
fn reviewer_claude_md_states_read_only_mandate() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    let rendered = render_claude_md(&input);
    // Must contain the read-only mandate section.
    assert!(
        rendered.contains("Read-only mandate"),
        "reviewer CLAUDE.md must contain read-only mandate section",
    );
    // Must contain both lease id and workspace path — reviewers need both to
    // navigate the workspace that the engine checked out to the PR head.
    assert!(rendered.contains(&input.lease_id));
    assert!(
        rendered.contains(input.workspace_path.to_str().unwrap()),
        "reviewer CLAUDE.md must include workspace path (workspace is checked out to PR head)",
    );
    // Must mention that the workspace is at the PR head.
    assert!(
        rendered.contains("checked out to the PR head"),
        "reviewer CLAUDE.md must mention that the workspace is at the PR head",
    );
    // Must NOT contain the standard PR-required delivery mandate from
    // the implementation worker CLAUDE.md.
    assert!(
        !rendered.contains("Pull requests are the deliverable"),
        "reviewer CLAUDE.md must not include the standard PR-required reminder",
    );
    // Must not instruct the reviewer to create a PR — the tool is listed
    // only as a *forbidden* action, not as a delivery requirement.
    assert!(
        !rendered.contains("A task is not complete until a PR exists"),
        "reviewer CLAUDE.md must not include the implementation PR mandate",
    );
}

#[test]
fn reviewer_claude_md_mentions_allowed_read_only_tools() {
    let mut input = sample_input();
    input.worker_kind = WorkerKind::Reviewer;
    let rendered = render_claude_md(&input);
    assert!(rendered.contains("gh pr diff"), "must mention gh pr diff");
    assert!(rendered.contains("gh pr view"), "must mention gh pr view");
    assert!(rendered.contains("jj log"), "must mention jj log");
}

#[test]
fn standard_claude_md_is_unchanged_by_reviewer_branch() {
    // Verify the reviewer kind change does not affect standard workers.
    let input = sample_input(); // WorkerKind::Standard
    let rendered = render_claude_md(&input);
    assert!(rendered.contains("Pull requests are the deliverable"));
    assert!(rendered.contains("cube pr ensure"));
    assert!(rendered.contains("LOCAL MIRROR"));
}

#[test]
fn heal_hook_command_replaces_shim_path() {
    let old_cmd = "BOSS_EVENTS_SOCKET='/tmp/events.sock' BOSS_LEASE_ID='lease-1' \
                   BOSS_RUN_ID='run-1' BOSS_WORKSPACE='/tmp/ws' \
                   '/old/bazel-bin/tools/boss/event-shim/boss-event'";
    let new_path = PathBuf::from("/stable/bin/boss-event");
    let healed = heal_hook_command(old_cmd, &new_path);
    assert!(
        healed.contains("'/stable/bin/boss-event'"),
        "should contain new path: {healed}",
    );
    assert!(
        !healed.contains("/old/bazel-bin"),
        "should not contain old path: {healed}",
    );
    // Env vars and other args must be preserved unchanged.
    assert!(healed.contains("BOSS_EVENTS_SOCKET="));
    assert!(healed.contains("BOSS_WORKSPACE="));
}

#[test]
fn heal_hook_command_handles_path_with_spaces() {
    let old_cmd = "BOSS_EVENTS_SOCKET='/tmp/e.sock' BOSS_LEASE_ID='l' \
                   BOSS_RUN_ID='r' BOSS_WORKSPACE='/tmp/ws' \
                   '/Users/x/Library/Application Support/Boss/bin/boss-event'";
    let new_path = PathBuf::from("/Users/y/Library/Application Support/Boss/bin/boss-event");
    let healed = heal_hook_command(old_cmd, &new_path);
    assert!(
        healed.contains("'/Users/y/Library/Application Support/Boss/bin/boss-event'"),
        "spaces in new path must be inside single quotes: {healed}",
    );
}

#[test]
fn heal_hook_command_no_op_when_no_boss_event_present() {
    let cmd = "SOME_VAR='val' /unrelated/binary";
    let new_path = PathBuf::from("/stable/boss-event");
    let healed = heal_hook_command(cmd, &new_path);
    assert_eq!(healed, cmd, "should return original when boss-event not found");
}

#[test]
fn heal_worker_settings_json_updates_all_hook_events() {
    // Stage a worker settings file (with a stale bazel-bin
    // boss-event path) in a settings dir, then heal the whole dir.
    let settings_dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-heal".into(),
        lease_id: "lease-heal".into(),
        workspace_path: PathBuf::from("/some/workspace/mono-agent-heal"),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/old/bazel-bin/tools/boss/event-shim/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    let settings_file = settings_dir.path().join("mono-agent-heal.json");
    std::fs::write(&settings_file, render_settings_json(&input)).unwrap();

    let new_path = PathBuf::from("/stable/bin/boss-event");
    heal_worker_settings_json(settings_dir.path(), &new_path);

    let settings = std::fs::read_to_string(&settings_file).unwrap();
    // All seven hook events must now reference the stable path.
    for hook in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "Notification",
        "SessionEnd",
    ] {
        assert!(
            settings.contains("/stable/bin/boss-event"),
            "{hook} hook still references stale path after heal: {settings}",
        );
    }
    assert!(
        !settings.contains("/old/bazel-bin"),
        "healed settings file must not contain the old bazel-bin path: {settings}",
    );
    // The settings file must still be valid JSON.
    let _: serde_json::Value = serde_json::from_str(&settings).unwrap();
}

#[test]
fn heal_worker_settings_json_skips_missing_settings_dir() {
    let dir = TempDir::new().unwrap();
    let new_path = PathBuf::from("/stable/boss-event");
    // Missing directory must be a no-op, not a panic.
    heal_worker_settings_json(&dir.path().join("does-not-exist"), &new_path);
    // An existing-but-empty dir is also a no-op.
    heal_worker_settings_json(dir.path(), &new_path);
}

#[test]
fn revision_implementation_adds_gh_pr_create_guard_to_pre_tool_use() {
    let mut input = sample_input();
    input.execution_kind = "revision_implementation".into();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");
    // Must have 5 entries: the shim, the deterministic path guard, the
    // always-on boss-launch guard, the checkleft push guard (standard
    // worker), and the revision-only gh-pr-create guard.
    assert_eq!(
        pre.len(),
        5,
        "revision_implementation PreToolUse must have shim + path guard + boss-launch guard + checkleft push guard + pr guard, got {pre:?}",
    );
    // The revision pr-guard is the Bash-matcher entry whose command
    // inspects `gh pr create`; the boss-launch guard is also Bash-matched,
    // so disambiguate by content.
    let pr_guard = pre
        .iter()
        .find(|e| e["hooks"][0]["command"].as_str().unwrap_or("").contains("create"))
        .expect("revision PreToolUse must include a gh-pr-create guard");
    // Guard command must reference the deny decision and both gh pr create and cube pr ensure.
    let guard_cmd = pr_guard["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        guard_cmd.contains("gh") && guard_cmd.contains("pr") && guard_cmd.contains("create"),
        "guard command must inspect gh pr create: {guard_cmd}",
    );
    assert!(
        guard_cmd.contains("cube") && guard_cmd.contains("ensure"),
        "guard command must also block cube pr ensure: {guard_cmd}",
    );
    assert!(
        guard_cmd.contains("block"),
        "guard command must produce a block decision: {guard_cmd}",
    );
}

#[test]
fn chore_implementation_has_shim_and_path_guard_but_no_revision_guard() {
    let input = sample_input(); // execution_kind: "chore_implementation"
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");
    // chore: [boss-event shim, deterministic path guard, boss-launch
    // guard, checkleft push guard]. The revision-only `gh pr create`
    // guard must NOT be present.
    assert_eq!(
        pre.len(),
        4,
        "chore_implementation PreToolUse must have shim + path guard + boss-launch guard + checkleft push guard, got {pre:?}",
    );
    assert_eq!(
        pre[0]["matcher"],
        serde_json::Value::String("*".into()),
        "first PreToolUse hook must be the catch-all shim",
    );
    let path_guard = pre[1]["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        path_guard.contains("BOSS_DATA_DIR=") && path_guard.contains(PATH_GUARD_SCRIPT_NAME),
        "second PreToolUse hook must be the path guard, got {path_guard}",
    );
    // No revision guard: nothing inspects `cube ... ensure`.
    for entry in pre {
        let cmd = entry["hooks"][0]["command"].as_str().unwrap_or("");
        assert!(
            !cmd.contains("ensure"),
            "chore must not carry the revision gh-pr-create guard: {cmd}",
        );
    }
}

/// Every worker session — regardless of kind — must carry a PreToolUse
/// guard that blocks *launching Boss itself*: the macOS app, its bundled
/// engine, or an app-macos test that can spawn the real app. `bazel build`
/// must stay allowed. The guard is a Bash-matcher inline-Python decision
/// hook (distinct from the revision gh-pr-create Bash guard).
#[test]
fn every_worker_blocks_launching_boss_in_pre_tool_use() {
    for kind in ["chore_implementation", "revision_implementation"] {
        let mut input = sample_input();
        input.execution_kind = kind.into();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse must be an array");
        // Disambiguate from the gh-pr-create guard by content.
        let guard = pre
            .iter()
            .find(|e| {
                // The guard's regex escapes the dot (`Boss\.app`), so match
                // on backslash-free substrings of the command.
                let c = e["hooks"][0]["command"].as_str().unwrap_or("");
                c.contains("Contents/MacOS/Boss") && c.contains("app-macos")
            })
            .unwrap_or_else(|| panic!("{kind} PreToolUse must include a boss-launch guard"));
        assert_eq!(
            guard["matcher"], "Bash",
            "{kind} boss-launch guard must match the Bash tool",
        );
        let cmd = guard["hooks"][0]["command"].as_str().unwrap_or("");
        // Covers app launch (open / bundle binary), engine binary,
        // bazel run of app-macos/engine, and swift run — and blocks.
        // Must NOT reference `swift test` or `bazel test` (those run the
        // app-macos unit suite, which has no test_host and is allowed).
        assert!(
            cmd.contains("Contents/MacOS/Boss")
                && cmd.contains("Resources/bin/engine")
                && cmd.contains("app-macos")
                && cmd.contains(r"swift\s+run")
                && cmd.contains("block"),
            "{kind} boss-launch guard must block app/engine/run launches: {cmd}",
        );
        assert!(
            !cmd.contains(r"swift\s+(run|test)") && !cmd.contains("test\\b[^\\n]*tools/boss/app-macos"),
            "{kind} boss-launch guard must NOT block app-macos unit tests: {cmd}",
        );
    }
}

/// Defense-in-depth: even if `execution_kind` is wrong (e.g. a revision
/// re-dispatched as `task_implementation` due to a bug), the guard fires
/// as long as `task_kind == "revision"`.  This ensures the structural
/// invariant holds regardless of execution-kind derivation errors.
#[test]
fn revision_task_kind_adds_gh_pr_create_guard_even_with_wrong_execution_kind() {
    let mut input = sample_input();
    // Simulate the bug scenario: execution_kind was mis-derived as
    // task_implementation but the task itself is a revision.
    input.execution_kind = "task_implementation".into();
    input.task_kind = Some("revision".into());

    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let pre = parsed["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse must be an array");

    assert_eq!(
        pre.len(),
        5,
        "revision task_kind must add the pr guard (shim + path guard + boss-launch guard + checkleft push guard + pr guard) even when execution_kind is wrong, got {pre:?}",
    );
    let pr_guard = pre
        .iter()
        .find(|e| e["hooks"][0]["command"].as_str().unwrap_or("").contains("create"))
        .expect("revision task_kind must include a gh-pr-create guard");
    let guard_cmd = pr_guard["hooks"][0]["command"].as_str().unwrap_or("");
    assert!(
        guard_cmd.contains("block"),
        "guard must produce a block decision: {guard_cmd}",
    );
}

/// Locate the deterministic path-guard PreToolUse hook command (the
/// one that invokes the gate script), if present.
fn path_guard_command(parsed: &serde_json::Value) -> Option<String> {
    parsed["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .filter_map(|e| e["hooks"][0]["command"].as_str())
        .find(|c| c.contains(PATH_GUARD_SCRIPT_NAME))
        .map(str::to_owned)
}

#[test]
fn settings_json_adds_deterministic_path_guard_hook() {
    // Every session must carry the deterministic Boss-data-dir gate
    // as a PreToolUse hook. The hook invokes the gate script with the
    // Boss data dir passed via BOSS_DATA_DIR so the script resolves
    // candidate paths against the right boundary.
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let cmd = path_guard_command(&parsed).expect("PreToolUse must include the deterministic path-guard hook");
    assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
    // The data dir is the Boss state dir (events socket parent),
    // single-quoted because of the space in "Application Support".
    assert!(
        cmd.contains("BOSS_DATA_DIR='/Users/brianduff/Library/Application Support/Boss'"),
        "guard must pass the Boss data dir via BOSS_DATA_DIR: {cmd}",
    );
    // The script path lives outside any workspace, in the shared
    // worker-settings dir.
    let script = path_guard_script_path();
    assert!(
        cmd.contains(&shell_escape(&script.display().to_string())),
        "guard must invoke the absolute gate-script path: {cmd}",
    );
}

#[test]
fn path_guard_present_for_revision_sessions_too() {
    // The gate is session-kind-agnostic: revision sessions get it
    // alongside their gh-pr-create guard.
    let mut input = sample_input();
    input.execution_kind = "revision_implementation".into();
    input.task_kind = Some("revision".into());
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    assert!(
        path_guard_command(&parsed).is_some(),
        "revision sessions must also carry the deterministic path guard",
    );
}

#[test]
fn path_guard_script_has_the_load_bearing_logic() {
    // Guard against an accidental edit that guts the script. The
    // deterministic gate hinges on: reading BOSS_DATA_DIR, resolving
    // symlinks/.. via realpath, a component-wise prefix test, emitting
    // a block decision, and pointing at the sanctioned recovery path.
    let s = PATH_GUARD_SCRIPT;
    assert!(s.contains("BOSS_DATA_DIR"), "must read the data dir from env");
    assert!(s.contains("realpath"), "must canonicalise paths via realpath");
    assert!(
        s.contains("expanduser") && s.contains("expandvars"),
        "must expand ~ and $VAR indirection"
    );
    assert!(
        s.contains("\"decision\"") && s.contains("\"block\""),
        "must be able to emit a block decision"
    );
    assert!(
        s.contains("boss task restore") || s.contains("boss shake"),
        "block message must point at the sanctioned recovery surface"
    );
}

#[test]
fn write_workspace_files_writes_path_guard_script_outside_workspace() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-guard".into(),
        lease_id: "lease-guard".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    write_workspace_files(&input).unwrap();

    let script = path_guard_script_path();
    assert!(script.exists(), "gate script must be written: {}", script.display());
    // Must live outside the workspace tree (same rule as the
    // settings file — never shipped into a worker PR).
    assert!(
        !script.starts_with(dir.path()),
        "gate script must live outside the workspace: {}",
        script.display(),
    );
    let body = std::fs::read_to_string(&script).unwrap();
    assert_eq!(body, PATH_GUARD_SCRIPT, "written script must match the source");
    // And the engine must never drop the gate script into the
    // workspace's .claude/ where VCS could pick it up.
    assert!(
        !dir.path().join(".claude").join(PATH_GUARD_SCRIPT_NAME).exists(),
        "gate script must not be written into the workspace .claude/ dir",
    );
}

#[test]
fn heal_worker_settings_json_refreshes_path_guard_script() {
    // On engine restart the heal sweep must (re)materialise the gate
    // script so a live worker whose settings reference it still has a
    // working PreToolUse gate even after TMPDIR churn.
    let settings_dir = TempDir::new().unwrap();
    // A settings file must exist for the dir to be considered live.
    std::fs::write(settings_dir.path().join("ws.json"), "{}").unwrap();

    heal_worker_settings_json(settings_dir.path(), &PathBuf::from("/stable/boss-event"));

    let script = settings_dir.path().join(PATH_GUARD_SCRIPT_NAME);
    assert!(script.exists(), "heal must refresh the gate script");
    assert_eq!(std::fs::read_to_string(&script).unwrap(), PATH_GUARD_SCRIPT);
}

// ── checkleft pre-push guard ──────────────────────────────────────────

/// Locate the checkleft pre-push guard PreToolUse hook command (the
/// entry that invokes the push-guard script by its filename).
fn checkleft_push_guard_command(parsed: &serde_json::Value) -> Option<String> {
    parsed["hooks"]["PreToolUse"]
        .as_array()?
        .iter()
        .filter_map(|e| e["hooks"][0]["command"].as_str())
        .find(|c| c.contains(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME))
        .map(str::to_owned)
}

#[test]
fn standard_worker_gets_checkleft_push_guard() {
    // A standard (implementation) worker must carry the deterministic
    // pre-push checkleft gate as a Bash-matched PreToolUse hook.
    let input = sample_input(); // Standard chore worker
    let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
    let cmd = checkleft_push_guard_command(&parsed)
        .expect("standard worker PreToolUse must include the checkleft push guard");
    assert!(cmd.contains("python3"), "guard must run via python3: {cmd}");
    let script = checkleft_push_guard_script_path();
    assert!(
        cmd.contains(&shell_escape(&script.display().to_string())),
        "guard must invoke the absolute push-guard script path: {cmd}",
    );
    // The guard is Bash-matched (it inspects the command string).
    let entry = parsed["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .unwrap_or("")
                .contains(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME)
        })
        .unwrap();
    assert_eq!(entry["matcher"], "Bash", "push guard must match the Bash tool");
}

#[test]
fn reviewer_and_triage_workers_omit_checkleft_push_guard() {
    // Reviewer / triage workers cannot push (their deny rules block it),
    // so the push guard would never fire — it must be omitted.
    for kind in [WorkerKind::Reviewer, WorkerKind::Triage] {
        let mut input = sample_input();
        input.worker_kind = kind.clone();
        let parsed: serde_json::Value = serde_json::from_str(&render_settings_json(&input)).unwrap();
        assert!(
            checkleft_push_guard_command(&parsed).is_none(),
            "{kind:?} worker must not carry the checkleft push guard",
        );
    }
}

#[test]
fn remote_workers_omit_checkleft_push_guard() {
    // Remote SSH workers skip the push guard: the gate script is never
    // shipped to the remote host (same reason the path guard is dropped).
    let input = sample_input();
    let parsed: serde_json::Value = serde_json::from_str(&render_remote_settings_json(&input)).unwrap();
    assert!(
        checkleft_push_guard_command(&parsed).is_none(),
        "remote workers must not carry the checkleft push guard",
    );
}

#[test]
fn checkleft_push_guard_script_has_the_load_bearing_logic() {
    // Guard against an accidental edit that guts the script. The gate
    // hinges on: detecting a push command, resolving checkleft (env
    // override → bin/checkleft → PATH), running `checkleft run`, gating
    // on the exit code, and surfacing the BYPASS_ guidance on a block.
    let s = CHECKLEFT_PUSH_GUARD_SCRIPT;
    assert!(s.contains("is_push_command"), "must detect push commands");
    assert!(s.contains("BOSS_CHECKLEFT_BIN"), "must honour the binary override");
    assert!(
        s.contains("bin") && s.contains("checkleft"),
        "must resolve the repobin-installed checkleft path",
    );
    assert!(s.contains("returncode"), "must gate on checkleft's exit code");
    assert!(
        s.contains("\"decision\"") && s.contains("\"block\"") || s.contains("'block'"),
        "must be able to emit a block decision",
    );
    assert!(s.contains("BYPASS_"), "block message must surface the bypass guidance");
}

#[test]
fn write_workspace_files_writes_checkleft_push_guard_script_outside_workspace() {
    let _shared = lock_shared_settings_dir();
    let _home = HomeGuard::new();
    let dir = TempDir::new().unwrap();
    let input = WorkerSetupInput {
        run_id: "run-clguard".into(),
        lease_id: "lease-clguard".into(),
        workspace_path: dir.path().to_path_buf(),
        events_socket_path: PathBuf::from("/tmp/events.sock"),
        boss_event_path: PathBuf::from("/tmp/boss-event"),
        draft_pr_mode: false,
        execution_kind: "chore_implementation".into(),
        task_kind: Some("chore".into()),
        worker_kind: WorkerKind::Standard,
    };
    write_workspace_files(&input).unwrap();

    let script = checkleft_push_guard_script_path();
    assert!(
        script.exists(),
        "push-guard script must be written: {}",
        script.display()
    );
    assert!(
        !script.starts_with(dir.path()),
        "push-guard script must live outside the workspace: {}",
        script.display(),
    );
    let body = std::fs::read_to_string(&script).unwrap();
    assert_eq!(
        body, CHECKLEFT_PUSH_GUARD_SCRIPT,
        "written script must match the source"
    );
    assert!(
        !dir.path()
            .join(".claude")
            .join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME)
            .exists(),
        "push-guard script must not be written into the workspace .claude/ dir",
    );
}

#[test]
fn heal_worker_settings_json_refreshes_checkleft_push_guard_script() {
    let settings_dir = TempDir::new().unwrap();
    std::fs::write(settings_dir.path().join("ws.json"), "{}").unwrap();

    heal_worker_settings_json(settings_dir.path(), &PathBuf::from("/stable/boss-event"));

    let script = settings_dir.path().join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME);
    assert!(script.exists(), "heal must refresh the push-guard script");
    assert_eq!(std::fs::read_to_string(&script).unwrap(), CHECKLEFT_PUSH_GUARD_SCRIPT);
}

// ── checkleft pre-push guard execution tests ──────────────────────────
//
// These run the actual guard script (via python3) against a simulated
// Bash tool_input payload, using a fake checkleft binary so the gate's
// block/approve behaviour is verified end-to-end and deterministically.

/// Write an executable fake `checkleft` that prints `stdout` and exits
/// with `exit_code`. Returns its path.
fn write_fake_checkleft(dir: &Path, name: &str, exit_code: i32, stdout: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let script = format!("#!/bin/sh\ncat <<'CHECKLEFT_EOF'\n{stdout}\nCHECKLEFT_EOF\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Run the checkleft push guard against a simulated Bash command and
/// return the decision JSON. `checkleft_bin` is passed via
/// `BOSS_CHECKLEFT_BIN` (a nonexistent path simulates "no checkleft").
fn run_push_guard(command: &str, cwd: &Path, checkleft_bin: &Path) -> serde_json::Value {
    use std::io::Write as _;
    let script_dir = TempDir::new().unwrap();
    let script_path = script_dir.path().join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME);
    std::fs::write(&script_path, CHECKLEFT_PUSH_GUARD_SCRIPT).unwrap();

    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd.display().to_string(),
    })
    .to_string();

    let mut child = std::process::Command::new("python3")
        .arg(&script_path)
        .env("BOSS_CHECKLEFT_BIN", checkleft_bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must be available");
    child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "push guard produced invalid JSON for {command:?}: {e}\nstdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[test]
fn push_guard_blocks_jj_git_push_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error[rustfmt]: needs formatting");
    let decision = run_push_guard("jj git push -b boss/foo --allow-new", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "block",
        "a failing checkleft must block the push: {decision}"
    );
    let reason = decision["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("error[rustfmt]"),
        "block reason must echo the findings: {reason}"
    );
    assert!(
        reason.contains("BYPASS_"),
        "block reason must include bypass guidance: {reason}"
    );
}

#[test]
fn push_guard_blocks_git_push_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error[clippy]: bad");
    let decision = run_push_guard("git push --force-with-lease github my-branch", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "block",
        "git push with a failing checkleft must block: {decision}"
    );
}

#[test]
fn push_guard_allows_push_when_checkleft_passes() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 0, "checks: no findings");
    let decision = run_push_guard("jj git push -b boss/foo", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "a clean checkleft must allow the push: {decision}"
    );
}

#[test]
fn push_guard_approves_non_push_command_without_running_checkleft() {
    let dir = TempDir::new().unwrap();
    // checkleft would fail if invoked — but a non-push command must never
    // invoke it, so the decision is approve.
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error: would block");
    let decision = run_push_guard("jj describe -m 'wip'", dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "non-push command must approve: {decision}"
    );
}

#[test]
fn push_guard_approves_describe_with_push_phrase_in_message() {
    let dir = TempDir::new().unwrap();
    let checkleft = write_fake_checkleft(dir.path(), "checkleft", 1, "error: would block");
    // "git push" is inside the quoted commit message — shlex keeps it as a
    // single token, so it must NOT be treated as a push.
    let decision = run_push_guard(r#"jj describe -m "git push the fix to prod""#, dir.path(), &checkleft);
    assert_eq!(
        decision["decision"], "approve",
        "push phrase in a commit message must not block: {decision}"
    );
}

#[test]
fn push_guard_approves_when_no_checkleft_binary() {
    let dir = TempDir::new().unwrap();
    // A nonexistent override path → resolve returns None → fail open.
    let missing = dir.path().join("does-not-exist-checkleft");
    let decision = run_push_guard("jj git push -b boss/foo --allow-new", dir.path(), &missing);
    assert_eq!(
        decision["decision"], "approve",
        "a repo without a checkleft binary must allow the push (fail open): {decision}",
    );
}

// ── REVISION_PR_GUARD_COMMAND execution tests ─────────────────────────
//
// These tests actually run the guard script (via `sh -c`) to verify
// its behaviour end-to-end, including the shlex tokenisation fix.

/// Run the revision PR guard against a simulated Bash tool_input payload
/// and return the `decision` field from its JSON output.
fn run_revision_guard(bash_command: &str) -> String {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": bash_command}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(REVISION_PR_GUARD_COMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("sh must be available");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!(
            "guard produced invalid JSON for command {:?}: {e}\nstdout={stdout}\nstderr={}",
            bash_command,
            String::from_utf8_lossy(&out.stderr),
        )
    });
    parsed["decision"].as_str().unwrap_or("missing").to_owned()
}

// --- false-positive regression tests (must APPROVE) ---

#[test]
fn guard_approves_jj_describe_with_cube_pr_ensure_in_message() {
    // The bug: `jj describe -m "...cube pr ensure..."` was blocked
    // because the phrase appeared inside the quoted commit message.
    let decision =
        run_revision_guard(r#"jj describe -m "fix(boss-engine): extend editorial hook to intercept cube pr ensure""#);
    assert_eq!(
        decision, "approve",
        "guard must NOT block jj describe when the phrase is in the commit message",
    );
}

#[test]
fn guard_approves_git_commit_with_gh_pr_create_in_message() {
    let decision = run_revision_guard(r#"git commit -m "docs: explain how gh pr create interacts with the hook""#);
    assert_eq!(
        decision, "approve",
        "guard must NOT block git commit when the phrase is in the commit message",
    );
}

#[test]
fn guard_approves_echo_with_pr_creation_phrase() {
    let decision = run_revision_guard(r#"echo "cube pr ensure is documented here""#);
    assert_eq!(decision, "approve", "echo must not be blocked");
}

#[test]
fn guard_approves_jj_describe_with_gh_pr_create_in_message() {
    let decision = run_revision_guard(r#"jj describe -m "fix: the gh pr create story in this branch""#);
    assert_eq!(decision, "approve", "jj describe must not be blocked");
}

// --- true-positive tests (must BLOCK) ---

#[test]
fn guard_blocks_gh_pr_create() {
    let decision = run_revision_guard("gh pr create --title 'My PR' --body 'content'");
    assert_eq!(decision, "block", "guard must block a bare gh pr create",);
}

#[test]
fn guard_blocks_cube_pr_ensure() {
    let decision = run_revision_guard("cube pr ensure --branch feat/foo --title 'My PR'");
    assert_eq!(decision, "block", "guard must block a bare cube pr ensure",);
}

#[test]
fn guard_blocks_gh_pr_create_with_git_dir_prefix() {
    let decision = run_revision_guard("GIT_DIR=.jj/repo/store/git gh pr create --title 'x' --body 'y'");
    assert_eq!(
        decision, "block",
        "guard must block gh pr create even with a GIT_DIR= prefix",
    );
}

#[test]
fn guard_blocks_cube_pr_ensure_in_compound_command() {
    // A compound command: benign `jj describe` first, then a real
    // `cube pr ensure` — the guard must catch the second part.
    let decision = run_revision_guard(r#"jj describe -m "push changes" && cube pr ensure --branch feat/x"#);
    assert_eq!(
        decision, "block",
        "guard must block cube pr ensure in a compound command",
    );
}

#[test]
fn guard_block_message_names_matched_command() {
    use std::io::Write as _;
    let stdin_payload = serde_json::json!({
        "tool_input": {"command": "cube pr ensure --branch b"}
    })
    .to_string();

    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(REVISION_PR_GUARD_COMMAND)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    let reason = parsed["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("cube pr ensure"),
        "block reason must name the matched command, got: {reason}",
    );
}
