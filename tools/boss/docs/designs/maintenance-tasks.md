# Boss: Automations (scheduled maintenance work)

## Problem

Boss's work model assumes a human (or the Boss coordinator) decides *what* to do and *when*. Every task is filed by hand, dispatched off the kanban, and the deliverable is a merged PR. That is the right shape for feature work, but it leaves a whole class of work unserved: the recurring, low-stakes housekeeping that nobody wants to remember to file. "Fix clippy warnings." "Look for duplicated code and extract a helper if it makes sense." "Bump the dependencies that have a clean changelog." This work is *valuable* but *episodic* — most days there is nothing to do, and on the days there is, it should not jump the queue ahead of the human's real priorities.

Today the only way to get this is to remember to file a chore, periodically, forever. That is exactly the kind of standing instruction a machine should hold.

This doc designs **Automations**: a standing, triggered instruction that periodically asks "is there a concrete maintenance task to do right now?" and, if so, spawns a normal task to do it. Automations live *outside* the normal backlog — they are created and managed from a new top-level **Automations** tab, and the tasks they produce run in a dedicated 3-agent pool so they never contend with interactive work. The work they spawn is otherwise an ordinary task: it lands in a worker pane, produces a PR, and trampolines to GitHub review like everything else. Only its *origin* (an automation) and its *pool* (automations) differ.

## Goals

- A first-class **automation** entity: a standing instruction with a **trigger**, a **product** (and optional repo), a **standing-instruction prompt**, and an **open-task cap**.
- Initially one trigger type — a cron-like **schedule** — with a schema that is **open to other trigger types later** (event-driven, manual-only) without a migration to the core shape.
- A **two-phase execution model**: phase-1 *triage* decides whether concrete work exists right now and is allowed to **skip** the occurrence; phase-2 *execute* spawns a normal task and runs it to a PR.
- A **dedicated pool of 3 agents**, fully distinct from the main 8-worker pool, with an Agents-tab affordance to switch between the two.
- **Robust scheduling**: catch up after a missed fire (laptop was closed) unless the next fire is imminent, and **retry** rather than drop an occurrence when execution is transiently impossible (VPN down, remote unreachable).
- A per-automation **open-task limit** enforced at fire time so pending changes can't pile up.
- Automation-produced tasks are **excluded from the normal backlog/kanban** and surfaced only under the Automations tab.
- CLI verbs (`boss automation …`) and a SwiftUI **Automations** tab, both thin clients over the engine, which owns scheduling, pool accounting, and reconciliation.

## Non-goals

- **Trigger types other than `schedule`.** The schema is built to accept them (tagged kind + payload), but only the cron variant is implemented in v1. Event-driven triggers (e.g. "on every merge to main") are explicitly deferred.
- **Automations that span products or repos.** An automation belongs to exactly one product and targets exactly one repo per fire, mirroring the one-product-per-work-item rule in `work-taxonomy.md`.
- **Multi-task fan-out per fire.** A single triage run produces **at most one** task. If maintenance naturally splits, that is two automations (or two fires), not one fire spawning a batch. This keeps open-task-limit accounting and provenance 1:1-per-fire.
- **Auto-merge of automation PRs.** Produced tasks trampoline to GitHub review and a human merges, exactly like every other task. Nothing about the downstream lifecycle is special.
- **Pre-emption / dynamic pool sizing.** The automations pool is fixed at 3, the main pool stays fixed at 8 (`coordinator.rs` `MAX_WORKER_POOL_SIZE`). A long-running maintenance task is not killed to make room.
- **A general job scheduler.** This is not cron-as-a-service; the only thing an automation can do is run a triage agent against a standing instruction.

## Alternatives considered

### A1 — External scheduler (system `cron` / launchd) instead of an in-engine tick

Register each automation's cron expression with the OS scheduler; on fire, the OS runs `boss automation run <id>`.

Rejected. The engine is the only component that knows the open-task count, the automations-pool capacity, and whether the machine can currently reach the git remote — an OS-level cron firing blind would either ignore the open-task limit or have to re-implement the engine's accounting over the CLI. More decisively, Boss is a laptop app that is *frequently asleep*: launchd's catch-up semantics for missed wakeups are coarse and not configurable per the "skip-if-imminent" rule the brief requires. The engine already runs interval sweepers (`spawn_merge_poller`, the orphan/dead-PID sweeps in `app.rs`, all built on a shared `spawn_loop(... Duration ...)` helper) and already persists `next_due_at`-style bookkeeping for other pollers. An in-engine automation scheduler reuses that machinery and keeps all dispatch policy in one place. **Chosen: in-engine periodic scheduler loop.**

