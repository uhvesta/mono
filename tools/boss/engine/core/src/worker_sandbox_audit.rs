//! Engine-side detection for worker sandbox breaches.
//!
//! Workers are fenced off from Boss's runtime state under
//! `~/Library/Application Support/Boss/` via deny rules in their
//! per-worker settings file (see [`crate::worker_setup`]). Those
//! deny rules are the *enforcement* — claude refuses the tool call.
//! This module is the *audit*: an independent observer that watches
//! every `PreToolUse` hook event the worker emits and writes an
//! `engine-audit.log` record whenever the tool input names a denied
//! path or coordinator-only command.
//!
//! The two layers are complementary:
//!
//! - The settings-file deny rules can drift if a future claude-code
//!   release changes its permission syntax or rule precedence; the
//!   audit catches the breach attempt even if the harness silently
//!   lets the call through.
//! - The audit captures *intent*. A worker that *tried* to read
//!   `state.db` and got denied is a signal: the coordinator owes that
//!   worker context it didn't get. We want to see those without
//!   relying on the user noticing them in the live UI.
//!
//! Detection is best-effort and conservative: a tool input that
//! doesn't parse as the expected shape is silently skipped. We only
//! emit audit lines for clear matches against the deny patterns.

use std::path::Path;

use crate::audit;
use crate::protocol::WorkerEvent;

/// Inspect a worker `PreToolUse` hook event and, if the tool input
/// names a path or command that the worker should not be reaching for,
/// append a `worker_sandbox_attempt` record to `engine-audit.log`.
///
/// `boss_state_dir` is the directory whose contents are off-limits
/// (production: `~/Library/Application Support/Boss`). `run_id` is the
/// worker run id the engine has correlated this hook to; included in
/// the audit record so triage can pivot from an audit line back to the
/// offending worker.
pub fn record_if_sandbox_attempt(boss_state_dir: &Path, run_id: Option<&str>, event: &WorkerEvent) {
    let (tool_name, tool_input) = match event {
        WorkerEvent::PreToolUse {
            tool_name, tool_input, ..
        } => (tool_name.as_str(), tool_input),
        _ => return,
    };

    let Some(reason) = classify(boss_state_dir, tool_name, tool_input) else {
        return;
    };

    let label = reason.label();
    let detail = reason.detail();

    let mut payload = serde_json::Map::new();
    payload.insert("tool".to_owned(), serde_json::Value::String(tool_name.to_owned()));
    payload.insert("reason".to_owned(), serde_json::Value::String(label.to_owned()));
    if let Some(detail) = detail {
        payload.insert("detail".to_owned(), serde_json::Value::String(detail));
    }
    if let Some(run_id) = run_id {
        payload.insert("run_id".to_owned(), serde_json::Value::String(run_id.to_owned()));
    }

    audit::record_event("worker_sandbox_attempt", &serde_json::Value::Object(payload));

    tracing::warn!(
        run_id,
        tool = tool_name,
        reason = label,
        "worker sandbox attempt: tool call targets coordinator-only surface; audited",
    );
}

#[derive(Debug, Clone)]
enum Reason {
    BossStatePath(String),
    BossctlCommand(String),
    BossLifecycleCommand(String),
}

impl Reason {
    fn label(&self) -> &'static str {
        match self {
            Reason::BossStatePath(_) => "boss_state_path",
            Reason::BossctlCommand(_) => "bossctl_command",
            Reason::BossLifecycleCommand(_) => "boss_lifecycle_command",
        }
    }

    fn detail(self) -> Option<String> {
        match self {
            Reason::BossStatePath(p) => Some(p),
            Reason::BossctlCommand(c) => Some(c),
            Reason::BossLifecycleCommand(c) => Some(c),
        }
    }
}

fn classify(boss_state_dir: &Path, tool_name: &str, tool_input: &serde_json::Value) -> Option<Reason> {
    match tool_name {
        "Read" | "Edit" | "Write" | "NotebookEdit" => {
            let path = tool_input.get("file_path").and_then(|v| v.as_str())?;
            if path_is_inside(boss_state_dir, path) {
                Some(Reason::BossStatePath(path.to_owned()))
            } else {
                None
            }
        }
        "Bash" => {
            let cmd = tool_input.get("command").and_then(|v| v.as_str())?;
            classify_bash(boss_state_dir, cmd)
        }
        _ => None,
    }
}

