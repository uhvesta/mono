# Boss

## Overview

Boss is a macOS application that lets one human operate a team of nine
Claude Code sessions: one **Boss session** that plans and coordinates,
and eight **worker sessions** that execute work in parallel against
real repositories. Each worker is an unmodified `claude` binary running
in an embedded `libghostty` terminal pane and operating inside its own
reusable [cube](../../../cube/docs/main.md) workspace. Boss-engine, a
Rust service, is the policy and persistence layer that ties them
together.

The product organizes work using a durable taxonomy of products,
projects, tasks, and chores ([`work-taxonomy`](work-taxonomy.md)). The
human spends most of their time in **Work mode** — a kanban board
([`work-kanban`](work-kanban.md)) — and in conversation with the Boss
session itself; the eight worker panes are a secondary control-room
view in **Agents mode**. Review and merge of agent-produced PRs
trampoline to GitHub. The Boss CLI ([`work-cli`](work-cli.md)) and
SwiftUI app share the same engine over a Unix socket, with
subscription-based propagation of state changes
([`work-subscriptions`](work-subscriptions.md)).

This document is the canonical V2 spec. Component-level details live
in the linked sub-design docs. Decisions and rationale that led here
are recorded in [`v2-design-risks`](v2-design-risks.md).

## Goals

- Multiple Claude Code sessions in parallel against the same repository
  without trampling each other.
- A persistent durable model for work — products, projects, tasks,
  chores — that the human and agents both manipulate.
- A coordinator role (Boss session) that plans, decomposes, and
  delegates; workers execute.
- Crash- and restart-tolerant: closing the app, restarting the engine,
  rebooting the machine, or losing a single worker should not lose
  in-progress work.
- One canonical CLI for human and script use; one CLI for the
  coordinator; one direct-RPC path for the SwiftUI app — all backed by
  one engine.
- Sanctioned integrations only: Boss runs the unmodified `claude`
  binary and shells out to `gh`, `jj`, `git`, `cube`, and repo-local
  tooling. No custom Claude tool handlers.

## Non-goals (V2)

- Distributed execution across multiple machines.
- Native rendering of PR diffs, comments, or CI dashboards. Review
  trampolines to github.com.
- Auto-merge from the app. Humans merge on GitHub.
- A general ACP / SDK client. V2 is terminal-embed only.
- Cross-product tasks (a single work item that spans products).
- Pre-emption of running tasks.
- Dynamic worker pool sizing. The pool is fixed at 8.

## Core concepts

### Product, project, task, chore

The durable planning model. See [`work-taxonomy`](work-taxonomy.md)
for schema; status enum is `todo / active / blocked / in_review / done`
in storage, projected onto kanban columns Backlog / Doing / Review /
Done in the UI.

### Boss session

The single coordinating Claude Code session. Runs in its own
libghostty pane; uses `bossctl` to plan, decompose, dispatch, and
probe workers. Bootstrapped with a contract that forbids it from doing
implementation work directly.

### Worker session

One of eight Claude Code sessions running inside libghostty panes.
Each worker is bound to one **execution** at a time and runs inside a
cube-leased workspace. Workers do not have access to `bossctl` or any
Boss-only authority.

### Execution

The long-lived operational record for advancing one work item. Owns
the work-item link, the cube lease, the workspace path, and the
operational status. May span multiple agent runs over its lifetime.
See [`work-execution`](work-execution.md) for full schema and
lifecycle.

### Run

One period of active agent occupancy against an execution. A new run
starts when an agent picks up a ready execution; it ends when the
worker reaches a terminal or wait state.
See [`work-execution`](work-execution.md).

### Attention item

A durable record of human-pending work attached to an execution
(`question`, `decision_required`, `review_required`, `merge_notice`,
`info`). Renders in the inbox / triage surface.
See [`work-execution`](work-execution.md).

### Workspace lease

Cube's exclusive claim on one reusable local checkout for the
duration of an execution.
See [cube design](../../../cube/docs/main.md).

## Architecture

