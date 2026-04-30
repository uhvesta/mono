//! Per-lease worker config files written into a leased cube workspace
//! before `claude` is spawned.
//!
//! The engine writes two files into `<workspace_path>/.claude/`:
//!
//! - `CLAUDE.md` — a worker-facing system prompt that constrains the
//!   claude session: jj-first VCS rules, do-not-touch-sibling-workspaces
//!   advisory, lease lifecycle reminders, PR-required-for-task-work
//!   reminder.
//! - `settings.json` — claude hooks config that wires every hook event
//!   (`SessionStart` … `SessionEnd`) to the `boss-event` shim binary, so
//!   the engine's events socket sees a structured stream of worker
//!   activity (consumed by 6c's events socket / 6d's normalizer).
//!
//! Phase 6e ships the renderers and a tiny `write_workspace_files()`
//! helper. Actual call-sites (the worker spawn flow) land in 6f.

use std::io;
use std::path::{Path, PathBuf};

/// All the inputs a worker-config render needs. The shape is
/// deliberately minimal — anything more (project-specific guidance,
/// allowlisted tools) lives in higher layers and is rendered separately.
#[derive(Debug, Clone)]
pub struct WorkerSetupInput {
    /// Cube lease id for this worker. Surfaced to claude via the
    /// `BOSS_LEASE_ID` env var (set elsewhere); referenced in CLAUDE.md
    /// so a confused worker can describe its own lease.
    pub lease_id: String,
    /// Filesystem path of the leased workspace (the worker's cwd).
    pub workspace_path: PathBuf,
    /// Engine events socket path; injected into `settings.json` via the
    /// `BOSS_EVENTS_SOCKET` env var so the shim knows where to connect.
    pub events_socket_path: PathBuf,
    /// Absolute path to the `boss-event` shim binary the engine will
    /// place into the worker's PATH per lease (Phase 6b ships the
    /// binary; this template references it by absolute path so a hook
    /// fires even if the user's PATH is unusual).
    pub boss_event_path: PathBuf,
}

/// Render the worker-facing CLAUDE.md.
pub fn render_claude_md(input: &WorkerSetupInput) -> String {
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    format!(
        "# Boss worker rules\n\
         \n\
         You are running inside a Boss-managed worker session. The engine\n\
         spawned you in a leased cube workspace and is observing this\n\
         session via claude hooks routed to its events socket.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         The lease is held for the lifetime of this run. Do not lease,\n\
         release, or otherwise mutate cube state — the engine owns lease\n\
         lifecycle.\n\
         \n\
         ## VCS\n\
         \n\
         Use `jj` for all VCS operations. Do not invoke `git` directly\n\
         except via `gh` for GitHub operations.\n\
         \n\
         - `jj git fetch` to sync with origin.\n\
         - `jj new main` for a fresh task; `jj edit <bookmark>` to resume.\n\
         - `jj describe -m '...'` to set commit messages; `jj git push\n\
           -b <bookmark>` to publish.\n\
         - Never run `jj git push --deleted` or `git push --delete`\n\
           without explicit user approval.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           under `~/Documents/dev/workspaces/` belong to other workers\n\
           and concurrent edits will corrupt their state.\n\
         - Do not modify cube's database, lease state, or workspace\n\
           registry. The engine reconciles state on its own.\n\
         \n\
         ## Pull requests\n\
         \n\
         Any task work must end in a PR — local commits are not enough.\n\
         Use `gh pr create` once your branch has commits and tests pass.\n\
         Do not hard-wrap PR bodies.\n\
         \n\
         ## Coordinator\n\
         \n\
         The engine's coordinator (`bossctl`) may probe this session\n\
         between turns. Treat probes as you would a question from a\n\
         human reviewer — short, specific answers.\n"
    )
}