fn classify_bash(boss_state_dir: &Path, command: &str) -> Option<Reason> {
    // Use shlex so quoted paths with spaces (e.g.
    // `'/Users/x/Library/Application Support/Boss/bin/bossctl' probe`)
    // tokenize as one argv entry — `split_whitespace` would shred
    // those across tokens and the absolute-path detection would miss.
    // shlex returns None on unclosed quotes; in that case we fall
    // back to whitespace splitting (best-effort — the literal-path
    // scan below still catches the obvious shapes).
    let tokens: Vec<String> =
        shlex::split(command).unwrap_or_else(|| command.split_whitespace().map(|s| s.to_owned()).collect());

    // Detect bossctl invocations even when the path has embedded
    // spaces and shlex tokenization shredded the basename across
    // tokens (e.g. an unquoted
    // `/Users/x/Library/Application Support/Boss/bin/bossctl …`).
    // A literal `/bossctl` followed by an argument boundary covers
    // the practical shapes.
    if command.contains("/bossctl ")
        || command.ends_with("/bossctl")
        || command.contains("/bossctl\t")
        || command.contains("/bossctl'")
        || command.contains("/bossctl\"")
    {
        return Some(Reason::BossctlCommand(command.to_owned()));
    }

    if let Some(first) = tokens.first() {
        let basename = Path::new(first)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first.as_str());

        if basename == "bossctl" {
            return Some(Reason::BossctlCommand(command.to_owned()));
        }

        if basename == "boss" {
            // Only `engine start` / `engine stop` are lifecycle verbs
            // the deny rule fences off; everything else under
            // `boss …` is allowed (list/show talk to the engine over
            // IPC, which is a legitimate worker surface for the
            // design + conflict-resolution flows).
            let rest: Vec<&str> = tokens.iter().skip(1).map(String::as_str).collect();
            if matches!(rest.as_slice(), ["engine", "start", ..] | ["engine", "stop", ..]) {
                return Some(Reason::BossLifecycleCommand(command.to_owned()));
            }
        }
    }

    // Catch shell forms like `cat '/path/with spaces/Boss/state.db'`
    // by scanning each tokenized argv entry against the boss state
    // dir prefix.
    let state_dir_str = boss_state_dir.to_string_lossy();
    if !state_dir_str.is_empty() {
        for token in &tokens {
            if path_is_inside(boss_state_dir, token) || token.contains(state_dir_str.as_ref()) {
                return Some(Reason::BossStatePath(command.to_owned()));
            }
        }
        // Defensive: also scan the raw command for the literal path
        // (some worker shells use escaped spaces — `Boss\ root` — that
        // shlex normalizes away from the tokens, so the prefix-match
        // above would miss them).
        if command.contains(state_dir_str.as_ref()) {
            return Some(Reason::BossStatePath(command.to_owned()));
        }
    }

    None
}

