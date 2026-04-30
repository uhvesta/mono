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
| 1     | Engine and CLI foundations                     | ✅ shipped                   |
| 2     | Multi-client subscriptions                     | ✅ shipped (verify back-pressure) |
| 3     | Kanban Work tab                                | 🟡 board done, three filters left |
| 4     | Execution layer + cube V2 prereqs              | ✅ shipped                   |
| 5     | ExecutionCoordinator                           | 🟡 mostly shipped — `request_execution` RPC + waiting-state semantics gap |
| 6     | libghostty embedding and worker spawn          | ❌ not started               |
| 7     | Boss session and bossctl                       | ❌ not started               |
| 8     | Review and attention                           | 🟡 attention-item schema only |
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

### Phase 1: Engine and CLI foundations — ✅ shipped

**Goal.** Refactor the engine for multi-client use and ship the
public-facing `boss` CLI for work-taxonomy CRUD.

**Status.** Complete. Evidence:

- Config split: `RuntimeConfig` separate from `AcpConfig`,
  `anthropic_api_key` optional
  (`engine/src/config.rs:9-49`).
- Request envelope with `request_id` correlation
  (`engine/src/protocol.rs:24-65`).
- Shared protocol module imported by engine and CLI; Swift client
  marshals the same envelopes
  (`app-macos/Sources/EngineClient.swift`).
- `boss` CLI binary at `tools/boss/cli/` with `product`, `project`,
  `task`, `chore` subcommands (Create / List / Show / Update / Move
  / Delete / Reorder) and `engine status / start / stop / active /
  paused / archived` (`cli/src/main.rs:82-123`).
- `--json`, `--quiet`, `--no-input`, `--no-autostart` global flags
  (`cli/src/main.rs:38-52`); TTY prompts on interactive create
  flows.

**Pending.** None known. Close this phase out once you confirm the
"Done when" criteria below still hold under a concurrent-client
smoke test.

**Done when (acceptance, kept for the record).**

- `boss product create --name X --json` writes through the engine
  and emits stable JSON.
- A second concurrent client can connect to the engine without
  breaking the first.
- All work CRUD flows work end-to-end against a temp engine in tests.

**References.** [`work-cli`](../../designs/work-cli.md);
[`work-subscriptions`](../../designs/work-subscriptions.md) Phase 1.

---

### Phase 2: Multi-client subscriptions — ✅ shipped (verify slow-client policy)

**Goal.** Make work mutations from any client propagate to all
connected clients in real time.

**Status.** Substantially complete. Evidence:

- `ServerState` with `WorkDb`, `TopicBroker`, `AtomicU64`
  `work_revision` (`engine/src/app.rs:207-244`).
- `subscribe` / `unsubscribe` RPCs and `Subscribed` / `Unsubscribed`
  responses (`engine/src/protocol.rs:79-84`).
- Topics `work.products` and `work.product.<id>` defined and
  published (`engine/src/protocol.rs:9-12`; publish sites at
  `engine/src/app.rs:346, 558, 694, 725, 756, 789, 823, 892`).
- `publish_work_invalidation()` fires after DB writes commit
  (`engine/src/app.rs:1394-1458`).
- macOS app subscribes on connect and refetches on invalidation
  (`app-macos/Sources/ChatViewModel.swift`,
  `app-macos/Sources/EngineClient.swift`).

**Pending.**

- Verify the bounded outbound queue + same-topic coalescing + slow-
  client disconnect policy is actually enforced end-to-end. The
  `TopicBroker` infrastructure is there, but I have not confirmed
  the back-pressure tests cover a deliberately-slow consumer. If
  the policy is missing or weak, add it.

**Done when (acceptance, kept for the record).**

- A `boss task update <id> --status active` from a terminal causes
  the macOS app's Work tab to update within ~1s without manual
  refresh.

**Depends on.** Phase 1.

**References.**
[`work-subscriptions`](../../designs/work-subscriptions.md).

---

### Phase 3: Kanban Work tab — remaining filters

**Goal.** Finish the kanban Work tab by adding the two filter
toggles and upgrading project filtering to multi-select.

**Status.** The board itself has shipped. Already in place in
`tools/boss/app-macos/Sources/{Models.swift, ChatViewModel.swift,
ContentView.swift}`: `selectedProductID`, `boardGrouping`,
`selectedCardID` state; four fixed columns with status mapping;
task/chore cards with project label, blocked badge, PR link;
drag-and-drop and menu-based move actions; quick-add in Backlog;
project-grouping toggle; card inspector for create/edit; real-time
updates via subscriptions; UserDefaults persistence.

**Deliverables (delta only).**

