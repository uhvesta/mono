# Boss: Work Tab Kanban Design

## Overview

The Work tab should move from a tree-first navigator to a board-first view.
The primary purpose of the tab is to help the human boss understand the state
of work at a glance and move work forward. A kanban board does that better than
the current hierarchy-focused presentation.

The board should use four fixed columns:

- **Backlog**
- **Doing**
- **Review**
- **Done**

Cards on the board should be only **tasks** and **chores**. Projects remain
important, but they should organize cards through filtering and grouping rather
than appearing as peer work items in the main board.

This design builds on the existing work taxonomy. It does not replace the
current product/project/task/chore model; it changes how that model is exposed
in the Work tab.

## Goals

- Make planned, active, review-ready, and completed work obvious at a glance.
- Treat tasks and chores as the main units of day-to-day tracking.
- Let the user filter or group work by project without losing a product-level
  view.
- Preserve the existing storage model and canonical status values where
  possible.
- Keep the first version lightweight enough for the current macOS PoC.

## Non-Goals

- Making projects into board cards.
- Designing full agent assignment or automation from the board.
- Adding a fifth permanent `Blocked` column.
- Replacing product/project CRUD with a new planning system.
- Building dependency graphs, estimates, or roadmap tooling.

## Why Kanban Here

The current work taxonomy is good at modeling structure, but the current UI
leans too hard on hierarchy. The Work tab should optimize for answering
questions like:

- What is not started yet?
- What is currently in progress?
- What is waiting on review?
- What has finished recently?

Those are workflow questions, not hierarchy questions. The hierarchy still
matters, but it should be supporting context around the board instead of the
main event.

## Primary Model

The Work tab should be scoped to a single **product** at a time. Within that
product:

- **Projects** are organizational containers.
- **Tasks** are project-scoped board cards.
- **Chores** are product-scoped board cards.

The board should never render products or projects as cards. The user manages
projects through filters, grouping controls, and inspectors.

## Status Mapping

The existing backend status model is already close to what the kanban board
needs. The frontend should project statuses into board columns like this:

- `todo` -> `Backlog`
- `active` -> `Doing`
- `blocked` -> `Doing`
- `in_review` -> `Review`
- `done` -> `Done`

`blocked` should stay a first-class status in storage, but it should not create
a separate permanent column. Instead, blocked cards should render as a special
kind of `Doing` card:

- blocked badge or accent color,
- sorted to the top of the `Doing` column by default,
- included by a quick `Blocked only` filter.

This keeps the board aligned with the requested four-column workflow while
preserving operational signal.

