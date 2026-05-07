# Boss: Work Execution and Workspace Design

## Overview

Boss already has a durable planning model for products, projects, tasks, and
chores. What it does not yet have is a durable execution model for turning one
of those planned work items into a concrete agent attempt against an isolated
local checkout.

This design defines that execution layer.

The recommended shape is:

- Boss remains the source of truth for product/project/task intent.
- Cube becomes the local execution control plane for repository materialization,
  reusable workspaces, lease lifecycle, and workspace reset/setup.
- `jj`, `git`, `gh`, `bazel`, and repo-local tooling remain available inside a
  leased workspace; Cube does not replace them.

This design assumes Boss will eventually operate a fixed pool of long-lived
agents rather than an unbounded "create as many as you like" model.

Under that model:

- work items become eligible for execution,
- a coordinator assigns an available agent,
- the agent works until it reaches a wait or terminal state,
- the agent returns to the pool,
- the underlying work may remain open while waiting on a human, a PR merge, or
  a dependency.

The key new Boss concepts are:

- a long-lived **execution** that owns repo/workspace/PR context for one work
  item, and
- a shorter-lived **run** that represents the period a specific agent is
  actually occupied by that execution.

## Problem

Boss needs to coordinate multiple agents working on the same product at the
same time.

That creates a few concrete requirements:

- each agent needs a local isolated working copy,
- those working copies should be reusable so incremental builds stay warm,
- the system needs a safe lease model so agents do not trample each other,
- Boss needs enough execution state to understand what is running, what
  workspace it is using, and when that workspace should be released,
- stacked or related tasks should not force Boss or Cube to become a second
  version-control system.

The existing work taxonomy intentionally stops short of execution. It stores
`repo_remote_url` as canonical repository identity and explicitly leaves local
workspace coordination to a separate layer.

## Goals

- Give Boss a first-class execution model without collapsing planning and
  execution into one table.
- Use `repo_remote_url` as the stable repository identity Boss already knows.
- Keep one clear owner for each kind of state.
- Make leasing explicit and durable enough for crash recovery and operator
  inspection.
- Keep the minimum viable version narrow enough to ship before deep PR/stack
  automation exists.
- Support both isolated tasks and related tasks without requiring Cube to own
  the full dependency graph.

## Non-Goals

- Making Cube the source of truth for projects, tasks, or agent assignment.
- Making Boss the source of truth for workspace pool state.
- Sharing SQLite tables directly between Boss and Cube.
- Modeling full PR, branch, and commit graphs inside Boss in the first phase.
- Replacing raw `jj` or `git` usage inside the leased workspace.
- Solving distributed multi-host execution in the first phase.

## Existing Domain Boundary

The current Boss taxonomy is already close to the right shape:

- `Product.repo_remote_url` is the canonical repository identity.
- products and projects are planning concepts,
- tasks and chores are the smallest executable work units,
- local checkout and workspace concerns belong in a separate execution layer.

This design keeps that boundary intact rather than introducing local checkout
paths into the product or project records.

## Core Concepts

### 1. Product

The **product** remains the top-level software unit and the home of the
canonical repository remote URL.

For execution purposes, the only required repository input from Boss is:

- `repo_remote_url`

If a future product needs multiple repos, Boss can extend the product model
later. The first phase should stay single-repo-per-product.

### 2. Task / Chore

Tasks and chores remain planned work items.

They answer:

- what should be done,
- where it belongs in the product/project hierarchy,
- what its visible planning status is,
- which PR URL, if any, is associated with the work item.

They do **not** answer:

- which agent is currently executing it,
- which workspace is being used,
- whether the current execution attempt succeeded or failed.

Those belong to executions and runs.

### 3. Execution

An **execution** is the long-lived operational record for one project-design
job, task implementation, or chore implementation.

This is the record that owns execution context such as:

- which work item is being advanced,
- which repo it runs against,
- which workspace lease is currently attached,
- whether the work is still active, waiting, complete, or abandoned.

An execution should record at least:

- `id`
- `work_item_id`
- `kind`
- `status`
- `repo_remote_url`
- `cube_repo_id`
- `cube_lease_id`
- `workspace_path`
- `started_at`
- `finished_at`

Recommended execution kinds:

- `project_design`
- `task_implementation`
- `chore_implementation`

Recommended execution status values:

- `queued`
- `ready`
- `leasing`
- `running`
- `waiting_human`
- `waiting_review`
- `waiting_merge`
- `waiting_dependency`
- `completed`
- `failed`
- `abandoned`

Important distinction:

- a **task** is planned work,
- an **execution** is the long-lived operational owner of that work once Boss
  starts trying to advance it.

One work item may have multiple executions over time if the work is retried or
restarted.

### 4. Run

A **run** is one period of active agent occupancy against an execution.

Runs are deliberately shorter-lived than executions. A run answers:

- which agent is busy right now,
- when that agent started work,
- when that agent became free again,
- whether that agent run succeeded, failed, or yielded.

A run should record at least:

- `id`
- `execution_id`
- `agent_id`
- `status`
- `started_at`
- `finished_at`
- `error_text`
- `result_summary`

Recommended run status values:

- `starting`
- `active`
- `completed`
- `failed`
- `canceled`
- `yielded`

Important distinction:

- an **execution** answers "is this work still live?"
- a **run** answers "is an agent actively occupied by it right now?"

An execution may have multiple runs over its lifetime. This is what allows an
agent to become available again while the execution remains open and its
workspace lease stays attached.

### 5. Cube Repo Pool

Cube owns a **repo pool** for each canonical repository. In Cube terms this
includes:

- canonical remote URL,
- local source/materialization path,
- workspace root,
- workspace naming/prefix policy,
- integration branch,
- reset/setup policy.

Boss should not persist those local execution details as canonical product
metadata.

### 6. Cube Workspace Lease

A **workspace lease** is Cube’s exclusive claim on one reusable local
workspace.

Boss should treat the returned `lease_id` and `workspace_path` as opaque
execution handles:

- Boss stores them in the run record,
- Cube remains the source of truth for whether a lease is valid,
- the agent uses the workspace path while the run is active.

## Ownership and State Boundaries

### Boss Owns

- products, projects, tasks, and chores,
- task/project relationships and priorities,
- agent lifecycle and assignment,
- execution records and run history,
- the scheduler/coordinator that decides what becomes ready and which agent
  should pick it up,
- user-visible run status,
- any future task dependency graph such as "task B depends on task A",
- the decision to start, retry, cancel, or abandon an execution.

### Cube Owns

- repository materialization/cloning,
- repo-to-workspace pool configuration,
- local workspace discovery,
- lease acquisition and release,
- workspace reset/setup rules,
- stale lease inspection and repair,
- any local cache/setup metadata needed to keep workspaces reusable.

### Underlying Tools Own

- `jj`: local change graph, rebases, working-copy history,
- `git`: remote transport,
- `gh` / GitHub: PR state,
- `bazel` and repo-local tools: builds, tests, repo-specific automation.

### Explicit Rule

Boss and Cube must not share one database.

The integration contract should be command/API based. Boss stores only the
Cube identifiers and paths it needs to coordinate an execution. Cube stores
only the workspace/repo metadata it needs to manage local execution safely.

## Fixed Agent Pool and Coordination

### Fixed Agent Pool

This design assumes a fixed pool of reusable agents managed by Boss.

That means:

- Boss should not require the human to manually create a fresh agent for every
  new project or task,
- agents are generic workers that can be assigned to whatever execution is
  ready,
- an agent is considered available when it is not attached to an active run.

### Autonomous Pickup

Boss should be able to drive autonomous work pickup from durable state.

Examples:

- when a new project is created, Boss should create a `project_design`
  execution whose expected output is a repo-backed design doc plus follow-up
  tasks/phases,
- when a new task is created, Boss should create a task execution or mark the
  task as waiting on dependencies,
- when an agent becomes available, Boss should select a ready execution and
  start a new run for it.

The human remains in control of priorities and approvals, but the steady-state
pickup behavior should be automatic.

### Agent Done vs Work Done

These must be treated as different questions.

An **agent is done** when its current run ends. At that point the agent can
return to the pool.

The **work is done** only when the execution reaches a terminal operational
state such as:

- `completed`
- `abandoned`

