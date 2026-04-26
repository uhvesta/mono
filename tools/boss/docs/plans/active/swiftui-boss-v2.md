# Boss SwiftUI V2 Plan

## Goal

Build a new version of Boss that keeps the strongest parts of the current
macOS SwiftUI shell, preserves the current product/project/task/chore model,
and introduces a Ghostty-based control room for live multi-agent execution.

The earlier idea was to remove the Rust backend entirely. That is no longer a
goal. If keeping the Rust backend makes the durable work model, CLI surface,
and execution tracking easier to preserve, that is acceptable and currently
preferred.

The new system is centered around one "Boss" Claude session that coordinates a
fixed set of worker-agent Claude sessions. The Boss session should not do work
itself. Its job is planning, delegation, monitoring, and aggregation.

## Product Shape

### Core experience

- The main macOS app remains the primary surface.
- The `Work` mode remains the main place the human interacts with Boss.
- The Boss terminal/panel in `Work` is special and remains the primary command
  surface.
- The `Agents` mode becomes a live control room built out of embedded Ghostty
  panes, but it is secondary and mostly operational.
- There is one dedicated Boss Claude terminal and eight worker Claude
  terminals.
- Workers are shown in a fixed `2 x 4` tiled grid.
- The Boss session can inspect and control workers through a local control
  surface exposed by the app.

### What we keep from the current app

- SwiftUI app shell.
- Split-view information architecture.
- Segmented top-level mode switch (`Agents` / `Work`), unless we later decide
  the Boss control room needs its own top-level tab.
- Boss-specific framing in the UI, including status chips, selection state,
  and room for future work-management integration.
- The special Boss panel in `Work` as the main human-facing interaction point.
- The current project and task tracking model.
- The ability for Boss to create and manage work items.
- The existing `boss` work CLI direction and engine-backed persistence model.

### What we remove or simplify

- ACP as the core app architecture for the new live terminal control room.
- The current "chat transcript as the main primitive" approach for workers.
- Any requirement that live terminal orchestration be mediated through the same
  frontend model used for the old transcript-driven PoC.

### Current recommendation

Adopt a hybrid architecture:

- keep Rust for durable work state, work CLI, and execution tracking where it
  already exists or is close to existing designs
- move the live agent-control-room experience into the SwiftUI app with
  embedded Ghostty terminals
- make the Boss terminal capable of both:
  - managing work items through the existing `boss` CLI surface
  - coordinating worker terminals through a new control surface
- keep the human mostly interacting through the `Work` mode Boss panel rather
  than through the raw agent panes

## Proposed V2 Architecture

### App layers

1. `BossMacApp`
   - SwiftUI shell, window management, app lifecycle.
2. `BossWorkspaceModel`
   - Shared observable app state for Boss session, worker sessions, selection,
     status, and command routing.
3. `GhosttyRuntime`
   - Shared embedded Ghostty runtime for all panes.
4. `TerminalSessionModel`
   - Per-pane state for terminal title, cwd, renderer health, Claude status,
     launch lifecycle, and control metadata.
5. `BossControlService`
   - Orchestration layer for live worker control. This may live in Swift, Rust,
     or a narrow hybrid boundary depending on which implementation gives the
     cleanest integration with the existing engine.
6. `BossCommandBridge`
   - The concrete mechanism that makes the control surface available inside the
     Boss terminal session.
7. `BossEngine` or equivalent retained backend
   - Durable owner of products, projects, tasks, chores, work CLI behavior,
     and longer-lived execution records.

### Domain split

The recommended split is:

- SwiftUI app + Ghostty:
  - special Boss panel in `Work`
  - pane layout
  - embedded terminals
  - launch/focus/interrupt/send-to-pane controls
  - live Claude state monitoring
  - Boss control-room presentation
- Rust backend:
  - persistent work state
  - `boss` CLI for products/projects/tasks/chores
  - execution records, if we keep the execution model from the existing design
  - optional command routing backend if that ends up cleaner than duplicating
    protocol/state logic in Swift

### Session layout

- `1` Boss session.
- `8` worker sessions.
- All sessions are embedded Ghostty surfaces in one app-owned runtime.
- Boss and workers are launched by the app, even if the durable work model
  continues to live in the backend.

### Initial UI layout

Use the current Boss app shell with two distinct interaction layers:

- `Work` remains the primary planning and command surface
- `Agents` remains the lower-level live execution surface

#### Work mode

Keep the current high-level structure:

- work board / product + project + task UI
- special Boss panel docked in the work experience
- Boss status, execution summary, and selected-work context visible near the
  Boss panel

The Boss panel in `Work` is where the human normally asks for work to be
planned, decomposed, started, or monitored.

#### Agents mode

Change the `Agents` detail area to a control-room layout:

- top section: large Boss terminal
- secondary header: Boss status, worker summary, control status
- main section: fixed `2 x 4` grid of worker terminals
- optional side strip: selected worker details, alerts, or command results

This keeps raw worker terminals available for inspection and intervention
without making them the primary day-to-day interaction surface.

## Command and Control Surface

### Requirement