`active` is also a load-bearing dispatch state, not just a column label — see
[Doing column = "live or queued"](#doing-column--live-or-queued). The kanban
treats `Doing` membership as authoritative for "the engine is running or
queueing this," so moving a card into or out of `Doing` has dispatch side
effects beyond the status update.

## Information Architecture

Recommended layout:

```text
┌────────────────────┬──────────────────────────────────────────────────────┐
│ Products / Filters │ Work Board                                           │
│────────────────────│──────────────────────────────────────────────────────│
│ Boss               │ Product: Boss                                        │
│                    │ Filters: All projects | Search | Blocked only        │
│ Projects           │ Group by: None / Project                             │
│ [x] Work taxonomy  │                                                      │
│ [x] Agent polish   │ Backlog   Doing   Review   Done                      │
│ [ ] Infra cleanup  │ ┌──────┐  ┌──────┐ ┌──────┐ ┌──────┐                 │
│                    │ │card  │  │card  │ │card  │ │card  │                 │
│ Options            │ │card  │  │card  │ │card  │ │card  │                 │
│ [x] Show chores    │ └──────┘  └──────┘ └──────┘ └──────┘                 │
│ [ ] Blocked only   │                                                      │
└────────────────────┴──────────────────────────────────────────────────────┘
```

### Left Sidebar

The left sidebar should stop behaving like a deep tree of products, projects,
tasks, and chores. It should instead hold board context:

- product selection,
- project filter checkboxes or pills,
- quick toggles such as `Show chores` and `Blocked only`,
- optional saved filter state across launches.

This keeps the board wide and readable while still giving the user structural
control.

### Main Board

The main area should be a four-column kanban board. Each column shows cards for
the currently selected product after project/filter rules are applied.

Each column should show:

- a title,
- a count badge,
- cards ordered with the most urgent/active items first.

The first version can use vertical scrolling inside the full board scroll view.
Independent horizontal board virtualization is unnecessary at PoC scale.

## Cards

Cards should represent either:

- a task that belongs to a project, or
- a chore that belongs directly to the product.

Each card should show, at minimum:

- title,
- kind icon (`task` vs `chore`),
- project label when applicable,
- blocked state when applicable,
- PR link when present,
- updated-at or created-at secondary metadata.

Optional first-version metadata:

- project color accent,
- assignee placeholder for future agent linkage,
- short description preview on expanded cards.

## Project Grouping and Filtering

Projects should influence the board in two ways.

### 1. Filtering

The user should be able to filter to:

- all projects,
- one project,
- multiple projects,
- chores only,
- one or more projects plus chores.

This makes it possible to answer both product-level and project-level workflow
questions without changing screens.

### 2. Grouping

The user should be able to switch column rendering between:

- `Ungrouped`: one flat list per status column.
- `Group by project`: sections inside each status column.

When grouped by project:

- project-backed tasks appear in per-project sections,
- chores appear in a `Chores` or `No project` section,
- empty project sections remain hidden by default.

Grouping should be a presentation choice only. It should not change storage or
identity.

## Board Interactions

### Create

The Work tab should support quick creation directly into `Backlog`.

Recommended flows:

- `New Task`: requires a selected product and project.
- `New Chore`: requires only a selected product.
- Quick-add affordance at the top of the `Backlog` column.

### Move

Cards should be movable between columns. Drag-and-drop is the ideal interaction
for the macOS app, but the first version can also support:

- move menu in the card,
- keyboard/action menu in the inspector.

Column moves should update canonical item status:

- drop into `Backlog` -> `todo`
- drop into `Doing` -> `active` unless explicitly marked blocked
- drop into `Review` -> `in_review`
- drop into `Done` -> `done`

Blocking and unblocking should be a separate action from column movement.

The `Doing` column has additional dispatch semantics — see
[Doing column = "live or queued"](#doing-column--live-or-queued) below — so
moves into and out of `Doing` are not pure metadata edits.

### Doing column = "live or queued"

The `Doing` column is the only column whose membership is required to mirror
the engine's actual execution state. Cards in `Doing` are either being worked
on by a worker right now, or queued waiting for a free worker; cards outside
`Doing` are not. The board must not show "active" work that the engine isn't
actually running.

This adds three invariants on top of the simple
"drop -> set tasks.status" behavior described above. They apply equally to
human moves (drag, menu, inspector edit) and to engine-driven moves (Boss
session calling `bossctl work start`).

#### 1. Dropping a card into `Doing` schedules it.

When the human drops a chore or task into `Doing` (or sets its status to
`active` via the inspector), the kanban must do two things in one user-visible
operation:

1. update `tasks.status` to `active`, as today, and
2. ask the engine to dispatch an execution for that work item.

The dispatch path is the existing `RequestExecution` RPC the Boss session
already uses via `bossctl work start`. The engine's coordinator handles
worker-pool capacity, lease acquisition, and the actual spawn; the kanban does
not need its own scheduler.

If a worker is already running for the work item (the existing execution is
`running` or `waiting_human`), `RequestExecution` is a no-op on the dispatch
side — the engine already treats a non-terminal execution for the same work
item as the one that owns the slot. The status update still fires. The kanban
move is therefore safe to retry without spawning duplicates.

If no worker pool slot is free, the execution lands in `ready` and the
coordinator picks it up when capacity opens. The card stays in `Doing` the
whole time — that's the "queued" half of the column's contract.

This is a deliberate addition to the dispatch model. The Boss session remains a
dispatcher, but the human can also dispatch directly from the board for cases
that don't need Boss-side decomposition or planning. The two paths converge on
the same `RequestExecution` RPC, so the engine sees a single authoritative
flow regardless of who initiated it.

#### 2. A card cannot leave `Doing` while a worker is actively running it.

Manually moving a chore out of `Doing` (drag back to `Backlog`, drag forward
to `Review`/`Done`, or status-edit to anything other than `active`) is
disallowed while the chore has a `running` or `waiting_human` execution with a
live worker pane. The kanban surfaces this as one of:

- the drag is rejected with a tooltip explaining a worker is active and
  pointing at "Stop worker" / "Cancel execution" affordances,
- the inspector status field is read-only with a similar explanation, or
- attempting the move opens a confirmation dialog that, on accept, first
  issues a stop/cancel, then performs the move.

The exact UX should follow the macOS HIG; what matters for the design is the
invariant: the kanban must never show a card in a column whose status
contradicts the live worker reality.

`done`, `archived`, and `failed` are not blocked — those are valid
`active`-to-terminal transitions the engine itself drives at run completion;
the human flowing the card the same way after a successful PR merge is
allowed.

#### 3. On engine startup, `Doing` rehydrates dispatch.

When the engine starts (or restarts after a crash), it scans for work items in
status `active` whose latest execution is in a terminal state (or that have no
execution at all). Those items are in `Doing` per the kanban but no worker is
running them, which violates the column's contract. The startup reconcile
must re-issue `RequestExecution` for each so they re-enter the dispatch queue.

The opposite case — `tasks.status != 'active'` but a non-terminal execution
exists — is handled by the existing auto-advance path:
`start_execution_run` already flips `tasks.status` to `active` whenever an
execution moves to `running`, and the new broadcast (PR #171, PR #174) fires
a work-tree invalidation so the board reflects it. So that direction
self-heals; only the `active`-without-worker direction needs the new startup
reconcile.

The reconcile must distinguish "card is in `Doing` because the human dropped
it there post-restart, before the engine could re-dispatch" from "card is in
`Doing` because someone manually set status to `active` to bypass dispatch
entirely." A `tasks.dispatch_intent` column or equivalent could disambiguate;
in the simpler reading, every `active` chore without a live worker is a
dispatch candidate and the human's escape hatch is to first drag the card
elsewhere. The simpler reading is preferred unless it produces footguns in
practice.

#### Invariant in one line

A chore is in `Doing` iff the engine is actively running it, or has it queued
ready, or is in the act of rehydrating dispatch for it on startup. Any other
state is a bug.

### Inspect and Edit

Selecting a card should open a detail inspector without navigating away from
the board. The inspector should allow editing:

- name,
- description,
- status,
- project,
- PR URL.

Projects and products can still have detail views, but those should not displace
the board as the default Work experience.

## Sorting

Within each column, recommended default ordering is:

1. blocked cards first inside `Doing`,
2. explicit `ordinal` when present for project tasks,
3. most recently updated items,
4. alphabetical fallback.

This respects existing ordered phases where they exist without making the whole
board feel like a strict sequential plan.

## Relationship to Existing Work Taxonomy

This design intentionally reuses the current domain model:

- `Product` remains the top-level scope.
- `Project` remains the container for meaningful feature work.
- `Task` remains the canonical backend term for project-scoped work.
- `Chore` remains a product-scoped work item.

No new table is required for board columns. The board is a derived view over
the existing `tasks` table plus project metadata.

The main state change is in the frontend: the Work tab should treat the board
as the primary presentation and the hierarchy as supporting context.

## Engine and Protocol Impact

The existing store and `get_work_tree` response are enough for a first board
implementation. The frontend can derive columns locally from:

- product,
- projects,
- tasks,
- chores.

Recommended near-term additions:

- update-work-item support for changing `project_id` on tasks,
- a focused list/query endpoint later if the board grows beyond one product's
  practical size,
- optional persistent UI preferences for project filters and grouping mode.

No schema change is required for the first cut unless we later decide to store
explicit board ordering separate from `ordinal`.

## Frontend State Model

The current work state is detail-first. A board-first UI should add:

- `selectedProductID: String?`
- `selectedProjectFilterIDs: Set<String>`
- `includeChores: Bool`
- `showBlockedOnly: Bool`
- `boardGrouping: .none | .project`
- `selectedCardID: String?`

The existing `selectedWorkNodeID` model can remain for inspector/edit flows,
but it should no longer drive the overall layout of the Work tab.

## Implementation Plan

### Phase 1: Board Projection

1. Keep the current engine APIs and persistence model.
2. Replace the tree-first Work sidebar with product and project filters.
3. Render tasks and chores into the four fixed kanban columns.
4. Add inspector-based editing from a selected card.

### Phase 2: Workflow Actions

5. Add move actions between columns.
6. Add drag-and-drop for cards.
7. Add quick-create in `Backlog`.
8. Add blocked styling and blocked-only filtering.

### Phase 3: Polish

9. Persist selected product, filters, and grouping mode.
10. Add richer project grouping visuals and counts.
11. Add keyboard shortcuts and better empty states.

### Phase 4: Doing-column dispatch (new)

12. Drop into `Doing` (drag, menu, or inspector status edit) sends
    `RequestExecution` after the status update.
13. Block moves out of `Doing` while the chore has a non-terminal execution
    backed by a live worker pane, with a "stop worker first" escape hatch.
14. On engine start, reconcile `active`-with-no-live-worker work items by
    re-issuing `RequestExecution` so they re-enter the dispatch queue.

These three together promote `Doing` from a passive status label to an
authoritative view of what the engine is actually running or queuing. See
[Doing column = "live or queued"](#doing-column--live-or-queued) above for
the invariants and the interactions with the existing Boss-session dispatch
path (`bossctl work start`).

## Design Decisions

- The Work tab should be board-first, not tree-first.
- Only tasks and chores should appear as board cards.
- Projects should organize the board through filters and grouping.
- The board should use the fixed columns `Backlog`, `Doing`, `Review`, and
  `Done`.
- `blocked` should remain a status but render inside `Doing`.
- The first implementation should derive the board from the existing work tree
  instead of adding new backend concepts.
