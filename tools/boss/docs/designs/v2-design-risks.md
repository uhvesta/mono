# Boss V2 — Design Risks

## Purpose

The Boss V2 plan ([swiftui-boss-v2.md](../plans/active/swiftui-boss-v2.md))
introduces several large architectural decisions at once. Some of those
decisions are easy to reverse later; others lock in months of work and a
specific failure surface.

This document tracks the load-bearing decisions — the ones where picking wrong
is expensive — and gives each one a small, targeted exploration that resolves
the unknowns before we commit.

The intended workflow is:

1. List the risk and why it matters.
2. List the candidate options.
3. Identify hard constraints that disqualify options outright.
4. Identify the decisive unknowns — what we don't know that would tip the
   decision.
5. Define the smallest POC or investigation that resolves those unknowns.
6. Record the decision and its rationale.

We work through risks one at a time. The plan in `swiftui-boss-v2.md` should
be revised once the major risks have decisions.

## Risk template

```
## RN: <short title>

### Why it matters
<what becomes hard to reverse if we choose wrong>

### Options
<candidate paths>

### Hard constraints
<things that disqualify an option outright; "no" here ends the conversation
for that option>

### Decision dimensions
<axes we trade off when no option is disqualified>

### Decisive unknowns
<what we don't currently know that would tip the choice>

### Proposed exploration
<smallest experiment, conversation, or document review that resolves the
unknowns>

### Resolution criteria
<how we know we have an answer>

### Decision
<filled in last>
```

## R1: Worker shape — terminal-embed vs native chat

### Why it matters

Every other V2 decision sits on top of this one:

- the control surface (`bossctl`, probing, capacity)
- worker isolation
- per-worker workspaces
- crash/resume behavior
- the ongoing rendering surface we own forever

Picking "native chat via SDK/ACP" and then reversing means scrapping a custom
client and a large rendering layer. Picking "terminal-embed via libghostty"
and reversing means swapping pane contents — much cheaper.

The decision is also the one most exposed to external constraints (company
policy, upstream API stability) that we do not fully control.

### Options

1. **Terminal-embed.** Each worker is an unmodified `claude` session running
   inside an embedded `libghostty` surface. Boss is also a `claude` session in
   its own pane. We inherit Claude Code's entire product surface (slash
   commands, tool rendering, plan mode, hooks, todos, MCP UX, paste, image
   attach, `/memory`, etc.). Control is via keystroke injection plus an
   out-of-band channel (hooks, session JSONL, or screen scrape).

2. **Native chat via SDK/ACP.** Each worker is a Claude session driven by our
   own client (Claude SDK or an ACP client we own). We render messages and
   tool calls in SwiftUI. Control is via structured API calls; observation is
   via structured events.

3. **Hybrid.** Terminal-embed for fidelity, with a structured side-channel
   (e.g. Claude Code hooks) to give Boss orchestration-grade signal without
   scraping the screen.

### Hard constraints

- **Company tool/agent-client policy.** ACP and SDK-based workers require us
  to run a custom Claude client that implements its own tool handlers (file
  edit, bash, MCP, etc.). If the user's company restricts custom Claude
  clients to the official `claude` binary, options 2 and 3-via-SDK are
  disqualified outright.
- **`libghostty` build viability.** Embedding requires a private C API
  (`ghostty_surface_new`), Zig 0.15 toolchain, and the macOS Metal toolchain.
  The existing prototype at `tools/ghostty-proto/` proves this is buildable
  today, but upstream churn could break us. This is a soft constraint, not a
  hard one — it is a maintenance cost, not a disqualifier.
- **Claude Code stability.** Terminal-embed inherits whatever Claude Code
  ships, including breaking UX changes. Hooks are a documented extension
  point; screen output is not.

### Decision dimensions

