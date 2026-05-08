# Boss: Work Taxonomy

## Overview

Boss currently treats the agent as the primary unit of organization. This is
useful for the execution layer, but it is too low-level for planning and
tracking product work. We want Boss to understand a lightweight taxonomy of
work entities that sits above agents:

- **Product**: the highest-level unit of organization. A product is usually a
  single repository and represents a distinct thing being built or maintained
  (for example Boss, or Flunge).
- **Project**: a substantial body of work within a product, such as a feature,
  migration, or architectural redesign.
- **Task / Phase**: a self-contained chunk of work within a project. In
  practice, this should line up closely with a single PR.
- **Chore**: a small standalone task that belongs to a product but does not
  need to sit inside a project.

The taxonomy in this document defines the domain model. The preferred kanban
presentation for the Work tab is described separately in
[`work-kanban`](work-kanban.md).

In the near term, these concepts should be stored persistently in the Boss
backend and made visible in the UI. They should remain orthogonal to agents for
now: work entities exist in their own navigator, and no agent-assignment model
is required yet.

## Goals

- Give Boss a durable representation of product planning concepts.
- Support a simple hierarchy that feels natural for software work.
- Keep the first version lightweight: metadata, relationships, and status, not
  deep workflow automation.
- Add these entities to the UI without disturbing the current multi-agent flow.
- Leave room for a later model where agents operate on products, projects,
  tasks, and chores.

## Non-Goals

- Replacing agents as the execution primitive.
- Building scheduling, dependency management, or roadmap tooling.
- Modeling PRs, commits, and branches as first-class entities in this phase.
- Designing the full agent/work interaction model.
- Supporting cross-product graphs or multi-repo orchestration beyond a small
  amount of descriptive metadata.

## Core Domain Model

### 1. Product

The product is the top-level container. It represents a coherent software unit
that a human would naturally refer to by name.

Suggested fields:

- `id`
- `name`
- `slug`
- `description`
- `repo_remote_url`
- `status` (`active`, `paused`, `archived`)
- `created_at`
- `updated_at`

Notes:

- `repo_remote_url` should be the canonical repository identity for a product.
- The product model should not assume a single long-lived local checkout path.
  Multiple agents may work in separate local workspaces for the same product,
  and local path coordination belongs in a separate execution/workspace layer.
  That execution layer is defined separately in
  [`work-execution`](work-execution.md).
- We should still leave space for future multi-repo support. A simple
  follow-up field like `related_repo_remote_urls_json` is enough for now; we do
  not need to optimize the first UI or API around it.

### 2. Project

Projects live under a product and represent meaningful bodies of work.

Suggested fields:

- `id`
- `product_id`
- `name`
- `slug`
- `description`
- `goal`
- `status` (`planned`, `active`, `blocked`, `done`, `archived`)
- `priority` (`low`, `medium`, `high`)
- `created_at`
- `updated_at`

A project is intentionally coarse. If the unit of work would reasonably produce
multiple PRs, it is probably a project.

### 3. Task / Phase

This is the smallest planned unit that still belongs to a project. It should be
small enough to map roughly to a single PR.

Suggested fields:

- `id`
- `product_id`
- `project_id`
- `name`
- `description`
- `status` (`todo`, `active`, `blocked`, `in_review`, `done`)
- `priority` (`low`, `medium`, `high` — defaults to `medium`)
- `ordinal`
- `pr_url`
- `created_at`
- `updated_at`

Terminology:

- Internally, Boss should use `task` as the canonical backend/API term.
- The UI can display "phase" when the item is part of an ordered sequence in a
  project.
- This avoids duplicating concepts while preserving the language the user may
  prefer in context.
- Phases are conceptually ordered. `ordinal` is not just display metadata; it
  is part of the model and should support explicit reordering.
- Over time, a design doc will likely become the canonical source of truth for
  a project's phases, with Boss synchronizing that structure into its SQL
  store. This design should leave room for that future workflow.

### 4. Chore

A chore is a task-like object that belongs to a product directly instead of to
project-based feature work.

Suggested fields:

- `id`
- `product_id`
- `name`
- `description`
- `status` (`todo`, `active`, `blocked`, `in_review`, `done`)
- `priority` (`low`, `medium`, `high` — defaults to `medium`)
- `pr_url`
- `created_at`
- `updated_at`

Conceptually, chores are first-class in the UI. In storage, they should reuse
the same mechanics as tasks.

#### Naming convention: do NOT prefix names with priority

Tasks and chores carry priority as a **structured field** (the `priority`
column above), not as a literal prefix in `name`. Do not write
`[HIGH] Drop or gate the per-pane screen-scrape` or
`[MEDIUM] Stop accumulating Boss assistant chunks` — pass
`--priority high` / `--priority medium` instead, or leave it unset for the
`medium` default.

