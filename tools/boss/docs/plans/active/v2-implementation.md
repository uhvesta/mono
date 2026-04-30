# Boss V2 Implementation Plan

This is the phased build plan for delivering the Boss V2 spec
([`designs/main.md`](../../designs/main.md)). The design decisions
that shaped V2 are recorded in
[`v2-design-risks.md`](../../designs/v2-design-risks.md); this plan
turns those into ordered work.

This plan supersedes the earlier
[`swiftui-boss-v2.md`](swiftui-boss-v2.md), which was the brainstorm
that started V2 — left in place for history but no longer the active
plan.

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

## Where we start

Current state at the time this plan was written:

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

Each phase has: **Goal**, **Deliverables**, **Done when**, **Depends
on**, **References**.

---

### Phase 1: Engine and CLI foundations

**Goal.** Refactor the engine for multi-client use and ship the
public-facing `boss` CLI for work-taxonomy CRUD.

**Deliverables.**

- Engine refactor:
  - Split work-only runtime config from agent/ACP config; engine can
    start without `ANTHROPIC_API_KEY`.
  - Request envelope with `request_id` correlation; responses echo
    `request_id`.
  - Shared protocol crate (`boss-protocol`) used by engine and CLI.
  - Shared client crate (`boss-client`) wrapping socket discovery,
    auto-start, request/response, JSON decoding.
- New `boss` Rust binary at `tools/boss/cli/`.
- `boss product`, `boss project`, `boss task`, `boss chore`
  subcommands with `create / list / show / update / move / delete`
  and `reorder` for project tasks.
- `boss engine status / start / stop`; `--no-autostart`.
- `--json`, `--quiet`, `--no-input` flags; stable exit codes; TTY
  prompts for interactive create flows.

**Done when.**

- `boss product create --name X --json` writes through the engine and
  emits stable JSON.
- A second concurrent client can connect to the engine without
  breaking the first.
- All work CRUD flows work end-to-end against a temp engine in tests.

**Depends on.** None — builds on existing PoC code.

**References.** [`work-cli`](../../designs/work-cli.md);
[`work-subscriptions`](../../designs/work-subscriptions.md) Phase 1.

---

### Phase 2: Multi-client subscriptions

**Goal.** Make work mutations from any client propagate to all
connected clients in real time.

**Deliverables.**

- Shared `ServerState` owned by `run_server`, with `WorkDb`,
  `AgentRegistry`, `SessionRegistry`, `TopicBroker`.
- `subscribe` / `unsubscribe` RPCs and `subscribed` / `unsubscribed`
  responses.
- Topics: `work.products` and `work.product.<id>`.
- Engine publishes invalidation events from work mutations after the
  DB write commits.
- Monotonic `work_revision`, exposed in subscribe responses and read
  responses.
- Bounded outbound queue per session; coalescing of same-topic
  invalidations; disconnect on slow client.
- macOS app subscribes on connect and refetches on relevant
  invalidations.

**Done when.**

- A `boss task update <id> --status active` from a terminal causes
  the macOS app's Work tab to update within ~1s without manual
  refresh.

**Depends on.** Phase 1.

**References.**
[`work-subscriptions`](../../designs/work-subscriptions.md).

---

### Phase 3: Kanban Work tab

**Goal.** Replace the tree-first Work tab with a kanban board.

**Deliverables.**

- Frontend state: `selectedProductID`, `selectedProjectFilterIDs`,
  `includeChores`, `showBlockedOnly`, `boardGrouping`,
  `selectedCardID`.
- Four fixed columns (Backlog / Doing / Review / Done) with status
  mapping per the design.
- Card primitives for tasks and chores; project label, blocked
  badge, PR link.
- Move actions between columns (menu first, drag-and-drop second).
- Quick-add affordance at top of Backlog.
- Project filters and project-grouping toggle in left sidebar.
- Inspector for create/edit on a selected card.

**Done when.**

- Human can manage all current work-taxonomy CRUD through the kanban
  board without dropping to the tree view.
- Status changes from `boss` CLI propagate live to the board.

**Depends on.** Phase 2.

**References.** [`work-kanban`](../../designs/work-kanban.md).

---

### Phase 4: Execution layer plumbing + cube V2 prereqs

**Goal.** Add durable execution / run / attention state and land the
cube features Boss will hard-depend on.

**Deliverables.**

- Cube V2 prerequisites (tracked separately under cube; Boss waits
  on these but does not block its own phase 4 plumbing):
  - `flock` around `claim_workspace`.
  - `cube workspace setup` engine with fingerprinted policies.
  - `cube workspace lease` auto-create from `--source` on pool
    exhaustion.
  - `cube workspace heartbeat`, release `--reason crash --keep-dirty`,
    `cube workspace force-release`.