| Axis | Terminal-embed | Native chat (SDK/ACP) |
|---|---|---|
| Rendering surface inherited | Everything Claude Code ships, free | Almost nothing; we reimplement each tool's UI |
| Control fidelity | Keystroke injection; observation via hooks/scrape/JSONL | Structured events both directions |
| Failure recovery | `claude --resume` exists; reattach a terminal | We own resume entirely |
| Boss-side fidelity | Strong — Boss is a real Claude TUI | Weak unless we build a lot |
| Worker-side fidelity | Strong, but workers are dense TUI panes | Can be nicer if we build the chrome |
| Company policy fit | Runs the approved binary unchanged | Requires custom-client approval |
| Reversibility | Easy to swap pane contents to chat later | Hard — humans grow to depend on chat affordances |
| POC cost | Existing 5-pane prototype to build on | None yet; substantial upfront |

### Decisive unknowns

1. **Policy.** Does the user's company allow a custom Claude client (SDK or
   ACP) running its own tool handlers, or only the official `claude` binary?
   This is a yes/no question with a single answer; until it's answered, we
   cannot evaluate option 2 honestly.

2. **Hook coverage.** Do Claude Code's hooks (`PreToolUse`, `PostToolUse`,
   `Stop`, `UserPromptSubmit`, etc.) carry enough signal for Boss-side
   orchestration — at minimum: turn-started, turn-finished, tool-invoked,
   blocked-on-prompt, exited? If yes, terminal-embed gets structured events
   without sacrificing TUI fidelity.

3. **Session JSONL viability.** Claude Code writes session transcripts to
   disk. Are they stable enough in format and timing to use as a secondary
   structured channel for richer observation (e.g. extracting probe
   responses)? This is a fallback if hooks under-cover.

4. **Boss product gap.** What specifically does Boss want to give the human
   that Claude Code's TUI doesn't already give? If most of the wanted
   affordances are *chrome around* a terminal (work-item context next to the
   pane, status chips, fixed grid layout, focus management, keyboard
   shortcuts to switch panes), terminal-embed is sufficient. If the wanted
   affordances are *inside* the conversation (collapsible tool calls,
   click-to-jump-to-file, native diff viewer, native attachment UX), that
   pushes toward native chat.

### Proposed exploration

Done in this order, stopping early if a hard constraint resolves the
question:

1. **Resolve policy.** A short conversation or document check answers
   unknown 1. Output: written note in this doc — "custom clients allowed
   under conditions X" or "official binary only."

2. **Hook coverage POC.** Extend the existing `tools/ghostty-proto/`
   prototype with a Claude Code hook configuration that writes
   `PreToolUse` / `PostToolUse` / `Stop` / `UserPromptSubmit` events to a
   Unix domain socket. Have a small listener log them. Run a typical task
   in the embedded pane and verify the event stream is sufficient to
   answer:

   - is the worker currently in a turn?
   - did the worker stop, and why?
   - is the worker awaiting input?
   - did the worker invoke a destructive tool?

   Output: a written assessment in this doc with the event log and any
   gaps.

3. **Session JSONL inspection.** Read the JSONL files Claude Code writes
   for the same task. Note format stability, timing, and whether they
   contain anything hooks miss (e.g. the assistant's last message text for
   probe-style queries). Output: a paragraph in this doc.

4. **Boss product gap inventory.** List 5–10 concrete affordances we want
   in Boss V2 that go beyond what `claude` already shows. For each, mark
   "chrome around the pane" vs "inside the conversation." Output: a short
   table in this doc.

5. **Decide.** With 1–4 in hand, write the decision and rationale below.

### Findings

Distilled results of running the proposed exploration above. Full POC
artefacts live outside this doc to keep it navigable.

#### On unknown 2 — hook coverage

POC at `/tmp/boss-hook-poc-001/` against `claude 2.1.119`. A
`.claude/settings.json` subscribed to all hook events; each fired event
was appended to a JSONL file for inspection.

- **Q1 (in a turn?)**: solid. `UserPromptSubmit` → `Stop` window is
  authoritative.
- **Q2 (stopped, why?)**: `Stop` fires reliably with
  `last_assistant_message`, but the documented `stop_reason` field is
  **not present** in v2.1.119 payloads. "Why" must be derived by
  correlating unmatched `PreToolUse` ids and parsing the last assistant
  message. Workable but fragile.
