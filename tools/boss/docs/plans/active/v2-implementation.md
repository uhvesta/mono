# Boss V2 Implementation Plan

This is the phased build plan for delivering the Boss V2 spec
([`designs/main.md`](../../designs/main.md)). The design decisions
that shaped V2 are recorded in
[`v2-design-risks.md`](../../designs/v2-design-risks.md); this plan
turns those into ordered work.

This plan supersedes the earlier
[`swiftui-boss-v2.md`](../done/swiftui-boss-v2.md), which was the
brainstorm that started V2 and has been archived to `plans/done/`.

## Principles

- **Vertical slices over horizontal layers.** Each phase ends in
  something observable end-to-end, not "the engine half of feature X."
- **Ship continuously.** Each phase should be merge-able and useful in
  isolation; nothing gates on a future phase except where the
  dependency is structural.
- **PoC code stays running until replaced.** The current single-agent
  ACP code in `tools/boss/engine/` and `tools/boss/app-macos/`
  continues to function while V2 components land alongside it.
  Cutover happens at the end of phase 6.
- **Cube prereqs land in their own track.** The four cube V2
  prerequisites (`flock`, `workspace setup`, `--source` auto-create,
  lease-lifecycle commands) are tracked in
  [cube remaining-work](../../../../cube/docs/remaining-work.md). Boss
  phases that depend on them call that out explicitly.
- **One commit per phase milestone**, ideally. Phases break down into
  commit-sized increments at implementation time; this doc is at the
  phase level.

## Status at a glance (audited 2026-04-30 against `main`)

| Phase | Title                                          | Status                       |
|-------|------------------------------------------------|------------------------------|
| 1     | Engine and CLI foundations                     | 🟢 named deliverables shipped; CLI follow-ups (Product/Project delete+move) landed |
| 2     | Multi-client subscriptions                     | 🟡 mostly shipped — outbound queue is unbounded, no coalescing or slow-client disconnect |
| 3     | Kanban Work tab                                | 🟢 shipped                   |
| 4     | Execution layer + cube V2 prereqs              | 🟡 mostly shipped — cube driver missing `status` subcommand |
| 5     | ExecutionCoordinator                           | 🟡 partially shipped — multiple gaps (worker affinity/LRU, `executions.<id>` topic, `request_execution` RPC, waiting-state semantics) |
| 6     | libghostty embedding and worker spawn          | ❌ not started               |
| 7     | Boss session and bossctl                       | ❌ not started               |
| 8     | Review and attention                           | 🟡 schema + manual PR-URL only — no auto-detect, no poller, no Triage UI, no re-engage |
| 9     | Resume and continuity                          | ❌ not started               |
| 10    | Transcripts and hardening                      | ❌ schema columns only       |

The biggest open item is **Phase 6** (libghostty + worker spawn);
Phases 7 and 9 effectively wait on it. Phase 8 has the data model
but none of the live PR / poller / triage logic. Phase 10 hardening
is essentially untouched.

Each phase below carries its own **Status (done)** and **Pending**
sections with file references so they can be ticked off
incrementally.

## Where we start (historical snapshot)

State at the time this plan was first written, kept for context.
The phase-by-phase Status sections below are the live source of
truth.

- `tools/boss/engine/`: ~3500 LOC Rust. Single-agent ACP transcript
  model (`acp.rs`), work-taxonomy SQLite store (`work.rs`),
  per-connection state. Multi-agent extensions in
  `boss-poc-2-engine-multi-agent` branch.
- `tools/boss/app-macos/`: ~3000 LOC SwiftUI. Tree-first Work
  navigator, single-agent chat detail. Sidebar multi-agent layer in
  `boss-poc-2-app-multi-agent` branch.
- `tools/cube/`: ~1700+ LOC Rust. Repo + workspace + lease commands
  implemented; setup engine, auto-create, lease-lifecycle commands,
  and stacked-PR features still pending.
- `tools/ghostty-proto/`: standalone 5-pane libghostty embedding
  prototype. Not integrated into boss app.

