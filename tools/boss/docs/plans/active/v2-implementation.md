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
| 4     | Execution layer + cube V2 prereqs              | 🟢 named deliverables shipped — cube driver covers `status` / `heartbeat` / `force-release`; live callers land in Phases 8-9 |
| 5     | ExecutionCoordinator                           | 🟢 named deliverables shipped — affinity-first/LRU worker selection, 8-worker hard cap, priority + FIFO ready queue, `executions.<id>` topic, `request_execution` RPC, `RunWaitState` enum |
| 6     | libghostty embedding and worker spawn          | 🟡 mostly shipped — 6a–6g (Phase-5-independent) + 6f-1..3 done; 6f-4..8 (engine→app RPC + spawn flow + cutover) remain |
| 7     | Boss session and bossctl                       | ❌ not started               |
| 8     | Review and attention                           | 🟡 schema + manual PR-URL only — no auto-detect, no poller, no Triage UI, no re-engage |
| 9     | Resume and continuity                          | ❌ not started               |
| 10    | Transcripts and hardening                      | ❌ schema columns only       |
| 11    | Bazel `GhosttyKit` integration                 | ❌ not started — hard prereq for shipping the post-6f Bazel `.app` |

The biggest remaining piece in Phase 6 is the engine→app pane RPC
chain (6f-4..8): protocol wire types, engine-side dispatch, app-side
pane allocator, ExecutionCoordinator wiring, and PoC chat-code
cutover. Until those land, the engine ingests hook events but no
worker is ever spawned by the coordinator. Phases 7 and 9 still
effectively wait on Phase 6 closing. Phase 8 has the data model but
none of the live PR / poller / triage logic. Phase 10 hardening is
essentially untouched. **Phase 11** is a build-system gap exposed
by Phase 6a: the SwiftPM build includes the libghostty-driven Agents
grid, but the Bazel build (the documented `bazel run //tools/boss/
app-macos:BossMacApp` production path) excludes `Sources/Ghostty/`
and falls through to a placeholder. Must land before — or as part of
— the 6f cutover, otherwise the Bazel-built app loses Agents-mode
functionality post-cutover.

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

### Phase 2: Multi-client subscriptions — 🟢 shipped

**Goal.** Make work mutations from any client propagate to all
connected clients in real time.

**Status (done).**

- `ServerState` with `WorkDb`, `TopicBroker`, `AtomicU64`
  `work_revision` (`engine/src/app.rs:207-244`).
- `subscribe` / `unsubscribe` RPCs (`protocol.rs:79-84`) and
  `Subscribed` / `Unsubscribed` responses (`protocol.rs:185-190`)
  — both directions wired.
- Topics `work.products` and `work.product.<id>` defined and
  published (`protocol.rs:9-12`; publish sites across the work
  mutation handlers in `app.rs`).
- `publish_work_invalidation()` fires after DB writes commit
  (`app.rs:1394-1458`).
- `work_revision` exposed on **read** responses too, via
  `send_response_with_revision` (`ListProducts`/`ListProjects`/
  `ListTasks`/`ListChores`/`GetWorkItem`/`GetWorkTree` in
  `app.rs:799-905, 1080`).
