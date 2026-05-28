//! Per-worker config the engine materializes before `claude` is spawned.
//!
//! The engine writes two files into `<workspace_path>/.claude/`:
//!
//! - `CLAUDE.md` — a worker-facing system prompt that constrains the
//!   claude session: jj-first VCS rules, do-not-touch-sibling-workspaces
//!   advisory, lease lifecycle reminders, PR-required-for-task-work
//!   reminder.
//! - `.gitignore` — single-pattern (`*`) gitignore that hides every
//!   per-worker file the engine drops in `.claude/` (the `CLAUDE.md`
//!   above and the `initial-prompt.txt` written by the runner) from
//!   `jj status` / `git status`. Without this, workers regularly
//!   snapshot the engine plumbing into their PRs. The pattern is
//!   self-excluding, so the `.gitignore` itself doesn't show up either.
//!
//! and one file **outside every workspace**, under the per-user system
//! temp dir (see [`worker_settings_path`]):
//!
//! - the worker *settings* file — claude hooks config that wires every
//!   hook event (`SessionStart` … `SessionEnd`) to the `boss-event` shim
//!   binary, so the engine's events socket sees a structured stream of
//!   worker activity. Also pins `permissions.defaultMode` to `auto` and
//!   carries the `deny` rules that fence the worker off from Boss's
//!   runtime state. The engine points the spawned session at it with
//!   `claude --settings <abs-path>`.
//!
//!   On top of the `deny` globs, the `PreToolUse` hooks include a
//!   *deterministic* Boss-data-dir gate (see [`PATH_GUARD_SCRIPT`]): a
//!   small script that canonicalises the working dir and every candidate
//!   path and blocks any tool call that resolves inside the Boss data
//!   dir, regardless of which tool dresses up the access or whether the
//!   session model spots the path string. The script is written next to
//!   the settings file by [`write_workspace_files`] / refreshed by
//!   [`heal_worker_settings_json`].
//!
//!   This file is deliberately **never** written into the workspace
//!   tree — not as `.claude/settings.json`, not as
//!   `.claude/settings.local.json`. Repos commonly check in a shared,
//!   *tracked* `.claude/settings.json` (e.g. `deny` rules for generated
//!   testdata). The `.gitignore` we drop in `.claude/` cannot hide an
//!   already-tracked file, and we cannot assume any repo gitignores
//!   `settings.local.json` either — so any file we drop in the
//!   workspace risks being picked up by `jj git push` and shipped into
//!   the worker's PR (clobbering the repo's shared policy and leaking
//!   Boss-session ids / local Boss.app hook paths). Writing the settings
//!   *outside* the workspace removes the VCS from the equation entirely.
//!
//!   `claude --settings <file>` loads the file as *additional* settings,
//!   merged on top of (not replacing) the repo's own project
//!   `.claude/settings.json`, so the repo's deny rules survive and the
//!   worker still runs unattended with the engine's hooks. (Permission
//!   mode is also forced via the `--permission-mode auto` CLI flag the
//!   runner passes, so the worker runs autonomously regardless.)
//!
//! This module is just the renderers and a tiny `write_workspace_files()`
//! helper. Call-sites in the worker spawn flow are wired separately.

use std::io;
use std::path::{Path, PathBuf};

use serde_json;

/// All the inputs a worker-config render needs. The shape is
/// deliberately minimal — anything more (project-specific guidance,
/// allowlisted tools) lives in higher layers and is rendered separately.
#[derive(Debug, Clone)]
pub struct WorkerSetupInput {
    /// Run id this spawn corresponds to. Baked into the hook command
    /// in the worker settings file as a `BOSS_RUN_ID=<run_id>` inline-assignment
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
    /// Engine events socket path; injected into the worker settings file via the
    /// `BOSS_EVENTS_SOCKET` env var so the shim knows where to connect.
    pub events_socket_path: PathBuf,
    /// Absolute path to the `boss-event` shim binary the engine will
    /// place into the worker's PATH per lease. This template
    /// references the shim by absolute path so a hook fires even if
    /// the user's PATH is unusual.
    pub boss_event_path: PathBuf,
    /// When `true`, the CLAUDE.md includes a directive to use
    /// `--draft` when running `gh pr create`. Omitted when `false`
    /// so workers on default installs see no behaviour change.
    pub draft_pr_mode: bool,
    /// Execution kind (e.g. `"chore_implementation"`, `"revision_implementation"`).
    /// Used to install kind-specific hook guards — currently a PreToolUse deny
    /// for `gh pr create` on `revision_implementation` executions.
    pub execution_kind: String,
    /// Task kind from the underlying work item (e.g. `"revision"`, `"chore"`).
    /// `None` for non-task work items (products, projects).
    ///
    /// Defense-in-depth: the `gh pr create` guard is keyed off the task kind
    /// in ADDITION to the execution kind, so a mis-derived execution kind
    /// (e.g. a revision re-dispatched as `task_implementation` due to a bug)
    /// still cannot open a new PR.
    pub task_kind: Option<String>,
}

