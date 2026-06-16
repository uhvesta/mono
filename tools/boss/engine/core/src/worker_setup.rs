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

/// The kind of worker being spawned, used to select the per-kind tool
/// denylist. Kept in this module so the denylist rules and the kind
/// definition are co-located and can evolve together.
///
/// New kinds should document their read/write access contract in a comment
/// so reviewers can verify the deny rules match the stated posture.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum WorkerKind {
    /// Normal implementation worker (task, chore, revision, etc.). Has
    /// write access to its leased workspace; can push branches and open
    /// PRs (subject to kind-specific guards such as the revision PR guard).
    #[default]
    Standard,
    /// Read-only reviewer worker (design §9). Reads the PR diff and workspace
    /// files; MUST NOT mutate files, push commits, or interact with GitHub
    /// write endpoints. The deny rules in [`reviewer_deny_rules`] are the
    /// primary enforcement layer for this mandate.
    Reviewer,
    /// Automation triage worker (Maint task 6). Investigates the repo and
    /// emits a single decision marker (`automation: task <id>` /
    /// `automation: skip — …`), optionally running one
    /// `boss task create --automation`. It MUST NOT do the work itself: no
    /// file edits, commits, pushes, or PRs — and crucially there is **no PR
    /// deliverable**, so it must not receive the [`WorkerKind::Standard`]
    /// "a PR is the deliverable / print the PR URL as your last line"
    /// CLAUDE.md, which otherwise overrides the marker contract and leaves
    /// the run ending without a decision marker. The deny rules in
    /// [`triage_deny_rules`] enforce the no-work posture; `boss task create`
    /// is intentionally left allowed.
    Triage,
}

/// All the inputs a worker-config render needs. The shape is
/// deliberately minimal — anything more (project-specific guidance,
/// allowlisted tools) lives in higher layers and is rendered separately.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
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
    /// Worker kind — determines the per-kind tool denylist installed in the
    /// worker settings file. Defaults to [`WorkerKind::Standard`] which adds
    /// no additional denies beyond the static sandbox rules. Set to
    /// [`WorkerKind::Reviewer`] to enforce the read-only mandate (§9).
    pub worker_kind: WorkerKind,
}