Names describe the work. Priority lives on the field. The kanban renders
a colour-coded chip on every card so a quick scan still surfaces high-priority
work, and `boss task list --priority high --json` (and the same on chores)
filters by the field rather than relying on substring matches against name.

The `[HIGH]` / `[MEDIUM]` / `[LOW]` prefix style was a workaround for a
missing field. It's no longer needed; new tasks and chores should not
adopt it. Existing rows with such prefixes are not auto-stripped; clean them
up by hand or via a one-shot script when convenient.

## Storage Model

### Recommendation

Add a lightweight SQLite store owned by the engine. SQLite is a better fit than
ad hoc JSON files because we already have hierarchical entities, we want simple
filtering/listing, and future work will likely add relationships from agents,
runs, and PRs back to these records.

Suggested location on macOS:

- `~/Library/Application Support/Boss/state.db`

Expose an override for development/testing, for example:

- `BOSS_DB_PATH`

### Schema Shape

Use four tables in the first cut:

#### `products`

- `id TEXT PRIMARY KEY`
- `name TEXT NOT NULL`
- `slug TEXT NOT NULL UNIQUE`
- `description TEXT NOT NULL DEFAULT ''`
- `repo_remote_url TEXT`
- `status TEXT NOT NULL`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`

#### `projects`

- `id TEXT PRIMARY KEY`
- `product_id TEXT NOT NULL REFERENCES products(id)`
- `name TEXT NOT NULL`
- `slug TEXT NOT NULL`
- `description TEXT NOT NULL DEFAULT ''`
- `goal TEXT NOT NULL DEFAULT ''`
- `status TEXT NOT NULL`
- `priority TEXT NOT NULL`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`
- unique index on `(product_id, slug)`

#### `tasks`

- `id TEXT PRIMARY KEY`
- `product_id TEXT NOT NULL REFERENCES products(id)`
- `project_id TEXT REFERENCES projects(id)`
- `kind TEXT NOT NULL`
- `name TEXT NOT NULL`
- `description TEXT NOT NULL DEFAULT ''`
- `status TEXT NOT NULL`
- `priority TEXT NOT NULL DEFAULT 'medium'`
- `ordinal INTEGER`
- `pr_url TEXT`
- `deleted_at TEXT`
- `created_at TEXT NOT NULL`
- `updated_at TEXT NOT NULL`

Where:

- `kind = 'project_task'` means a normal task/phase.
- `kind = 'chore'` means a standalone chore.
- `project_id` is required for `project_task` and null for `chore`.
- Soft deletion should be used for tasks and chores. `deleted_at` marks a row
  hidden from normal list queries while preserving history and future sync
  behavior.

#### `metadata`

- optional small key/value table for schema versioning and future settings.

This gives chores first-class behavior in the UI while keeping backend storage
simple.

### Timestamp Format

All `created_at`, `updated_at`, `deleted_at`, `started_at`, `finished_at`, and
`resolved_at` columns store **Unix epoch seconds as a decimal string**
(e.g. `"1778180145"`). This is the canonical wire format: every JSON response
that includes one of these fields — `boss chore list --json`, the kanban tree
broadcasts, etc. — emits the same shape, and consumers (CLIs, the macOS app)
parse it as `i64` seconds since 1970-01-01T00:00:00Z.

A pre-canonical write path produced ISO 8601 strings
(`"2026-05-07T18:55:45.000Z"`) for some rows. The engine runs a one-time
migration on startup that rewrites any ISO-format timestamp it finds back to
epoch seconds, so older databases self-heal the first time a current engine
opens them.

## Engine Responsibilities

The engine should own:

- schema creation and migration,
- CRUD operations for work entities,
- hierarchy-aware list queries,
- validation of parent/child relationships,
- reordering operations for project tasks,
- stable IDs and timestamps.

The engine should not yet own:

- agent assignment,
- PR synchronization,
- repo discovery,
- automatic status derivation from git state.

### Protocol Additions

Add a small set of JSON message types alongside the existing agent protocol.

Frontend to engine:

```json
{"type":"list_products"}
{"type":"create_product","name":"Boss","repo_remote_url":"git@github.com:spinyfin/mono.git"}
{"type":"create_project","product_id":"prod_boss","name":"Work taxonomy"}
{"type":"create_task","product_id":"prod_boss","project_id":"proj_taxonomy","name":"Backend schema"}
{"type":"create_chore","product_id":"prod_boss","name":"Fix sidebar selection bug"}
{"type":"update_work_item","id":"task_123","patch":{"status":"active","pr_url":"https://github.com/..."}}
{"type":"reorder_project_tasks","project_id":"proj_taxonomy","task_ids":["task_2","task_1","task_3"]}
{"type":"delete_work_item","id":"task_123"}
{"type":"get_work_tree","product_id":"prod_boss"}
```