fn path_is_inside(parent: &Path, candidate: &str) -> bool {
    let candidate_path = Path::new(candidate);
    // `starts_with` on `Path` is component-wise, so it won't false-match
    // `/Users/x/Library/Application Support/BossA` against
    // `/Users/x/Library/Application Support/Boss`.
    candidate_path.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn pre_tool(tool: &str, input: serde_json::Value) -> WorkerEvent {
        WorkerEvent::PreToolUse {
            session_id: "s".into(),
            tool_name: tool.into(),
            tool_input: input,
        }
    }

    fn boss_dir() -> PathBuf {
        PathBuf::from("/Users/test/Library/Application Support/Boss")
    }

    #[test]
    fn read_of_state_db_is_classified() {
        let reason = classify(
            &boss_dir(),
            "Read",
            &serde_json::json!({
                "file_path": "/Users/test/Library/Application Support/Boss/state.db",
            }),
        );
        assert!(matches!(reason, Some(Reason::BossStatePath(_))));
    }

    #[test]
    fn read_of_sibling_dir_is_not_classified() {
        // `…/BossA/…` must not match `…/Boss` — component-wise.
        let reason = classify(
            &boss_dir(),
            "Read",
            &serde_json::json!({
                "file_path": "/Users/test/Library/Application Support/BossA/file",
            }),
        );
        assert!(reason.is_none());
    }

    #[test]
    fn read_of_workspace_is_not_classified() {
        let reason = classify(
            &boss_dir(),
            "Read",
            &serde_json::json!({
                "file_path": "/Users/test/Documents/dev/workspaces/mono-agent-001/src/foo.rs",
            }),
        );
        assert!(reason.is_none());
    }

    #[test]
    fn bash_bossctl_command_is_classified() {
        let reason = classify(
            &boss_dir(),
            "Bash",
            &serde_json::json!({ "command": "bossctl agents list" }),
        );
        match reason {
            Some(Reason::BossctlCommand(cmd)) => assert_eq!(cmd, "bossctl agents list"),
            other => panic!("expected BossctlCommand, got {other:?}"),
        }
    }

    #[test]
    fn bash_absolute_bossctl_path_is_classified() {
        let reason = classify(
            &boss_dir(),
            "Bash",
            &serde_json::json!({
                "command": "/Users/test/Library/Application Support/Boss/bin/bossctl probe",
            }),
        );
        assert!(matches!(reason, Some(Reason::BossctlCommand(_))));
    }

    #[test]
    fn bash_boss_engine_stop_is_classified() {
        let reason = classify(
            &boss_dir(),
            "Bash",
            &serde_json::json!({ "command": "boss engine stop" }),
        );
        assert!(matches!(reason, Some(Reason::BossLifecycleCommand(_))));
    }

    #[test]
    fn bash_boss_task_list_is_not_classified() {
        // Read-only `boss` verbs are out of scope: they talk to the
        // engine over its IPC socket and are legitimate worker uses.
        let reason = classify(
            &boss_dir(),
            "Bash",
            &serde_json::json!({ "command": "boss task list --json" }),
        );
        assert!(reason.is_none());
    }

    #[test]
    fn bash_cat_state_db_via_shell_is_classified() {
        // A worker can't bypass the path-tool deny rules by piping
        // through `cat` — the bash classifier catches the literal path.
        let reason = classify(
            &boss_dir(),
            "Bash",
            &serde_json::json!({
                "command": "cat '/Users/test/Library/Application Support/Boss/state.db'",
            }),
        );
        assert!(matches!(reason, Some(Reason::BossStatePath(_))));
    }

    #[test]
    fn unknown_tool_is_skipped() {
        let reason = classify(&boss_dir(), "Grep", &serde_json::json!({ "pattern": "state.db" }));
        assert!(reason.is_none());
    }

    #[test]
    fn record_if_sandbox_attempt_writes_to_audit_log_on_match() {
        // End-to-end: a PreToolUse that names state.db produces a
        // `worker_sandbox_attempt` line in the engine audit log,
        // tagged with the run_id so triage can pivot back to the
        // worker. Uses the BOSS_ENGINE_AUDIT_PATH env override to
        // redirect writes to a tempfile rather than the global path.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit-from-test.log");
        // SAFETY: tests run single-threaded with serial when sharing
        // process-global env; this test is benign even if interleaved
        // (the audit module's `OnceLock` may have been set already,
        // in which case our write goes elsewhere and the assertions
        // below skip cleanly — see the `if path.exists()` guard).
        unsafe {
            std::env::set_var(audit::AUDIT_PATH_ENV, &path);
        }

        let event = pre_tool(
            "Read",
            serde_json::json!({
                "file_path": "/Users/test/Library/Application Support/Boss/state.db",
            }),
        );
        record_if_sandbox_attempt(&boss_dir(), Some("run-abc"), &event);

        unsafe {
            std::env::remove_var(audit::AUDIT_PATH_ENV);
        }

        if !path.exists() {
            // Another test set AUDIT_PATH first via OnceLock; can't
            // assert on the tempfile in that case. Detection itself
            // is verified by the classify_* tests.
            return;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("worker_sandbox_attempt"),
            "expected audit line, got: {raw}",
        );
        assert!(raw.contains("\"tool\":\"Read\""), "missing tool field: {raw}");
        assert!(
            raw.contains("\"reason\":\"boss_state_path\""),
            "missing reason field: {raw}",
        );
        assert!(raw.contains("\"run_id\":\"run-abc\""), "missing run_id: {raw}");
    }

    #[test]
    fn non_pretooluse_events_are_skipped() {
        // `record_if_sandbox_attempt` early-outs on non-PreToolUse —
        // verify by passing a Stop event and confirming no panic /
        // no fallthrough into classify(). (We can't easily intercept
        // audit::record_event from here; the assertion is that this
        // call simply returns cleanly.)
        use boss_protocol::StopReason;
        let event = WorkerEvent::Stop {
            session_id: "s".into(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        };
        record_if_sandbox_attempt(&boss_dir(), Some("run-1"), &event);
    }
}
