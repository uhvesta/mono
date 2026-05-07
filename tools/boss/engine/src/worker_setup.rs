//! Per-lease worker config files written into a leased cube workspace
//! before `claude` is spawned.
//!
//! The engine writes three files into `<workspace_path>/.claude/`:
//!
//! - `CLAUDE.md` — a worker-facing system prompt that constrains the
//!   claude session: jj-first VCS rules, do-not-touch-sibling-workspaces
//!   advisory, lease lifecycle reminders, PR-required-for-task-work
//!   reminder.
//! - `settings.json` — claude hooks config that wires every hook event
//!   (`SessionStart` … `SessionEnd`) to the `boss-event` shim binary, so
//!   the engine's events socket sees a structured stream of worker
//!   activity.
//! - `.gitignore` — single-pattern (`*`) gitignore that hides every
//!   per-worker file the engine drops in `.claude/` (including the
//!   `initial-prompt.txt` written by the runner) from `jj status` /
//!   `git status`. Without this, workers regularly snapshot the engine
//!   plumbing into their PRs. The pattern is self-excluding, so the
//!   `.gitignore` itself doesn't show up either.
//!
//! This module is just the renderers and a tiny `write_workspace_files()`
//! helper. Call-sites in the worker spawn flow are wired separately.

use std::io;
use std::path::{Path, PathBuf};