- **Q3 (awaiting input?)**: **gap**. The `Notification` hook (the
  documented channel for `permission_prompt` / `idle_prompt`) does not
  fire in `claude -p` non-interactive flow. The denied-tool path is
  silent: only the post-hoc `permission_denials[]` array in the final
  result reveals it. In live interactive sessions (libghostty PTY) it
  should fire, but for any headless Boss orchestration this is missing.
- **Q4 (destructive tool?)**: solid. `PreToolUse` carries the full
  `tool_input` (including raw `command` / `file_path`) **before** the
  call runs, so Boss can pattern-match and even short-circuit via the
  hook exit code.

Recommended recipe: launch workers with
`--output-format stream-json --input-format stream-json --include-hook-events`
and treat the stream as the orchestration channel.
Hooks-as-side-channel-file is a viable alternative when stream-json is
awkward.

**Verdict**: hooks are sufficient for Q1, Q2, and Q4. Q3 needs a
separate strategy — most likely the stream-json `Notification` event in
interactive mode, with screen-scrape as a defensive fallback.

#### On unknown 3 — session JSONL viability

Claude Code writes per-session JSONL transcripts to disk:

- Transcripts: `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`
  (encoded-cwd replaces `/` with `-`).
- Subagents: `<sessionId>/subagents/agent-<id>.jsonl`.
- **Live process registry**: `~/.claude/sessions/<pid>.json` maps pid
  → `sessionId` + `cwd` + `status` (`busy`/`idle`). This is the
  discovery key for matching a running pane to its transcript.
- File mode 0600.

Format is JSONL with explicit per-record `version` (currently
`2.1.119`). Common envelope: `type`, `timestamp` (ISO-8601 ms),
`sessionId`, `uuid`, `parentUuid`, `cwd`, `gitBranch`. Event types
observed include `user`, `assistant`, `system`, `attachment` (sub-types
include `task_reminder`, `auto_mode`, `queued_command`), `last-prompt`,
`permission-mode`, `queue-operation`, `pr-link`,
`file-history-snapshot`.

**Write timing**: per-event flush, not turn- or session-batched. mtime
moves within seconds of activity. Suitable for `kqueue` /
`DispatchSource` real-time tail.

Coverage that JSONL provides and hooks structurally cannot:

- Raw assistant text and thinking blocks (hooks see tool boundaries,
  not model prose).
- Full tool result bodies (`Edit` `structuredPatch`, `Bash` stdout/
  stderr, `Agent` `usage` / `toolStats`).
- Todo list state via `task_reminder` attachments (the legacy
  `~/.claude/todos/` is now stale).
- Plan / permission-mode transitions, queue ops, PR links,
  slash-command echoes.
- Subagent transcripts as separate, parseable files.

**Verdict**: viable as the primary observation channel. Boss V2 should:

1. Enumerate `~/.claude/sessions/*.json` to discover live Claude
   processes and resolve their transcript paths.
2. Tail with `DispatchSource` (per-event flush makes this real-time).
3. Parse line-by-line, dispatch on `type`; tolerate unknown types and
   `version` ranges.
4. Use hooks only where pre-decision matters (PreToolUse gating,
   stop-on-error). For pure observation, JSONL covers strictly more
   ground.

Risks to flag: undocumented format (pin parser to known `version`
ranges), no rotation (long sessions produce large files; tail-from-end
is fine, full-replay is O(file)), and privacy (transcripts contain raw
prompts and tool outputs — treat as sensitive).

#### On unknown 4 — Boss product gap

Inventory of 15 candidate Boss-only affordances, each classified as
chrome-around-the-pane (terminal-embed sufficient) or
inside-the-conversation (requires rendering control):