For code work, `completed` usually means the relevant PR stack is merged or the
change is otherwise fully landed. For project-design work, `completed` may mean
the design doc and follow-up tasks were created and accepted.

### Wait States

The design needs explicit non-terminal wait states because many execution
phases should release the agent without closing the work:

- `waiting_human`: a person needs to review, redirect, or answer a question,
- `waiting_review`: a PR exists and is waiting for review feedback,
- `waiting_merge`: the PR is approved and waiting to land,
- `waiting_dependency`: this execution cannot proceed until another execution
  advances first.

Recommended default behavior:

- `waiting_human`, `waiting_review`, and `waiting_merge` release the agent but
  usually keep the workspace lease attached to the execution,
- `waiting_dependency` releases the agent and normally also releases the lease,
  because the execution is not actively progressing.

This keeps agents available while still preserving expensive local state when
that state is likely to be resumed soon.

## Human Attention Model

Waiting states need a durable human-attention mechanism. Status alone is not
enough.

The recommended model is:

- executions move into explicit `waiting_*` states,
- Boss creates or updates a durable attention item attached to that execution,
- the UI renders that attention item in an inbox/queue surface,
- the Boss agent may also summarize or relay that item in chat,
- chat is a convenience surface, not the source of truth.

### Attention Kinds

Recommended attention kinds:

- `question`
- `decision_required`
- `review_required`
- `merge_notice`
- `info`

### Wait-State Mapping

Recommended default mapping:

- status `waiting_human` -> create an inbox item of kind `review_required` or
  `decision_required`, and optionally have the Boss agent notify the human in
  chat,
- status `waiting_review` -> move the work item into the review swimlane, but
  do not create an interruptive human-attention item by default unless there is
  something explicitly actionable for the human,
- status `waiting_merge` -> usually no interruptive inbox item; the work
  remains visible in workflow state and may surface a low-priority
  `merge_notice`,
- status `waiting_dependency` -> no human-attention item by default.

This keeps review-ready work visible in the board workflow while reserving the
inbox and Boss-agent chat for items that genuinely need a human response.

### UI Surface

The minimum UI model should expose both:

- workflow state, such as the review swimlane for `waiting_review`,
- attention state, such as an inbox or review queue for explicit human action.

The same execution may appear in both places:

- the board answers "where is this work in the flow?"
- the inbox answers "what needs my attention right now?"

### Boss Agent Surface

The Boss agent should be one way, but not the only way, to obtain the human's
attention.

Recommended behavior:

- the coordinator creates the durable attention item first,
- the Boss agent may send a message summarizing that item,
- the UI can also show badges, queues, and drill-down state independent of the
  Boss agent chat.

This avoids making the conversational surface the only place where important
state exists.

### Sequential and Stacked Tasks

Boss, not Cube, should own the prerequisite rules for sequential project work.

The minimum viable rule should be explicit and simple:

- project tasks may declare dependencies or rely on project ordering,
- only executions whose prerequisites are satisfied become `ready`,
- tasks intended to be stacked should still be started sequentially rather than
  all at once.

The default first policy should be "strict serial by project order." Boss can
later relax that with a more specific stacked-task policy once the basic
execution model is stable.

## Workflow Detail Without a Phase Enum

The earlier detailed `phase` catalog was too specific for this design stage.
The durable model should stay simpler:

- use coarse execution `status` for scheduling and UI grouping,
- use execution `kind`, attention-item kind, run summaries, and transcripts to
  capture finer workflow detail,
- avoid encoding a large speculative phase enum into the initial schema.

Examples of detail that should live outside the core execution status:

- whether a project-design execution is drafting a doc versus synthesizing
  follow-up tasks,
- whether an implementation run is editing code versus running self-checks,
- whether a `waiting_human` state is asking for design approval, code review,
  or a product decision.

That detail still matters, but it is better captured in:

- the current attention item,
- the latest run summary,
- transcript/log metadata,
- optional future structured annotations once real usage stabilizes.

## Repository Materialization

### Decision

Cube should own repository materialization.

That means Cube, not Boss, is responsible for creating or maintaining the
local repo source path and the reusable workspace pool derived from the product
remote.

### Why Cube Should Own It

If Boss owns cloning, Boss immediately becomes responsible for:

- local path layout,
- source checkout bootstrap,
- workspace-root creation,
- workspace naming,
- reset/setup policy,
- stale-workspace repair,
- deciding when a repo needs to be re-fetched or re-provisioned.

Those are execution-layer concerns, not planning-layer concerns.

Cube is already the natural owner of repo/workspace metadata, and this matches
its existing repo + workspace model.

### Minimum Viable Product

The minimum usable Boss/Cube integration is one combined MVP:

- Boss starts from `Product.repo_remote_url`,
- Cube resolves or materializes the repo locally,
- Cube returns a stable repo id,
- Boss leases a workspace from that repo,
- the agent runs inside that leased workspace.

That means Boss cannot rely on pre-registered Cube repo pools as part of the
product contract. Manual bootstrap may still be useful during development, but
it is not part of the minimum viable product.

The required Cube operation for that MVP is an "ensure" command:

```text
cube repo ensure --origin git@github.com:spinyfin/mono.git --json
```

That operation should:

- resolve or create the Cube repo record for the remote,
- ensure the source/materialized repo exists locally,
- ensure the workspace pool exists or is discoverable,
- return a stable Cube repo id.

Boss should prefer this API once it exists.

## Boss and Cube Shared State

### Contract Shape

Boss should integrate with Cube through a narrow machine-readable contract.

Recommended minimum contract:

```text
cube repo ensure --origin <repo_remote_url> --json
cube workspace lease <cube_repo_id> --task <summary> --json
cube workspace release --lease <cube_lease_id> --json
cube workspace status --workspace <workspace_path> --json
```

`repo ensure` is the only new command required for the narrow MVP. The rest
already match Cube’s intended workspace lifecycle.

### What Boss Stores

Boss should persist only execution-facing fields returned by Cube:

- `cube_repo_id`
- `cube_lease_id`
- `workspace_path`
- timestamps and execution state

Boss may also persist a snapshot of `repo_remote_url` on the execution so
history remains interpretable even if the product record changes later.

### What Cube Stores

Cube stores:

- repo configuration,
- workspace inventory,
- current lease metadata,
- workspace setup/reset metadata.

Boss should not query Cube’s database directly.

### Why Not Share a Database

A shared database would blur ownership in exactly the area where the system
most needs clarity:

- is Boss allowed to mutate a lease row?
- is Cube allowed to mutate execution/run rows?
- which schema evolves first?
- how are failures handled if one process writes only half of a shared
  transaction?

The cleaner answer is:

- Boss writes Boss tables,
- Cube writes Cube tables,
- the integration point is explicit command/API calls with durable identifiers.

## Execution and Run Lifecycle

### Execution Creation

An execution is created when Boss decides a work item should enter the
autonomous pipeline.

Examples:

- a newly created project creates a `project_design` execution,
- a newly created task creates a task execution or is marked as waiting on a
  dependency,
- a retry or restart creates a fresh execution for previously failed work.

Creating an execution does not require an immediate workspace lease or agent
assignment. It creates durable operational state that the coordinator can act
on.

### Run Start

A run starts when the coordinator assigns an available agent to a ready
execution.

That is the moment Boss should:

1. choose the agent,
2. create the run row,
3. resolve the execution’s `repo_remote_url`,
4. call Cube to ensure the repo exists locally,
5. acquire or reuse a workspace lease for the execution,
6. update the execution with `cube_repo_id`, `cube_lease_id`, and
   `workspace_path`,
7. mark the run `active` and the execution `running`,
8. prompt the agent to work inside that workspace.

### Agent Availability

While a run is active:

- the agent is busy,
- the execution is `running`,
- the workspace lease is attached to the execution.

When the run ends:

- the agent returns to the pool,
- the run becomes historical execution data,
- the execution transitions either to a wait state or a terminal state.

This is the main mechanism that prevents agents from being locked while a human
or forge workflow is the bottleneck.

### Terminal and Wait Outcomes

When a run ends, Boss should distinguish between:

- **terminal execution outcomes** such as `completed`, `failed`, or
  `abandoned`,
- **wait outcomes** such as `waiting_human`, `waiting_review`,
  `waiting_merge`, or `waiting_dependency`.

The agent is released in both cases. The difference is whether the execution
remains open.