- **Bounded outbound queue, coalescing, slow-client disconnect**
  (PR #137). Per-session queue capped at `MAX_SESSION_QUEUE = 256`
  (`app.rs:253`); same-topic invalidations are coalesced into a
  single pending envelope (`app.rs:1793-1845` covers the unit
  tests); a session whose queue overflows is dropped rather than
  blocking the publisher (`app.rs:297-310`).
- macOS app subscribes on connect and refetches on invalidation
  (`app-macos/Sources/ChatViewModel.swift`,
  `app-macos/Sources/EngineClient.swift`).
- End-to-end test for the "Done when" criterion lives at
  `engine/tests/work_crud.rs::cli_status_update_propagates_to_subscriber_within_one_second`
  — drives a `WorkItemPatch { status: "active" }` from one client
  and asserts a second subscribed client receives the matching
  `work_invalidated` event in under 1s.

**Done when (acceptance, kept for the record).**

- A `boss task update <id> --status active` from a terminal causes
  the macOS app's Work tab to update within ~1s without manual
  refresh. Verified by the integration test above.

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

### Phase 4: Execution layer plumbing + cube V2 prereqs — 🟢 named deliverables shipped

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
- Cube driver `CubeClient` trait + `CommandCubeClient` cover
  `ensure_repo`, `lease_workspace`, `create_change`,
  `release_workspace`, `workspace_status`, `heartbeat_lease`, and
  `force_release_lease` (`engine/src/coordinator.rs`). Status returns
  a typed `CubeWorkspaceStatus` struct (state + lease + holder + task
  + lease epochs) so the Phase 9 reconcile can do a three-way compare
  without reparsing JSON.

**Pending.**

- No live caller for `heartbeat_lease` yet — the timer that pings
  every 5 minutes for held leases lands with Phase 9.
- No live caller for `force_release_lease` yet — engine-side reconcile
  on startup that releases orphan leases lands with Phase 9.
- No live caller for `workspace_status` yet — used by the same Phase 9
  reconcile loop. The driver method is available so Phase 9 doesn't
  block on driver work.
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

### Phase 5: ExecutionCoordinator — 🟢 named deliverables shipped

**Goal.** Add the server-global scheduler that turns work-item
events into runs.

**Status (done).**

- `ExecutionCoordinator` with `kick()` entry point and internal
  `run_scheduler` loop (`coordinator.rs`); construction supports an
  optional `ExecutionPublisher` via `with_publisher()`.
- Auto-creation of execution stubs by work-item kind (project →
  `project_design`, chore → `chore_implementation`, task →
  `task_implementation`) via `reconcile_work_item_execution()` in
  `work.rs`, fired from `publish_work_invalidation` after every
  work mutation.
- `WorkerPool` enforces a hard 8-worker cap
  (`MAX_WORKER_POOL_SIZE`); larger configs are clamped with a
  warning. Default pool size remains config-driven via
  `BOSS_WORKER_POOL_SIZE`.
- Worker selection is affinity-first via
  `preferred_workspace_id`, then LRU among idle workers. Workers
  record the cube workspace they last ran in on release; the
  scheduler passes `preferred_workspace_id` from the execution
  through to `cube workspace lease --prefer …` so cube reuses the
  same workspace when available.
- Priority + FIFO-within-priority ready queue: executions carry an
  integer `priority` column; `list_ready_executions` orders by
  `priority DESC, created_at ASC, id ASC`. Higher-priority work
  preempts older queued work as soon as a worker frees up.
- `RunOutcome::wait_state` is now a `RunWaitState` enum (`Terminal`
  / `WaitingDependency` / `WaitingHuman` / `WaitingReview` /
  `WaitingMerge`). The coordinator releases the cube lease for
  `Terminal` and `WaitingDependency` and retains it for the three
  human-/review-/merge-bound states, matching the V2 design.
- `executions.<id>` topic publishing: `BrokerExecutionPublisher`
  fans an `ExecutionInvalidated` `TopicEventPayload` through the
  shared `TopicBroker` whenever an execution transitions
  (`execution_started`, `execution_run_completed`,
  `execution_run_failed`, `execution_start_failed`). Each
  invalidation bumps the shared `work_revision`.
- `request_execution(work_item_id, opts)` RPC
  (`FrontendRequest::RequestExecution`,
  `FrontendEvent::ExecutionRequested`) creates or refreshes a
  ready execution for the work item with optional priority and
  preferred-workspace overrides, then kicks the coordinator.
- Run lifecycle is unchanged in shape (start → active → end) but
  end-state mapping is enum-driven; `start_execution_run` now also
  records `cube_workspace_id` so the engine can correlate
  workspaces across reconnects in later phases.

**Pending.** No deliverables remain for Phase 5 itself. Live
exercise of the new RPCs (a `boss work start` CLI surface,
re-engagement after review) ships as part of Phases 7–8.

**Done when (acceptance, kept for the record).**

- Creating a task via CLI causes the engine to autonomously create
  an execution, lease a cube workspace, and update execution state
  — even though no `claude` is yet running. The paper trail is
  observable in SQLite + per-execution files.

**Depends on.** Phase 4.

**References.** [`work-execution`](../../designs/work-execution.md)
Phase C; [`v2-design-risks`](../../designs/v2-design-risks.md) R5.

---

### Phase 6: libghostty embedding and worker spawn — 🟡 mostly shipped

**Goal.** End-to-end vertical slice: engine spawns real `claude`
workers in libghostty panes, workers do work, events flow back.

**Status (in `main`).**

- Substrate from earlier phases: execution and run rows, cube
  driver, scheduler with affinity-first/LRU and 8-worker hard cap.
- All Phase-5-independent slices (6a–6g) shipped:
  - 6a: libghostty embedding in `app-macos` Agents mode (PR #138).
  - 6b: `boss-event` shim binary at `tools/boss/event-shim/`
    (PR #143).
  - 6c: events socket scaffold + `LOCAL_PEERPID` lookup (PR #145).
  - 6d: `WorkerEvent` enum + hook normalizer in `boss-protocol`
    (PR #140).
  - 6e: CLAUDE.md / settings.json templating in `boss-engine`
    (PR #141).
  - 6g: transcript tail-watcher in `boss-engine` (PR #142).
- First three Phase-5-dependent slices (6f-1..3) shipped:
  - 6f-1: `WorkerRegistry` with `proc_pidinfo` ancestor walk
    (PR #146).
  - 6f-2: events socket annotates `IncomingHookEvent.run_id` via
    the registry (PR #150, originally #147 — re-cut after the 6f-1
    base branch was deleted on merge).
  - 6f-3: engine `serve()` binds the events socket and runs the
    accept loop on startup (PR #151, originally #148 — same
    re-cut reason).
- Engine→app pane RPC design landed in
  [`designs/engine-app-rpc`](../../designs/engine-app-rpc.md)
  (PR #149).

**Pending.** The remaining 6f sub-slices wire the design's RPC
chain end-to-end. Each is a self-contained PR.

- **6f-4: protocol additions.** `RegisterAppSession` request,
  `EngineRequest` event variant, `EngineResponse` request variant,
  `EngineToAppRequest` / `EngineToAppResponse` enums in
  `boss-protocol`. Wire types only; no engine or app implementation.
- **6f-5: engine-side dispatch.** `ServerState` tracks the registered
  app session, pushes `EngineRequest` events to it, awaits matching
  `EngineResponse` requests with timeout. Pending-request map keyed
  by request id; on app-disconnect, every pending sender resolves as
  `AppDisconnected`.
- **6f-6: app-side pane allocator.** Refactor
  `WorkersWorkspaceModel` to support on-demand allocation (currently
  pre-spawns 8). Wire `EngineRequest` handling. Implement shell-pid
  lookup via `proc_listpids` (best-effort; brittleness softened by
  the registry's ancestor walk).
- **6f-7: spawn-flow wiring.** `ExecutionCoordinator` calls
  `engine.spawn_worker_pane()` when a run starts; registers the
  returned `shell_pid`; releases the pane on terminal state. This
  is the end-to-end vertical slice that closes Phase 6's acceptance
  criterion.
- **6f-8: PoC chat-code cutover.** Remove the chat-style Agents UI
  references in `app-macos`. Boss panel in Work mode keeps using
  `ChatViewModel` for now — Phase 7 replaces it with a Boss
  libghostty pane.

**Done when (acceptance).**

- Human can run `boss task create --name X` from terminal; engine
  picks up the task, leases a workspace, spawns `claude` in a
  libghostty pane, the worker does the work and opens a PR. Engine
  observes the full lifecycle via WorkerEvents.

**Build-system gap (tracked as Phase 11).** The 6a SwiftPM build
links `GhosttyKit` and ships the live grid, but the Bazel build
(the documented `bazel run //tools/boss/app-macos:BossMacApp`
production path) excludes `Sources/Ghostty/*.swift` via a
single-level glob and falls through to a placeholder via
`#if canImport(GhosttyKit)`. After 6f cutover, the Bazel-built
`.app` would have a non-functional Agents tab. Phase 11 closes
this gap and must land before — or as part of — 6f.

**Depends on.** Phase 5 for 6f only; 6a–6e and 6g do not depend on
Phase 5. Cube V2 prereqs already landed in Phase 4. 6f also depends
on Phase 11 (Bazel `GhosttyKit` integration) for the production
build path.

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

### Phase 11: Bazel `GhosttyKit` integration — ❌ not started (prereq for 6f)

**Goal.** Make the Bazel-built `.app` ship the same libghostty-driven
Agents grid as the SwiftPM build, so the documented production path
(`bazel run //tools/boss/app-macos:BossMacApp`) keeps working through
and after the Phase 6f cutover.

**Why this is its own phase.** Phase 6a got the Workers grid into the
app via SwiftPM by adding `GhosttyKit` as a `binaryTarget` in
`Package.swift` and excluding `Sources/Ghostty/*.swift` from the
Bazel `swift_library` glob. That was deliberate: integrating the
xcframework into Bazel is a non-trivial build-system problem
(per-developer `zig` build, ~200 MB binary, per-arch slices), and
keeping it out of the 6a PR avoided a multi-track scope creep.

That deferral has a cost: until Phase 11 lands, the Bazel build of
the macOS app falls through to a placeholder for Agents mode. The
Phase 6f cutover removes the chat-based PoC Agents UI entirely; once
that lands without Phase 11, the Bazel `.app` has no working Agents
mode at all. So Phase 11 is a hard prereq for 6f.

**Status (done).** Nothing.

- `tools/boss/app-macos/Package.swift` declares
  `.binaryTarget(name: "GhosttyKit", path:
  "ThirdParty/GhosttyKit.xcframework")` (SwiftPM only).
- `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` builds the
  xcframework via the upstream `ghostty-org/ghostty` repo at
  `origin/main` using `zig@0.15`.
- `tools/boss/app-macos/BUILD.bazel`'s `srcs = glob(["Sources/
  *.swift"])` is single-level and excludes `Sources/Ghostty/*.swift`.

**Pending.**

- **xcframework production path under Bazel.** Pick one of:
  - **(a) Pinned binary in a separate repo or Git LFS / artifact
    registry.** Build the xcframework once per ghostty SHA + arch,
    publish to a place a Bazel `http_archive` (or similar) can fetch
    by SHA256. Pros: reproducible, no per-developer zig install.
    Cons: needs an artifact-publishing pipeline and a release process
    for new ghostty versions.
  - **(b) Bazel-driven build of the xcframework.** Wire `genrule` /
    `rules_zig` to clone ghostty at a pinned commit and run
    `zig build` inside the Bazel sandbox. Pros: single source of
    truth for ghostty version. Cons: every developer + CI worker
    needs zig + Metal toolchain available, and per-build cost is
    high.
  - **(c) Hybrid.** Bootstrap script writes to a known cache path;
    Bazel target imports from that cache via a developer-config
    macro; CI uses the same cache via a one-time setup step. Pros:
    incremental migration. Cons: more moving parts.
- **Bazel rule wiring.** Add the xcframework as an
  `apple_dynamic_xcframework_import` (or static equivalent) and link
  the resulting target into `boss_mac_app_lib`. Confirm the static
  archive (`libghostty-internal-fat.a`) and Carbon framework + libc++
  linker requirements come through the Bazel link.
- **Glob change.** Once `GhosttyKit` is on the Bazel side too, change
  the `srcs` glob in `BUILD.bazel` from `["Sources/*.swift"]` to
  `["Sources/**/*.swift"]` so `Sources/Ghostty/*.swift` is included.
- **Drop the `#if canImport(GhosttyKit)` gate** in `ContentView.swift`
  once both build paths import the module unconditionally.
- **Update README** to remove the bootstrap-required dance for the
  Bazel path (SwiftPM may still need it for fast iteration unless we
  wire a SwiftPM ↔ Bazel shared cache).

**Done when (acceptance).**

- `bazel build //tools/boss/app-macos:BossMacApp` succeeds without
  the developer first running `bootstrap-ghosttykit.sh`.
- The Bazel-built `.app`'s Agents tab shows the live 4 × 2 grid
  (same as `swift run BossMacApp`), not the placeholder.
- `bazel test`-able CI does not require a pre-installed `zig`
  (option (a) or (c) — option (b) accepts this cost).

**Depends on.** Phase 6a (which introduces the SwiftPM xcframework
path that Phase 11 unifies with).

**References.** Phase 6a PR; ghostty upstream `macos/GhosttyKit.xcframework`
build target; `rules_apple` xcframework import docs.

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