| Affordance | Category | Why |
|---|---|---|
| Fixed 3×3 Boss + 8 grid | chrome-around | Window-manager concern; outside any single session |
| Per-pane work-item context strip | chrome-around | Reads engine state, renders adjacent to the pane |
| Cross-worker capacity / status chips | chrome-around | Header widget, no message rendering needed |
| Keyboard focus router (cmd-1..9) | chrome-around | OS-level focus routing |
| Attention / alert surfacing | chrome-around | Triggered at engine layer; surfaced via native notifications |
| Boss-driven worker dispatch palette | chrome-around | Launcher, agnostic to pane type |
| Cross-worker search | hybrid | Ingestion works either way; previews + jump-to-message cleaner with native chat |
| Work-item ↔ session binding & auto-resume | chrome-around | Engine bookkeeping; pane renders whatever it's handed |
| Inline work-item linking in messages (`#task_123` chips) | inside-conversation | Requires intercepting message render |
| Native diff viewer for tool calls | inside-conversation | Needs structured `tool_use` payload + render control |
| Click-to-jump-to-file from tool calls | inside-conversation | Clickable affordance inside the message stream |
| Collapsible / summarized tool calls | inside-conversation | Selective render; only possible if app owns the view |
| Custom approval / permission UI | inside-conversation | Permission requests must be intercepted, not consumed by TUI |
| Boss-authored worker steering messages with distinct provenance | inside-conversation | Requires sender styling control |
| Aggregate Boss dashboard view | hybrid | Chrome is fine; per-row legibility benefits from message structure |

Tally: 7 chrome-around, 6 inside-conversation, 2 hybrid. Even split by
count, but the chrome-around items are the **load-bearing scaffolding**
of a multi-agent cockpit (grid, focus, alerts, capacity, dispatch,
binding, context strip) — they are what differentiate Boss from "9
terminal tabs" and they all work with terminal-embed. The
inside-conversation items are the **daily-use quality ceiling**
(collapsible tool calls, native diffs, custom permission sheets) —
without them, a human watching 8 raw TUI streams is reading dense ANSI.

**Verdict**: terminal-embed is sufficient for an MVP and delivers all
the load-bearing scaffolding. The inside-conversation list should be
treated as the forcing function for a possible later migration to
native chat — and the engine / observation-channel boundary should be
designed so that swap is a frontend change, not a re-architecture.

### Resolution criteria

We have an answer when:

- the policy question has a definitive yes/no
- the hook + JSONL channels have been characterized concretely (not
  speculatively)
- the Boss product-gap inventory makes it clear whether the wanted
  affordances are chrome-around or inside-the-conversation
- a written decision can point at those facts as its rationale

### Decision

_Pending exploration._

## R4: Per-worker workspace and concurrency

### Why it matters

Eight concurrent worker Claude sessions cannot share a single repo
checkout. Without per-worker isolation they collide on the working
tree, build outputs, git/jj state, and uncommitted changes. The
multi-agent architecture collapses to "one worker at a time" without a
workspace strategy.

The decision interacts with worker startup latency, Bazel / pnpm cache
hit rate, setup-state caching (decoded secrets, generated code,
`pnpm install`), and PR lifecycle integration.

### Options

1. **Fresh clone per task.** Trivially safe; clone time on every task,
   no cache reuse, expensive setup re-runs every time.
2. **Git / jj worktree per worker.** Cheap to create, shares object
   store; still re-runs setup per worktree, jj workspace tooling is
   rougher.
3. **Reusable workspace pool with leases (cube).** Long-lived
   workspace directories reset to clean `main` between leases; build
   outputs and setup state preserved. Designed and partially implemented
   at `tools/cube/` with a thorough design doc
   (`tools/cube/docs/main.md`). Aligned with the user's existing
   `~/Documents/dev/workspaces/` convention.

### Hard constraints

- Must work with `jj` (per global CLAUDE.md preference).
- Must preserve Bazel / pnpm cache reuse — cold builds in this monorepo
  are expensive.
- Must compose with the existing `gh pr create` / `jj git push` flow.
- Must survive worker crash without manual cleanup.

### Working decision