The Boss Claude instance needs a native way to query and control workers from
inside its terminal session, without depending on custom MCP installation.

It also needs continued access to the work-management surface so it can create,
update, inspect, and organize products/projects/tasks/chores the way Boss does
today.

The control surface also needs to support "start this work item now if an agent
is available" semantics, because some tasks should move directly from planning
to execution.

### Preferred approach

Expose an app-owned local CLI command, for example `bossctl`, only to the Boss
session.

This is additive, not a replacement for the existing `boss` CLI direction.
Recommended command split:

- `boss`
  - durable work and planning operations
- `bossctl`
  - live agent and terminal orchestration operations

Capabilities should include:

- `bossctl agents list`
- `bossctl agents status`
- `bossctl agents focus <id>`
- `bossctl agents send <id> --text ...`
- `bossctl agents interrupt <id>`
- `bossctl agents transcript <id>`
- `bossctl agents launch <id>`
- `bossctl agents stop <id>`
- `bossctl workspace summary`
- `bossctl work start <work-item-id>`
- `bossctl work status <work-item-id>`

The Boss session should be able to use both command families:

- `boss ...` for work items
- `bossctl ...` for worker control

This can be implemented as:

- a small executable bundled by the app, or
- a shell script shim that talks to a local Unix domain socket / named pipe /
  loopback HTTP service exposed by the app

The key requirement is not "CLI" specifically. The key requirement is:

- Boss Claude can invoke it from the shell
- workers cannot invoke it
- it has structured, scriptable output

### Isolation model

Only the Boss session gets the control command on `PATH`.

Worker sessions should launch in an environment that:

- does not include the Boss control command
- does not include app-internal control credentials
- does not include authority to mutate sibling sessions directly

Worker sessions may still have ordinary repo tooling on `PATH`; the isolation
requirement is about orchestration authority, not about removing normal coding
tools.

## Boss behavior contract

The Boss Claude instance must be bootstrapped with a strict operating contract:

- do not implement code directly
- do not edit files directly
- do not run project work yourself unless explicitly put into a fallback mode
- decompose work
- delegate work to workers
- monitor progress
- aggregate status
- create and manage work items when needed
- ask the human for decisions when coordination or prioritization is ambiguous

This should be enforced in two places:

1. launch/bootstrap prompt for the Boss session
2. command-surface design that makes delegation easier than direct work

The Boss is also allowed to actively probe workers when it needs better
confidence about execution state. It does not have to treat worker status as
purely passive telemetry.

## Intent Inference and Execution Policy

The Boss should infer whether a user request is ready for immediate execution
or should only become planned work.

### Immediate execution case

If a request appears:

- self-contained
- small enough to fit a normal agent task
- concrete enough to execute without a larger planning pass

then Boss should:

1. create the necessary project/task structure if needed
2. start execution immediately if an agent is available
3. otherwise leave the task queued / ready-to-start

Examples:

- a bounded bug fix
- a small feature with clear acceptance criteria
- a contained refactor

### Planning-first case

If a request appears to be part of a broader planning effort, scoping exercise,
or multi-part initiative, Boss should:

1. create or update the relevant project and task items
2. avoid starting execution automatically
3. present the planned work for review or later manual start

Examples:

- "let's scope this feature"
- "here are five things we probably need to do"
- early decomposition of a larger initiative

### Capacity rule

If all worker agents are busy:

- Boss may still create projects/tasks/chores
- Boss must not start new execution immediately
- Boss should mark the work as queued or ready and explain that execution is
  waiting on capacity

### Manual start rule

The human must also be able to start work items directly from the project/work
UI, subject to the same capacity constraints.

Manual start should:

- create an execution if one does not exist
- assign an available worker if one is free
- otherwise leave the work in a queued/ready state

### Suggested decision model

The first implementation does not need a perfect planner. A practical policy is
enough:

- Boss decides between:
  - `plan_only`
  - `plan_and_start`
- that decision is based on:
  - request size
  - whether the request is singular vs multi-part
  - whether required context already exists
  - current worker availability

This can start as explicit prompt/policy logic in Boss and later become a more
structured scheduler decision in the backend.

## Agent Probing and State Verification

Boss should be able to query workers before making important workflow
transitions.

### Why probing matters

Terminal liveness and heuristic Claude-state detection are useful, but they are
not enough to answer questions like:

- is this task actually complete?
- is the agent blocked but has not surfaced it clearly yet?
- is the work ready for review, or is the agent still cleaning things up?
- does the agent need another instruction before yielding?

For those cases, Boss should be able to ask the assigned agent directly.

### Probe model

Boss may issue a lightweight follow-up request to a worker such as:

- summarize current status
- report whether the task is complete
- report whether tests have run
- report whether a PR is ready
- report blockers or next steps

This should be treated as normal orchestration work, not as a special escape
hatch.

### Transition rule

Boss should avoid marking a work item as a more-advanced state such as:

- ready for review
- waiting human
- completed

unless one of the following is true:

1. the state is directly evidenced by structured app/backend state
2. the agent has explicitly reported that status
3. Boss has probed the agent and received a satisfactory response