- Frontend state additions:
  - `includeChores: Bool` (default true) — when false, hide chore
    cards from the board.
  - `showBlockedOnly: Bool` (default false) — when true, show only
    items with `blocked == true`.
  - Replace single-select `selectedWorkProjectFilterID: String?`
    with `selectedProjectFilterIDs: Set<String>` to allow
    multi-project filtering.
- Sidebar UI:
  - "Include chores" and "Show blocked only" toggles in the left
    sidebar Options section.
  - Project filter list switches from radio-style (All / one
    project) to multi-select checkboxes; "All Projects" clears the
    set.
- Persistence: add the three new state fields to UserDefaults
  alongside the existing board state.

**Done when.**

- The two toggles correctly filter board contents in real time.
- Multiple projects can be selected simultaneously and the board
  reflects the union.
- The three new selections survive an app restart.

**Depends on.** Phase 2.

**References.** [`work-kanban`](../../designs/work-kanban.md).

---

### Phase 4: Execution layer plumbing + cube V2 prereqs — ✅ shipped

**Goal.** Add durable execution / run / attention state and land the
cube features Boss will hard-depend on.

**Status.** Complete. Evidence:

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
- Cube driver subsystem: `CubeClient` trait + `CommandCubeClient`
  shells out to `cube repo ensure / workspace lease / release /
  status` (`engine/src/coordinator.rs:33-180`).

**Pending.**

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

### Phase 5: ExecutionCoordinator — 🟡 mostly shipped, one RPC + one semantics gap

**Goal.** Add the server-global scheduler that turns work-item
events into runs.

**Status (done).**

- `ExecutionCoordinator` struct with `coordinate()`
  (`engine/src/coordinator.rs:177-260`).
- Auto-creation of execution stubs by work-item kind (project →
  `project_design`, task → `task_implementation`, chore →
  `chore_implementation`) (`engine/src/work.rs:1520-1580`,
  `create_missing_executions`).
- Capacity enforcement via `WorkerPool::new(cfg.worker_pool_size)`
  with configurable hard cap (`engine/src/config.rs:42-49`,
  `engine/src/app.rs:215`).
- Worker selection (affinity-first via `preferred_workspace_id`,
  then LRU) inside `WorkerPool` (`engine/src/coordinator.rs:177+`).
- Run lifecycle start → active → end with `RunOutcome`
  (`engine/src/coordinator.rs:500-560`, `engine/src/runner.rs`).
- Lease retain/release decisions driven by
  `outcome.release_workspace`
  (`engine/src/coordinator.rs:515-560`).
- `executions.<id>` topic published on state changes via
  `TopicBroker`.

**Pending.**

- `request_execution(work_item_id, opts)` RPC is not in the
  protocol. `grep -rn "request_execution\|RequestExecution"
  engine/src cli/src` returned nothing. Today the only entry point
  is `CreateExecution` driven manually. Either add the RPC or
  rename the design to match what we shipped (decide which).
- Lease retain/release is currently keyed off a single
  `release_workspace: bool` on `RunOutcome`, not the four
  `waiting_*` states the design calls for
  (`waiting_human` / `waiting_review` / `waiting_merge` retain,
  `waiting_dependency` releases). Audit whether the boolean is
  correct for every wait state we'll hit in Phase 8, or promote it
  to the explicit enum then.

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

### Phase 8: Review and attention — 🟡 schema only

**Goal.** Close the human-in-the-loop review cycle.

**Status (done).**

- `work_attention_items` table and CRUD
  (`engine/src/work.rs:90-99` for the `WorkAttentionItem` struct;
  CRUD across `work.rs:754-820`).
- Attention items are created when a run finishes in a state that
  needs human input (`engine/src/coordinator.rs:533-550`,
  triggered when the runner returns
  `execution_status: "waiting_human"` at
  `engine/src/runner.rs:178`).

**Pending.**

- Engine: PR detection — pattern-match worker
  `last_assistant_text` for GitHub PR URL; periodic
  `gh pr list --head <branch>` discovery using cube's deterministic
  branch names. (No `gh pr` invocations in engine source today.)
- Engine: GitHub poller — every 60s, `gh pr view --json state,
  mergedAt, statusCheckRollup, reviews, comments` for each
  execution in `waiting_review` or `waiting_merge`.
- Engine: extend attention-item creation to also fire on
  `waiting_review` (currently only the `waiting_human` path
  creates them).
- Engine: `request_re_engagement(work_item_id)` RPC: re-leases the
  workspace (with `preferred_workspace_id`), resumes the claude
  session via `--resume`, sends synthesized comments-as-prompt.
- App: Triage / Needs Attention surface in Work mode. Lists
  attention items with primary action ("Open PR" / "Re-engage
  worker" / "View blocker" / "Re-dispatch").
- App: Work-mode card detail shows PR URL, status ribbon, "Open
  in browser", "Re-engage worker (N comments)".
- Engine: detect `state: MERGED` → execution `completed`, lease
  released.

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