Engine to frontend:

```json
{"type":"products_list","products":[...]}
{"type":"projects_list","product_id":"prod_boss","projects":[...]}
{"type":"work_tree","product":{"id":"prod_boss", "...":"..."},"projects":[...],"tasks":[...],"chores":[...]}
{"type":"work_item_created","item":{...}}
{"type":"work_item_updated","item":{...}}
{"type":"project_tasks_reordered","project_id":"proj_taxonomy","task_ids":["task_2","task_1","task_3"]}
{"type":"work_item_deleted","id":"task_123"}
{"type":"work_error","message":"project_id is required for project_task"}
```

Notes:

- We should prefer one canonical `work item` payload shape over four separate
  frontend models wherever possible.
- `get_work_tree` is the most useful read API for the first UI. It lets the app
  fetch one product and all of its children without implementing a large query
  surface.

## UI Model

### Navigation

Keep agents and work entities separate at the top level.

Recommended shape:

- A top-level app mode switch with two destinations: `Agents` and `Work`.
- `Agents` keeps the current multi-agent sidebar and chat detail experience.
- `Work` opens a separate navigator for products, projects, tasks, and chores.

This avoids forcing unrelated concepts into the same sidebar while still making
them feel like primary Boss concepts.

### Work Navigator

This document does not prescribe the main Work tab layout. The current
preferred presentation is the kanban board described in
[`work-kanban`](work-kanban.md).

The important taxonomy constraint is that the UI should preserve these
relationships:

- products are the top-level scope,
- projects contain ordered project tasks,
- chores belong directly to products,
- tasks and chores remain distinct from projects in presentation and behavior.

### Detail Views

Minimal detail views are sufficient initially:

- Product detail: description, repo information, projects, chores.
- Product detail should present the canonical remote URL, not a single local
  repo path.
- Project detail: goal, status, ordered tasks.
- Task detail: description, status, PR URL, parent project.
- Chore detail: description, status, PR URL, parent product.

The first version does not need rich editing. Simple create/edit sheets or
inline forms are enough.

## Frontend State Model

Add a parallel work state model to the macOS app:

- `products: [Product]`
- `projectsByProductID: [String: [Project]]`
- `tasksByProjectID: [String: [Task]]`
- `choresByProductID: [String: [Task]]`
- `selectedWorkNode: WorkNodeID?`
- `navigationMode: .agents | .work`

Additional board-specific state such as selected product, project filters, and
grouping mode is covered in [`work-kanban`](work-kanban.md).

This should be separate from the current `agents` array and transcript state.

## Relationship to Agents

For this phase, work entities are informational and organizational only.

Explicitly defer:

- assigning an agent to a product/project/task/chore,
- deriving prompts from work items,
- linking an agent transcript to a work item,
- automatically creating a task from a PR.

When that integration happens later, the likely model is:

- agents execute against work items,
- agent runs create activity history on tasks/chores,
- projects become containers for multi-step execution plans.

That future direction is one reason to persist these entities now rather than
leaving them as purely frontend state.

## Implementation Plan

### Phase A: Backend Persistence

1. Introduce a SQLite-backed store in the engine.
2. Add schema creation/migration on startup.
3. Implement CRUD and `get_work_tree`.
4. Add protocol messages and validation.

### Phase B: Frontend Work Navigator

5. Add frontend models for product, project, and task/chore.
6. Add a top-level `Agents` / `Work` navigation switch.
7. Build the work tree navigator and detail panes.
8. Add create/edit flows for the minimal metadata fields.

### Phase C: Polish

9. Remember expanded/selected work nodes across launches.
10. Add search/filtering within work entities.
11. Add simple status badges, PR links, and task reordering UI affordances.

## Design Decisions

- Products use `repo_remote_url` as their canonical repository identity.
- The product model should not store a single canonical local repo root.
- Future multi-repo support should extend the product model with additional
  remote identities rather than local checkout paths.
- `Task` is the canonical backend term; `phase` is a project-contextual label
  for ordered tasks.
- Ordered phases/tasks are part of the domain model, and Boss should be able to
  reorder them explicitly.
- In the longer-term direction, a project design doc may become the canonical
  source of truth for phase structure, with Boss synchronizing that structure
  into SQL.
- Deletion of tasks and chores should be soft deletion, not hard deletion.

## Recommended Defaults

- Treat `task` as the canonical backend term and `phase` as contextual UI copy.
- Store chores in the `tasks` table with `kind = 'chore'`.
- Use SQLite for persistence from the start.
- Keep work navigation separate from agent navigation.
- Ship read/write CRUD first and defer automation/agent linkage.