### Initial implementation policy

The first implementation can keep this simple:

- Boss decides when confidence is low
- if confidence is low, Boss probes the assigned worker before changing state
- probe responses are visible in app state and optionally in the work history

This does not require a fully formal verification protocol on day one, but it
does require that Boss be able to interrogate workers on demand.

## State Model

### Per terminal session

- stable session id
- role: `boss` or `worker`
- display title
- working directory
- terminal readiness
- Claude presence
- Claude state: `starting`, `ready`, `working`, `awaiting_input`, `exited`
- renderer health
- last control action
- last observed activity timestamp
- last summary snippet

### Workspace state

- selected pane
- selected worker
- Boss health
- aggregate worker counts
- alerts / blocked workers
- command history
- app bootstrap state
- current product/project/task context
- work data synchronization state
- available worker count
- queued/runnable work items
- active execution assignments
- last probe result by worker
- probe-required execution states, if we decide to track them explicitly

## Monitoring Strategy

We should reuse what worked in the Ghostty prototype:

- screen-based detection of Claude readiness / working state
- prompt-region heuristics
- explicit detection of transient setup prompts such as workspace trust

But V2 should add a stronger side channel where possible:

- app-issued commands are tracked explicitly
- worker launch / interrupt / prompt-submit actions are recorded by the app
- the Boss control surface can return structured status independent of screen
  scraping
- Boss-issued probes and their responses are recorded as first-class events

This means Ghostty observation remains useful for UI liveness, but operational
state should increasingly come from app-owned models.

For work items and execution metadata, the source of truth should remain the
durable backend rather than screen scraping.

That backend should be able to answer:

- which work items are merely planned
- which are runnable
- which are queued waiting on capacity
- which are actively assigned to agents

It is also useful for the system to answer:

- when Boss last probed a given agent
- what status the agent last explicitly reported
- whether a transition like `ready for review` was inferred or confirmed

## Migration Strategy

### Phase 1: shell-preserving rewrite

- keep the current SwiftUI shell and mode switch
- introduce shared Ghostty runtime and embedded panes
- replace current `Agent` transcript model for live agents with
  terminal-session models
- show one Boss pane plus eight worker panes
- preserve existing Work mode behavior during this phase

### Phase 2: control surface

- implement `BossControlService`
- expose `bossctl` to the Boss session only
- support list/status/send/interrupt/focus operations
- log command activity in app state
- keep `boss` available for work-item management
- support `start work item` operations gated by worker availability
- support targeted worker probes from Boss

### Phase 3: Boss bootstrap contract

- launch Boss Claude with a dedicated bootstrap prompt
- make Boss read both:
  - the work CLI reference
  - the worker-control reference
- verify Boss uses workers instead of doing work locally
- verify Boss can still create and update work items correctly
- verify Boss chooses reasonably between `plan_only` and `plan_and_start`
- verify Boss probes agents before making uncertain workflow-state transitions

### Phase 4: Work-mode reintegration

- reconnect the `Work` mode only where needed for the new control-room flow
- preserve the existing work persistence model unless there is a strong reason
  to change it
- unify agent execution status with work-item status in the UI where useful
- add manual "start task/project work now" affordances in the Work UI

## Open Design Questions

### 1. Where should the Boss terminal live?

Current recommendation:

- Boss remains special in `Work`
- `Agents` exposes the lower-level terminal control room
- the Boss terminal may still appear in `Agents`, but that is secondary to its
  role in `Work`

Alternative:

- Boss gets its own mode or dedicated window

### 2. What is the control transport?

Candidates:

- bundled CLI over Unix domain socket
- bundled CLI over loopback HTTP
- direct file-based inbox/outbox queue

Current recommendation:

- bundled CLI over Unix domain socket with JSON output

Open sub-question:

- should this be implemented directly by the Swift app, or should it route
  through the retained Rust backend so all command surfaces live behind one
  service boundary?

### 3. How strict should worker isolation be?

Current recommendation:

- soft isolation first: PATH/env separation only
- stronger isolation later if needed

### 4. Should workers be fixed or dynamic?

You asked for a fixed initial shape. Current recommendation:

- V2 starts with exactly eight workers
- dynamic worker counts can come later

## First Implementation Slice

The first slice should prove the new architecture with the least moving parts:

1. keep the current Boss app shell
2. preserve the Boss panel in `Work` as the primary human interaction surface
3. replace `Agents` detail with one Boss Ghostty pane plus an `8`-worker grid
4. auto-launch Claude in all panes
5. preserve the existing work model and `boss` CLI availability
6. provide a minimal `bossctl` with:
   - `agents list`
   - `agents status`
   - `agents send`
   - `work start`
7. give `bossctl` only to the Boss pane
8. bootstrap Boss with:
   - "coordinate only; never do implementation work directly"
   - "you may create and manage work items through `boss`"
   - "start bounded work immediately when appropriate and capacity allows"
   - "probe agents when you need confidence before changing task state"

If that works, we can decide how much of the orchestration path belongs in
Swift versus Rust, without disturbing the existing work model.