### A2 — Reuse the main worker pool with a priority/quota instead of a second pool

Keep one `WorkerPool`, tag automation executions low-priority, and reserve (or cap) some slots for them.

Rejected. The brief mandates isolation, and isolation by quota inside a shared pool is strictly more complex than a second pool: `claim_worker`/`release_worker` (`coordinator.rs`) would grow a per-class accounting layer, and a burst of maintenance work could still starve interactive work (or vice-versa) at the margins, inviting pre-emption — which is a non-goal. A second `WorkerPool::new(3)` is the same proven primitive (`Vec<WorkerSlot>` behind a mutex) instantiated twice; routing is a single branch in `drain_ready_queue`. The cost is 3 extra panes in the app, which the brief already calls for. **Chosen: a separate, fixed automations pool of 3.**

### A3 — One hidden "Maintenance" project owns all automation tasks (the brief's open question 1)

The brief sketches a single hidden project that owns every automation-produced task, with backlog exclusion keyed on project membership.

Rejected as the *primary* mechanism (see Chosen approach → Provenance). A project is the wrong primitive here: the open-task limit is **per-automation**, so even with a shared project we still need a per-task `source_automation_id` to count correctly — the project adds nothing to accounting. And automation tasks are product-level housekeeping, structurally like chores (`kind = 'chore'`, `project_id = NULL`), not members of a feature project. Auto-creating a magic project per product just to hang tasks off it adds a lifecycle to manage (what is its status? does it show in `boss project list`?) for no benefit. **Chosen: provenance via a `source_automation_id` FK; exclusion keyed on that FK; no synthetic project.** The brief's "Maintenance tasks" framing is satisfied by the Automations *tab* grouping tasks by their automation, which is what a human actually wants to see.

## Chosen approach

The engine gains an `automations` table, a per-fire `automation_runs` history table, a `source_automation_id` provenance column on `tasks`, a second worker pool, and a periodic scheduler loop. The CLI gains a `boss automation` noun; the app gains an Automations tab and an Agents-tab pool switcher. Everything downstream of "a task was produced" is unchanged.

### Data model

#### `automations` table

```sql
CREATE TABLE IF NOT EXISTS automations (
    id                  TEXT PRIMARY KEY,             -- auto_<ts>_<n>
    short_id            INTEGER,                      -- per-product A-namespace (A1, A2…)
    product_id          TEXT NOT NULL REFERENCES products(id),
    name                TEXT NOT NULL,                -- display label
    repo_remote_url     TEXT,                         -- explicit target repo; NULL → product primary
    trigger_kind        TEXT NOT NULL,                -- 'schedule' (extensible discriminator)
    trigger_config      TEXT NOT NULL,                -- JSON payload, shape depends on trigger_kind
    standing_instruction TEXT NOT NULL,               -- the prompt
    open_task_limit     INTEGER NOT NULL DEFAULT 1,   -- per-automation open cap
    catch_up_window_secs INTEGER,                     -- override of engine default (see Scheduling)
    enabled             INTEGER NOT NULL DEFAULT 1,
    created_via         TEXT NOT NULL DEFAULT 'unknown',
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    -- bookkeeping, updated by the scheduler
    last_fired_at       TEXT,
    last_outcome        TEXT,                          -- mirrors latest automation_runs.outcome
    next_due_at         TEXT                           -- UTC RFC3339; computed from the cron + tz
);

CREATE UNIQUE INDEX IF NOT EXISTS automations_product_short_id_idx
    ON automations(product_id, short_id) WHERE short_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS automations_due_idx
    ON automations(enabled, next_due_at);
```