- Engine: `work_executions`, `work_runs`, `work_attention_items`
  tables and CRUD (manual creation initially; no scheduler yet).
- Engine: transcript-path indexing from runs to on-disk locations.
- Engine: per-execution dir layout under
  `~/Library/Application Support/Boss/executions/<id>/`.
- Engine: cube driver subsystem — invokes
  `cube repo ensure / workspace lease / release / status / heartbeat
  / force-release` over subprocess JSON.

**Done when.**

- Engine can create an execution row, lease a workspace via cube,
  release it, and persist the full lifecycle in SQLite — all driven
  manually via CLI or test, no autonomy yet.
- Cube prereqs are merged.

**Depends on.** Phase 1. Cube prereqs must merge before Boss takes
the cube hard dependency in phase 5.

**References.** [`work-execution`](../../designs/work-execution.md)
Phase B; [cube remaining-work](../../../../cube/docs/remaining-work.md).

---

### Phase 5: ExecutionCoordinator

**Goal.** Add the server-global scheduler that turns work-item events
into runs.

**Deliverables.**

- `ExecutionCoordinator` component in engine, driven by an internal
  event bus.
- Reactions:
  - new project → create `project_design` execution.
  - new task → create `task_implementation` execution (or
    `waiting_dependency`).
  - new chore → create `chore_implementation` execution.
  - agent available + ready execution → start a run.
- Capacity enforcement: 8-worker hard cap; queue with priority +
  FIFO-within-priority.
- Worker selection: affinity-first via `preferred_workspace_id`,
  then LRU among free.
- `request_execution(work_item_id, opts)` RPC.
- Run lifecycle: start → active → end (terminal or wait state).
- Lease retain/release rules per execution wait state (retain on
  `waiting_human` / `waiting_review` / `waiting_merge`; release on
  `waiting_dependency` / terminal).
- Subscription topic `executions.<id>` published on state changes.

**Done when.**

- Creating a task via CLI causes the engine to autonomously create an
  execution, lease a cube workspace, and update execution state —
  even though no `claude` is yet running. The paper trail is
  observable in SQLite + per-execution files.

**Depends on.** Phase 4 (cube driver, execution tables).

**References.** [`work-execution`](../../designs/work-execution.md)
Phase C; [`v2-design-risks`](../../designs/v2-design-risks.md) R5.

---

### Phase 6: libghostty embedding and worker spawn

**Goal.** End-to-end vertical slice: engine spawns real `claude`
workers in libghostty panes, workers do work, events flow back.

**Deliverables.**

- macOS app: import the `tools/ghostty-proto` embedding pattern.
  Build a 1 + 8 pane layout in Agents mode. Each pane is a
  `TerminalSessionModel` bound to one engine-managed session id.
- Engine: spawns `claude` subprocess for each scheduled run,
  attached to a libghostty pane via app coordination (engine asks
  app to host a pane; app returns pane handle; engine wires stdio).
- Engine: writes per-lease `<workspace>/.claude/CLAUDE.md` (jj-first
  rules, do-not-touch-siblings advisory) before spawn.
- Engine: writes per-lease `<workspace>/.claude/settings.json` with
  hook config for SessionStart, UserPromptSubmit, PreToolUse,
  PostToolUse, Stop, Notification, SessionEnd.
- New `boss-event` shim binary: reads stdin, posts to engine events
  socket. Bundled with engine; written into worker's PATH per lease.
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

**Done when.**

- Human can run `boss task create --name X` from terminal; engine
  picks up the task, leases a workspace, spawns `claude` in a
  libghostty pane, the worker does the work and opens a PR. Engine
  observes the full lifecycle via WorkerEvents.

**Depends on.** Phase 5; cube V2 prereqs landed.

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R1, R2, R4; [`work-execution`](../../designs/work-execution.md).

---

### Phase 7: Boss session and bossctl

**Goal.** Add the coordinator role and the Boss-only CLI.

**Deliverables.**

- App: spawn a 1 Boss libghostty pane in addition to the 8 worker
  panes. Persistent docked Boss panel in Work mode.
- `bossctl` Rust binary at `tools/boss/cli/bossctl` (or shared crate
  with `boss`).
- `bossctl agents list / status / focus / send / interrupt / launch
  / stop / transcript`, `bossctl probe <id>`, `bossctl workspace
  summary`.
- `bossctl work start / cancel` aliases for symmetry with `boss`.
- Engine: control socket auth refinement — two trust roots (app pid
  + Boss session pid) via LOCAL_PEERPID subtree match; three RPC
  authorization tiers (user / app+Boss / Boss-only).