```text
┌──────────────────────────────────────────────────────────────────────┐
│                         macOS app (SwiftUI)                          │
│                                                                      │
│  ┌──────────────┐  ┌────────────────┐  ┌─────────────────────────┐   │
│  │  Work mode   │  │   Agents mode  │  │   Triage / inbox        │   │
│  │   (kanban)   │  │  (1 + 8 panes) │  │   (attention items)     │   │
│  └──────────────┘  └────────────────┘  └─────────────────────────┘   │
│                                                                      │
│         libghostty embedding ────► 1 Boss + 8 workers (claude)       │
└─────────┬────────────────────────────────────────────────────────────┘
          │ engine RPC socket (subscription protocol)
          ▼
┌──────────────────────────────────────────────────────────────────────┐
│                Boss-engine (Rust, single process)                    │
│                                                                      │
│  RPC server   |   ExecutionCoordinator  |  Subscription broker       │
│  Work store   |   Run / attention store |  GitHub poller             │
│  Worker-event collector (hooks-to-socket + JSONL tail)               │
│                                                                      │
│  SQLite at ~/Library/Application Support/Boss/state.db               │
└─────────┬───────────────────────────────────┬────────────────────────┘
          │ subprocess JSON                   │ events socket
          ▼                                   ▼
┌──────────────────────────┐    ┌──────────────────────────────────────┐
│   Cube                   │    │   Workers (8 × claude in libghostty) │
│                          │    │                                      │
│  Repo pool, leases,      │    │  Each in a leased workspace.         │
│  workspace setup.        │    │  Each posts hook events via the      │
│  Own SQLite at           │    │  boss-event shim → events socket.    │
│  ~/.local/share/cube/    │    │  Each writes per-session JSONL at    │
└──────────────────────────┘    │  ~/.claude/projects/<cwd>/<sid>.jsonl│
                                └──────────────────────────────────────┘
                                                    │
                                                    │ gh CLI
                                                    ▼
                                        github.com  (PRs, comments, merges)
```

### Process model

The macOS app launches as the parent. It spawns:

- **Boss-engine**: long-lived Rust service.
- **Boss session**: one `claude` TUI in a libghostty pane.
- **8 worker sessions**: `claude` TUIs in libghostty panes.

Each pane is a separate process tree under the app. Boss session and
workers are siblings — neither is a child of the other.

The app itself never leaves the foreground, but the engine is
supervisor-style: if it crashes, the app restarts it; if the app
crashes, the engine continues running so workers don't lose state
unnecessarily.

## Boss-engine

A single Rust service. Owns:

- **Work store**: SQLite (products, projects, tasks, chores). See
  [`work-taxonomy`](work-taxonomy.md).
- **Execution store**: `work_executions`, `work_runs`,
  `work_attention_items` tables. See
  [`work-execution`](work-execution.md).
- **ExecutionCoordinator**: the scheduler. Subscribes to "agent
  available" and "work item state changed" events; assigns ready
  executions to free agents; manages capacity (8 hard cap).
- **Cube driver**: invokes `cube` over subprocess `--json` for repo
  ensure, workspace lease, release, status.
- **Worker-event collector**: listens on the events Unix socket for
  hook posts from workers; tails per-session JSONL files for richer
  content.
- **GitHub poller**: 60-second `gh pr view --json` polling for any
  execution in `waiting_review` / `waiting_merge` state.
- **RPC server + subscription broker**: serves work CRUD, execution
  control, agent-control RPCs over the control socket. Multi-client.
  See [`work-subscriptions`](work-subscriptions.md).

Single SQLite file at `~/Library/Application Support/Boss/state.db`.
Override via `BOSS_DB_PATH` for development.

## Cube

External Rust binary maintained alongside Boss in the same repo.
Owns repository materialization, workspace pool, lease lifecycle, and
setup-state caching. Boss talks to cube only through the documented
JSON CLI (`cube repo ensure`, `cube workspace lease`,
`cube workspace release`, `cube workspace status`,
`cube workspace heartbeat`, `cube workspace force-release`).

Boss does not read cube's database directly. Cube does not see Boss's
work items.

See [cube design](../../../cube/docs/main.md) for full spec and
[cube remaining work](../../../cube/docs/remaining-work.md) for the
implementation gap list (heartbeat, force-release, and `workspace
setup` are V2 prerequisites).

## Workers

Each worker is the unmodified `claude` binary running in TUI mode
inside a libghostty pane embedded in the SwiftUI app. The worker
launches with:

- `cwd` = the cube-leased workspace path
- env: a sanitized allowlist (`PATH` without `bossctl`, `HOME`,
  `USER`, `SHELL`, `TERM`, `LANG`, locale vars, `BOSS_TASK_ID`,
  `BOSS_LEASE_ID`, `CUBE_LEASE_ID`, `CUBE_REPO`)