The PoC code is the foundation. V2 components either replace
PoC pieces (single-agent → multi-worker; tree → kanban) or are net-new
(execution layer, libghostty embedding, hooks-to-socket events,
bossctl).

## Phases

Each phase has: **Status (done)**, **Pending**, **Done when**
(acceptance criteria, kept for the record), **Depends on**, and
**References**. Closed phases drop the **Pending** block.

---

### Phase 1: Engine and CLI foundations — 🟢 named deliverables shipped

**Goal.** Refactor the engine for multi-client use and ship the
public-facing `boss` CLI for work-taxonomy CRUD.

**Status (in `main`).**

- Config split: eager `WorkConfig` (cwd, db_path, worker_pool_size)
  vs lazy `AgentConfig` (acp + cube), materialized via `OnceLock` on
  first agent op. The engine in server mode boots without
  `ANTHROPIC_API_KEY` or any agent env present
  (`engine/src/config.rs`).
- Request envelope with `request_id` correlation
  (`protocol/src/wire.rs`).
- Shared `boss-protocol` crate at `tools/boss/protocol/` consumed by
  engine and CLI; carries both wire enums and the data shapes
  (Product / Project / Task / WorkItem / `Create*Input` / patch /
  execution rows). Swift client marshals the same envelopes
  (`app-macos/Sources/EngineClient.swift`).
- Shared `boss-client` crate at `tools/boss/client/` owns socket
  discovery (env + `--socket-path` override), engine autostart, and
  request/response correlation. CLI consumes it via
  `Discovery::from_env(...)` + `BossClient::connect(&discovery)`
  (`cli/src/main.rs`).
- `boss` CLI binary at `tools/boss/cli/` with subcommands shipped:
  - `product` — Create / List / Show / Update / Move / Delete.
  - `project` — Create / List / Show / Update / Move / Delete.
  - `task` — Create / List / Show / Update / Move / Delete /
    Reorder.
  - `chore` — Create / List / Show / Update / Move / Delete.
  - `engine` — Status / Start / Stop.
  - Product/Project `delete` is a soft archive (sets status=archived);
    the engine refuses hard delete for these entities. `move --to`
    accepts the entity's lifecycle status enum
    (active/paused/archived for products; planned/active/blocked/
    done/archived for projects).
- `--json`, `--quiet`, `--no-input`, `--no-autostart` global flags;
  TTY prompts on interactive create flows.
- Concurrent-client integration test: in-process engine on temp
  socket + temp DB exercises full work CRUD round-trip and verifies
  a second client receives `topic_event` invalidations from the
  first (`engine/tests/work_crud.rs`). `app::serve` is the public
  entry point used by tests, taking explicit socket + optional
  pid-file paths so no env mutation is required.

**Pending.**

- `boss chore reorder` — chores aren't ordered within projects in
  the schema, so this is a no-op until the schema introduces
  ordering. No action needed unless the schema changes.

**Done when (acceptance, kept for the record).**

- `boss product create --name X --json` writes through the engine
  and emits stable JSON.
- A second concurrent client can connect to the engine without
  breaking the first.
- All work CRUD flows work end-to-end against a temp engine in tests.

**References.** [`work-cli`](../../designs/work-cli.md);
[`work-subscriptions`](../../designs/work-subscriptions.md) Phase 1.

---

### Phase 2: Multi-client subscriptions — 🟡 mostly shipped

**Goal.** Make work mutations from any client propagate to all
connected clients in real time.

**Status (done).**

- `ServerState` with `WorkDb`, `TopicBroker`, `AtomicU64`
  `work_revision` (`engine/src/app.rs:207-244`).
- `subscribe` / `unsubscribe` RPCs (`protocol.rs:79-84`) and
  `Subscribed` / `Unsubscribed` responses (`protocol.rs:185-190`)
  — both directions wired.
- Topics `work.products` and `work.product.<id>` defined and
  published (`protocol.rs:9-12`; publish sites at `app.rs:346,
  558, 694, 725, 756, 789, 823, 892`).