/// Render the worker-facing CLAUDE.md.
pub fn render_claude_md(input: &WorkerSetupInput) -> String {
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let draft_directive = if input.draft_pr_mode {
        "\n## PR creation mode\n\
         \n\
         Default PR creation mode: pass `--draft` to `cube pr ensure`\n\
         (or `gh pr create`) unless the chore description explicitly says\n\
         to create a non-draft PR.\n"
    } else {
        ""
    };
    format!(
        "# Boss worker rules\n\
         \n\
         You are running inside a Boss-managed worker session. The engine\n\
         spawned you in a leased cube workspace and observes this session\n\
         via claude hooks.\n\
         \n\
         ## Pull requests are the deliverable\n\
         \n\
         **A task is not complete until a PR exists.** Local commits are NOT enough.\n\
         \n\
         - Push your branch and open a PR with `gh pr create` once\n\
           commits exist and tests pass.\n\
         - **If a PR already exists** (resuming or addressing review),\n\
           push new commits to update it; do NOT open a duplicate. Check:\n\
           `gh pr list --head $(jj log -r @ --no-graph -T 'bookmarks' | head -1)`\n\
           or `gh pr view`.\n\
         - Do not hard-wrap PR bodies.\n\
         - Print the PR URL on its own line as the last thing in your final response.\n\
         - Before pushing, run `jj diff -r @`. If the diff is empty,\n\
           do NOT commit, push, or open a PR — stop and explain.\n\
         \n\
         ## Your workspace\n\
         \n\
         - Workspace path: `{workspace}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n\
         \n\
         ## VCS\n\
         \n\
         Use `jj` for all VCS. Do not invoke `git` directly except via `gh`.\n\
         \n\
         - `jj git fetch` to sync; `jj new main` for a fresh task;\n\
           `jj edit <bookmark>` to resume.\n\
         - `jj describe -m '...'` to set commit messages;\n\
           `jj git push -b <bookmark>` to publish.\n\
         - Never `jj git push --deleted` or `git push --delete`\n\
           without explicit user approval.\n\
         - `.claude/` is gitignored by the engine. Do not force-track\n\
           or commit anything inside it (no `--force`,\n\
           no `jj file track .claude/...`).\n\
         \n\
         ### Commit messages must be inline\n\
         \n\
         Always pass `-m \"…\"` to `git commit`, `git rebase`, `jj commit`,\n\
         `jj describe`, and amend/squash flows (`git commit --amend`,\n\
         `jj squash`, `jj split`). The worker environment has no usable\n\
         `$EDITOR` — commands that fall through to one fail. Fix by\n\
         re-running with `-m`.\n\
         \n\
         ## Creating a PR from a jj workspace\n\
         \n\
         Cube workspaces are secondary jj workspaces. There is no `.git/`\n\
         at the workspace root, so bare `gh` calls fail with\n\
         `fatal: not a git repository`. Use `cube pr ensure` instead —\n\
         it resolves the remote `owner/repo` from `jj git remote` and\n\
         passes `-R <owner/repo>` to `gh`, so no `GIT_DIR` guess is needed.\n\
         \n\
         ### Canonical PR creation recipe\n\
         \n\
         ```sh\n\
         jj describe -m \"your commit message\"\n\
         jj bookmark create my-feature -r @\n\
         cube pr ensure --branch my-feature --title \"Your PR title\" --body \"PR description\"\n\
         ```\n\
         \n\
         `cube pr ensure` is idempotent: if a PR for `my-feature` already\n\
         exists, it prints its URL and exits 0 without opening a duplicate.\n\
         **Rule: `jj git push -b <bookmark>` requires `--allow-new` the first\n\
         time when calling jj directly; `cube pr ensure` handles this for you.**\n\
         \n\
         To update an existing PR (just push new commits):\n\
         \n\
         ```sh\n\
         jj git push -b my-feature   # no --allow-new needed for subsequent pushes\n\
         ```\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           under `~/Documents/dev/workspaces/` belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is coordinator/engine-only.\n\
           Never read, write, or touch it. Ask the coordinator for\n\
           work-taxonomy context; do not query the DB yourself.\n\
           `bossctl` is coordinator-only.\n\
         \n\
         ## Coordinator\n\
         \n\
         The coordinator may probe this session between turns. Treat probes\n\
         as questions from a human reviewer — short, specific answers.\n\
         {draft_directive}"
    )
}