/// Render the worker-facing CLAUDE.md.
///
/// For [`WorkerKind::Reviewer`] workers, returns a reviewer-specific CLAUDE.md
/// that prominently states the read-only mandate and omits PR-creation
/// instructions (reviewers never open or update PRs).
pub fn render_claude_md(input: &WorkerSetupInput) -> String {
    if input.worker_kind == WorkerKind::Reviewer {
        return crate::pr_review::render_reviewer_claude_md(
            &input.lease_id,
            &input.workspace_path.display().to_string(),
        );
    }
    if input.worker_kind == WorkerKind::Triage {
        return crate::automation_triage::render_triage_claude_md(&input.lease_id);
    }
    let workspace = input.workspace_path.display();
    let lease = &input.lease_id;
    let draft_directive = if input.draft_pr_mode {
        "\n## PR creation mode\n\
         \n\
         Default PR creation mode: pass `--draft` to `cube pr create`\n\
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
         - Open a PR with `cube pr create` once commits exist and tests pass.\n\
         - **If a PR already exists** (resuming or addressing review),\n\
           push new commits to it with `cube pr update`; do NOT open a\n\
           duplicate. `cube pr create` errors if a PR already exists, and\n\
           `cube pr update` errors if none does. Check first with:\n\
           `gh pr list --head $(jj log -r @ --no-graph -T 'bookmarks' | head -1)`\n\
           or `gh pr view`.\n\
         - Do not hard-wrap PR bodies.\n\
         - **NEVER pass the PR body as `--body \"<inline text>\"`** — the shell\n\
           evaluates backticks and `$(...)` inside double-quoted strings, which\n\
           corrupts any body that contains inline code. Always write the body to\n\
           a file and use `--body-file` (see the recipe below).\n\
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
         - `jj git fetch` to sync; `jj new main@origin` for a fresh task;\n\
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
         `fatal: not a git repository`. Use `cube pr create` instead —\n\
         it resolves the remote `owner/repo` from `jj git remote` and\n\
         passes `-R <owner/repo>` to `gh`, so no `GIT_DIR` guess is needed.\n\
         \n\
         ### Canonical PR creation recipe\n\
         \n\
         Write the PR body to a temp file — never embed it inline on the command\n\
         line. This protects backticks, `$(...)`, and `${{VAR}}` from shell evaluation.\n\
         \n\
         ```sh\n\
         jj describe -m \"your commit message\"\n\
         jj bookmark create my-feature -r @\n\
         body=$(mktemp)\n\
         cat > \"$body\" << 'PRBODY'\n\
         ## Summary\n\
         Your description here. Inline code like `crate-name` and `$(cmd)` is safe.\n\
         PRBODY\n\
         cube pr create --branch my-feature --title \"Your PR title\" --body-file \"$body\"\n\
         ```\n\
         \n\
         `cube pr create` errors if an open PR already exists for the branch\n\
         (use `cube pr update` for that — see below). **Rule: `jj git push -b\n\
         <bookmark>` requires `--allow-new` the first time when calling jj\n\
         directly; `cube pr create` handles this for you.**\n\
         \n\
         To update an existing PR (push new commits to it):\n\
         \n\
         ```sh\n\
         cube pr update --branch my-feature   # pushes to the PR; errors if none exists\n\
         ```\n\
         \n\
         ### `origin` is the real GitHub upstream (shared object store)\n\
         \n\
         Every cube workspace is a **secondary jj workspace** that SHARES one\n\
         object store with its siblings — there is no per-workspace clone. That\n\
         store has a single `origin` remote pointing at the real GitHub\n\
         upstream, so `jj git push -b my-feature` reaches GitHub directly.\n\
         \n\
         - Prefer `cube pr create` / `cube pr update` for all pushes: they push to the\n\
           github.com remote by URL and verify the result against GitHub, and\n\
           — because the workspace has no top-level `.git` — it resolves\n\
           `-R <owner/repo>` for `gh` so PR creation Just Works.\n\
         - Because the store is shared, a `jj git fetch` in ANY workspace\n\
           advances the remote-tracking bookmarks (e.g. `main@origin`) seen by\n\
           ALL of them. Don't be alarmed if refs move without you fetching.\n\
         - A solid belt-and-suspenders check that a push actually landed is to\n\
           compare your local commit against GitHub's head sha (do not infer\n\
           success from the push command's own output alone):\n\
         \n\
         ```sh\n\
         # local commit you intended to ship\n\
         jj log -r my-feature --no-graph -T commit_id\n\
         # what GitHub actually has (must match)\n\
         gh api repos/<owner>/<repo>/branches/my-feature --jq .commit.sha\n\
         # for a specific PR head:\n\
         gh api repos/<owner>/<repo>/pulls/<n> --jq .head.sha\n\
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

/// Whether the worker settings should install the engine-data-dir
/// sandbox: the `deny` globs over the Boss support dir plus the
/// deterministic [`PATH_GUARD_SCRIPT`] `PreToolUse` hook.
///
/// Local workers run on the same machine as the engine and MUST be
/// fenced off its `state.db` / events socket / dispatch log. A remote
/// SSH worker runs on a host with no Boss engine, so there is nothing to
/// fence — and the "data dir" derived from the forwarded events socket's
/// parent (`/tmp` on the remote) is not a Boss dir at all, so installing
/// the sandbox there would deny the worker all of `/tmp` and invoke a
/// path-guard script that was never shipped to the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineDataDirSandbox {
    /// Install the data-dir deny globs + path-guard hook (local workers).
    Enabled,
    /// Omit them (remote SSH workers).
    Disabled,
}

/// Render the worker settings file. Wires every claude hook event to
/// the `boss-event` shim with absolute paths so the hook fires
/// regardless of `PATH`. The engine points the session at this via
/// `claude --settings`; it is written outside the workspace tree.
pub fn render_settings_json(input: &WorkerSetupInput) -> String {
    let value = settings_value(input, EngineDataDirSandbox::Enabled);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

/// Inline Python decision hook that guards revision tasks from opening new
/// PRs. Uses `shlex.split()` to tokenise the Bash command string so that
/// PR-creation phrases inside quoted arguments (commit messages, `--body`
/// strings) do NOT trigger the block — only the actual invoked program +
/// verb/subcommand tokens are inspected.
///
/// Blocks PR *creation* (`gh pr create`, `cube pr create`, and the
/// deprecated `cube pr ensure` which still create-or-reuses) and allows PR
/// *updates* (`cube pr update`), matching a revision worker's intent: push
/// commits to the existing parent PR, never open a new one. The block reason
/// names the offending command AND prints the exact `cube pr update` command
/// — reusing the `--branch`/`--head` value from the blocked command when the
/// worker supplied one — so no jj forensics are needed to recover.
///
/// Specifically:
///   • `jj describe -m "fix: intercept cube pr create"` → APPROVE
///   • `git commit -m "docs about gh pr create"` → APPROVE
///   • `cube pr update --branch feat/foo` → APPROVE
///   • `cube pr create --branch feat/foo` → BLOCK ("cube pr create")
///   • `cube pr ensure --branch feat/foo` → BLOCK ("cube pr ensure")
///   • `gh pr create --head feat/foo` → BLOCK ("gh pr create")
///   • `jj describe -m "msg" && cube pr create` → BLOCK ("cube pr create")
///
/// Fallback: when `shlex.split` fails (unmatched quotes or exotic syntax)
/// the script falls back to whitespace-splitting, which may have false
/// positives for heredoc content but is conservative and safe.
const REVISION_PR_GUARD_COMMAND: &str = concat!(
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

/// Render worker settings for a *remote* (SSH-dispatched) worker.
///
/// Identical to [`render_settings_json`] — the same `boss-event` hooks
/// wired for every event and the same static Boss-launch / revision
/// guards — but without the engine-data-dir sandbox (see
/// [`EngineDataDirSandbox`]). The caller fills `events_socket_path` with
/// the worker-visible *forwarded* socket path on the remote (e.g.
/// `/tmp/boss-events-<run>.sock`) and `boss_event_path` with the remote
/// shim (typically the bare `boss-event` resolved on the remote PATH).
pub fn render_remote_settings_json(input: &WorkerSetupInput) -> String {
    let value = settings_value(input, EngineDataDirSandbox::Disabled);
    serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable")
}

fn settings_value(input: &WorkerSetupInput, sandbox: EngineDataDirSandbox) -> serde_json::Value {
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

    // For revision tasks, add a PreToolUse guard that blocks any PR-creation
    // invocation (`gh pr create`, `cube pr create`, or the deprecated
    // `cube pr ensure`) while allowing PR updates (`cube pr update`).
    // Revision workers push commits to an existing PR; opening a new PR
    // violates the one-PR-per-task invariant. The guard tokenises the command
    // with shlex so PR-creation phrases inside quoted arguments (commit
    // messages, --body strings) do NOT trigger the block.
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
    //
    // Remote SSH workers skip this entirely (see `EngineDataDirSandbox`):
    // their `events_socket_path` is the forwarded `/tmp` socket, not a
    // Boss data dir, and the python guard script is never shipped there.
    if sandbox == EngineDataDirSandbox::Enabled
        && let Some(state_dir) = input.events_socket_path.parent()
    {
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

    // Block a worker from *launching Boss itself* — the macOS app or its
    // bundled engine. A worker that starts Boss attaches to the operator's
    // live engine on `/tmp/boss-engine.sock` (the dev build defaults to that
    // socket), collides with the running engine, and triggers repeated macOS
    // permission prompts. This is the route-independent guard: it blocks the
    // launch *action* regardless of how it's reached (a `verify`-style
    // "run the app" step, a direct `open`, `bazel run`, `swift run`, etc.),
    // so we keep skills like `verify` enabled while making "run the real
    // Boss" impossible from a sandboxed worker.
    //
    // Blocks (matcher `Bash`, inspecting the command string):
    //   - `open` of a Boss.app bundle / `-a Boss` / `-b dev.spinyfin.bossmacapp`
    //   - executing the bundled app or engine binary
    //     (`Boss.app/Contents/MacOS/Boss`, `…/Resources/bin/engine`)
    //   - `bazel run` of `//tools/boss/app-macos` or `//tools/boss/engine`
    //   - `swift run`
    //
    // Deliberately does NOT block `bazel test`/`swift test` of app-macos:
    // those targets are `macos_unit_test`s with no test_host, so they run
    // pure view-model/logic unit tests and do not launch the app or engine
    // (a dedicated regression test, TestSourcesDoNotCallRealOpenerTests, even
    // fails the build if a test reaches a real OS opener). Blocking them would
    // only cost app-macos workers their pre-push test gate for no safety gain.
    // `bazel build` is likewise allowed. Mirrors the inline-Python decision
    // hook used by the revision `gh pr create` guard below.
    let boss_launch_guard_command = concat!(
        "python3 -c \"",
        "import json,sys,re; ",
        "inp=json.load(sys.stdin); ",
        "cmd=inp.get('tool_input',{}).get('command',''); ",
        r#"m=re.search(r'(\bopen\b[^\n]*(Boss\.app|-a\s+Boss\b|-b\s+dev\.spinyfin\.bossmacapp))|Boss\.app/Contents/MacOS/Boss|Boss\.app/Contents/Resources/bin/engine|((bazel|bazelisk)\s+run\b[^\n]*tools/boss/(app-macos|engine))|(\bswift\s+run\b)',cmd); "#,
        "msg='Workers must not launch or run Boss itself. This command would start the Boss app or its bundled engine, which attaches to the operator live engine on /tmp/boss-engine.sock, collides with the running engine, and triggers OS permission prompts. Building and unit tests are fine (bazel build, bazel test); launching/running the app or engine is not. Runtime and UI verification are the coordinator job.'; ",
        "print(json.dumps({'decision':'block','reason':msg}) if m else json.dumps({'decision':'approve'})); ",
        "\""
    );
    pre_tool_use_hooks.push(serde_json::json!({
        "matcher": "Bash",
        "hooks": [
            {
                "type": "command",
                "command": boss_launch_guard_command,
            }
        ],
    }));

    // Deterministic pre-push checkleft gate (standard implementation
    // workers only). Matches Bash commands that push (`jj git push` /
    // `git push`) and blocks the push when the repo's checkleft reports
    // errors, echoing the findings + bypass guidance. The whole worker
    // fleet pushes with jj, whose native `jj git push` does not run git's
    // pre-push hook — so this restores the gate at the harness layer (the
    // same mechanism as the path guard above).
    //
    // Scoped to local standard workers:
    //   • Reviewer / triage workers cannot push (their deny rules block
    //     it), so the guard would never fire — omit it.
    //   • Remote SSH workers skip it for the same reason the path guard
    //     does: the gate script is materialised next to the local worker
    //     settings and is never shipped to the remote host. Remote workers
    //     are still covered for the sanctioned flows by the cube verb
    //     gates (`cube pr ensure` / `cube pr push`).
    if sandbox == EngineDataDirSandbox::Enabled && input.worker_kind == WorkerKind::Standard {
        let guard_command = format!(
            "python3 {script}",
            script = shell_escape(&checkleft_push_guard_script_path().display().to_string()),
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

    let is_revision =
        input.execution_kind == "revision_implementation" || input.task_kind.as_deref() == Some("revision");
    if is_revision {
        pre_tool_use_hooks.push(serde_json::json!({
            "matcher": "Bash",
            "hooks": [
                {
                    "type": "command",
                    "command": REVISION_PR_GUARD_COMMAND,
                }
            ],
        }));
    }

    let mut value = serde_json::json!({
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
            "deny": deny_rules(input, sandbox),
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
    });

    // Reviewer sessions are latency-sensitive review passes: fast mode
    // keeps Opus quality while cutting turnaround. Scoped to reviewers
    // only — implementation/design/investigation workers are unaffected.
    if input.worker_kind == WorkerKind::Reviewer {
        value["fastMode"] = serde_json::json!(true);
    }

    value
}

/// Build the permission deny list. Returns a JSON array of strings in
/// claude-code permission syntax: `<Tool>(<pattern>)`.
///
/// The Boss state directory is derived from `events_socket_path`'s
/// parent — both live under `~/Library/Application Support/Boss/` in
/// production, but tests / future relocations get the same treatment
/// without a hardcoded path.
fn deny_rules(input: &WorkerSetupInput, sandbox: EngineDataDirSandbox) -> Vec<String> {
    let mut rules = Vec::new();

    // The engine-data-dir globs only make sense for a local worker (the
    // events socket's parent is the Boss support dir). A remote worker's
    // `events_socket_path` is the forwarded `/tmp` socket, so these would
    // wrongly fence the worker off all of `/tmp`; skip them there. The
    // static `bossctl` / `boss engine` guards below still apply to both.
    if sandbox == EngineDataDirSandbox::Enabled
        && let Some(state_dir) = input.events_socket_path.parent()
    {
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

    // Per-kind extension: reviewer and triage workers both get the read-only /
    // no-publish denylist on top of the static rules above. Standard
    // implementation workers get nothing extra (they must be able to edit,
    // push, and open PRs).
    match input.worker_kind {
        WorkerKind::Reviewer => rules.extend(reviewer_deny_rules(&input.workspace_path)),
        WorkerKind::Triage => rules.extend(triage_deny_rules()),
        WorkerKind::Standard => {}
    }

    rules
}

/// Tool deny rules for reviewer workers, enforcing the read-only mandate
/// from design §9 ("Automated reviewer pass on every agent-authored PR").
///
/// These rules are appended on top of the static deny rules that apply to
/// every worker kind. They are kept as a named function (rather than inlined
/// in `deny_rules`) so task 3 — which wires the reviewer execution kind to
/// the spawn path — can confirm the exact rule set in tests.
///
/// **Read-only posture**: the reviewer reads the PR diff and workspace
/// files but must not write, push, or post to any external surface.
///
/// Rules cover:
/// - File-write tools (`Edit`, `Write`) — **scoped to `workspace_path`**, not
///   a blanket `**` (see below)
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// # Why the file-write deny is scoped, not blanket
///
/// The reviewer's mandate is to never change *the PR or its branch*. It must
/// still write exactly one engine-owned artifact: its `ReviewResult` JSON
/// (see [`crate::structured_output`]), which lives **outside** the checkout in
/// an engine scratch dir (the system temp dir). A blanket `Write(**)`/
/// `Edit(**)` would block that (deny rules take precedence over allow rules in
/// claude-code, so the path cannot be carved back out with an allow).
///
/// Instead the file-write deny is scoped to the **worker-workspaces root** —
/// the parent of `workspace_path`, under which every per-worker checkout lives
/// (`~/Documents/dev/workspaces/<repo>-agent-NNN`). That keeps the reviewer
/// unable to write to its own PR/repo *or* any sibling worker's workspace
/// (preserving the cross-worker isolation boundary the blanket deny gave),
/// while permitting the out-of-tree artifact write in `$TMPDIR`. Writing
/// engine scratch does not change the PR, so this does not weaken the
/// read-only mandate. The Boss support dir stays denied via the separate
/// data-dir globs in [`deny_rules`]. If `workspace_path` has no parent
/// (degenerate), the deny falls back to the workspace itself.
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub, so
/// they are safe for a read-only reviewer to run (e.g. to navigate the
/// history for context).
pub fn reviewer_deny_rules(workspace_path: &Path) -> Vec<String> {
    let fence = workspace_path.parent().unwrap_or(workspace_path).display();
    let mut rules = vec![format!("Edit({fence}/**)"), format!("Write({fence}/**)")];
    rules.extend(publish_deny_rules());
    rules
}

/// Tool deny rules for triage workers (Maint task 6, [`WorkerKind::Triage`]).
///
/// A triage worker investigates the repo and emits a decision marker; it must
/// NOT do the work itself — no edits, commits, pushes, or PRs. The rule set is
/// identical to [`reviewer_deny_rules`] today (both share the read-only /
/// no-publish posture in [`no_publish_deny_rules`]) but is exposed under its
/// own name so the two postures can diverge and so triage tests can assert the
/// exact set independently.
///
/// Note: `boss task create --automation …` is intentionally **not** denied —
/// creating exactly one task is the triage worker's sole write action, and it
/// goes through the engine IPC (with its own transactional open-task cap),
/// not through any of the rules above.
///
/// Unlike the reviewer (which writes one out-of-tree artifact and so gets a
/// workspace-scoped file-write deny), a triage worker writes no file at all,
/// so its file-write deny stays the blanket `Write(**)`/`Edit(**)`.
pub fn triage_deny_rules() -> Vec<String> {
    let mut rules = vec![
        // File-write tools — deny all edits and writes regardless of path.
        "Edit(**)".to_owned(),
        "Write(**)".to_owned(),
    ];
    rules.extend(publish_deny_rules());
    rules
}

/// Shared no-publish deny set used by both reviewer and triage workers:
/// neither kind may push commits or write to GitHub. The file-write deny is
/// kind-specific and lives in [`reviewer_deny_rules`] / [`triage_deny_rules`]
/// (workspace-scoped vs. blanket), so it is NOT part of this set.
///
/// Rules cover:
/// - VCS push — `jj git push` and `git push` in all their CLI forms
/// - PR mutation via `gh` — create, merge, close, edit, comment, review
/// - Issue write via `gh` — create, comment, close, edit
/// - `cube pr create` / `cube pr update` — Boss's PR helpers
///
/// Note: `jj describe`, `jj bookmark create`, and similar *local* VCS
/// operations are intentionally not denied. They touch only the local
/// repo state and can never publish commits or PR changes to GitHub.
fn publish_deny_rules() -> Vec<String> {
    vec![
        // VCS push — both the bare command and the trailing-args form.
        "Bash(jj git push)".to_owned(),
        "Bash(jj git push:*)".to_owned(),
        "Bash(git push)".to_owned(),
        "Bash(git push:*)".to_owned(),
        // gh PR mutations — creation, merge, close, edit, comments, reviews.
        "Bash(gh pr create)".to_owned(),
        "Bash(gh pr create:*)".to_owned(),
        "Bash(gh pr merge)".to_owned(),
        "Bash(gh pr merge:*)".to_owned(),
        "Bash(gh pr close)".to_owned(),
        "Bash(gh pr close:*)".to_owned(),
        "Bash(gh pr edit)".to_owned(),
        "Bash(gh pr edit:*)".to_owned(),
        "Bash(gh pr comment)".to_owned(),
        "Bash(gh pr comment:*)".to_owned(),
        "Bash(gh pr review)".to_owned(),
        "Bash(gh pr review:*)".to_owned(),
        // gh issue mutations — these workers should never file or update issues.
        "Bash(gh issue create)".to_owned(),
        "Bash(gh issue create:*)".to_owned(),
        "Bash(gh issue comment)".to_owned(),
        "Bash(gh issue comment:*)".to_owned(),
        "Bash(gh issue close)".to_owned(),
        "Bash(gh issue close:*)".to_owned(),
        "Bash(gh issue edit)".to_owned(),
        "Bash(gh issue edit:*)".to_owned(),
        // cube pr operations — Boss's PR management helper.
        "Bash(cube pr)".to_owned(),
        "Bash(cube pr:*)".to_owned(),
    ]
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

/// Filename of the deterministic pre-push checkleft gate script. Written
/// next to the worker settings file (same dir, shared fate) and invoked
/// by the `PreToolUse` hook with its absolute path.
const CHECKLEFT_PUSH_GUARD_SCRIPT_NAME: &str = "boss-checkleft-push-guard.py";

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

/// Deterministic pre-push checkleft gate, run as a `PreToolUse` hook on
/// every Bash tool call for a standard (implementation) worker.
///
/// The whole worker fleet pushes with jj, and `jj git push` is a native
/// implementation that does NOT run git's `pre-push` hook — so an
/// installed git hook is inert for workers. This script restores the
/// gate at the harness layer: it inspects the Bash command, and when the
/// command is a push (`jj git push` or `git push`) it runs the repo's
/// checkleft against the outgoing changes *before* the push is allowed.
/// If checkleft reports errors the push is blocked and the findings (plus
/// the `BYPASS_` guidance) are echoed back so the worker can act.
///
/// All policy lives in checkleft: the script shells out and trusts the
/// exit code (0 = allow, non-zero = block). It is fail-open by
/// construction — a non-push command, a repo with no checkleft binary
/// (e.g. no `bin/checkleft` and none on PATH), or any error
/// resolving/running checkleft all *approve* — so the gate can never
/// wedge a session; its only deterministic action is to block a push that
/// checkleft itself rejected. checkleft's own "no CHECKS.yaml → exit 0"
/// behaviour means repos without convention checks are transparently
/// allowed.
///
/// The checkleft binary is resolved from (in order) `BOSS_CHECKLEFT_BIN`
/// (an override used by tests), `<repo-root>/bin/checkleft` (the
/// repobin-installed path), then a `checkleft` on `PATH`.
const CHECKLEFT_PUSH_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Deterministic pre-push checkleft gate (Claude Code PreToolUse hook).

Boss workers push with jj. `jj git push` is a native implementation that does
not run git's pre-push hook, so an installed git hook is inert for the worker
fleet. This hook restores the gate at the harness layer: it inspects every Bash
command and, when the command is a push (`jj git push` or `git push`), runs the
repository's checkleft against the outgoing changes before the push proceeds.
If checkleft reports errors the push is blocked and the findings (plus bypass
guidance) are echoed back so the worker can fix them or add a BYPASS_ directive.

All policy lives in checkleft: this script shells out and trusts the exit code
(0 = allow, non-zero = block). It is fail-open by construction -- a non-push
command, a repo with no checkleft binary, or any error resolving/running
checkleft all approve -- so the gate can never wedge a session; its only
deterministic action is to block a push that checkleft itself rejected.

The PreToolUse payload arrives as JSON on stdin; a decision JSON is written to
stdout. The checkleft binary is resolved from (in order) the BOSS_CHECKLEFT_BIN
env var, `<repo-root>/bin/checkleft` (the repobin-installed path), and a
`checkleft` on PATH.
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys

# Warm-cache checkleft runs are seconds; cap the wait so a wedged checkleft can
# never hang a push attempt. On timeout we fail open (approve) rather than
# strand the session -- the cube verb gates are the belt for that rare case.
CHECKLEFT_TIMEOUT_SECONDS = 240

ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
DELIMS = {"&&", "||", ";", "|", "&"}


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def command_groups(command):
    try:
        tokens = shlex.split(command, posix=True)
    except Exception:
        tokens = command.split()
    groups = []
    cur = []
    for tok in tokens:
        if tok in DELIMS:
            if cur:
                groups.append(cur)
            cur = []
        else:
            cur.append(tok)
    if cur:
        groups.append(cur)
    return groups


def is_push_command(command):
    # shlex tokenisation means a push phrase inside a quoted argument (a commit
    # message, a --body string) is a single token and never matches, so
    # `jj describe -m "git push the fix"` is correctly not treated as a push.
    for group in command_groups(command):
        i = 0
        while i < len(group) and ENV_ASSIGN_RE.match(group[i]):
            i += 1
        rest = group[i:]
        if not rest:
            continue
        prog = os.path.basename(rest[0])
        if prog == "jj":
            for j in range(1, len(rest) - 1):
                if rest[j] == "git" and rest[j + 1] == "push":
                    return True
        elif prog == "git":
            if "push" in rest[1:]:
                return True
    return False


def find_repo_root(start):
    cur = os.path.abspath(start)
    while True:
        if os.path.isdir(os.path.join(cur, ".jj")) or os.path.exists(os.path.join(cur, ".git")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            return os.path.abspath(start)
        cur = parent


def resolve_checkleft(root):
    override = os.environ.get("BOSS_CHECKLEFT_BIN", "").strip()
    if override:
        return override if os.path.exists(override) else None
    candidate = os.path.join(root, "bin", "checkleft")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return candidate
    return shutil.which("checkleft")


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit("approve")
    if not isinstance(payload, dict):
        emit("approve")
    if (payload.get("tool_name") or "") != "Bash":
        emit("approve")
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        emit("approve")
    command = tool_input.get("command")
    if not isinstance(command, str) or not command.strip():
        emit("approve")
    if not is_push_command(command):
        emit("approve")

    cwd = payload.get("cwd") or os.getcwd()
    root = find_repo_root(cwd)
    checkleft = resolve_checkleft(root)
    if not checkleft:
        # No checkleft available -> nothing to enforce (repo may not use it).
        emit("approve")

    try:
        proc = subprocess.run(
            [checkleft, "run"],
            cwd=root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=CHECKLEFT_TIMEOUT_SECONDS,
        )
    except Exception:
        # Could not run checkleft (timeout / exec error) -> fail open.
        emit("approve")

    if proc.returncode == 0:
        emit("approve")

    findings = (proc.stdout or "").strip()
    extra = (proc.stderr or "").strip()
    # Empty stdout with non-empty stderr means checkleft exited nonzero before
    # producing any findings -- this is an internal/operational error (e.g. a
    # VCS detection failure), not a policy violation. Use a clearly distinct
    # message so users don't try to fix policy or reach for BYPASS unnecessarily.
    if not findings:
        reason = (
            "Push blocked: checkleft internal error — this is "
            "a bug, not a policy violation. Please report it.\n\n"
            + extra
        )
    else:
        reason = (
            "Push blocked: checkleft found errors that must be fixed before "
            "pushing to GitHub.\n\n"
            + findings
            + "\n\nFix the findings above and retry the push. If a finding is a "
            "genuine false positive, add a `BYPASS_<CHECK_NAME>=<reason>` line to "
            "your commit message (jj describe) or the PR description, then retry. "
            "Do not bypass without a real justification."
        )
    emit("block", reason)


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

/// Absolute path to the deterministic pre-push checkleft gate script.
/// Shared across every session (the script resolves the repo + checkleft
/// binary at invocation time), so it lives once in the
/// [`worker_settings_dir`] alongside the per-workspace settings files.
pub fn checkleft_push_guard_script_path() -> PathBuf {
    worker_settings_dir().join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME)
}

/// Write the [`CHECKLEFT_PUSH_GUARD_SCRIPT`] into `dir`, creating it if
/// needed. Idempotent: overwrites any existing copy with the current
/// source so a stale script from an older engine build is refreshed.
/// Returns the path written.
pub fn ensure_checkleft_push_guard_script_in(dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(CHECKLEFT_PUSH_GUARD_SCRIPT_NAME);
    std::fs::write(&path, CHECKLEFT_PUSH_GUARD_SCRIPT)?;
    Ok(path)
}

/// Substring that marks a hook command as engine-injected. Every
/// `boss-event` hook command inline-prefixes `BOSS_RUN_ID=...` (see
/// [`settings_value`]); a per-run identity like this is never checked
/// into a repo's tracked `.claude/settings.json`, so it is a reliable
/// signature for a *leaked* engine hook left inside a reused workspace.
const LEAKED_HOOK_SIGNATURE: &str = "BOSS_RUN_ID=";

/// Remove stale engine-injected hook registrations from any
/// `.claude/settings.json` / `.claude/settings.local.json` left inside
/// the workspace tree.
///
/// Background: the engine writes worker settings *outside* the
/// workspace (see module docs and [`worker_settings_path`]) and points
/// the session at them via `claude --settings`. But cube workspaces are
/// warm caches reused across executions, and `.claude/` is gitignored
/// (`*`), so a `settings.json` written into the tree by a pre-fix engine
/// build survives `jj new main` indefinitely. Claude merges hooks from
/// that in-tree file *and* the engine's `--settings` file, so the
/// `boss-event` Stop hook fires twice — once with the live `BOSS_RUN_ID`
/// and once with the stale prior one. The stale Stop event then leaks
/// into the engine's completion path, mis-attributing / preempting the
/// live execution's completion and leaving its task stuck in `Doing`
/// with the agent un-reaped.
///
/// Best-effort: this strips only hook groups whose command carries the
/// [`LEAKED_HOOK_SIGNATURE`], leaving any legitimately repo-tracked
/// content (deny rules, non-boss hooks) intact. IO / parse failures are
/// logged and skipped — a malformed user file must never abort worker
/// setup, and a settings file with no leaked hooks is left byte-for-byte
/// untouched.
pub fn purge_leaked_worker_hooks(workspace_path: &Path) {
    let claude_dir = workspace_path.join(".claude");
    for name in ["settings.json", "settings.local.json"] {
        let path = claude_dir.join(name);
        if let Err(err) = purge_leaked_hooks_in_file(&path) {
            tracing::warn!(
                path = %path.display(),
                ?err,
                "worker setup: failed to purge leaked boss hooks from in-workspace settings; leaving file untouched",
            );
        }
    }
}

/// Strip leaked `boss-event` hook groups from a single settings file.
/// Removes the file entirely if nothing meaningful remains. Returns
/// `Ok(())` (a no-op) when the file is absent, is not JSON, or carries
/// no leaked-hook signature.
fn purge_leaked_hooks_in_file(path: &Path) -> io::Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    // Cheap pre-check: only touch files that actually carry a leaked
    // hook. A clean repo settings.json is left exactly as-is.
    if !raw.contains(LEAKED_HOOK_SIGNATURE) {
        return Ok(());
    }
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                %err,
                "worker setup: in-workspace settings carries BOSS_RUN_ID but is not parseable JSON; leaving untouched",
            );
            return Ok(());
        }
    };
    if !strip_leaked_hooks(&mut value) {
        return Ok(());
    }
    // A file that was *only* leaked engine config (empty after the
    // strip) is removed so the no-settings-in-tree invariant is fully
    // restored. Anything else is rewritten with the leak stripped.
    if value.as_object().is_some_and(serde_json::Map::is_empty) {
        std::fs::remove_file(path)?;
        tracing::info!(
            path = %path.display(),
            "worker setup: removed stale engine-only settings file from reused workspace tree",
        );
        return Ok(());
    }
    let serialized = serde_json::to_string_pretty(&value).expect("settings JSON value is always serializable");
    std::fs::write(path, serialized)?;
    tracing::info!(
        path = %path.display(),
        "worker setup: stripped stale boss-event hooks from in-workspace settings file",
    );
    Ok(())
}

/// Remove hook groups carrying the [`LEAKED_HOOK_SIGNATURE`] from the
/// `hooks` map of a settings value. Drops an event key when its array
/// becomes empty, and the whole `hooks` key when no events remain.
/// Returns true if anything was removed.
fn strip_leaked_hooks(value: &mut serde_json::Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return false;
    };
    let mut changed = false;
    let event_keys: Vec<String> = hooks.keys().cloned().collect();
    for event in event_keys {
        let Some(groups) = hooks.get_mut(&event).and_then(|g| g.as_array_mut()) else {
            continue;
        };
        let before = groups.len();
        groups.retain(|group| !hook_group_is_leaked(group));
        if groups.len() != before {
            changed = true;
        }
        if groups.is_empty() {
            hooks.remove(&event);
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    changed
}

/// A hook group `{matcher, hooks: [{type, command}, ...]}` is leaked if
/// any of its inner command strings carries the signature.
fn hook_group_is_leaked(group: &serde_json::Value) -> bool {
    group.get("hooks").and_then(|h| h.as_array()).is_some_and(|inner| {
        inner.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains(LEAKED_HOOK_SIGNATURE))
        })
    })
}

/// Absolute path to Claude Code's user-global config file
/// (`~/.claude.json`). This is the store Claude consults for the
/// first-run folder-trust dialog (the per-project `hasTrustDialogAccepted`
/// flag); it is *separate* from the `--settings` file the engine passes.
///
/// Resolved from `$HOME` (the convention used elsewhere in the engine,
/// e.g. [`crate::config`]). Returns `None` if `HOME` is unset, in which
/// case pre-trust is skipped and the worker falls back to today's
/// behaviour (it may block on the dialog).
fn claude_global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude.json"))
}

/// Pre-accept Claude Code's first-run folder-trust dialog for
/// `workspace_path` so a headless Boss worker never blocks on it.
///
/// Boss/cube materialises the workspace itself, specifically for the
/// agent to work in, so it is trusted by construction — there is no
/// untrusted third-party content. But the folder-trust dialog ("Is this
/// a project you created or one you trust?") is a *separate* first-run
/// gate that `--permission-mode auto` and the `--settings` file do not
/// cover: it is keyed off the per-project `hasTrustDialogAccepted` flag
/// in Claude's user-global `~/.claude.json`, and is evaluated before any
/// repo- or `--settings`-supplied config. A headless worker has no human
/// to press "1", so it wedges here. We carry the trust intent through by
/// seeding that flag for the workspace path before `claude` launches.
///
/// Best-effort: failure to pre-trust is logged and swallowed (it only
/// costs the worker today's behaviour, not correctness), so it never
/// aborts worker setup.
pub fn pre_trust_workspace(workspace_path: &Path) {
    let Some(config_path) = claude_global_config_path() else {
        tracing::warn!(
            workspace = %workspace_path.display(),
            "worker setup: HOME unset, cannot pre-trust workspace in ~/.claude.json; worker may block on the folder-trust dialog",
        );
        return;
    };
    if let Err(err) = pre_trust_workspace_in(&config_path, workspace_path) {
        tracing::warn!(
            config = %config_path.display(),
            workspace = %workspace_path.display(),
            ?err,
            "worker setup: failed to pre-trust workspace in ~/.claude.json; worker may block on the folder-trust dialog",
        );
    }
}

/// Set `projects[<workspace_path>].hasTrustDialogAccepted = true` in the
/// Claude config at `config_path`, preserving every other key.
///
/// - A missing or empty config file is treated as an empty object (fresh
///   install) and created.
/// - A config that already records this workspace as trusted is a no-op:
///   we do not rewrite the file. This matters because `~/.claude.json` is
///   a *shared* file that live `claude` sessions in other workspaces
///   rewrite frequently; cube re-uses a fixed pool of workspaces, so
///   after each is trusted once the engine never touches the file again,
///   keeping the read-modify-write race window to first-spawn-per-workspace.
/// - A config that exists but does not parse as JSON is left **untouched**
///   (we return the parse error rather than clobber the user's file).
/// - The write is atomic (temp file in the same dir + rename) so a
///   concurrent reader never observes a half-written config.
fn pre_trust_workspace_in(config_path: &Path, workspace_path: &Path) -> io::Result<()> {
    let key = workspace_path.display().to_string();

    let mut root: serde_json::Value = match std::fs::read_to_string(config_path) {
        Ok(s) if s.trim().is_empty() => serde_json::json!({}),
        Ok(s) => serde_json::from_str(&s).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e),
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "~/.claude.json is not a JSON object"))?;
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "~/.claude.json `projects` is not an object"))?;
    let entry = projects
        .entry(key)
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "~/.claude.json project entry is not an object",
            )
        })?;

    // Already trusted → no-op. Don't rewrite the shared file.
    if entry.get("hasTrustDialogAccepted").and_then(serde_json::Value::as_bool) == Some(true) {
        return Ok(());
    }
    entry.insert("hasTrustDialogAccepted".to_owned(), serde_json::Value::Bool(true));
    // Claude pairs the trust flag with this counter; seed it if absent so
    // the onboarding flow doesn't re-prompt either. Leave any existing
    // value untouched.
    entry
        .entry("projectOnboardingSeenCount")
        .or_insert_with(|| serde_json::Value::from(0));

    let serialized = serde_json::to_string_pretty(&root).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(config_path, serialized.as_bytes())
}