- `publish_work_invalidation()` fires after DB writes commit
  (`app.rs:1394-1458`).
- macOS app subscribes on connect and refetches on invalidation
  (`app-macos/Sources/ChatViewModel.swift`,
  `app-macos/Sources/EngineClient.swift`).

**Pending.**

- **Outbound queue is unbounded.** `TopicBroker` uses
  `mpsc::unbounded_channel()` (`app.rs:255`); the publish path is
  `let _ = sender.send(envelope.clone())` (`app.rs:359`) with no
  back-pressure. The deliverable calls for a bounded queue —
  pick a sensible bound and enforce it.
- **No same-topic coalescing.** `grep` for "coalesce" /
  "coalesc" in engine source returns nothing. Two rapid mutations
  on the same product currently emit two invalidations.
- **No slow-client disconnect policy.** Nothing closes the
  session when the per-session queue fills. Once the queue is
  bounded this becomes a hard requirement.
- Confirm `work_revision` is exposed on **read** responses too,
  not just subscribe responses. The deliverable calls for both;
  I verified subscribe but not the read side.

**Done when (acceptance, kept for the record).**

- A `boss task update <id> --status active` from a terminal causes
  the macOS app's Work tab to update within ~1s without manual
  refresh.

**Depends on.** Phase 1.

**References.**
[`work-subscriptions`](../../designs/work-subscriptions.md).

---

### Phase 3: Kanban Work tab — 🟢 shipped

**Goal.** Replace the tree-first Work tab with a kanban board.

**Status (in `main`).** The full deliverable list has shipped in
`tools/boss/app-macos/Sources/{Models.swift, ChatViewModel.swift,
ContentView.swift}`:

- Board state: `selectedWorkProductID`, `selectedProjectFilterIDs`
  (multi-select), `includeChores`, `showBlockedOnly`,
  `workBoardGrouping`, `selectedWorkCardID`.
- Four fixed columns (Backlog / Doing / Review / Done) with status
  mapping (`blocked` renders inside Doing).
- Card primitives for tasks and chores; project label, blocked
  badge, PR link.
- Move actions between columns (menu and drag-and-drop).
- Quick-add affordance at top of Backlog.
- Sidebar: product picker, multi-select project filter checkboxes
  (with "All Projects" reset), and Options section with "Include
  chores" and "Show blocked only" toggles.
- Inspector for create/edit on a selected card.
- UserDefaults persistence for product, project filter set,
  includeChores, showBlockedOnly, and grouping.
- Real-time board updates via the Phase 2 subscription mechanism.

**Done when (acceptance, kept for the record).**

- Human can manage all current work-taxonomy CRUD through the
  kanban board without dropping to the tree view.
- Status changes from `boss` CLI propagate live to the board.

**Depends on.** Phase 2.

**References.** [`work-kanban`](../../designs/work-kanban.md).

---

### Phase 4: Execution layer plumbing + cube V2 prereqs — 🟡 mostly shipped

**Goal.** Add durable execution / run / attention state and land the
cube features Boss will hard-depend on.

**Status (done).**

- Cube V2 prerequisites all landed
  ([cube remaining-work "All V2 prerequisites have landed"](../../../../cube/docs/remaining-work.md)):
  per-repo `flock`, `cube workspace setup` engine with
  fingerprinted reuse, lease auto-create from `--source` on pool
  exhaustion, `lease`/`heartbeat`/`release --reason --keep-dirty`/
  `force-release`, TTL with `expire_stale_leases`.
- `work_executions`, `work_runs`, `work_attention_items` tables and
  CRUD (`engine/src/work.rs:1032-1076` DDL; CRUD across
  `work.rs:333-820`).
- `WorkRun` carries `transcript_path` and `artifacts_path`
  (`engine/src/work.rs:75-87`); transcript path indexing wired.
- Cube driver `CubeClient` trait + `CommandCubeClient` for
  `ensure_repo` (`coordinator.rs:99`), `lease_workspace`
  (`coordinator.rs:121`), `release_workspace`
  (`coordinator.rs:170`).