/// All the inputs a worker-config render needs. The shape is
/// deliberately minimal — anything more (project-specific guidance,
/// allowlisted tools) lives in higher layers and is rendered separately.
#[derive(Debug, Clone)]
pub struct WorkerSetupInput {
    /// Run id this spawn corresponds to. Baked into the hook command
    /// in `settings.json` as a `BOSS_RUN_ID=<run_id>` inline-assignment
    /// prefix so the `boss-event` shim always sees it on stdin's env,
    /// regardless of whether claude propagates the worker pane's env
    /// to its hook subprocess. The shim splices this into every hook
    /// payload as `_boss_run_id`, which is how the engine correlates
    /// hook events to live-worker-state slots.
    pub run_id: String,
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
    /// place into the worker's PATH per lease. This template
    /// references the shim by absolute path so a hook fires even if
    /// the user's PATH is unusual.
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
         ## Pull requests are the deliverable\n\
         \n\
         **A task is not complete until a PR exists for it.** Local\n\
         commits are NOT enough. Workers that stop with only local\n\
         commits are treated as incomplete and the engine will probe\n\
         you to push and open a PR before transitioning the work item\n\
         to review.\n\
         \n\
         - Push your branch and open a PR with `gh pr create` once\n\
           your branch has commits and tests pass.\n\
         - **If a PR for this branch already exists** (e.g. you are\n\
           resuming work via `--prefer`, or addressing review\n\
           comments), push your new commits to update it; do NOT\n\
           open a duplicate PR. Check first with\n\
           `gh pr list --head $(jj log -r @ --no-graph -T 'bookmarks' | head -1)`\n\
           or simply `gh pr view` from inside the workspace.\n\
         - Do not hard-wrap PR bodies — GitHub renders single newlines\n\
           inside paragraphs as visible breaks.\n\
         - Before ending the run, print the PR URL on its own line as\n\
           the final thing in your final response so the engine can\n\
           pick it up automatically.\n\
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
         - `.claude/` is gitignored by the engine on every spawn. Do\n\
           not force-track or commit anything inside it (no\n\
           `--force`, no `jj file track .claude/...`) — those files\n\
           are per-worker plumbing, not part of the project.\n\
         \n\
         ### Commit messages must be inline\n\
         \n\
         Never invoke `git commit`, `git rebase`, `jj commit`, or\n\
         `jj describe` without an explicit `-m \"…\"` message. The same\n\
         rule applies to amend and squash flows (`git commit --amend`,\n\
         `jj squash`, `jj split`): pass `-m` inline. The worker\n\
         environment intentionally has no usable `$EDITOR`, so any\n\
         command that falls through to one will fail fast — fix it by\n\
         re-running with `-m`, not by changing the editor.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           under `~/Documents/dev/workspaces/` belong to other workers\n\
           and concurrent edits will corrupt their state.\n\
         - Do not modify cube's database, lease state, or workspace\n\
           registry. The engine reconciles state on its own.\n\
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
    // Inline-prefix all three env vars the shim needs. `BOSS_RUN_ID`
    // is the load-bearing one for live-worker-state correlation: if
    // it's missing from the shim's env, the splice that adds
    // `_boss_run_id` to the payload silently fails and the engine
    // drops the hook event, pinning the worker's activity at
    // `Spawning`. Setting it here (rather than relying on env
    // inheritance from the worker pane through claude into the hook
    // subprocess) guarantees the shim sees it regardless of how
    // claude handles env propagation.
    let command = format!(
        "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} {shim}",
        socket = shell_escape(&input.events_socket_path.display().to_string()),
        lease = shell_escape(&input.lease_id),
        run_id = shell_escape(&input.run_id),
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

/// Single-pattern gitignore body. `*` matches every entry in
/// `.claude/` — including dotfiles and the `.gitignore` itself, since
/// gitignore globs apply to leading-dot names. Both git and jj (with a
/// git backend) honor this in-tree gitignore, so worker setup files
/// stop appearing in `jj status` / `git status`.
const CLAUDE_DIR_GITIGNORE: &str = "*\n";

/// Write CLAUDE.md, settings.json, and a self-excluding `.gitignore`
/// under `<workspace>/.claude/`, creating the directory if needed.
/// Caller is responsible for ensuring the workspace itself exists.
pub fn write_workspace_files(input: &WorkerSetupInput) -> io::Result<WrittenFiles> {
    let claude_dir = input.workspace_path.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let settings_path = claude_dir.join("settings.json");
    let gitignore_path = claude_dir.join(".gitignore");

    std::fs::write(&claude_md_path, render_claude_md(input))?;
    std::fs::write(&settings_path, render_settings_json(input))?;
    std::fs::write(&gitignore_path, CLAUDE_DIR_GITIGNORE)?;

    Ok(WrittenFiles {
        claude_md_path,
        settings_path,
        gitignore_path,
    })
}

#[derive(Debug, Clone)]
pub struct WrittenFiles {
    pub claude_md_path: PathBuf,
    pub settings_path: PathBuf,
    pub gitignore_path: PathBuf,
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
            run_id: "run-sample".into(),
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
    fn settings_json_inlines_run_id_into_every_hook_command() {
        // BOSS_RUN_ID must be inline-prefixed on every hook command so
        // the `boss-event` shim can splice `_boss_run_id` into the
        // payload regardless of whether claude propagates env from the
        // worker pane to its hook subprocess. Without this, the engine
        // can't correlate hook events to runs and the live worker
        // state stays pinned at `Spawning` for the worker's lifetime.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
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
    fn write_workspace_files_creates_claude_dir_and_writes_all_files() {
        let dir = TempDir::new().unwrap();
        let input = WorkerSetupInput {
            run_id: "run-1".into(),
            lease_id: "test-lease".into(),
            workspace_path: dir.path().to_path_buf(),
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            boss_event_path: PathBuf::from("/tmp/boss-event"),
        };

        let written = write_workspace_files(&input).unwrap();

        assert!(written.claude_md_path.exists());
        assert!(written.settings_path.exists());
        assert!(written.gitignore_path.exists());
        assert_eq!(
            written.claude_md_path,
            dir.path().join(".claude").join("CLAUDE.md")
        );
        assert_eq!(
            written.gitignore_path,
            dir.path().join(".claude").join(".gitignore")
        );

        let claude_md_contents = std::fs::read_to_string(&written.claude_md_path).unwrap();
        assert!(claude_md_contents.contains("test-lease"));

        // settings.json must be valid JSON on disk.
        let settings_contents = std::fs::read_to_string(&written.settings_path).unwrap();
        let _: serde_json::Value = serde_json::from_str(&settings_contents).unwrap();

        // The .gitignore must use the catch-all `*` pattern so every
        // engine-injected file in `.claude/` (including dotfiles and
        // `.gitignore` itself) is hidden from `jj status` / `git status`.
        let gitignore_contents = std::fs::read_to_string(&written.gitignore_path).unwrap();
        assert_eq!(gitignore_contents, "*\n");
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
        let pr_offset = rendered.find("Pull requests are the deliverable").expect(
            "expected the strengthened PR heading to be present",
        );
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
    }

    #[test]
    fn write_workspace_files_overwrites_existing_files() {
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