/// Write `contents` to `path` atomically: write a sibling temp file and
/// rename it over `path`. The rename is atomic on POSIX, so a concurrent
/// reader sees either the old or the new file, never a partial write.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    // Tag the temp name with the pid so concurrent engine writes (should
    // not happen — one engine — but cheap insurance) don't collide.
    let tmp = dir.join(format!(".claude.json.boss-tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
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

    // Reused (warm-cached) workspaces can carry a stale `.claude/
    // settings.json` written into the tree by an older engine build.
    // Claude merges hooks from it *and* the engine's `--settings`
    // file, so the `boss-event` Stop hook would fire twice — once with
    // the live `BOSS_RUN_ID` and once with the stale prior one. Purge
    // the leak before the worker session reads its settings.
    purge_leaked_worker_hooks(&input.workspace_path);

    // Pre-accept Claude Code's first-run folder-trust dialog for this
    // workspace. Boss/cube created the workspace for the agent, so it is
    // trusted by construction; without this the headless worker wedges on
    // the dialog (no human to press "1"). Best-effort — see
    // [`pre_trust_workspace`].
    pre_trust_workspace(&input.workspace_path);

    let claude_md_path = claude_dir.join("CLAUDE.md");
    let gitignore_path = claude_dir.join(".gitignore");

    std::fs::write(&claude_md_path, render_claude_md(input))?;
    std::fs::write(&gitignore_path, CLAUDE_DIR_GITIGNORE)?;

    let settings_path = worker_settings_path(&input.workspace_path);
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
        // The PreToolUse gate scripts live next to the settings file
        // (same dir, shared fate) and the hooks invoke them by absolute
        // path; write them whenever we materialise the settings file.
        ensure_path_guard_script_in(parent)?;
        ensure_checkleft_push_guard_script_in(parent)?;
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
    format!("{}{}{}", &command[..open_pos], new_escaped, &command[close_pos + 1..])
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
    // pointing at the gate scripts in it. Refresh them (TMPDIR churn or an
    // older engine build may have removed/staled them) so the gates
    // survive an engine restart, not just a fresh spawn.
    if let Err(err) = ensure_path_guard_script_in(settings_dir) {
        tracing::warn!(
            dir = %settings_dir.display(),
            ?err,
            "failed to refresh path-guard script during settings heal",
        );
    }
    if let Err(err) = ensure_checkleft_push_guard_script_in(settings_dir) {
        tracing::warn!(
            dir = %settings_dir.display(),
            ?err,
            "failed to refresh checkleft push-guard script during settings heal",
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
fn heal_single_settings_json(settings_path: &Path, new_boss_event_path: &Path) -> io::Result<bool> {
    let content = match std::fs::read_to_string(settings_path) {
        Ok(c) => c,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };

    let mut parsed: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut changed = false;

    if let Some(hooks) = parsed.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_name, entries) in hooks.iter_mut() {
            if let Some(arr) = entries.as_array_mut() {
                for entry in arr.iter_mut() {
                    if let Some(inner_hooks) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        for inner in inner_hooks.iter_mut() {
                            if let Some(cmd) = inner.get("command").and_then(|c| c.as_str()).map(str::to_owned) {
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
        let new_content =
            serde_json::to_string_pretty(&parsed).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(settings_path, new_content)?;
    }

    Ok(changed)
}

#[cfg(test)]
#[path = "worker_setup_tests.rs"]
mod tests;