**Pending.**

- Cube driver does **not** currently invoke `cube workspace status`
  even though the original deliverable named it. No `status` method
  on `CubeClient`, no shell-out site (`grep` for "workspace.*status"
  in `coordinator.rs` returns nothing). Either add it or drop it
  from the deliverable.
- Cube driver also does not invoke `cube workspace heartbeat` or
  `cube workspace force-release`. The cube CLI supports both;
  engine-side callers are missing. Heartbeat is needed by Phase 9;
  force-release is needed by reconcile.
- Per-execution on-disk dir layout under
  `~/Library/Application Support/Boss/executions/<id>/` is modelled
  (path stored on `WorkRun`) but not yet populated by anything that
  writes actual files. That's fine until Phase 6 spawns workers
  that produce artifacts; revisit if anything in Phase 5 needs it
  sooner.

**Done when (acceptance, kept for the record).**

- Engine can create an execution row, lease a workspace via cube,
  release it, and persist the full lifecycle in SQLite — all
  driven manually via CLI or test, no autonomy yet. (Covered by the
  `successful_run_moves_execution_to_waiting_human_and_releases_worker`
  integration test in `engine/src/coordinator.rs:785-845`.)
- Cube prereqs are merged. ✅

**Depends on.** Phase 1.

**References.** [`work-execution`](../../designs/work-execution.md)
Phase B; [cube remaining-work](../../../../cube/docs/remaining-work.md).

---

### Phase 5: ExecutionCoordinator — 🟡 partially shipped

**Goal.** Add the server-global scheduler that turns work-item
events into runs.

**Status (done).**

- `ExecutionCoordinator` struct exists (`coordinator.rs:231-237`)
  with `new()`, `worker_pool()`, `kick()`, and an internal
  `run_scheduler` driving the loop.
- Auto-creation of execution stubs by work-item kind (project →
  `project_design`, chore → `chore_implementation`, task →
  `task_implementation`) via `reconcile_work_item_execution()`
  (`work.rs:400-468`); integration site at `app.rs:1406`.
- `WorkerPool` with a configurable size cap
  (`coordinator.rs:187-198`; `config.rs:42-49`,
  env `BOSS_WORKER_POOL_SIZE`, default 1).
- `RunOutcome` struct and run lifecycle start → active → end
  (`runner.rs:21-26`, scheduler at `coordinator.rs:500-560`).
- Single boolean `release_workspace` on `RunOutcome` drives the
  retain/release decision (`runner.rs:25`,
  `coordinator.rs:515-560`).

**Pending.**

- **No `coordinate()` method** on `ExecutionCoordinator`. The
  prior status block claimed one — wrong. The real entry point is
  `kick()`; the loop is `run_scheduler` internally. Either rename
  the contract in the design or add a method named to match.
- **Worker selection is naive.** `claim_idle_worker()`
  (`coordinator.rs:200-209`) picks the first idle worker. The
  deliverable calls for affinity-first via
  `preferred_workspace_id`, then LRU among free; neither is
  implemented (`grep` for `preferred_workspace_id` / `affinity` /
  `lru` in `coordinator.rs` → nothing). This needs to land before
  Phase 6 spawns workers that benefit from affinity.
- **Hard 8-worker cap is not asserted.** The pool size is
  configurable and defaults to 1. The deliverable's "8-worker hard
  cap" is a runtime config value, not a code-level invariant. If
  the design wants it as a hard cap, enforce it in `WorkerPool`
  construction.
- **Priority + FIFO-within-priority queue** is not implemented.
  The scheduler walks ready executions in DB-iteration order. No
  priority field on executions; no ordered queue.
- **`executions.<id>` topic publishing** is not wired. No topic
  constant in `protocol.rs`; no publish call site in coordinator.
  `TopicBroker` is only used for `work.*` topics today. Phase 8's
  triage UI will need this.
- **`request_execution(work_item_id, opts)` RPC** is not in the
  protocol. The only entry point today is `CreateExecution` driven
  manually. Either add the RPC or rename the design to match what
  we shipped (decide which).