**Use cube as the workspace layer.** The reusable-workspace-pool model
is the only option that amortizes setup + build cache cost across
tasks, and it's already substantially designed and partially
implemented at `tools/cube/` (~1700 LOC across `app.rs`, `cli.rs`,
`store.rs`, etc.).

Lease lifetime is **per task**, not per worker-slot: workers are
decoupled from any specific workspace and acquire one each time
they're assigned work. Boss-engine calls `cube workspace lease` on
assignment; the leased path becomes the worker's `cwd` when launching
`claude`. On task end (PR merged, abandoned, or worker crash),
Boss-engine calls `cube workspace release`.

Pool size: at least `worker_count + headroom` per repo. With 8 workers,
12 workspaces per repo is a reasonable starting size.

### Decisive unknowns

1. **Cube readiness.** ~1700 LOC exists but functional coverage of the
   documented commands is unknown. Required: an audit listing which
   commands work today (`workspace lease`, `workspace release`,
   `change create`, `pr sync`, `pr merge`) and a concrete gap list.
2. **Lease lifetime boundary cases.** Per-task is the working
   decision, but: what if a task spans multiple PRs? What if a PR is
   merged and a follow-up bug forces re-work? Does Boss prefer the
   same workspace for warm cache, or any free one?
3. **Boss / cube / worker integration shape.** Library call vs
   subprocess `--json` invocation. Working preference: subprocess JSON
   for looser coupling and easier testability. Cube already exposes
   `--json` per its design doc.
4. **jj-vs-git in workers.** Claude Code defaults to `git` for VCS
   operations; cube and the user prefer `jj`. Options: a
   workspace-local `CLAUDE.md` instructing jj-first usage, an alias
   shim, or a hook intercept. Default recommendation: workspace-local
   `CLAUDE.md` refreshed on each lease.
5. **Cross-product workspaces.** Boss's work taxonomy spans multiple
   products. Each product has its own cube pool; Boss must route lease
   requests to the right pool by `product_id`.
6. **Setup state freshness.** Cube caches `pnpm install` and similar
   across leases; when does the fingerprint get re-validated? Per-lease
   check vs scheduled refresh.

### Proposed exploration

1. **Cube audit.** Read `tools/cube/src/` end-to-end. Run cube against
   a real mono workspace through the basic flow: `workspace lease` →
   edit → `change create` → `pr sync` → `pr merge` →
   `workspace release`. Document what works, what's stubbed, what
   fails.
2. **Lease lifetime boundary cases.** Half-page write-up resolving the
   open cases in unknown 2.
3. **Integration sketch.** One-page worked example: task assigned →
   Boss-engine invokes cube via subprocess JSON → leased path passed
   to worker spawn → worker launches `claude` in that directory → on
   task end, Boss-engine releases.
4. **jj-vs-git decision.** Pick one of {workspace-local `CLAUDE.md`,
   alias shim, hook intercept} with rationale.
5. **Smoke test.** Lease a workspace via cube, launch `claude` in that
   directory non-interactively to do a small task, release. Verify the
   leased workspace is a viable Claude Code working directory.

### Findings

Distilled results of running the proposed exploration above. Smoke-test
artefacts live outside this doc.

#### On unknown 1 — cube readiness

Audit + smoke test against cube at `28200da`. `cargo build -p cube`
and `bazel build //tools/cube` both succeed cleanly. The
lease/release/status loop works end-to-end against a throwaway pool
in `/tmp/`. The Claude-in-cube smoke test (`claude -p` inside a
leased dir) wrote only to the leased path — no escape.

Capability table (`tools/cube/src/`):

| Command | State | Ref |
|---|---|---|
| `repo add` / `list` / `info` | IMPLEMENTED | `app.rs:117`, `141`, `159` |
| `workspace lease` | IMPLEMENTED (single-pool, no auto-create, no setup engine, no flock) | `app.rs:185` |
| `workspace release` | IMPLEMENTED (resets via `jj git fetch && jj new main`) | `app.rs:223` |
| `workspace status` | IMPLEMENTED (delegates to `jj status`) | `app.rs:242` |
| `workspace setup` | STUBBED — returns "No setup steps configured" | `app.rs:256` |
| `change *`, `stack *`, `pr *`, `graph`, `doctor` | MISSING — all return `NotImplemented` | `app.rs:271-298` |