- Per-lease `<workspace>/.claude/CLAUDE.md` written by Boss-engine
  before spawn, instructing jj-first VCS conventions and forbidding
  any `boss*` commands. Deleted by cube on workspace release.
- Per-lease `<workspace>/.claude/settings.json` written by Boss-engine
  before spawn, configuring hooks (`SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`,
  `Notification`, `SessionEnd`) to invoke the `boss-event` shim,
  which forwards each event payload to the events socket.

Workers cannot spawn other workers, cannot reach the engine RPC
socket for live operations (LOCAL_PEERPID auth — see below), and have
no access to `bossctl`. They can read the durable work store
(work-taxonomy CRUD is user-tier auth and survives sandbox-thin
isolation), but writing won't have meaningful effect since execution
ownership is per-task.

## Boss session

Same shape as a worker — `claude` in libghostty — but with three
differences:

1. `bossctl` is on its `PATH`. (Workers do not see this.)
2. Spawned with a bootstrap prompt teaching the coordinator contract:
   - never implement code directly
   - never edit files directly
   - decompose work into tasks; delegate via `bossctl work start`
   - probe workers via `bossctl probe` when state confidence is low;
     probes happen at natural `Stop` boundaries
   - auto-dispatch only inside `plan_and_start` decompositions;
     otherwise queue work and wait for human review
3. Boss session does not run inside a cube workspace. Its `cwd` is a
   stable Boss-owned directory used for scratch and conversation
   continuity.

## CLI surfaces

### `boss`

User-facing CLI on the human's shell `PATH`. Talks to engine over
the control socket. Auto-starts engine if missing. See
[`work-cli`](work-cli.md) for the command map.

Available to humans. Available to workers (via user-tier auth) but
the per-lease `.claude/CLAUDE.md` advisory tells them not to invoke
it.

### `bossctl`

Coordinator-only CLI. Lives in a Boss-session-only directory; added
to `PATH` only when the app spawns the Boss libghostty pane. Not on
worker `PATH`.

Live agent control: `agents list / status / focus / send / interrupt
/ launch / stop / transcript`, `probe <id>`, `workspace summary`.
Plus `work start <id>` / `cancel <id>` for symmetry with `boss`.

## SwiftUI app

The app is a thin presentation layer. It does not shell out to
`bossctl`; it makes engine RPC calls directly over the control
socket.

Three primary surfaces:

- **Work mode**: kanban board, the human's main interaction surface.
  See [`work-kanban`](work-kanban.md).
- **Agents mode**: control-room view of the 1 Boss + 8 worker
  libghostty panes in a fixed 2×4 worker grid. Used for direct
  observation and intervention.
- **Triage / Needs attention**: surfaces attention items and
  exception states (`waiting_human`, `blocked`, `awaiting_input`,
  declared-lost-on-restart, etc.). See
  [`work-execution`](work-execution.md) human-attention model.

A persistent **Boss panel** is docked alongside the kanban — it's
where the human types to the Boss session for planning and
delegation.

## Control plane

### Control socket

One Unix socket at
`~/Library/Application Support/Boss/control.sock`, mode 0600.

**Two trust roots**, validated via
`getsockopt(SOL_LOCAL, LOCAL_PEERPID)` and ppid-walk subtree match:

- The macOS app pid.
- The Boss session pid.

**Three RPC authorization tiers**:

- **User-tier** (file ACL alone): durable work-taxonomy CRUD —
  `list_products`, `create_task`, etc.
- **App / Boss subtree-tier**: state-mutating RPCs —
  `request_execution`, `cancel_execution`, work-state writes.
- **Boss subtree-only-tier**: live agent control — `agents.*`,
  probes, focus, send, interrupt.

Workers are siblings under the app, not under Boss session. They
pass user-tier checks (durable CRUD) but fail subtree match for any
live or state-mutating operation.

### Engine RPC schema

Request envelope with `request_id` correlation; responses echo
`request_id`; unsolicited topic notifications use `request_id: null`.
See [`work-subscriptions`](work-subscriptions.md) for the framed
protocol.

Key RPCs:

- Work CRUD: `list_products`, `create_product`, `update_product`,
  `create_project`, `create_task`, `create_chore`, `update_work_item`,
  `delete_work_item`, `reorder_project_tasks`, `get_work_tree`.