**Trigger representation — tagged `kind` + JSON payload.** This is the exact pattern Product already uses for external trackers: `external_tracker_kind TEXT` + `external_tracker_config` holding a `serde_json::Value` (`protocol/src/types.rs`, the `Product` struct). We mirror it so we add trigger types later without a schema migration. In protocol code the trigger is a serde-tagged enum:

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTrigger {
    Schedule { cron: String, timezone: String },   // timezone = IANA name, e.g. "America/Los_Angeles"
    // future: Event { event: String, filter: ... },
    // future: Manual {},
}
```

`trigger_kind` is the persisted discriminator (so `automations_due_idx` and the scheduler can filter cheaply); `trigger_config` is the serialized variant body. A non-`schedule` automation simply never matches the schedule scan and is inert until its trigger type is implemented.

The `Automation` struct in `boss-protocol` carries ≥8 fields, so per the repo builder convention it uses `#[derive(bon::Builder)]` with `#[builder(on(String, into))]`; `Option<T>` fields (`repo_remote_url`, `catch_up_window_secs`, `last_fired_at`, …) are builder-optional, `open_task_limit`/`enabled` carry `#[builder(default = …)]`, and the DB `map_automation` mapper uses an explicit struct literal (never the builder), matching the `map_task`/`map_product` rule in `work.rs`.

#### `automation_runs` table (run history)

Every fire — including no-ops and transient failures — records a row. This is the source of truth for the open-task limit's denominator is *not* here (that's a live count over `tasks`), but it is the audit trail the Automations tab shows.

```sql
CREATE TABLE IF NOT EXISTS automation_runs (
    id                  TEXT PRIMARY KEY,
    automation_id       TEXT NOT NULL REFERENCES automations(id),
    scheduled_for       TEXT NOT NULL,                -- the cron occurrence this run satisfies (UTC)
    started_at          TEXT NOT NULL,
    finished_at         TEXT,
    triage_execution_id TEXT,                          -- the phase-1 work_execution
    outcome             TEXT NOT NULL,                 -- see enum below
    produced_task_id    TEXT REFERENCES tasks(id),     -- set iff outcome = 'produced_task'
    detail              TEXT                           -- skip reason / failure detail (free text)
);

CREATE INDEX IF NOT EXISTS automation_runs_by_automation_idx
    ON automation_runs(automation_id, scheduled_for);
```