- **Lease retain/release semantics** use a single
  `release_workspace: bool` on `RunOutcome` — and inspection of
  `runner.rs:181` shows the field is hardcoded `false` in the
  current path. The design calls for four `waiting_*` states
  (`waiting_human` / `waiting_review` / `waiting_merge` retain,
  `waiting_dependency` releases); promote the boolean to an enum
  before Phase 8 lights up review-driven releases.

**Done when (acceptance, kept for the record).**

- Creating a task via CLI causes the engine to autonomously create
  an execution, lease a cube workspace, and update execution state
  — even though no `claude` is yet running. The paper trail is
  observable in SQLite + per-execution files.

**Depends on.** Phase 4.

**References.** [`work-execution`](../../designs/work-execution.md)
Phase C; [`v2-design-risks`](../../designs/v2-design-risks.md) R5.

---

### Phase 6: libghostty embedding and worker spawn — ❌ not started (load-bearing)

**Goal.** End-to-end vertical slice: engine spawns real `claude`
workers in libghostty panes, workers do work, events flow back.

**Status (done).** Nothing on this phase has shipped to `main`.
Substrate from earlier phases is in place: execution and run rows,
cube driver, scheduler. The standalone embedding prototype lives at
`tools/ghostty-proto/` but has not been folded into the macOS app.
`grep -rn "ghostty\|libghostty\|TerminalSession"
app-macos/Sources` returns nothing; `tools/boss/bin/` contains only
the `boss` binary (no `boss-event` shim).

**Pending — full deliverables list.**

- App: import the `tools/ghostty-proto` embedding pattern. Build a
  1 + 8 pane layout in Agents mode. Each pane is a
  `TerminalSessionModel` bound to one engine-managed session id.
- Engine: spawn `claude` subprocess for each scheduled run,
  attached to a libghostty pane via app coordination (engine asks
  app to host a pane; app returns pane handle; engine wires stdio).
- Engine: write per-lease `<workspace>/.claude/CLAUDE.md` (jj-first
  rules, do-not-touch-siblings advisory) before spawn.
- Engine: write per-lease `<workspace>/.claude/settings.json` with
  hook config for SessionStart, UserPromptSubmit, PreToolUse,
  PostToolUse, Stop, Notification, SessionEnd.
- New `boss-event` shim binary: reads stdin, posts to engine events
  socket. Bundled with engine; written into worker's PATH per
  lease.
- Engine: events socket bound at
  `~/Library/Application Support/Boss/events.sock` (mode 0600).
- Engine: hook event ingestion + `lease_id` injection via
  LOCAL_PEERPID lookup.
- Engine: `WorkerEvent` enum normalization from raw hook payloads;
  derive `stop_reason`.
- Engine: per-worker tail-watcher on `transcript_path` for richer
  content.
- App: cutover from PoC single-agent transcript model to
  pane-per-worker. The old transcript code is removed.

**Done when (acceptance).**

- Human can run `boss task create --name X` from terminal; engine
  picks up the task, leases a workspace, spawns `claude` in a
  libghostty pane, the worker does the work and opens a PR. Engine
  observes the full lifecycle via WorkerEvents.

**Depends on.** Phase 5. (Cube V2 prereqs already landed in Phase 4.)

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R1, R2, R4; [`work-execution`](../../designs/work-execution.md).

---

### Phase 7: Boss session and bossctl — ❌ not started

**Goal.** Add the coordinator role and the Boss-only CLI.

**Status (done).** Nothing. There is no `bossctl` binary
(`tools/boss/bin/` contains only `boss`); no Boss pane in the app;
no probe model; no two-trust-root auth refinement; no worker spawn
env hygiene (because there is no worker spawn yet).

**Pending — full deliverables list.**

- App: spawn a 1 Boss libghostty pane in addition to the 8 worker
  panes. Persistent docked Boss panel in Work mode.
- `bossctl` Rust binary at `tools/boss/cli/bossctl` (or shared
  crate with `boss`).