### Lease Release

Workspace release should be tied to execution state, not agent state.

Recommended default behavior:

- release the lease when the execution is `completed`, `failed`, or
  `abandoned`,
- keep the lease when the execution is `waiting_human`, `waiting_review`, or
  `waiting_merge`,
- usually release the lease when the execution is `waiting_dependency`.

### Abandonment and Recovery

If Boss crashes or an agent disappears:

- Boss still has execution and run rows,
- Cube still has the lease row,
- recovery consists of reconciling durable Boss execution state with durable
  Cube lease state.

Boss should be able to mark an execution `abandoned` if its lease is no longer
usable or the work is intentionally dropped. Cube should be able to show stale
leases independently of Boss execution status.

### Human Response and Resume

When a human responds to a waiting execution, that response should be attached
to the execution, not to a specific still-reserved agent.

Recommended flow:

1. the human responds through either the UI or the Boss agent chat,
2. Boss converts that input into a durable execution event or attention-item
   resolution,
3. the coordinator updates execution state,
4. if more agent work is needed, the coordinator creates a new run and assigns
   the next available agent.

This means the original agent does not need to stay reserved while waiting.

### Continuity After Waiting

Execution continuity must not depend on reusing the same agent.

Boss should prefer the original agent when it is conveniently available, but
that is an optimization only. Any suitable agent should be able to resume the
execution from durable state.

The minimum resume context should include:

- the execution record,
- the current workspace path and lease id,
- the current PR or review state if any,
- the prior run summaries,
- the full interaction transcripts,
- the new human feedback or decision.

This is what lets a waiting execution continue even after the original agent
has moved on to other work.

## Related and Stacked Work

Boss should own the relationship between related tasks.

Examples:

- task B depends on task A,
- task B should be prompted as a follow-up to task A,
- tasks A and B are expected to land as stacked PRs.

Cube does not need to own that policy in the narrow MVP. Cube only needs to
provide isolated workspaces and safe lease management.

For the minimum viable version:

- related tasks can still run in separate leased workspaces,
- Boss can pass relationship context to the agent prompt,
- raw `jj` commands inside the workspace can handle local stacking mechanics.

If later needed, Boss can pass optional execution hints to Cube such as "start
this run from the same base as parent run X," but that should remain a later
optimization rather than a first requirement.

## Boss Storage Additions

Add two related execution tables:

- `work_executions` for long-lived operational ownership,
- `work_runs` for short-lived agent occupancy.
- `work_attention_items` for durable human-attention state.

Suggested schema shape:

```text
work_executions
- id TEXT PRIMARY KEY
- work_item_id TEXT NOT NULL
- kind TEXT NOT NULL
- status TEXT NOT NULL
- repo_remote_url TEXT NOT NULL
- cube_repo_id TEXT
- cube_lease_id TEXT
- workspace_path TEXT
- created_at TEXT NOT NULL
- started_at TEXT
- finished_at TEXT

work_runs
- id TEXT PRIMARY KEY
- execution_id TEXT NOT NULL
- agent_id TEXT NOT NULL
- status TEXT NOT NULL
- error_text TEXT
- result_summary TEXT
- created_at TEXT NOT NULL
- started_at TEXT
- finished_at TEXT

work_attention_items
- id TEXT PRIMARY KEY
- execution_id TEXT NOT NULL
- kind TEXT NOT NULL
- status TEXT NOT NULL
- title TEXT NOT NULL
- body_markdown TEXT NOT NULL
- created_at TEXT NOT NULL
- resolved_at TEXT
```