- Execution control: `request_execution(work_item_id, opts) →
  {state: started|queued, worker_id?}`,
  `cancel_execution(work_item_id, force?)`,
  `request_re_engagement(work_item_id)`.
- Agent control: `agents_list`, `agents_status`, `agents_send`,
  `agents_interrupt`, `agents_focus`, `agents_launch`, `agents_stop`,
  `probe(worker_id, text)`.
- Subscriptions: `subscribe(topics)`, `unsubscribe(topics)`.

### Subscription topics

Initial set:

- `work.products` — product list changes.
- `work.product.<id>` — all child changes within one product.
- `agents.list` — worker pool membership / status.
- `executions.<execution_id>` — per-execution state changes.
- `attention.inbox` — new / resolved attention items.

Notifications are invalidation-oriented, not patch-oriented. Clients
refetch on receive. See [`work-subscriptions`](work-subscriptions.md).

## Worker → engine event channel

### Primary: hooks-to-socket

Workers run hook scripts that POST event payloads to the engine
events socket via a small `boss-event` shim binary. Engine
authenticates the post via LOCAL_PEERPID (worker subtree allowed for
publishing its own events; cannot publish for siblings).

Engine normalizes hook payloads into a canonical `WorkerEvent`
stream:

```rust
enum WorkerEvent {
    SessionStarted { source: String, model: String },
    SessionEnded   { reason: String },
    TurnStarted    { prompt: String },
    ToolInvoked    { tool: String, input: Value },
    ToolCompleted  { tool: String, output: Value, error: Option<String> },
    AwaitingInput  { kind: AwaitingKind },
    TurnCompleted  { last_assistant_text: String,
                     derived_stop_reason: Option<String> },
    ProbeReplied   { probe_id: String, text: String },
}

enum AwaitingKind { PermissionPrompt, IdlePrompt, Other(String) }
```

Each event carries `session_id`, `lease_id`, `transcript_path`, and a
UTC timestamp. `lease_id` is injected by the engine via peer-PID
lookup; it is not in the raw hook payload.

### Secondary: session JSONL tail

Engine maintains a per-worker tail-watcher on the
`transcript_path` exposed in hook payloads (typically
`~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`). Provides
content the hooks don't carry: full assistant text bodies, tool
result bodies, todo updates, plan-mode entries.

### Backup: screen scrape for `AwaitingInput`

If a worker pane shows a permission-prompt-shaped region for >5
seconds without a `Notification` hook firing, engine emits a
synthetic `AwaitingInput` event by pattern-matching the libghostty
buffer. This is the only place screen scraping is authoritative.

### Probes

Boss session can request a worker probe via `bossctl probe <id>`.
Engine queues the probe; on the worker's next natural `Stop`,
engine injects the probe text as the next prompt. The follow-up
`Stop` produces a `ProbeReplied` event with the answer in
`last_assistant_text`. Probes never interrupt mid-turn.

## Workflows

### Task dispatch

1. Caller (Boss session via `bossctl`, or human via SwiftUI) invokes
   `request_execution(work_item_id)`.
2. Engine resolves `work_item.product_id → cube_pool_id`. If unset,
   work item moves to `blocked` with `reason: needs_pool_config`.
3. Engine checks capacity. If a worker slot is free:
   - Engine creates an execution row.
   - Engine invokes `cube workspace lease` with
     `preferred_workspace_id` if the work item has a recent lease.
   - Engine writes `<workspace>/.claude/CLAUDE.md` and
     `<workspace>/.claude/settings.json`.
   - Engine spawns `claude` in the worker's libghostty pane with
     `cwd = workspace_path`.
   - Execution status: `running`.
   - Engine returns `{state: "started", worker_id}`.
4. If no slot is free: execution status `queued`. Engine returns
   `{state: "queued"}`.
5. On `WorkerEvent::SessionEnded`: engine pops the highest-priority
   queued execution and dispatches.

Worker selection: affinity-first (use `preferred_workspace_id` if
free), then LRU among free workspaces.

Queue ordering: priority (low/medium/high), FIFO within priority.

### PR detection and review

When the worker opens a PR:

- Engine pattern-matches the worker's `last_assistant_text` for a
  GitHub PR URL.
- Engine also runs `gh pr list --head <expected-branch>`
  periodically using cube's deterministic branch names.

On detection: engine records `task.pr_url` and transitions execution
to `waiting_review`. Releases the agent (run ends) but **keeps the
workspace lease** attached to the execution, so re-engagement can
re-use the warm cache.