- `bossctl agents list / status / focus / send / interrupt /
  launch / stop / transcript`, `bossctl probe <id>`, `bossctl
  workspace summary`.
- `bossctl work start / cancel` aliases for symmetry with `boss`.
- Engine: control socket auth refinement — two trust roots (app
  pid + Boss session pid) via LOCAL_PEERPID subtree match; three
  RPC authorization tiers (user / app+Boss / Boss-only).
- Engine: probe model — Stop-boundary injection. Boss requests
  probe via `bossctl`; engine queues; on next worker `Stop`,
  engine injects probe text as the next prompt. The follow-up
  `Stop` produces a `ProbeReplied` event.
- Boss session bootstrap: a system prompt or session-init prompt
  teaching the coordinator contract (delegate, don't implement;
  auto-dispatch only inside `plan_and_start`; probe on low
  confidence).
- Worker spawn env hygiene: sanitized PATH excluding `bossctl`,
  fixed allowlist of env keys.

**Done when (acceptance).**

- Boss session can fully drive a workflow:
  human types planning prompt → Boss decomposes → Boss runs
  `bossctl work start <id>` → engine picks up → worker runs → Boss
  observes via `bossctl agents status` → on completion, Boss
  reports to human.
- A worker that tries to invoke `bossctl` fails (PATH miss). A
  worker that tries the live RPCs over the socket fails
  subtree-match auth.

**Depends on.** Phase 6 (worker panes must exist first).

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R3, R5, R8.

---

### Phase 8: Review and attention — 🟡 schema + manual PR-URL only

**Goal.** Close the human-in-the-loop review cycle.

**Status (done).**

- `work_attention_items` table, struct, and CRUD
  (`engine/src/work.rs:90` `WorkAttentionItem` struct;
  `work.rs:1066-1078` table DDL; `work.rs:754-820`
  `create_attention_item` / `list_attention_items` /
  `get_attention_item`).
- Attention items are created when a run finishes in
  `waiting_human` (`engine/src/coordinator.rs:533-550`, off
  `engine/src/runner.rs:178`).
- `tasks.pr_url` column on the work-taxonomy schema
  (`engine/src/work.rs:1020`); settable via `boss task update
  --pr-url <url>` and via the app's task edit form
  (`app-macos/Sources/ContentView.swift:1112`,
  `:1119`).
- PR URL is rendered in kanban card detail
  (`ContentView.swift:914` metadata row) and shown as a label on
  the card itself (`ContentView.swift:838-839`). This is the
  Phase 3 kanban work, but it incidentally fills the "card detail
  shows PR URL" half of one Phase 8 deliverable — only the
  *display* half; see Pending below.

**Pending.**

- Engine: **automatic** PR detection — pattern-match worker
  `last_assistant_text` for GitHub PR URL; periodic
  `gh pr list --head <branch>` discovery using cube's deterministic
  branch names. (`grep` for `last_assistant_text` and
  `github.com/.*pull` in `engine/src/` returns nothing. The
  `pr_url` field exists but only as a manual setter today.)
- Engine: GitHub poller — every 60s, `gh pr view --json state,
  mergedAt, statusCheckRollup, reviews, comments` for each
  execution in `waiting_review` or `waiting_merge`. (`grep` for
  `"gh pr"` in `engine/src/` → nothing.)
- Engine: extend attention-item creation to also fire on
  `waiting_review`. (`grep` for `waiting_review` / `waiting_merge`
  in `engine/src/` → nothing; only the `waiting_human` path
  creates attention items.)
- Engine: `request_re_engagement(work_item_id)` RPC: re-leases
  the workspace (with `preferred_workspace_id`), resumes the
  claude session via `--resume`, sends synthesized
  comments-as-prompt. (`grep` for `re_engage` /
  `request_re_engagement` → nothing.)
- App: Triage / Needs Attention surface in Work mode. Lists
  attention items with primary action ("Open PR" / "Re-engage
  worker" / "View blocker" / "Re-dispatch"). (`grep` for
  `Triage` / `AttentionItem` / `attention` in
  `app-macos/Sources/` → nothing — the attention-items table on
  the engine has no consumer in the app.)