All timestamp columns above store Unix epoch seconds as a decimal string,
matching the canonical format defined in
[`work-taxonomy.md`](work-taxonomy.md#timestamp-format).

Follow-up fields that may be useful later:

- `parent_execution_id`
- `attempt_number`
- `head_commit`
- `pr_url_snapshot`
- `resolution_kind`
- `resolution_summary`

The first version should resist adding more until the workflow proves it
necessary.

## Interaction Logs and Transcripts

Boss should durably store logs for all agent interactions, not only summarized
status fields in SQLite.

The recommended model is:

- SQLite stores the durable indices and associations,
- append-only files store the detailed per-run transcript and artifacts.

### Canonical Format

The canonical stored format should be structured and loss-minimizing, such as
JSONL or another append-only event format, rather than plain markdown alone.

That structured log should be rich enough to reconstruct:

- a readable markdown transcript,
- prompts and responses,
- tool calls and outputs,
- execution metadata and timing,
- human feedback events attached later.

Markdown can still be generated as a derived convenience view.

### Suggested Local Layout

Recommended local layout under Boss state:

```text
~/Library/Application Support/Boss/
  executions/<execution-id>/
    execution.json
    attention/
      <attention-id>.json
    runs/<run-id>/
      metadata.json
      transcript.jsonl
      transcript.md
      artifacts/
```

### Required Metadata

Each run transcript should be associated back to at least:

- `product_id`
- `project_id` or `task_id` / `chore_id`
- `execution_id`
- `run_id`
- `agent_id`
- `repo_remote_url`
- `cube_repo_id`
- `cube_lease_id`
- `workspace_path`
- timestamps

### Why Files, Not Only SQLite

Detailed interaction logs will be large, append-heavy, and useful to inspect
outside the app. Files are a better fit for that than stuffing every event into
SQLite rows.

SQLite should still index the runs and point to the canonical transcript paths.

## Recommended Rollout

The recommended implementation order is to prove the Cube-side execution
surface first, then wire Boss into it once the local repo/workspace contract
feels right.

### Phase A: Cube-Only Spike

Build the minimum Cube features needed to exercise the execution model without
changing Boss yet:

- add `cube repo ensure`,
- resolve or materialize a repo from a remote URL,
- return a stable Cube repo id,
- lease a workspace from that ensured repo,
- release the lease cleanly,
- verify that a human can run the full flow manually.

The goal of this phase is to validate the local execution contract:

1. `cube repo ensure --origin <repo_remote_url>`
2. `cube workspace lease <cube_repo_id> --task <summary>`
3. work inside the leased workspace with raw `jj` / `git` / repo tools
4. `cube workspace release --lease <cube_lease_id>`

This is a spike, not the full product MVP, because Boss still is not using the
contract yet. It exists to de-risk the Cube surface before Boss depends on it.

### Phase B: Boss Persistence Foundation

Add the minimum durable Boss state needed before any autonomous scheduling:

- `work_executions`
- `work_runs`
- `work_attention_items`
- transcript-path indexing from runs to on-disk logs

At the end of this phase, Boss should be able to:

- create and inspect executions manually,
- record runs against those executions,
- persist wait states and attention items,
- associate transcripts and artifacts with executions and runs.

This phase is still mostly backend plumbing. It does not require autonomous
pickup yet.

### Phase C: Boss Coordinator MVP

Add the first server-global `ExecutionCoordinator`.

Scope:

- react to newly created projects, tasks, and chores,
- create the right execution kind,
- evaluate prerequisite readiness,
- choose an available agent from the fixed pool,
- start a run,
- call Cube `repo ensure`,
- call Cube `workspace lease`,
- update execution status and workspace linkage,
- release the agent when the run ends,
- keep or release the workspace lease based on execution state.

At the end of this phase, Boss should be able to autonomously pick up:

- a new project and drive it into design work,
- a new standalone task/chore and drive it into implementation work,
- sequential project tasks only when prerequisites are satisfied.

### Phase D: Human Attention and Review Flows

Add the first complete human-in-the-loop workflow.

Scope:

- create attention items when executions enter human-facing wait states,
- expose those items in the UI as inbox/review state,
- map `waiting_human` and `waiting_review` into the appropriate workflow
  surfaces,
- allow the human to approve, redirect, request changes, or abandon work,
- persist that feedback as durable execution events.

At the end of this phase, a waiting execution should be resumable from human
feedback without needing the original agent to stay reserved.

### Phase E: Resume and Continuity

Add the minimum continuity needed for multi-run executions.

Scope:

- resume an execution from durable state rather than from in-memory agent
  context,
- allow any suitable agent to continue a waiting execution,
- reuse the retained workspace lease when appropriate,
- pass prior run summaries, transcripts, and human feedback into the resumed
  run,
- wake executions when dependencies, reviews, or merge events unblock them.

This phase is where the model stops being "one agent conversation per work
item" and becomes a real long-lived execution workflow.

### Phase F: Transcript and Artifact Logging

Add durable, file-backed logs for all agent interactions.

Scope:

- persist structured per-run transcripts,
- store run metadata and artifact paths under Boss local state,
- associate transcripts with execution ids, run ids, agents, repo/workspace
  context, and timestamps,
- generate markdown views as derived outputs where helpful.

At the end of this phase, Boss should have enough durable history that a new
agent, the human, or a debugging tool can reconstruct what happened.

### Phase G: Hardening and Recovery

Add the operational safety pieces.

Scope:

- surface stale runs vs stale leases,
- surface waiting states explicitly in the UI,
- add operator-facing repair flows,
- optionally add `cube doctor` integration from Boss,
- add retries and abandonment flows,
- add better diagnostics for "agent free, execution waiting, lease retained"
  cases.

This phase should come after the main functional loop exists, not before.

## Detailed Implementation Phases

If you want the smallest practical delivery sequence, build in this order:

1. Cube spike:
   `repo ensure`, materialization, leasing, release, JSON contract.
2. Boss execution tables:
   executions, runs, attention items, transcript indexing.
3. Fixed agent pool:
   replace or supplement the current ad hoc agent creation model with a durable
   pool of reusable workers.
4. Execution coordinator:
   server-global scheduler that creates executions and starts runs.
5. Project-design flow:
   new project -> design execution -> repo-backed doc -> follow-up tasks.
6. Task/chore flow:
   new task -> implementation execution -> review/merge waits -> completion.
7. Human attention flow:
   inbox/review queue, feedback capture, attention resolution.
8. Resume flow:
   resume waiting executions with any suitable agent from durable state.
9. Transcript storage:
   structured logs plus derived markdown.
10. Recovery:
   stale lease handling, abandoned executions, operator tooling.

That ordering is intentional:

- phases 1-4 create the execution substrate,
- phases 5-8 prove the actual autonomous workflow,
- phases 9-10 make the system inspectable and safe to operate.

## Boss Coordination Model

Boss needs a server-global coordination mechanism for this to work. Ad hoc
polling by individual agents is the wrong shape.

The recommended shape is an in-process `ExecutionCoordinator` backed by durable
database state and driven by events.

### Event Sources

The coordinator should react to events such as:

- project created,
- task or chore created,
- dependency satisfied,
- agent became available,
- run ended,
- human approved, redirected, or abandoned work,
- PR merged, closed, or changed review state,
- Cube lease check or repair results.

### Coordinator Responsibilities

The coordinator should:

- create executions from newly eligible work,
- evaluate which executions are `ready`,
- assign available agents,
- start and finish runs,
- move executions into wait states,
- create and resolve attention items,
- decide when to retain or release leases,
- wake waiting executions when prerequisites clear.

### Event Bus and State Tracking

Boss already needs a server-global topic broker for client subscriptions.
That subscription infrastructure should be treated as adjacent, not separate,
from execution coordination.

The important rule is:

- durable execution state lives in Boss tables,
- the coordinator reacts to committed state changes,
- the event bus is the trigger mechanism, not the source of truth.

Human responses from either the UI or the Boss agent should enter the same
durable event path so they can be replayed, audited, and associated back to the
right execution.

## Design Decisions

- `repo_remote_url` remains Boss’s canonical repository identity.
- local repo materialization belongs to Cube, not Boss.
- an execution is the unit of long-lived work ownership; a run is the unit of
  active agent occupancy.
- Boss and Cube communicate through explicit commands/APIs, not a shared DB.
- Boss uses a fixed pool of reusable agents rather than tying one agent to one
  work item for the whole lifecycle.
- Boss leases workspaces based on execution state, not agent occupancy alone.
- human attention is modeled as durable attention items, with chat as a
  secondary surface rather than the source of truth.
- execution continuity comes from durable execution state and transcripts, not
  from keeping the same agent reserved.
- Boss owns related-task policy; Cube owns workspace isolation.

## Related Designs

- [`main`](main.md)
- [`work-taxonomy`](work-taxonomy.md)
- [`work-cli`](work-cli.md)
- [`work-subscriptions`](work-subscriptions.md)