- Engine: probe model — Stop-boundary injection. Boss requests probe
  via `bossctl`; engine queues; on next worker `Stop`, engine
  injects probe text as the next prompt. The follow-up `Stop`
  produces a `ProbeReplied` event.
- Boss session bootstrap: a system prompt or session-init prompt
  teaching the coordinator contract (delegate, don't implement;
  auto-dispatch only inside `plan_and_start`; probe on low
  confidence).
- Worker spawn env hygiene: sanitized PATH excluding `bossctl`,
  fixed allowlist of env keys.

**Done when.**

- Boss session can fully drive a workflow:
  human types planning prompt → Boss decomposes → Boss runs
  `bossctl work start <id>` → engine picks up → worker runs → Boss
  observes via `bossctl agents status` → on completion, Boss reports
  to human.
- A worker that tries to invoke `bossctl` fails (PATH miss). A
  worker that tries the live RPCs over the socket fails subtree-match
  auth.

**Depends on.** Phase 6.

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R3, R5, R8.

---

### Phase 8: Review and attention

**Goal.** Close the human-in-the-loop review cycle.

**Deliverables.**

- Engine: PR detection — pattern-match worker `last_assistant_text`
  for GitHub PR URL; periodic `gh pr list --head <branch>`
  discovery using cube's deterministic branch names.
- Engine: GitHub poller — every 60s, `gh pr view --json state,
  mergedAt, statusCheckRollup, reviews, comments` for each
  execution in `waiting_review` or `waiting_merge`.
- Engine: attention items — created/updated when executions enter
  `waiting_human` / `waiting_review`; resolved on action.
- Engine: `request_re_engagement(work_item_id)` RPC: re-leases the
  workspace (with `preferred_workspace_id`), resumes the claude
  session via `--resume`, sends synthesized comments-as-prompt.
- App: Triage / Needs Attention surface in Work mode. Lists
  attention items with primary action ("Open PR" / "Re-engage
  worker" / "View blocker" / "Re-dispatch").
- App: Work-mode card detail shows PR URL, status ribbon, "Open in
  browser", "Re-engage worker (N comments)".
- Engine: detect `state: MERGED` → execution `completed`, lease
  released.

**Done when.**

- Worker opens a PR; human reviews on github.com; comments propagate
  to Triage; human clicks Re-engage; worker resumes and addresses
  comments; human merges on github.com; execution closes
  automatically on next poll.

**Depends on.** Phase 7.

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R7; [`work-execution`](../../designs/work-execution.md) Phase D.

---

### Phase 9: Resume and continuity

**Goal.** Survive crashes and restarts cleanly.

**Deliverables.**

- Engine: `claude --resume <session_id>` on worker crash; new run
  on the same execution, same workspace.
- Engine: startup three-way reconcile per `running` execution:
  SQLite ↔ cube lease state ↔ claude process state. Reattach where
  three-way OK; declare lost otherwise (execution → `abandoned`,
  cube force-release, audit log).
- Engine: cube lease heartbeat every 5 min for held leases.
- Engine: events socket reconnect handling — accept reconnects from
  workers whose engine restarted underneath them.
- App: full state replay on engine reconnect (not delta-since-seq
  yet).
- App: surface "These tasks were active when [engine restarted /
  machine restarted]; review and re-dispatch" affordance in Triage.
- Engine: OS-reboot policy — force-release all `boss/*` leases on
  startup; tasks that were `running` move to `queued`; no
  auto-resume on reboot.

**Done when.**

- Killing the engine while workers are running and restarting it
  results in the engine reattaching the live workers and continuing
  to receive their hook events. Killing a single worker mid-turn
  results in a clean `--resume` and the work continues.

**Depends on.** Phase 8 (so the failure cases include in-review
executions, not just running).

**References.** [`v2-design-risks`](../../designs/v2-design-risks.md)
R6; [`work-execution`](../../designs/work-execution.md) Phase E.

---

### Phase 10: Transcripts and hardening

**Goal.** Make Boss inspectable and operationally safe.

**Deliverables.**

- Engine: structured per-run transcripts at
  `~/.../executions/<exec-id>/runs/<run-id>/transcript.jsonl`,
  derived `transcript.md`.
- Engine: artifact directory per run.
- App: stale-runs and stale-leases visibility (listing executions
  with mismatch between cube and engine state).
- Engine: cube doctor integration — invoke `cube doctor` on demand
  from the app's diagnostics view.
- Engine: retry / abandonment flows; explicit "this execution
  failed; retry, abandon, or escalate?" decision surface.
- Screen-scrape fallback for `AwaitingInput`: if a worker pane shows
  a permission-prompt-shaped region for >5s without a `Notification`
  hook, emit a synthetic event.
- Polish: keyboard shortcuts, empty-state handling, error messages,
  better human-readable CLI output.

**Done when.**

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