- App: card detail additions on top of the existing PR URL line —
  status ribbon (PR open / draft / merged / failing checks),
  "Open in browser" button, "Re-engage worker (N comments)"
  action. (None of these strings appear in `ContentView.swift`.)
- Engine: detect `state: MERGED` → execution `completed`, lease
  released. (`grep` for `MERGED` / `mergedAt` in engine →
  nothing.)

**Done when (acceptance).**

- Worker opens a PR; human reviews on github.com; comments
  propagate to Triage; human clicks Re-engage; worker resumes and
  addresses comments; human merges on github.com; execution closes
  automatically on next poll.

**Depends on.** Phase 7 (re-engagement uses `--resume`, which
requires the Phase 6 worker spawn path).

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R7; [`work-execution`](../../designs/work-execution.md) Phase D.

---

### Phase 9: Resume and continuity — ❌ not started

**Goal.** Survive crashes and restarts cleanly.

**Status (done).** Nothing. `grep` for `--resume` and reconcile
logic in `engine/src/` returns nothing; no heartbeat-emitting code
in the engine; the events socket itself doesn't exist yet (Phase
6).

**Pending — full deliverables list.**

- Engine: `claude --resume <session_id>` on worker crash; new run
  on the same execution, same workspace.
- Engine: startup three-way reconcile per `running` execution:
  SQLite ↔ cube lease state ↔ claude process state. Reattach where
  three-way OK; declare lost otherwise (execution →
  `abandoned`, cube force-release, audit log).
- Engine: cube lease heartbeat every 5 min for held leases.
  (Cube's `workspace heartbeat` command is already implemented;
  this is the engine-side caller.)
- Engine: events socket reconnect handling — accept reconnects
  from workers whose engine restarted underneath them.
- App: full state replay on engine reconnect (not delta-since-seq
  yet).
- App: surface "These tasks were active when [engine restarted /
  machine restarted]; review and re-dispatch" affordance in
  Triage.
- Engine: OS-reboot policy — force-release all `boss/*` leases on
  startup; tasks that were `running` move to `queued`; no
  auto-resume on reboot.

**Done when (acceptance).**

- Killing the engine while workers are running and restarting it
  results in the engine reattaching the live workers and
  continuing to receive their hook events. Killing a single worker
  mid-turn results in a clean `--resume` and the work continues.

**Depends on.** Phase 8 (so the failure cases include in-review
executions, not just running). Foundationally also Phase 6.

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R6; [`work-execution`](../../designs/work-execution.md) Phase E.

---

### Phase 10: Transcripts and hardening — ❌ schema only

**Goal.** Make Boss inspectable and operationally safe.

**Status (done).**

- `WorkRun.transcript_path` and `WorkRun.artifacts_path` columns
  modelled (`engine/src/work.rs:75-87`); not yet populated.
- Structured logging in the engine via `tracing`, gated by
  `BOSS_LOG=debug`.

**Pending.**

- Engine: structured per-run transcripts at
  `~/.../executions/<exec-id>/runs/<run-id>/transcript.jsonl`,
  derived `transcript.md`.
- Engine: artifact directory per run (column exists; no writer).
- App: stale-runs and stale-leases visibility (listing executions
  with mismatch between cube and engine state).
- Engine: cube doctor integration — invoke `cube doctor` on demand
  from the app's diagnostics view. (Note: `cube doctor` itself is
  still `NotImplemented` in cube — see [cube remaining-work](../../../../cube/docs/remaining-work.md).
  Either land `cube doctor` first or relax this deliverable to
  whatever cube exposes.)
- Engine: retry / abandonment flows; explicit "this execution
  failed; retry, abandon, or escalate?" decision surface.
- Screen-scrape fallback for `AwaitingInput`: if a worker pane
  shows a permission-prompt-shaped region for >5s without a
  `Notification` hook, emit a synthetic event.
- Polish: keyboard shortcuts, empty-state handling, error
  messages, better human-readable CLI output.