`outcome` is a string discriminator (matching the codebase's string-enum convention for `status`/`kind`/`blocked_reason`):

- `produced_task` — triage created a task (`produced_task_id` set); phase 2 is underway.
- `skipped` — triage ran and decided nothing actionable exists right now (a recorded no-op).
- `suppressed_at_limit` — the fire was due but the open-task count was already at the cap, so no triage ran. (Recorded so the UI can explain why an enabled automation looks idle.)
- `failed_will_retry` — triage could not execute (transient infra: cube lease failed, git remote unreachable). The scheduler will re-attempt with backoff; `scheduled_for` is preserved.
- `failed_gave_up` — retries exhausted; the occurrence is abandoned and the schedule advances to the next occurrence.

#### Provenance: `tasks.source_automation_id`

```sql
ALTER TABLE tasks ADD COLUMN source_automation_id TEXT REFERENCES automations(id);
CREATE INDEX IF NOT EXISTS tasks_source_automation_idx
    ON tasks(source_automation_id, status) WHERE source_automation_id IS NOT NULL;
```

A non-null `source_automation_id`:
1. **Links a produced task back to its automation** — enabling `boss automation tasks <id>` and the tab's per-automation task list.
2. **Drives backlog/kanban exclusion** — the work-tree RPC and the app filter out any task where `source_automation_id IS NOT NULL`.
3. **Routes the task's execution to the automations pool** — the dispatcher reads it to pick the pool.
4. **Is the denominator for the open-task limit** — `COUNT(*) WHERE source_automation_id = ? AND status IN ('todo','ready','doing','in_review','blocked')`.

Produced tasks keep `kind = 'task'` and `project_id = NULL`; they are product-level like chores. We do **not** introduce a `kind = 'maintenance'` — the kind discriminator is about *deliverable shape* (PR vs doc), and a maintenance task's deliverable is an ordinary PR. Provenance, not kind, is the right axis (this is the same reasoning `design-producing-tasks.md` Q1 used to reject a parallel boolean flag).

#### Short-id namespace: a new `A` prefix

Automations get their own per-product `A` namespace (`A1`, `A2`, …), allocated by the same `allocate_short_id`/`short_id_sequences` machinery (`work.rs`) used for `T`/`P`. They are **not** added to `resolve_friendly_work_item_id` (that resolver returns *work items*; an automation is not a work item). Instead `boss automation` verbs resolve `A<n>` within the automation namespace. Justification: reusing `T`/`P` would make `boss automation show T42` ambiguous, and automations are a genuinely distinct noun deserving a distinct prefix, consistent with `friendly-numeric-ids-for-work-items.md`.

### Repo selection (brief's "prompt-only vs explicit field" question)

**Recommendation: an optional explicit `repo_remote_url` field, authoritative for the cube lease, with the standing instruction as documentation only.** A product can have multiple repos (`multi-repo-work-modeling.md`); the engine must lease a *specific* cube workspace before the triage agent runs, so it needs the target repo as structured data, not buried in prose it would have to parse. If `repo_remote_url` is null, default to the product's primary repo. The standing instruction may still *mention* the repo for the agent's benefit, but the explicit field is what the engine acts on, and the produced task inherits it via the existing `tasks.repo_remote_url` per-task override.

### Two-phase execution

#### Phase 1 — Triage

When the scheduler decides an automation should fire (due, enabled, under cap, machine able), it creates a `work_execution` of a new kind `automation_triage`, **bound to the automation, not to a task**, and enqueues it for the automations pool. A new `automation_runs` row is opened with `outcome = failed_will_retry` as a pessimistic default (flipped on success), so a crash mid-triage leaves a retryable record.

The triage worker is spawned into a cube workspace for the automation's repo exactly like any worker (`schedule_execution` → `cube lease` → `SpawnWorkerPane`), but its rendered `CLAUDE.md`/initial input is a **triage preamble** the engine composes:

> You are a maintenance **triage** agent for automation `A<n>` on product `<product>`. Standing instruction: *"<standing_instruction>"*. Decide whether a **single, concrete, actionable** task can be derived from this instruction **right now** in this repo. You are explicitly allowed to conclude that nothing appropriate exists.
> - If there **is** work: create exactly one task with `boss task create --automation A<n> --autostart "<concise title>" --description "<what to do>"`, then end your final message with the line `automation: task <the-new-T-id>`. **Do not do the work or open a PR yourself** — a separate worker will execute the task.
> - If there is **nothing** appropriate: end your final message with `automation: skip — <one-line reason>`.

Outcome detection mirrors `PrDetector` (`completion.rs`) and the proposed `DesignDetector`: a new **`AutomationTriageDetector`** inspects the triage execution's final output on Stop:

- final line matches `automation: task <id>` **and** a task with that id and `source_automation_id = this` exists → `automation_runs.outcome = produced_task`, `produced_task_id` set. Phase 2 proceeds automatically because the produced task is `autostart`.
- final line matches `automation: skip — <reason>` → `outcome = skipped`, `detail = reason`. No task. This is the explicit, agent-authored skip — distinct from a failure (see Scheduling → transient detection).
- neither marker present (worker errored, was reaped, or produced no decision) → left at `failed_will_retry`; the scheduler re-attempts.

The triage agent creating the task via `boss task create --automation` (rather than the engine reading a manifest) is the cheap path: `create_task` already exists (`work.rs`), and the `--automation` flag is a thin addition that stamps `source_automation_id` and re-checks the cap transactionally (so even a misbehaving agent can't exceed the limit). We considered an engine-side manifest à la `design-producing-tasks.md`, but that feature is itself unbuilt; reusing `boss task create` keeps this design landable on today's primitives.

#### Phase 2 — Execute

The produced task is an ordinary `tasks` row with `source_automation_id` set, `autostart = true`, `project_id = NULL`, `repo_remote_url` inherited from the automation. The engine requests an execution for it (the normal `request_execution` path), but the dispatcher routes it to the **automations pool** because `source_automation_id` is non-null. From there the lifecycle is identical to any task: Doing → worker opens a PR → `PrDetector` flips it to `in_review` → human reviews/merges on GitHub → `done`. The Automations tab shows this lifecycle; the main kanban never sees the task.

### Pool model

A second `WorkerPool` instance, `automation_pool: WorkerPool::new(3)`, lives in `ServerState` beside the existing main pool. Size comes from config `BOSS_AUTOMATION_POOL_SIZE` (default 3), parallel to `BOSS_WORKER_POOL_SIZE` (default 8). Routing is one branch in `drain_ready_queue` / `schedule_execution`:

```text
pool = if execution.kind == "automation_triage"
        || work_item.source_automation_id.is_some()
       { &automation_pool } else { &main_pool };
let worker = pool.claim_worker(exec_id, preferred_workspace).await;
```

`claim_worker`/`release_worker`/`release_worker_and_kick` are unchanged — they operate on whichever pool instance they're called against. Pool exhaustion (`DrainOutcome::PoolExhausted`) is per-pool: an exhausted automations pool defers automation work without touching main-pool throughput, and vice-versa. The scheduler heartbeat already re-kicks; nothing new is needed there.

The two pools draw cube workspaces from the same cube pool (workspaces are repo-scoped and fungible); only the *slot/pane* accounting is separate. The app renders 8 panes for the main pool and 3 for the automations pool.

### Scheduling semantics

A new interval loop, `automation_scheduler`, is spawned in `app.rs` alongside the other sweepers using the shared `spawn_loop(... Duration::from_secs(30) ...)` pattern. Each tick, for each enabled automation with `trigger_kind = 'schedule'`:

1. **Compute occurrences.** Parse the cron expression with a cron crate (`croner` or `cron`), iterate in the stored IANA timezone via `chrono-tz`. `next_due_at` is stored as UTC. (Timezone/DST handling below.)
2. **Open-task-limit gate (enforced here, at fire time).** Count open produced tasks: `status IN ('todo','ready','doing','in_review','blocked')` with this `source_automation_id`. ("Open" = anything not `done`/`archived`/soft-deleted — confirming the brief's open question 5.) If count ≥ `open_task_limit`: do **not** fire; record one `suppressed_at_limit` run for the missed occurrence and advance `next_due_at` to the following occurrence. (We advance rather than hold so a capped automation doesn't fire a stampede the instant a task merges.)
3. **Due check + catch-up after a miss.** If `now >= next_due_at`:
   - Let `following` be the next occurrence strictly after `next_due_at`.
   - If `following - now <= catch_up_window` → the missed fire is **stale**; skip it (record nothing, or a `skipped`/`detail="stale, within catch-up window"` run for observability) and set `next_due_at = following`. This is the "we're already nearly at the next one" rule.
   - Else → **catch up**: fire the missed occurrence now.
   `catch_up_window` defaults to an **engine constant of 15 minutes**, overridable per automation via `automations.catch_up_window_secs`. Rationale: 15 min is long enough that a brief sleep/wake doesn't lose a daily job, short enough that a "2pm weekday" job missed until 1:50pm next day correctly skips to the real 2pm.
4. **Ability check + transient retry.** Firing creates the `automation_triage` execution. If the **pre-start** steps fail transiently — cube lease error, git remote unreachable (VPN down), product repo unresolvable — this is detected exactly as the existing dispatcher detects it: `schedule_execution` increments `WorkExecution::pre_start_failure_count` and sets `dispatch_not_before` to a backoff time (the same `dispatch_not_before` epoch-seconds gate already on `work_executions`). The run is `failed_will_retry`; the scheduler re-attempts after the backoff, **preserving `scheduled_for`** so we retry *this* occurrence, not skip to the next. Backoff is exponential (e.g. 1, 2, 4, 8 … min) capped at the `catch_up_window`; once the backoff would push past the next occurrence, the run becomes `failed_gave_up` and the schedule advances.

**Transient inability vs genuine phase-1 skip — the key distinction:**

| Signal | Meaning | Recorded as |
|---|---|---|
| Pre-start failure (lease/remote/VPN) — worker never produced a decision | Can't execute right now | `failed_will_retry` → retry same occurrence |
| Worker ran, ended with `automation: skip — …` | Agent decided nothing to do | `skipped` → advance schedule, **no** retry |
| Worker ran, ended with `automation: task <id>` | Found work | `produced_task` → phase 2 |
| Worker ran but emitted neither marker (crash/reap mid-run) | Ambiguous failure | `failed_will_retry` (bounded retries) |

The discriminator is **whether the worker reached a decision marker**. A skip is an explicit, agent-authored statement; everything that prevents the agent from *getting to* a marker is treated as transient and retried (bounded).

#### Timezone / DST handling (brief's open question 4)

Store the IANA timezone name (not a fixed UTC offset) alongside the cron, so "every weekday at 2pm" means 2pm *local* across DST transitions. Compute occurrences with `chrono-tz`:
- **Spring-forward gap** (a wall-clock time that doesn't exist, e.g. 02:30 on the skip day): advance to the next valid instant — the job runs once, slightly later, not zero times.
- **Fall-back overlap** (a wall-clock time that occurs twice): fire on the **first** occurrence only; dedupe by `scheduled_for` so the second 01:30 doesn't double-fire.
`automation_runs.scheduled_for` (UTC) is the dedupe key — a given occurrence fires at most once regardless of clock weirdness.

### CLI surface (`boss automation`)

Mirrors `boss task` / `boss project` conventions: clap subcommand enum, `--product` resolution, `--json` via the existing `print_entity` (direct serialization, no wrapper), `--no-input` for non-interactive use, `A<n>` selector resolution.

```
boss automation create  --product <p> --name <n> --instruction <text>
                         --schedule <preset|raw-cron> [--timezone <IANA>]
                         [--repo <url>] [--open-task-limit N] [--disabled]
boss automation list     [--product <p>] [--json]
boss automation show     <A-id|id> [--json]
boss automation update   <A-id> [--name|--instruction|--schedule|--timezone|--repo|--open-task-limit …]
boss automation enable   <A-id>
boss automation disable  <A-id>
boss automation delete   <A-id>
boss automation run      <A-id> [--force]        # fire triage now; --force bypasses the open-task cap
boss automation runs     <A-id> [--json]         # run history (automation_runs)
boss automation tasks    <A-id> [--json]         # produced tasks + their current status
```

`--schedule` accepts either a **preset keyword** (e.g. `weekday-2pm`, `nightly`, `weekly-mon-am`, `hourly`) that the CLI compiles to a cron expression, or a **raw cron string** (validated by the same crate the engine uses, so the CLI rejects garbage before it reaches the DB). `boss automation run` enqueues an immediate `automation_triage` execution out of band of the schedule; it still respects the open-task cap unless `--force`.

### App UI

- **New top-level Automations tab.** Add `case automations = "Automations"` to `NavigationMode` (`Models.swift`) and a branch in the `ContentView` ZStack + the segmented mode `Picker`. The tab lists automations for the selected product — each row showing name, human-readable schedule, enabled toggle, `open/limit` count, last outcome, and next-due time. The detail pane is the edit form + run history (`automation runs`) + produced-task list with live lifecycle state.
- **Schedule editor.** A presets dropdown ("Every weekday at 2pm", "Every night", "Weekly on Monday morning", "Hourly", "Custom…") that compiles to cron, a **raw-cron escape hatch** text field with inline validation, and a timezone picker (defaulting to the system zone). The compiled cron is shown read-only beneath the presets so the user can see what they get.
- **Agents-tab pool switcher.** A segmented control in `WorkersDetailView` toggling **Main (8)** ↔ **Automations (3)**, rendering the corresponding pool's panes. The engine exposes both pools' live worker states over the existing worker-state subscription, tagged by pool.
- **Backlog/kanban exclusion.** `computeVisibleWorkItems()` (`ChatViewModel.swift`) filters out any task with non-null `source_automation_id`; the engine's work-tree RPC also excludes them server-side so the app never receives them in the main board feed. The Automations tab fetches them explicitly via the per-automation query.

### Engine ownership (brief's open question 2)

Confirmed: the **engine owns** the scheduling tick, occurrence computation, open-task-limit enforcement, pool accounting, triage dispatch, outcome detection, and run-history writes. The app and CLI are thin clients that create/edit/enable automations and render state pushed over the existing subscription layer (`work-subscriptions.md`). No scheduling logic lives in the app.

### Migration

- `ALTER TABLE tasks ADD COLUMN source_automation_id` — null for every existing row; no behavioral change to existing tasks.
- `CREATE TABLE automations`, `CREATE TABLE automation_runs` — new, empty.
- Extend `short_id_sequences` usage to the `A` namespace (no schema change; the sequence table is per-product already).
- No change to `work_executions` schema — the new `automation_triage` kind is just a new string value, and `dispatch_not_before`/`pre_start_failure_count` already exist.

### Implementation breakdown (follow-up tasks, in dependency order)

These are the PR-sized tasks a human would file once this design is approved; stack where there is a hard dependency.

1. **Schema + protocol types.** `automations` / `automation_runs` tables, `Automation`/`AutomationRun` structs (bon builder), `AutomationTrigger` tagged enum, `tasks.source_automation_id` column + mapper update, `A` short-id allocation. (Foundation; everything depends on it.)
2. **Engine CRUD + RPC.** `create/list/show/update/enable/disable/delete` automation methods in `work.rs`, RPC handlers, open-task-count query. (Depends on 1.)
3. **CLI `boss automation`.** All verbs, preset→cron compilation, raw-cron validation, `A<n>` resolution. (Depends on 2.)
4. **Second worker pool + dispatch routing.** `automation_pool`, config var, routing branch, per-pool exhaustion. (Depends on 1; independent of 3.)
5. **Scheduler loop + occurrence math.** `automation_scheduler` spawn_loop, cron/tz computation, catch-up + skip-if-imminent, open-task gate, run-history writes. (Depends on 1, 4.)
6. **Triage execution + outcome detection.** `automation_triage` execution kind, triage preamble rendering, `boss task create --automation`, `AutomationTriageDetector`, transient-retry wiring on `dispatch_not_before`. (Depends on 4, 5.)
7. **App: Automations tab + schedule editor.** (Depends on 2; can start against mocked RPC.)
8. **App: Agents-tab pool switcher + backlog exclusion.** (Depends on 4 for pool state; exclusion depends on 1.)

## Risks / open questions

1. **Confirmed — no synthetic Maintenance project (open question 1).** Provenance is `tasks.source_automation_id`; exclusion and accounting key on it; the Automations tab provides the grouping a project would have. Reviewer: confirm you're happy dropping the "one hidden project" framing in favor of provenance-by-FK.
2. **Confirmed — engine owns everything (open question 2).** Scheduling, pool accounting, reconciliation all in-engine; app/CLI are thin. No concern flagged; noted for completeness.
3. **Triage prompt quality (open question 3).** The whole value of phase 1 hinges on the triage agent reliably emitting exactly one decision marker and *not* doing the work itself. The marker protocol (`automation: task <id>` / `automation: skip — …`) and the "do not open a PR" instruction are the guardrails; the cap re-check at `boss task create --automation` is the backstop against fan-out. Reviewer: is a final-line marker robust enough, or do we want a dedicated `boss automation triage-result` verb the agent must call (stronger contract, more new surface)?
4. **Timezone/DST (open question 4).** Resolved: store IANA tz, compute with `chrono-tz`, dedupe occurrences by UTC `scheduled_for`, fire-once on fall-back, run-slightly-later on spring-forward. Reviewer: confirm "fire on first occurrence of an ambiguous wall-clock time" is the desired fall-back behavior.
5. **Open-task definition + enforcement point (open question 5).** Resolved: enforced at fire time in the scheduler; "open" = `todo|ready|doing|in_review|blocked` (not `done`/`archived`/soft-deleted). Reviewer: should `blocked` count as open? (I say yes — a blocked maintenance task is still pending change pile-up — but it's a judgment call.)
6. **`open_task_limit` default.** I propose **1** (one outstanding maintenance change per automation at a time — the most conservative anti-pile-up stance). Some automations ("fix any clippy warning") might reasonably allow more. Reviewer: is a default of 1 right, or should it be higher with a per-automation override (which the schema already supports)?
7. **Cron crate choice.** `croner` (actively maintained, DST-aware, supports seconds field) vs `cron` (older, simpler). Picking one is a small but real dependency decision deferred to implementation task 1.
8. **Suppressed-at-limit advancement.** I chose to *advance* `next_due_at` past a suppressed occurrence rather than hold it, so a freshly-merged automation doesn't immediately fire its whole missed backlog. The alternative (fire once as soon as it drops below cap) is arguably more responsive. Reviewer: advance-and-skip vs fire-once-on-recovery?
9. **Pool starvation within automations.** Three slots shared across all automations means a slow maintenance task can block triage of others. With `open_task_limit` defaulting low and maintenance being episodic this is unlikely to bite, but if it does, the fix is per-automation fairness in `claim_worker` — explicitly out of scope for v1.