SQLite store is real and tested (`store.rs`). No `flock` around
`claim_workspace`; concurrency relies on SQLite atomicity
(`store.rs:199`). The change/PR metadata tables described in the
design doc are absent; only `repos` and `workspaces` exist.

Bugs and gaps surfaced:

- `head_commit` recording is broken — `jj log` template includes the
  graph header (`app.rs:365`); needs `--no-graph -r @`.
- No `--database` CLI flag; out-of-tree callers must set
  `CUBE_DATA_DIR` (`paths.rs:6`). Boss V2 needs to know that.
- `repo add --source` accepts a seed path (`cli.rs:60`) but lease
  never reads it — no auto-create on pool exhaustion.
- Release does not clean up abandoned `jj` changes a worker may have
  created; working copy is clean for the next lease, but history
  accretes.

**Verdict**: cube is **usable today only for the workspace pooling
layer** — exactly what R4's working decision asks of it. Stacked-change
and PR features (`change *`, `stack *`, `pr *`) are entirely unbuilt;
Boss V2 must continue to drive `jj` / `gh` / `git` directly inside
leased workspaces. Gap-fixes to harden the pooling layer for V2, in
priority order: (1) fix the `head_commit` template bug, (2) add a
`--database`/explicit-data-dir CLI flag, (3) add `flock` around
`claim_workspace`, (4) auto-create workspaces from `--source` on pool
exhaustion, (5) implement `workspace setup` so per-repo bootstrap is
cube's job, not Boss's.

#### On unknown 2 — lease lifetime boundary cases

**Recommendation**: keep per-task lease as the primary boundary,
releasing on PR-merge-or-abandon for the current task. Boss passes a
`preferred_workspace_id` hint to `cube workspace lease`; cube prefers
the prior workspace when free, falls back to any free one after a
short wait.

**Rationale**: a task is the smallest stable identity Boss already
tracks (work-taxonomy `tasks.id`), so anchoring leases there avoids
inventing a new lifecycle object. Soft affinity preserves Bazel /
`pnpm` cache warmth without holding a workspace idle across human
review latency, which would starve the pool. Tradeoff: warm-cache hit
rate vs pool utilization — affinity is advisory, never blocking.

Boundary cases handled:

- Stacked PRs in one task: single lease spans the whole stack;
  release only when the *task* terminates.
- Rework after merge / follow-up bugfix on the same `task_id`:
  re-lease with `preferred_workspace_id`; cube's setup-fingerprint
  logic skips redundant provisioning.
- Task split mid-flight: each new task gets its own lease.
- Punted: cross-agent handoff (e.g. reviewer-bot leasing the same
  workspace) — defer until Boss models reviewer agents.
- Punted: explicit pinning across host reboot — rely on cube
  metadata persistence, no Boss-level guarantee.

#### On unknown 3 — Boss / cube / worker integration shape

**Working sketch.** Boss-engine drives cube via subprocess `--json`
invocations from inside the existing `tokio` task that owns each
agent's lifecycle:

```text
boss-engine                         cube                        claude
     |  cube workspace lease           |                              |
     |  --repo mono --task T-184       |                              |
     |  --json --holder boss/agent-7  >|                              |
     |                                 | (lock, reset, setup)         |
     |  <-- {lease_id, workspace_path, |                              |
     |       workspace_id, base_rev,   |                              |
     |       expires_at, setup_status} |                              |
     |                                                                |
     |  spawn claude --cwd <path> --session-id … ------------------>  |
     |  env: BOSS_TASK_ID, BOSS_LEASE_ID, CUBE_LEASE_ID, CUBE_REPO    |
     |                                                                |
     |  on Done / abandon / crash:                                    |
     |  cube workspace release --lease <lease_id> --json              |
     |  <-- {released: true, workspace_id, dirty: false}              |
```