**Done when (acceptance).**

- A user can use Boss for a full workday, including normal crashes
  and restarts, without losing work or needing to manually clean
  workspace state.

**Depends on.** Phase 9.

**References.** [`work-execution`](../../designs/work-execution.md)
Phases F + G.

---

## Cross-cutting concerns

These run across all phases, not on a separate timeline.

### Testing

- Unit tests for engine logic, CLI parsing, hook normalization,
  state-machine transitions.
- Integration tests with a temp engine + temp DB + tempfile cube
  instance. Cover: full work-CRUD flow, subscription propagation,
  scheduler dispatch under capacity, lease lifecycle, restart
  reconcile.
- Smoke tests against a real `claude` binary, gated to local
  development (CI doesn't have Claude credentials).
- Manual test: an unverified `claude` permission-prompt
  Notification trigger after Phase 8 lands, to retire the
  screen-scrape backup if Notification fires reliably in interactive
  PTY mode.

### Observability

- Engine: structured logs per RPC, per execution transition, per
  hook event. `BOSS_LOG=debug` to enable verbose.
- Engine: `boss engine status` shows uptime, active executions,
  queued executions, worker pool occupancy.
- Per-execution dir on disk is the ground truth for inspection.

### Documentation

- Each phase ends with: updated CLI `--help`, updated screenshots if
  UI-touching, updated cross-references between design docs.
- Worker-facing `.claude/CLAUDE.md` template lives in the engine
  source tree and is the canonical reference for what Boss tells
  workers.

### Error handling and surfaces

- Every RPC has a typed error code (`not_found`, `conflict`, etc.)
  per [`work-cli`](../../designs/work-cli.md).
- Cube command failures bubble up as engine errors with the cube
  exit code preserved.
- The engine never silently drops a state mutation; either it
  commits + publishes or it returns an error.

## Phase ordering rationale

The order is not arbitrary:

- Phases 1–3 are user-visible improvements that don't require
  workers to function. They make the planning surface usable and
  multi-client.
- Phases 4–5 add the execution substrate without claude actually
  running yet. This separates "scheduler logic correct?" from
  "worker integration correct?", which makes each problem
  individually debuggable.
- Phase 6 is the big vertical slice that brings claude into the
  picture. It's risky and load-bearing; ship it once the substrate
  is stable.
- Phase 7 adds the coordinator. It needs workers in place to be
  meaningful, but it's largely additive on top of phase 6.
- Phase 8 closes the human review loop. Doable earlier in
  principle, but more useful once the rest of the loop is real.
- Phases 9–10 are the operational polish that takes Boss from
  "demoable" to "usable for a workday."

Phases 1–3 can ship into the existing single-agent PoC without
touching the agent path. Phase 6 is the cutover point where the old
transcript model is removed.

## What's deferred past V2

- Native PR diff / comment / merge UI.
- Multi-product workers; cross-product tasks.
- Pre-emption.
- Dynamic worker pool sizing.
- Webhooks (all GitHub state polled in V2).
- ACP / SDK custom-client workers.
- Cube `change *`, `stack *`, `pr *`, `graph`, `doctor` commands
  (Boss V2 drives `jj` / `gh` / `git` directly inside leased
  workspaces).
- Delta-since-sequence subscription updates (V2 ships full state
  replay).
- Cross-host / remote execution.

## Related docs

- [`designs/main`](../../designs/main.md) — canonical V2 spec.
- [`designs/v2-design-risks`](../../designs/v2-design-risks.md) —
  decision log behind the spec.
- [`designs/work-taxonomy`](../../designs/work-taxonomy.md)
- [`designs/work-execution`](../../designs/work-execution.md)
- [`designs/work-kanban`](../../designs/work-kanban.md)
- [`designs/work-cli`](../../designs/work-cli.md)
- [`designs/work-subscriptions`](../../designs/work-subscriptions.md)
- [cube design](../../../../cube/docs/main.md)
- [cube remaining-work](../../../../cube/docs/remaining-work.md)