Engine polls `gh pr view --json state,mergedAt,statusCheckRollup,
reviews,comments` every 60 seconds for each `waiting_review`
execution. When `state: MERGED` is observed, execution transitions
to `completed`, lease is released.

The human reviews on github.com (via "Open in browser" from Work
mode); native diff/comment/merge UI is out of scope for V2.

### Re-engagement

When `gh pr view` shows new comments since the last engagement, an
attention item of kind `review_required` is created (or its count
updated). Work mode shows a "Re-engage worker" affordance.

On click:

1. Engine creates a new run on the same execution.
2. Engine re-leases the workspace via cube
   (`preferred_workspace_id` = the original lease's workspace).
3. Engine resumes the claude session: `claude --resume <session_id>`
   in that workspace.
4. Engine sends a synthesized prompt:
   ```
   The PR you opened (<pr_url>) has new review comments:
   <bullets with author + body>
   Please address them and update the PR.
   ```
5. Execution status: `running` again until the next worker `Stop`.

### Cancellation

Two modes via `cancel_execution`:

- **Soft cancel** (default): engine marks intent on the execution;
  on the next `WorkerEvent::TurnCompleted`, it declines to send
  another prompt and ends the run. Non-disruptive.
- **Hard cancel** (`force: true`): engine SIGINTs the worker's
  `claude` process. Run ends, execution moves to `abandoned`,
  cube lease released. Mid-turn work is discarded.

### Crash and resume

SQLite is the source of truth for execution + run + work-item state.

**App restart**: engine + workers survive. App reconnects to engine,
receives full state replay, resumes subscription stream.

**Engine restart**: workers survive (claude processes are independent
of engine). On startup, engine runs three-way reconcile per `running`
execution:

```text
SQLite says: execution running, claude session_id, lease_id
Cube says:   lease_id still held by boss/* OR not
Process:     claude pid alive OR not; if dead, --resume probe OR not

3 yes → reattach (rewire events socket; if claude died, --resume)
2 yes → reattach if "process dead" is the missing one (--resume)
< 2 yes → declare lost: execution → abandoned;
          cube force-release; record session_id in audit log
```

**Worker crash** (single claude dies): engine sees socket disconnect
or `WorkerEvent::SessionEnded` with reason ≠ `other`. Engine spawns
`claude --resume <session_id>` in the same workspace. Conversation
context preserved; mid-turn tool side effects best-effort.

**OS reboot**: cold start. Cube TTLs all expired by the time the
engine starts; engine force-releases all `boss/*` leases on its
startup reconcile. Executions that were `running` at reboot move to
`queued`. Human re-dispatches.

### Lease lifecycle and heartbeat

- TTL: 30 minutes.
- Engine heartbeats every 5 minutes per held lease.
- Missed heartbeat for 10 minutes (= 2 × interval) makes the lease
  eligible for cube force-release.

### Explicitly irrecoverable

- Mid-turn tool side effects when the tool was in-flight at kill
  time (claude infers completion on resume; discrepancies must be
  recovered by the next turn).
- LLM streaming output mid-token at kill.
- libghostty pane scrollback on app restart (re-renderable from
  JSONL transcript if we choose to; not done by default).

## Worker isolation

Layered defense. See R3 in
[`v2-design-risks`](v2-design-risks.md) for the full threat model.

- **PATH separation**: workers do not have `bossctl` on their PATH.
- **LOCAL_PEERPID subtree match** on the control socket: workers
  fail the live and state-mutating tiers because they're not under
  the Boss session subtree (and not under the app's subtree in any
  way that produces meaningful authority).
- **Per-worker `.claude/CLAUDE.md`** advisory: rules tell the worker
  not to interact with sibling sessions or invoke any `boss*`
  commands.
- **Cube per-worker workspace**: filesystem isolation between
  worker working trees.

Out of threat model: malicious code on the host, macOS sandbox
escapes, multi-user / remote scenarios. V2 is a personal-use tool on
one machine.

## Storage

### Boss-engine SQLite

Single file at `~/Library/Application Support/Boss/state.db`. Tables:

- `products`, `projects`, `tasks` — work taxonomy. See
  [`work-taxonomy`](work-taxonomy.md).
- `work_executions`, `work_runs`, `work_attention_items` — execution
  state. See [`work-execution`](work-execution.md).
- `metadata` — small key/value (schema version, settings).

### Cube SQLite

Cube's own store at `$XDG_DATA_HOME/cube/` or `~/.local/share/cube/`.
Boss does not read it; communicates only via cube's JSON CLI. See
[cube design](../../../cube/docs/main.md).

### Per-execution transcripts on disk

Long-form interaction logs under
`~/Library/Application Support/Boss/executions/<execution-id>/`:

```text
executions/<execution-id>/
  execution.json
  attention/
    <attention-id>.json
  runs/<run-id>/
    metadata.json
    transcript.jsonl   # structured, append-only
    transcript.md      # derived markdown view
    artifacts/
```

### Per-lease ephemeral files

Inside each leased workspace:

- `.claude/CLAUDE.md` — Boss worker rules; written before spawn,
  deleted by cube on release.
- `.claude/settings.json` — hook config; same lifecycle.

## Configuration

The user configures, per product:

- `repo_remote_url` — canonical repository identity.
- `cube_pool_id` — link to a cube repo pool. If unset, executions
  block with `needs_pool_config`.

Cube pool configuration (workspace_root, workspace_prefix, setup
steps) lives in cube's own configuration, not Boss's. See
[cube design](../../../cube/docs/main.md).

Engine env:

- `BOSS_DB_PATH`: override the SQLite path (dev/test).
- `BOSS_SOCKET_PATH`: override the control socket path.
- `CUBE_DATA_DIR`: forwarded to cube subprocesses.

Boss CLI env:

- `BOSS_SOCKET_PATH`: as above.
- `--no-autostart`: don't auto-launch engine.

## Migration from V1

The current PoC code at `tools/boss/engine/` and
`tools/boss/app-macos/` is the foundation; V2 is mostly additive.

What's preserved:

- Engine binary (`boss-engine`); becomes one of two CLI entrypoints
  (the other is the new `boss` user CLI).
- Existing work taxonomy schema and CRUD.
- macOS app shell and split-view IA.

What's replaced:

- ACP transcript model for live agents → libghostty terminal panes
  + hook events.
- Single-agent connection handling → multi-client subscription
  protocol.
- Per-connection agent registry → server-global execution coordinator.

What's added:

- ExecutionCoordinator + execution / run / attention tables.
- Cube driver subsystem.
- Worker-event socket and JSONL tail-watcher.
- `boss` CLI binary, separate from `boss-engine`.
- `bossctl` CLI binary.
- `boss-event` hook shim.
- libghostty embedding, pane management, the 2×4 worker grid.
- GitHub poller.

Phasing follows the work-execution rollout (Phase A through G); see
[`work-execution`](work-execution.md) §"Recommended Rollout".

## Out of scope for V2

- Native PR diff / comment rendering or in-app merge.
- Multi-product workers (a worker leasing across two products).
- Multi-repo product (`related_repo_remote_urls_json` future field is
  noted but not used).
- Pre-emption of running work.
- Dynamic worker pool sizing.
- ACP / SDK-based workers.
- Webhooks (Boss has no public endpoint; all GitHub state is polled).
- Cross-host / remote execution.
- Cube `change *`, `stack *`, `pr *`, `graph`, `doctor` commands —
  not required for V2 (Boss drives `jj` / `gh` / `git` directly
  inside leased workspaces). See
  [cube remaining work](../../../cube/docs/remaining-work.md).
- Cube prereqs: `workspace heartbeat`, release `--reason crash
  --keep-dirty`, `workspace force-release`, `workspace setup`,
  pool auto-create from `--source`. These must land before V2 takes
  the cube hard dependency.

## Related designs

- [`v2-design-risks`](v2-design-risks.md) — the decision log behind
  this spec; rationale lives there.
- [`work-taxonomy`](work-taxonomy.md) — product/project/task/chore
  schema.
- [`work-execution`](work-execution.md) — execution / run / attention
  model and lifecycle.
- [`work-kanban`](work-kanban.md) — Work mode kanban board.
- [`work-cli`](work-cli.md) — `boss` CLI command map.
- [`work-subscriptions`](work-subscriptions.md) — engine pub/sub
  protocol.
- [`engine-app-rpc`](engine-app-rpc.md) — engine → app pane RPCs
  layered on the frontend socket (Phase 6f spawn flow).
- [cube `main`](../../../cube/docs/main.md) — workspace pool layer.
- [cube `remaining-work`](../../../cube/docs/remaining-work.md) —
  cube V2 prerequisites.