/// Render the worker-facing `settings.json`. Wires every claude hook
/// event to the `boss-event` shim with absolute paths so the hook fires
/// regardless of `PATH`.
pub fn render_settings_json(input: &WorkerSetupInput) -> String {
    let value = settings_value(input);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

fn settings_value(input: &WorkerSetupInput) -> serde_json::Value {
    let command = format!(
        "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} {shim}",
        socket = shell_escape(&input.events_socket_path.display().to_string()),
        lease = shell_escape(&input.lease_id),
        shim = shell_escape(&input.boss_event_path.display().to_string()),
    );

    let hook = serde_json::json!({
        "matcher": "*",
        "hooks": [
            {
                "type": "command",
                "command": command,
            }
        ],
    });

    serde_json::json!({
        "hooks": {
            "SessionStart":     [hook.clone()],
            "UserPromptSubmit": [hook.clone()],
            "PreToolUse":       [hook.clone()],
            "PostToolUse":      [hook.clone()],
            "Stop":             [hook.clone()],
            "Notification":     [hook.clone()],
            "SessionEnd":       [hook],
        },
    })
}

/// Single-quote a shell argument, escaping internal quotes. Matches the
/// quoting claude's hook-spawning shell expects (POSIX `sh`).
fn shell_escape(value: &str) -> String {
    let escaped = value.replace('\'', r#"'\''"#);
    format!("'{escaped}'")
}

/// Write CLAUDE.md and settings.json under `<workspace>/.claude/`,
/// creating the directory if needed. Caller is responsible for ensuring
/// the workspace itself exists.
pub fn write_workspace_files(input: &WorkerSetupInput) -> io::Result<WrittenFiles> {
    let claude_dir = input.workspace_path.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let settings_path = claude_dir.join("settings.json");

    std::fs::write(&claude_md_path, render_claude_md(input))?;
    std::fs::write(&settings_path, render_settings_json(input))?;

    Ok(WrittenFiles {
        claude_md_path,
        settings_path,
    })
}

#[derive(Debug, Clone)]
pub struct WrittenFiles {
    pub claude_md_path: PathBuf,
    pub settings_path: PathBuf,
}

/// Convenience: absolute path to the per-lease `.claude/` dir.
pub fn claude_dir_for(workspace: &Path) -> PathBuf {
    workspace.join(".claude")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_input() -> WorkerSetupInput {
        WorkerSetupInput {
            lease_id: "lease-uuid-abc".into(),
            workspace_path: PathBuf::from("/Users/brianduff/Documents/dev/workspaces/mono-agent-007"),
            events_socket_path: PathBuf::from(
                "/Users/brianduff/Library/Application Support/Boss/events.sock",
            ),
            boss_event_path: PathBuf::from(
                "/Users/brianduff/Library/Application Support/Boss/bin/boss-event",
            ),
        }
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
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["matcher"], "*");
        }
    }

    #[test]
    fn settings_json_threads_socket_lease_and_shim_into_command() {
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("events.sock"));
        assert!(command.contains("lease-uuid-abc"));
        assert!(command.contains("boss-event"));
        assert!(command.starts_with("BOSS_EVENTS_SOCKET="));
    }

    #[test]
    fn shell_escape_quotes_paths_with_spaces() {
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
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
    fn write_workspace_files_creates_claude_dir_and_writes_both_files() {
        let dir = TempDir::new().unwrap();
        let input = WorkerSetupInput {
            lease_id: "test-lease".into(),
            workspace_path: dir.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            boss_event_path: PathBuf::from("/tmp/boss-event"),
        };

        let written = write_workspace_files(&input).unwrap();

        assert!(written.claude_md_path.exists());
        assert!(written.settings_path.exists());
        assert_eq!(
            written.claude_md_path,
            dir.path().join(".claude").join("CLAUDE.md")
        );

        let claude_md_contents = std::fs::read_to_string(&written.claude_md_path).unwrap();
        assert!(claude_md_contents.contains("test-lease"));

        // settings.json must be valid JSON on disk.
        let settings_contents = std::fs::read_to_string(&written.settings_path).unwrap();
        let _: serde_json::Value = serde_json::from_str(&settings_contents).unwrap();
    }

    #[test]
    fn write_workspace_files_overwrites_existing_files() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "stale content").unwrap();

        let input = WorkerSetupInput {
            lease_id: "new-lease".into(),
            workspace_path: dir.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            boss_event_path: PathBuf::from("/tmp/boss-event"),
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
}