Sample lease JSON:

```json
{ "lease_id": "lse_01HZX...", "workspace_id": "mono-agent-007",
  "workspace_path": "/Users/brianduff/Documents/dev/workspaces/mono-agent-007",
  "base_rev": "main@28200da", "expires_at": "2026-04-26T18:00:00Z",
  "setup_status": "fresh", "holder": "boss/agent-7" }
```

Error handling:

- **Lease fails (pool exhausted)**: engine emits a `WorkError`, marks
  the task `queued_waiting_workspace`, retries on the next workspace
  `released` notification (or polls every 30s).
- **Worker crash mid-task**: the `tokio::spawn` watching the ACP
  child sees process exit / disconnect; engine fires
  `cube workspace release --lease <id> --reason crash --keep-dirty`.
  Cube records dirty state but frees the slot.
- **Boss-engine crash**: lease TTL (30 min, heartbeated by the engine
  via `cube workspace heartbeat`) covers it. On engine restart,
  `cube workspace list --json --holder boss/*` reconciles and
  force-releases orphans.
- **Cube release fails**: engine logs, retains `lease_id` in
  `tasks.lease_id`, surfaces a `lease_release_failed` event so the
  user can `cube doctor` and `cube workspace force-release` manually.

`cube workspace heartbeat`, `--reason crash --keep-dirty`, and
`cube workspace force-release` are not implemented today (per
unknown 1) — these are gap-fixes the integration will require.

#### On unknown 4 — jj-vs-git in workers

**Decision**: workspace-local `CLAUDE.md` written by Boss-engine
post-lease. Path: `<workspace_path>/.claude/CLAUDE.md`. Writer:
Boss-engine, immediately after `cube workspace lease` returns and
before `claude` is spawned. Cube stays VCS-policy-agnostic; the
policy is Boss's.

Sample contents:

```markdown
# Boss worker rules (lease lse_01HZX...)
- This workspace is leased by Boss; do not run `jj git fetch` or
  `jj new main` (cube already did).
- Use `jj` for all VCS work: `jj st`, `jj diff`, `jj new`,
  `jj describe`, `jj squash`, `jj rebase`.
- Do NOT use `git commit / checkout / rebase / reset / stash`.
  Use `jj` equivalents.
- For PR sync and merge use `cube pr sync` / `cube pr merge` (when
  implemented), not raw `gh pr create` / `git push`.
- `git status`, `git log`, `git show` are read-only and OK.
- Honour the repo's own `CLAUDE.md`; the rules above only add
  jj-first VCS conventions.
- Task id: T-184. Lease id: lse_01HZX. Workspace: mono-agent-007.
```

**Idempotency**: overwrite-on-lease. The file is ephemeral; cube
release deletes `.claude/CLAUDE.md` (and `.claude/` if empty) as part
of its cleanup hook. No merge logic.

**Conflict with repo-tracked `CLAUDE.md`** (e.g.
`tools/boss/CLAUDE.md`): both are loaded; the Boss template
explicitly defers to repo rules so layering is additive, not
overriding.

**Why not the alternatives?** An alias shim
(`alias git=jj-git-wrapper`) doesn't survive subshells the agent
spawns through `bash -lc` and silently breaks scripts that need real
git. A pre-tool hook intercept is global, fragile across Claude Code
versions, and hides the rule from the model's reasoning rather than
teaching it.

#### On unknown 5 — cross-product workspaces

**Recommendation**: Boss resolves `work_item.product_id` to a cube
repo-pool id via a single `products.cube_pool_id` column (nullable).
Lease requests dispatch to that pool. A null mapping fails the work
item into a `needs_pool_config` blocked state with an actionable
error, rather than auto-provisioning.

**Rationale**: the work taxonomy already names `product_id` as the
canonical routing key, so a thin lookup is the cheapest correct
routing layer. Refusing to auto-create pools keeps cube's
"operator-curated workspace_root" assumption intact and prevents
Boss from silently creating workspaces under unexpected paths.
Tradeoff: explicit setup friction for new products vs predictable
filesystem layout and no surprise disk usage.