/// Render the worker settings file. Wires every claude hook event to
/// the `boss-event` shim with absolute paths so the hook fires
/// regardless of `PATH`. The engine points the session at this via
/// `claude --settings`; it is written outside the workspace tree.
pub fn render_settings_json(input: &WorkerSetupInput) -> String {
    let value = settings_value(input);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

fn settings_value(input: &WorkerSetupInput) -> serde_json::Value {
    // Inline-prefix all env vars the shim needs. `BOSS_RUN_ID` is the
    // load-bearing one for live-worker-state correlation: if it's
    // missing from the shim's env, the splice that adds `_boss_run_id`
    // to the payload silently fails and the engine drops the hook
    // event, pinning the worker's activity at `Spawning`. Setting it
    // here (rather than relying on env inheritance from the worker
    // pane through claude into the hook subprocess) guarantees the
    // shim sees it regardless of how claude handles env propagation.
    //
    // `BOSS_WORKSPACE` tells the shim where to write its on-disk event
    // buffer when the engine is unreachable (see the shim's
    // resilience docs). Without it the shim falls back to cwd, which
    // is normally the workspace anyway — but inline-prefixing is the
    // belt that survives any future change to how claude propagates
    // cwd to hook subprocesses.
    let command = format!(
        "BOSS_EVENTS_SOCKET={socket} BOSS_LEASE_ID={lease} BOSS_RUN_ID={run_id} BOSS_WORKSPACE={workspace} {shim}",
        socket = shell_escape(&input.events_socket_path.display().to_string()),
        lease = shell_escape(&input.lease_id),
        run_id = shell_escape(&input.run_id),
        workspace = shell_escape(&input.workspace_path.display().to_string()),
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

    // For revision tasks, add a PreToolUse guard that blocks any
    // `gh pr create` invocation. Revision workers push commits to an
    // existing PR; opening a new PR violates the one-PR-per-task
    // invariant. The guard is a small inline Python script that reads
    // the tool_input JSON from stdin and blocks if the command matches
    // `gh pr create` (tolerant of GIT_DIR=... prefixes and flags).
    //
    // Defense-in-depth: the guard fires when EITHER the execution kind
    // is `revision_implementation` OR the task kind is `revision`.
    // This ensures a mis-derived execution kind (e.g. revision
    // re-dispatched as `task_implementation`) still cannot open a PR.
    let mut pre_tool_use_hooks = vec![hook.clone()];

    // Deterministic Boss-data-dir gate (every session, every tool). The
    // script canonicalises the working dir and every candidate path
    // (Bash argv tokens + file-tool `file_path`/`notebook_path`) and
    // blocks if any resolves inside the Boss data dir. This is the real
    // gate the issue asks for: unlike the `deny` globs below and the
    // engine-side audit, it resolves symlinks / `..` / `~` / `$VAR`
    // indirection, so the boundary holds regardless of which tool
    // dresses up the access or whether the model spots the path string.
    // Matcher is `*` (not a per-tool list) so a future tool that takes a
    // path is covered without a settings change; the script fast-paths
    // tools it doesn't inspect. The Boss data dir is derived from
    // `events_socket_path`'s parent — same source as `deny_rules`.
    if let Some(state_dir) = input.events_socket_path.parent() {
        let guard_command = format!(
            "BOSS_DATA_DIR={dir} python3 {script}",
            dir = shell_escape(&state_dir.display().to_string()),
            script = shell_escape(&path_guard_script_path().display().to_string()),
        );
        pre_tool_use_hooks.push(serde_json::json!({
            "matcher": "*",
            "hooks": [
                {
                    "type": "command",
                    "command": guard_command,
                }
            ],
        }));
    }

    let is_revision = input.execution_kind == "revision_implementation"
        || input.task_kind.as_deref() == Some("revision");
    if is_revision {
        let guard_command = concat!(
            "python3 -c \"",
            "import json,sys,re; ",
            "inp=json.load(sys.stdin); ",
            "cmd=inp.get('tool_input',{}).get('command',''); ",
            r#"m=re.search(r'(?:^|\s|;|\||&|GIT_DIR=\S+\s+)gh\s+pr\s+create\b|cube\s+pr\s+ensure\b',cmd); "#,
            "msg='Revision tasks push commits to the existing parent PR; they must not open a new PR. Use jj git push to update the existing PR instead.'; ",
            "print(json.dumps({'decision':'block','reason':msg}) if m else json.dumps({'decision':'approve'})); ",
            "\""
        );
        pre_tool_use_hooks.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [
                {
                    "type": "command",
                    "command": guard_command,
                }
            ],
        }));
    }

    serde_json::json!({
        // Auto mode for the worker pane. The engine's worker prompt
        // already instructs claude not to ask for human permission,
        // but that instruction is soft and cannot stop claude from
        // blocking on a tool-use prompt if the harness permission
        // mode is interactive. `auto` runs the session
        // autonomously while still respecting the user's permission
        // `allow`/`deny` rules — unlike `bypassPermissions`, which
        // the user's environment policy disallows. Project-local
        // settings override user-global per key, so this wins even
        // when the human's `~/.claude/settings.json` defaults to
        // interactive.
        //
        // The `deny` rules fence the worker off from Boss's runtime
        // state. Workers operate on source code in their leased
        // workspace; the engine's `state.db`, dispatch events,
        // engine-audit log and the events socket all live under the
        // Boss support dir and are coordinator-only. A worker that
        // reads `state.db` directly can see ground truth the
        // coordinator hasn't shown it (breaks reproducibility);
        // writing to those files is catastrophic. Same logic for
        // `bossctl`: that's the coordinator's CLI, not the worker's.
        // The deny rules are belt; the engine-side audit in
        // `audit_worker_sandbox_attempt` is suspenders that logs
        // every attempt even if a future harness change lets a tool
        // call through.
        "permissions": {
            "defaultMode": "auto",
            "deny": deny_rules(input),
        },
        "hooks": {
            "SessionStart":     [hook.clone()],
            "UserPromptSubmit": [hook.clone()],
            "PreToolUse":       pre_tool_use_hooks,
            "PostToolUse":      [hook.clone()],
            "Stop":             [hook.clone()],
            "Notification":     [hook.clone()],
            "SessionEnd":       [hook],
        },
    })
}

/// Build the permission deny list. Returns a JSON array of strings in
/// claude-code permission syntax: `<Tool>(<pattern>)`.
///
/// The Boss state directory is derived from `events_socket_path`'s
/// parent — both live under `~/Library/Application Support/Boss/` in
/// production, but tests / future relocations get the same treatment
/// without a hardcoded path.
fn deny_rules(input: &WorkerSetupInput) -> Vec<String> {
    let mut rules = Vec::new();

    if let Some(state_dir) = input.events_socket_path.parent() {
        let dir = state_dir.display().to_string();
        // Both the bare directory and the `**` subtree are listed
        // explicitly: glob `**` doesn't match the directory itself in
        // every harness, and we want a `Read("…/Boss")` ls attempt to
        // be denied just like a `Read("…/Boss/state.db")`.
        for prefix in ["Read", "Edit", "Write"] {
            rules.push(format!("{prefix}({dir})"));
            rules.push(format!("{prefix}({dir}/**)"));
        }
    }

    // `bossctl` is the coordinator's CLI surface (probes, agents
    // list, work mutations). Workers don't drive the coordinator,
    // they answer to it. Block every shape:
    //   - bare `bossctl` (no args)
    //   - `bossctl <verb> …` via the `:*` shell-prefix glob
    //   - any absolute path that ends in `/bossctl` (the engine's
    //     spawn flow injects an absolute symlink dir, so plain
    //     `bossctl` is the normal shape — but lock the absolute
    //     form too in case a worker tries to bypass via `$HOME/bin`).
    rules.push("Bash(bossctl)".to_owned());
    rules.push("Bash(bossctl:*)".to_owned());

    // `boss` lifecycle verbs that bounce the engine out from under
    // the worker. The rest of the `boss` surface (list/show/etc.)
    // talks to the engine over its IPC socket which is fine, but
    // start/stop reach into engine process state.
    rules.push("Bash(boss engine start)".to_owned());
    rules.push("Bash(boss engine start:*)".to_owned());
    rules.push("Bash(boss engine stop)".to_owned());
    rules.push("Bash(boss engine stop:*)".to_owned());

    rules
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

/// Subdirectory (under the per-user system temp dir) that holds the
/// worker settings files. Lives outside every workspace so the
/// worker's `jj`/`git` never sees these files — see the module docs.
const WORKER_SETTINGS_SUBDIR: &str = "boss-worker-settings";

/// Filename of the deterministic Boss-data-dir access gate script.
/// Written next to the worker settings file (same dir, shared fate) and
/// invoked by the `PreToolUse` hook with its absolute path.
const PATH_GUARD_SCRIPT_NAME: &str = "boss-path-guard.py";

/// The deterministic Boss-data-dir access gate, run as a `PreToolUse`
/// hook for every tool call.
///
/// The `deny` globs in [`deny_rules`] catch the obvious literal-path
/// shapes, and the engine-side audit in [`crate::worker_sandbox_audit`]
/// observes attempts — but both depend on the path *appearing literally*
/// in the tool input. This script is the layer that does not: it
/// canonicalises the working directory and every candidate path
/// (expanding `~`, `$VAR` and `..`; resolving symlinks) and rejects on a
/// component-wise prefix match against the Boss data dir. That makes the
/// boundary identical regardless of which tool dresses up the access
/// (`sqlite3`, `duckdb`, `cp`, an editor, a relative `state.db` after a
/// `cd`, a `$HOME`-prefixed path in a shell var, etc.) and regardless of
/// whether the session model notices the path string.
///
/// The data dir is supplied via the `BOSS_DATA_DIR` env var (set by the
/// hook command). The script is fail-open: anything it cannot positively
/// resolve to a path under the data dir is approved, so a payload-shape
/// change can never wedge a session — the positive prefix match is the
/// only deterministic block.
const PATH_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Deterministic Boss data-directory access gate (Claude Code PreToolUse hook).

Blocks any tool call whose target path canonically resolves inside the Boss
data directory (state.db, its -wal/-shm sidecars, the events socket, engine
pid/state files, and any future sidecar). Unlike an LLM classifier this does
not depend on the model recognising a path string in argv: it canonicalises
the working directory and every candidate path -- expanding ~, environment
variables and .. , and resolving symlinks -- then rejects on a component-wise
prefix match against the data directory.

The data directory is supplied via the BOSS_DATA_DIR environment variable,
set by the engine in the hook command. The PreToolUse payload arrives as JSON
on stdin; a decision JSON is written to stdout.

Fail-open by design: anything that cannot be positively resolved to a path
under the data directory is approved, so a payload-shape change can never
wedge a session. The positive prefix match is the only deterministic block.
"""
import json
import os
import shlex
import sys

RECOVERY = (
    "Blocked: direct access to the Boss data directory "
    "(~/Library/Application Support/Boss) is not allowed from a coordinator "
    "or worker session. That directory is engine-owned -- state.db, its "
    "-wal/-shm sidecars, the events socket, and engine pid/state files must "
    "never be read, copied, moved, edited, or opened by a session (no "
    "sqlite3, duckdb, litecli, sqlite-utils, cp/mv/rm, cat/head/tail/hexdump, "
    "editors, lsof, etc.). To recover or inspect Boss state use the "
    "sanctioned surface instead: ask the coordinator, file a shake with "
    "'boss shake' describing what you need, or use the dedicated boss/bossctl "
    "verb once it exists (e.g. 'boss task restore'). Do not work around this "
    "gate."
)


def canonical(path, cwd):
    expanded = os.path.expanduser(os.path.expandvars(path))
    if not os.path.isabs(expanded):
        expanded = os.path.join(cwd, expanded)
    return os.path.realpath(expanded)


def is_inside(child, parent):
    parent = parent.rstrip(os.sep)
    if not parent:
        return False
    return child == parent or child.startswith(parent + os.sep)


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def main():
    raw_dir = os.environ.get("BOSS_DATA_DIR", "").strip()
    if not raw_dir:
        emit("approve")
    data_dir = os.path.realpath(os.path.expanduser(raw_dir))

    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit("approve")
    if not isinstance(payload, dict):
        emit("approve")

    tool = payload.get("tool_name") or ""
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        tool_input = {}
    cwd = payload.get("cwd") or os.getcwd()

    candidates = []
    for key in ("file_path", "notebook_path", "path"):
        value = tool_input.get(key)
        if isinstance(value, str) and value:
            candidates.append(value)

    raw_command = ""
    if tool == "Bash":
        command = tool_input.get("command")
        if isinstance(command, str):
            raw_command = command
            try:
                tokens = shlex.split(command)
            except Exception:
                tokens = command.split()
            candidates.extend(tokens)

    for candidate in candidates:
        try:
            if is_inside(canonical(candidate, cwd), data_dir):
                emit("block", RECOVERY)
        except Exception:
            continue

    # Substring belt for Bash: catches $VAR / ~ indirection and backslash-
    # escaped spaces that tokenisation + canonicalisation miss (e.g.
    # P="$HOME/Library/Application Support/Boss/state.db"; sqlite3 "$P").
    # Needles are derived from the *non*-realpath expanded dir so the
    # home prefix matches the literal command text even when the real
    # home contains symlinks (realpath data_dir would diverge from the
    # $HOME the shell expands to).
    if raw_command:
        expanded_dir = os.path.expanduser(raw_dir)
        needles = [data_dir, expanded_dir]
        home = os.path.expanduser("~")
        if expanded_dir.startswith(home + os.sep):
            needles.append(expanded_dir[len(home):].lstrip(os.sep))
        unescaped = raw_command.replace("\\", "")
        for needle in needles:
            if needle and (needle in raw_command or needle in unescaped):
                emit("block", RECOVERY)

    emit("approve")


if __name__ == "__main__":
    main()
"#;

/// Directory holding all per-workspace worker settings files. The
/// engine writes into it at spawn time and heals stale `boss-event`
/// paths in it on restart ([`heal_worker_settings_json`]).
///
/// Rooted at the per-user system temp dir (`$TMPDIR` on macOS, a
/// private per-user location), so the files are user-private and never
/// inside a workspace tree.
pub fn worker_settings_dir() -> PathBuf {
    std::env::temp_dir().join(WORKER_SETTINGS_SUBDIR)
}

/// Absolute path to the worker settings file for `workspace_path`. The
/// engine writes this file and points the worker's claude session at it
/// via `claude --settings <path>`; nothing is written into the
/// workspace tree itself.
///
/// Keyed by the workspace directory name (cube workspaces are uniquely
/// named, e.g. `mono-agent-003`), so re-leasing a workspace overwrites
/// the one file rather than accumulating one per lease.
pub fn worker_settings_path(workspace_path: &Path) -> PathBuf {
    let key = workspace_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "worker".to_owned());
    worker_settings_dir().join(format!("{key}.json"))
}

/// Absolute path to the deterministic Boss-data-dir gate script. Shared
/// across every session (the script is data-dir-agnostic; the dir is
/// passed at invocation via `BOSS_DATA_DIR`), so it lives once in the
/// [`worker_settings_dir`] alongside the per-workspace settings files.
pub fn path_guard_script_path() -> PathBuf {
    worker_settings_dir().join(PATH_GUARD_SCRIPT_NAME)
}

/// Write the [`PATH_GUARD_SCRIPT`] into `dir`, creating it if needed.
/// Idempotent: overwrites any existing copy with the current source so a
/// stale script from an older engine build is refreshed. Returns the
/// path written.
pub fn ensure_path_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PATH_GUARD_SCRIPT_NAME);
    std::fs::write(&path, PATH_GUARD_SCRIPT)?;
    Ok(path)
}

/// Write `CLAUDE.md` and a self-excluding `.gitignore` under
/// `<workspace>/.claude/`, and the worker settings file *outside* the
/// workspace at [`worker_settings_path`]. Creates parent directories as
/// needed. Caller is responsible for ensuring the workspace itself
/// exists.
///
/// The settings file is never written into the workspace tree — see the
/// module docs for why dropping session config into a VCS-visible path
/// (`settings.json` or `settings.local.json`) is the bug this avoids.
pub fn write_workspace_files(input: &WorkerSetupInput) -> io::Result<WrittenFiles> {
    let claude_dir = input.workspace_path.join(".claude");
    std::fs::create_dir_all(&claude_dir)?;

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let gitignore_path = claude_dir.join(".gitignore");

    std::fs::write(&claude_md_path, render_claude_md(input))?;
    std::fs::write(&gitignore_path, CLAUDE_DIR_GITIGNORE)?;

    let settings_path = worker_settings_path(&input.workspace_path);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
        // The PreToolUse gate script lives next to the settings file
        // (same dir, shared fate) and the hook invokes it by absolute
        // path; write it whenever we materialise the settings file.
        ensure_path_guard_script_in(parent)?;
    }
    std::fs::write(&settings_path, render_settings_json(input))?;

    Ok(WrittenFiles {
        claude_md_path,
        settings_path,
        gitignore_path,
    })
}

#[derive(Debug, Clone)]
pub struct WrittenFiles {
    pub claude_md_path: PathBuf,
    /// Absolute path to the worker settings file. Lives *outside* the
    /// workspace (under [`worker_settings_dir`]); the runner threads it
    /// into the spawn invocation as `claude --settings <path>`.
    pub settings_path: PathBuf,
    pub gitignore_path: PathBuf,
}

/// Convenience: absolute path to the per-lease `.claude/` dir.
pub fn claude_dir_for(workspace: &Path) -> PathBuf {
    workspace.join(".claude")
}

/// Replace the boss-event shim path in a single hook command string.
///
/// The command format produced by [`render_settings_json`] is:
/// `BOSS_EVENTS_SOCKET='...' BOSS_LEASE_ID='...' BOSS_RUN_ID='...' BOSS_WORKSPACE='...' '<shim_path>'`
///
/// This function finds the last single-quoted token that contains `boss-event`
/// and replaces it with a shell-escaped version of `new_boss_event_path`.
/// Returns the original string unchanged if no recognizable shim path is found.
pub(crate) fn heal_hook_command(command: &str, new_boss_event_path: &Path) -> String {
    let Some(shim_pos) = command.rfind("boss-event") else {
        return command.to_owned();
    };
    // Walk backward from shim_pos to find the opening single quote.
    let Some(open_pos) = command[..shim_pos].rfind('\'') else {
        return command.to_owned();
    };
    // Walk forward past "boss-event" to find the closing single quote.
    let after = shim_pos + "boss-event".len();
    let Some(close_offset) = command[after..].find('\'') else {
        return command.to_owned();
    };
    let close_pos = after + close_offset;
    let new_escaped = shell_escape(&new_boss_event_path.display().to_string());
    format!(
        "{}{}{}",
        &command[..open_pos],
        new_escaped,
        &command[close_pos + 1..]
    )
}

/// Walk every `*.json` file in `settings_dir` (the
/// [`worker_settings_dir`]) and update the boss-event shim path in each
/// to `new_boss_event_path`. A missing directory is a no-op; per-file
/// errors are logged but do not abort the sweep.
pub fn heal_worker_settings_json(settings_dir: &Path, new_boss_event_path: &Path) {
    let entries = match std::fs::read_dir(settings_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::warn!(
                dir = %settings_dir.display(),
                ?err,
                "failed to read worker settings dir for boss-event healing",
            );
            return;
        }
    };

    // The settings dir exists, so live workers may have PreToolUse hooks
    // pointing at the gate script in it. Refresh the script (TMPDIR churn
    // or an older engine build may have removed/staled it) so the gate
    // survives an engine restart, not just a fresh spawn.
    if let Err(err) = ensure_path_guard_script_in(settings_dir) {
        tracing::warn!(
            dir = %settings_dir.display(),
            ?err,
            "failed to refresh path-guard script during settings heal",
        );
    }

    for entry in entries.flatten() {
        let settings_path = entry.path();
        if settings_path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match heal_single_settings_json(&settings_path, new_boss_event_path) {
            Ok(true) => {
                tracing::info!(
                    settings = %settings_path.display(),
                    "healed boss-event path in worker settings file",
                );
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    settings = %settings_path.display(),
                    ?err,
                    "failed to heal boss-event path in worker settings file",
                );
            }
        }
    }
}

/// Returns `Ok(true)` if any hook commands were updated, `Ok(false)` if
/// the file was absent or unchanged.
fn heal_single_settings_json(
    settings_path: &Path,
    new_boss_event_path: &Path,
) -> io::Result<bool> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    let mut parsed: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut changed = false;

    if let Some(hooks) = parsed.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_name, entries) in hooks.iter_mut() {
            if let Some(arr) = entries.as_array_mut() {
                for entry in arr.iter_mut() {
                    if let Some(inner_hooks) = entry
                        .get_mut("hooks")
                        .and_then(|h| h.as_array_mut())
                    {
                        for inner in inner_hooks.iter_mut() {
                            if let Some(cmd) = inner
                                .get("command")
                                .and_then(|c| c.as_str())
                                .map(str::to_owned)
                            {
                                let healed = heal_hook_command(&cmd, new_boss_event_path);
                                if healed != cmd {
                                    inner["command"] = serde_json::Value::String(healed);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if changed {
        let new_content = serde_json::to_string_pretty(&parsed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(settings_path, new_content)?;
    }

    Ok(changed)
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
            draft_pr_mode: false,
            execution_kind: "chore_implementation".into(),
            task_kind: Some("chore".into()),
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
            // The boss-event shim is always the first entry for every
            // hook event. `PreToolUse` carries extra entries (the
            // deterministic path guard, plus a revision-only guard); the
            // other six events are wired exactly once.
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
    fn settings_json_inlines_workspace_into_every_hook_command() {
        // The shim writes its on-disk event buffer relative to
        // `BOSS_WORKSPACE` when the engine socket is unreachable. The
        // hook command must inline-prefix this env var so the buffer
        // lives in the lease's workspace regardless of cwd.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
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
    fn settings_json_denies_boss_state_dir_reads_writes_and_edits() {
        // The acceptance criterion for the worker-sandboxing change:
        // a worker spawned by the engine cannot, via Read / Edit /
        // Write, touch any file under the Boss state dir. The deny
        // list must name the dir and the `**` subtree for each tool
        // so a `Read("…/Boss")` ls and a `Read("…/Boss/state.db")`
        // both deny.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let deny = parsed["permissions"]["deny"]
            .as_array()
            .expect("deny array present");
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
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
            assert!(
                deny.iter().any(|r| *r == rule),
                "expected deny rule {rule} in {deny:?}",
            );
        }
    }

    #[test]
    fn settings_json_does_not_deny_workspace_paths() {
        // Defensive: a buggy deny rule that accidentally fences off
        // `~/Documents/dev/workspaces/…` would break every worker
        // (their lease lives there). Verify no deny rule names the
        // workspace root.
        let input = sample_input();
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        assert_eq!(
            parsed["permissions"]["defaultMode"],
            serde_json::Value::String("auto".into()),
            "expected permissions.defaultMode == 'auto', got: {parsed}",
        );
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
            draft_pr_mode: false,
            execution_kind: "chore_implementation".into(),
            task_kind: Some("chore".into()),
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
        // Empty-diff guard: the worker must verify the diff is non-empty
        // before pushing so the engine's empty-diff probe is never needed.
        assert!(
            rendered.contains("jj diff -r @"),
            "CLAUDE.md must remind workers to verify the diff before pushing",
        );
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
            draft_pr_mode: false,
            execution_kind: "chore_implementation".into(),
            task_kind: Some("chore".into()),
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
        assert!(
            rendered.contains("--branch"),
            "expected --branch flag guidance",
        );
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
            rendered.contains("fatal: not a git repository")
                || rendered.contains("no `.git/`"),
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
            boss_event_path: PathBuf::from(
                "/old/bazel-bin/tools/boss/event-shim/boss-event",
            ),
            draft_pr_mode: false,
            execution_kind: "chore_implementation".into(),
            task_kind: Some("chore".into()),
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse must be an array");
        // Must have 3 entries: the shim, the deterministic path guard,
        // and the revision-only gh-pr-create guard.
        assert_eq!(
            pre.len(),
            3,
            "revision_implementation PreToolUse must have shim + path guard + pr guard, got {pre:?}",
        );
        // The revision guard is the Bash-matcher entry.
        let pr_guard = pre
            .iter()
            .find(|e| e["matcher"] == serde_json::Value::String("Bash".into()))
            .expect("revision PreToolUse must include a Bash-matcher guard");
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse must be an array");
        // chore: [boss-event shim, deterministic path guard]. The
        // revision-only `gh pr create` guard must NOT be present.
        assert_eq!(
            pre.len(),
            2,
            "chore_implementation PreToolUse must have shim + path guard, got {pre:?}",
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

        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let pre = parsed["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse must be an array");

        assert_eq!(
            pre.len(),
            3,
            "revision task_kind must add the pr guard (shim + path guard + pr guard) even when execution_kind is wrong, got {pre:?}",
        );
        let pr_guard = pre
            .iter()
            .find(|e| e["matcher"] == serde_json::Value::String("Bash".into()))
            .expect("revision task_kind must include a Bash-matcher guard");
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
        let cmd = path_guard_command(&parsed)
            .expect("PreToolUse must include the deterministic path-guard hook");
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
        let parsed: serde_json::Value =
            serde_json::from_str(&render_settings_json(&input)).unwrap();
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
        assert!(s.contains("expanduser") && s.contains("expandvars"),
            "must expand ~ and $VAR indirection");
        assert!(s.contains("\"decision\"") && s.contains("\"block\""),
            "must be able to emit a block decision");
        assert!(s.contains("boss task restore") || s.contains("boss shake"),
            "block message must point at the sanctioned recovery surface");
    }

    #[test]
    fn write_workspace_files_writes_path_guard_script_outside_workspace() {
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
}