Boundary cases handled:

- Known product (mono, flunge): direct lookup, single hop to cube.
- Unknown / unconfigured product: work item moves to `blocked` with
  `reason = "no cube pool for product <slug>"`; surfaced in the Work
  navigator.
- Product with `repo_remote_url` but no pool: same blocked path;
  Boss does not infer pool config from the URL.
- Multi-repo product (future `related_repo_remote_urls_json`):
  defer; V2 routes only against the primary pool, additional repos
  require their own product entry.
- Punted: cross-product tasks (e.g. a coordinated change across
  mono + flunge) — out of scope for V2.

#### On unknown 6 — setup state freshness

**Recommendation**: re-validate cube setup fingerprints on every
lease acquisition (not on a wall-clock schedule), using cube's
existing per-step invalidation keys plus a Boss-injected
`secrets_version` marker. Secrets steps additionally carry an
absolute 12h max-age TTL that forces re-decode regardless of
fingerprint match.

**Rationale**: per-lease validation costs a hash of declared inputs
(lockfile mtimes, script SHA, version markers) and aligns
invalidation with the moment an agent actually needs a clean state,
so there's no window where a stale workspace is handed out. A
scheduled refresh would either run too often on idle pools or too
rarely on busy ones. Tradeoff: lease latency gains a sub-second
fingerprint check in exchange for eliminating "drifted while idle"
failure modes; secrets get a tighter ceiling because their staleness
is silent and security-relevant.

Boundary cases handled:

- Lockfile change (`pnpm-lock.yaml`, `Cargo.lock`): fingerprint
  diff → rerun `deps` step only.
- Setup script edited on `main`: script SHA differs → rerun affected
  step.
- Secrets rotated externally: Boss bumps `secrets_version`; next
  lease re-decodes. Independent 12h TTL bounds worst-case staleness
  for un-bumped rotations.
- Generated-code step where inputs are hard to enumerate: declared
  as `always` policy in cube config; accepted cost.
- Long-idle workspace: first lease pays full re-validation; no
  background refresh needed.
- Punted: cross-workspace shared caches (e.g. a single `pnpm`
  store) — orthogonal optimization, owned by cube.
- Punted: fingerprint format migration when a setup step's input
  list changes shape — handled by cube's metadata versioning.

### Resolution criteria

- Cube audit committed in writing.
- Integration sketch and lease-lifetime decision committed in writing.
- Smoke test performed and recorded.
- Decisions on integration boundary and jj-vs-git committed in this
  doc.

### Decision

_Working decision recorded above. Pending audit, smoke test, and
unknowns 1–6 to be resolved before the v2 plan adopts cube as a hard
dependency._

## Risk backlog

These risks have been identified but not yet worked through. They are
listed here so we don't lose them; we'll write each one up properly when
we get to it. Order is rough priority, not strict sequence.

- **R2: Worker → Boss structured channel.** Even with R1 resolved, we need
  to pick the concrete mechanism (hooks, JSONL, SDK events, screen
  scrape, or a layered combination) and define what events Boss
  subscribes to. Closely linked to R1.
- **R3: Worker isolation model.** How do we stop a worker from invoking
  Boss-only authority (e.g. `bossctl`) or otherwise escalating? PATH
  separation, auth tokens on the control socket, sandboxing, or
  combinations.
- **R5: Scheduler ownership.** Boss-Claude and the human can both start
  work. Decide which component arbitrates capacity and assignment, and
  what intent API both go through.
- **R6: Crash and resume.** What persists across app restarts? How do we
  reattach to running `claude` sessions, and which state lives in the
  engine vs the app vs Claude Code itself?
- **R7: Review and approval flow.** "Ready for review" is a state in the
  plan but the human's review affordance is undefined. Decide where in
  Work mode this lives.
- **R8: `boss` vs `bossctl` boundary.** `work start` straddles durable
  state and live orchestration. Decide whether they're two CLI personas
  on one backend, or genuinely separate services.
