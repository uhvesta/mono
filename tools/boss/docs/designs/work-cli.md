# Boss: Work CLI Design

## Overview

Boss needs a command-line interface for manipulating the persisted Work state
without going through the macOS app. The intended users are:

- humans working in a terminal,
- local coding agents that need a stable automation surface,
- scripts and automation that want to create or update work items.

The CLI should expose the same product/project/task/chore model already defined
in [`work-taxonomy`](work-taxonomy.md) and used by the macOS app. It should not
introduce a parallel storage path or a second source of truth.

The target shape is:

```text
boss product create
boss product list
boss project create
boss task create
boss chore create
boss task update task_123 --status review
boss task move task_123 --to review
```

## Goals

- Provide a human-friendly terminal interface for Boss work entities.
- Provide an agent-friendly, scriptable surface with stable machine-readable
  output.
- Reuse the engine-owned SQLite store and validation logic.
- Keep the work state consistent across the macOS app, the CLI, and future
  automations.
- Make common operations fast: create, inspect, list, update, move, reorder,
  delete.

## Non-Goals

- Replacing the macOS app as the primary board UI.
- Building a full interactive TUI.
- Adding deep workflow automation or agent assignment in this phase.
- Exposing every engine feature through the first version of the CLI.
- Supporting direct remote/multi-machine orchestration beyond the local Boss
  engine.

## Design Principles

### One Source of Truth

The Boss engine and its work database remain canonical. The CLI should not
write directly to SQLite in normal operation.

### Human and Agent Modes

The same commands should work for both humans and agents:

- Humans should be able to run `boss product create` and answer prompts.
- Agents should be able to run `boss product create --name Boss --json --no-input`
  and get deterministic output.

### Entity-First Command Grammar

The CLI should follow `boss <entity> <verb>` rather than a generic
`boss work <verb>`. That keeps commands short and makes discovery easy.

### Stable Automation Surface

Anything an agent is expected to parse should be available in JSON with stable
field names and exit codes.

## Proposed Binary Layout

Introduce a new user-facing binary named `boss`.

- `boss`: human and automation CLI.
- `boss-engine`: long-running local service and lower-level implementation
  detail.

The current `boss-engine` binary should remain because the macOS app already
depends on it. The new `boss` binary becomes the intended entry point for
humans and scripts.

## Runtime Model

### Primary Path

The CLI should talk to the local Boss engine over the existing unix socket.

That gives one path for:

- validation,
- persistence,
- future authorization and auditing,
- change notifications to other clients.

### Engine Discovery

The CLI should resolve the engine socket path using:

1. `--socket-path`
2. `BOSS_SOCKET_PATH`
3. default `/tmp/boss-engine.sock`

### Auto-Start

If the socket is missing, `boss` should auto-start the engine by default for
work commands, using the same local defaults the app uses.

Add:

- `--no-engine-autostart` to fail instead of launching the engine,
- `boss engine status`,
- `boss engine start`,
- `boss engine stop`.

Note: `--no-autostart` is a *separate* concern — it suppresses worker
auto-dispatch for newly created work items (and the seed `design` task on
`project create`), but still lets the CLI bring the engine up to service the
request. The engine is the system of record, so it must always be reachable to
create or track work. Suppressing transparent engine startup is the job of
`--no-engine-autostart` alone.

The explicit `engine` subcommands are useful for humans, but they are secondary
to the work commands.

### Auth Decoupling

Work-only commands must not require ACP auth or `ANTHROPIC_API_KEY`.

Today the engine loads ACP/auth config eagerly at startup. That is acceptable
for the current app PoC, but it is the wrong shape for a durable work CLI.

The engine should be refactored so that:

- work-state startup requires only work-db/socket configuration,
- ACP config is loaded lazily when an agent operation actually needs it,
- a local work-only engine can run even when no agent credentials are present.

This is required for the CLI to be reliable in scripts and on machines where
the human only wants planning/tracking behavior.

## Command Model

## Top-Level Entities

The first version should support:

- `product`
- `project`
- `task`
- `chore`
- `engine`

`task` and `chore` stay separate in the CLI even though chores reuse the tasks
table in storage. That matches the user’s mental model and keeps commands
clear.

## Verbs

Recommended first-phase verbs:

- `create`
- `list`
- `show`
- `update`
- `move`
- `delete`

Additional first-phase verb for ordered project tasks:

- `reorder`

Optional follow-up verbs:

- `archive`
- `pause`
- `done`
- `restore`

These can all be expressed through `update` initially, so they do not need to
block the first version.

## Examples

### Products

```bash
boss product create
boss product create --name Boss --repo git@github.com:spinyfin/mono.git
boss product list
boss product show boss
boss product update boss --description "Multi-agent coding manager"
boss product update boss --status paused
```

### Projects

```bash
boss project create --product boss --name "Work CLI"
boss project list --product boss
boss project show work-cli --product boss
boss project update work-cli --product boss --goal "Ship the first human/agent CLI"
boss project update work-cli --product boss --priority high --status active
```

### Tasks

```bash
boss task create --product boss --project work-cli --name "Socket request client"
boss task list --product boss --project work-cli
boss task show task_123
boss task update task_123 --status review --pr-url https://github.com/spinyfin/mono/pull/99
boss task move task_123 --to review
boss task reorder --project work-cli --product boss --ids task_2,task_1,task_3
boss task delete task_123
```

### Chores

```bash
boss chore create --product boss --name "Trim stale work items"
boss chore list --product boss
boss chore show task_456
boss chore update task_456 --status doing
boss chore move task_456 --to done
boss chore delete task_456
```

## Interactive vs Non-Interactive Behavior

### Interactive TTY Mode

When stdin and stdout are TTYs, commands should prompt for missing required
fields.

Examples:

- `boss product create` prompts for `name`, `description`, and `repo remote`.
- `boss project create` prompts for product selection, then project fields.
- `boss task create` prompts for product, project, task name, and description.

Selection prompts should be constrained and validated:

- products selected from existing products,
- projects filtered to the selected product,
- status values chosen from the canonical enum.

### Non-Interactive Mode

When stdin is not a TTY, or when `--no-input` is set:

- missing required inputs are errors,
- no prompts are shown,
- stdout is reserved for results,
- stderr is used for diagnostics.

This is the mode agents and scripts should use.

## Output Model

### Human Output

Default human output should be concise and readable:

- `create`/`update`/`move`/`delete`: one-line success plus short details,
- `show`: labeled fields,
- `list`: aligned table with IDs, names, status, and key context.

Example:

```text
Created task task_123
Name: Socket request client
Product: Boss
Project: Work CLI
Status: backlog
```

### Machine Output

Add:

- `--json` for JSON output,
- `--quiet` for reduced human output,
- `--no-input` to forbid prompts.

JSON output should be stable and explicit:

- `create`/`show`/`update`: return the entity object,
- `list`: return an object with a typed array field,
- `delete`: return `{ "id": "...", "deleted": true }`,
- `move`: return the updated task/chore object.

Example:

```json
{
  "task": {
    "id": "task_123",
    "product_id": "prod_boss",
    "project_id": "proj_work_cli",
    "kind": "project_task",
    "name": "Socket request client",
    "status": "backlog"
  }
}
```

## Exit Codes

Use stable non-zero codes for automation:

- `0`: success
- `2`: usage or validation error
- `3`: not found
- `4`: conflict or ambiguous selector
- `5`: engine unavailable
- `6`: engine returned application error
- `7`: internal CLI failure

The exact numbers are less important than keeping them stable once published.

## Selectors and Identity

### Products and Projects

Products and projects should accept selectors by:

- `id`
- `slug`

Do not rely on fuzzy name matching in the first version. Names are too easy to
duplicate or change.

### Tasks and Chores

Tasks and chores should be addressed by `id` in the first version.

That matches the current data model and avoids inventing task slugs before
there is a proven need for them.

### Disambiguation

When a command accepts a project slug, require product context if necessary:

```bash
boss project show work-cli --product boss
```

This keeps resolution simple and deterministic.

## Status vocabulary

The board (kanban) names are the canonical CLI vocabulary. They are what `--status` help lists, what `move --to` advertises, and what `--json` and human output emit for a leaf work item's `status`:

- `backlog`
- `doing`
- `blocked`
- `review`
- `done`

The engine still *stores* the original strings, and they remain accepted as input aliases everywhere a status is parsed (`--status`, `--to`), so old scripts and stored data keep working:

- `todo` ↔ `backlog`
- `active` ↔ `doing`
- `in_review` (also spelled `in-review`) ↔ `review`
- `blocked` ↔ `blocked` (unchanged)
- `done` ↔ `done` (unchanged)

Because output is always remapped to the board name, a row stored as `active` lists/shows as `doing` regardless of which spelling set it. The translation lives entirely in the CLI (`status_vocab` in `tools/boss/cli/src/main.rs`); the engine, wire protocol, and SQL rows are untouched, keeping the database explicit.

## Engine Protocol Changes

The current socket protocol is frontend-oriented and assumes one active UI
connection. A robust CLI requires a cleaner request/response path.

## Requirements

### 1. Multiple Concurrent Clients

The engine should support:

- the macOS app,
- one or more CLI invocations,
- future automation processes,

all at the same time.

Today, work state is shared via SQLite, but connection-local event handling is
still shaped around a single client. The CLI design should treat multi-client
behavior as a first-class requirement.

### 2. Request Correlation

The CLI needs a deterministic way to match a request to its result.

Add a `request_id` field to socket requests and responses. The engine should
return exactly one correlated response per command.

Recommended wire shape:

Request:

```json
{
  "request_id": "req_123",
  "type": "create_task",
  "product_id": "prod_boss",
  "project_id": "proj_work_cli",
  "name": "Socket request client"
}
```

Response:

```json
{
  "type": "response",
  "request_id": "req_123",
  "ok": true,
  "result": {
    "task": { "...": "..." }
  }
}
```

Error response:

```json
{
  "type": "response",
  "request_id": "req_123",
  "ok": false,
  "error": {
    "code": "not_found",
    "message": "unknown project: proj_work_cli"
  }
}
```

### 3. Broadcast Work Events

In addition to direct responses, the engine should broadcast work-change events
to subscribed clients so the macOS app stays in sync when a CLI command
modifies state.

That includes:

- `work_item_created`
- `work_item_updated`
- `project_tasks_reordered`
- `work_item_deleted`

The CLI does not need to subscribe in the first version, but the app should.

## Backend API Surface

The current engine already has create/update/delete plus:

- `list_products`
- `list_projects`
- `get_work_tree`

The CLI should add explicit read methods so it does not have to fetch an entire
product tree for every lookup.

Recommended additions:

- `get_product`
- `get_project`
- `get_task`
- `list_tasks`
- `list_chores`

Optional later addition:

- `list_work_items` with filters for product/project/kind/status

The first version can implement separate task/chore list APIs if that keeps the
shape simpler.

## CLI Architecture

## New Crate

Add a new Rust crate under `tools/boss/cli/`:

- binary name: `boss`
- dependency on shared protocol/client code
- no direct dependency on AppKit or macOS UI code

## Shared Client Layer

Factor a small reusable socket client library out of the engine/frontend code.

Responsibilities:

- socket discovery,
- optional engine auto-start,
- request serialization,
- response correlation,
- error mapping,
- JSON decoding.

This client should be used by:

- the new CLI,
- potentially future TUI/web frontends,
- tests.

## Shared Protocol Types

The request/response schema should live in a shared Rust module or crate rather
than being defined ad hoc inside `app.rs`.

That reduces drift between:

- engine server handling,
- CLI client code,
- future non-macOS clients.

## Human Prompt Layer

For interactive flows, add a thin prompt layer on top of Clap parsing.

Recommended behavior:

- Clap parses explicit flags and positionals first.
- Any missing required values are filled via prompt only when TTY + input
  allowed.
- Prompting happens in the CLI layer, not in the engine.

This keeps the engine deterministic and makes agent automation straightforward.

## Initial Command Set

The first implementation should ship:

- `boss product create|list|show|update`
- `boss project create|list|show|update`
- `boss task create|list|show|update|move|delete|reorder`
- `boss chore create|list|show|update|move|delete`

That is enough to cover the current macOS app’s work lifecycle.

## Future Extensions

Reasonable follow-ons after the first version:

- `boss board show --product boss`
- `boss product archive`
- `boss task restore`
- `boss import` / `boss export`
- shell completion
- `--jq`-friendly JSON refinements
- agent-related commands under `boss agent ...`

## Testing Strategy

### Unit Tests

- selector parsing,
- status alias mapping,
- prompt gating (`TTY` vs `--no-input`),
- exit code mapping.

### Integration Tests

- start temporary engine on a temp socket and temp DB,
- create/list/update/delete across product/project/task/chore flows,
- verify JSON output shapes,
- verify app-facing broadcast events still occur when CLI mutates work state.

### Golden Tests

- human-readable `list` output,
- `show` output,
- error messages for common failures.

## Implementation Plan

### Phase 1: Runtime Foundations

1. Split work-only runtime config from agent/ACP config.
2. Add multi-client engine handling for work commands.
3. Add correlated request/response support.
4. Extract shared protocol and socket client code.

### Phase 2: Core CLI

5. Add the `boss` Rust binary.
6. Implement `product`, `project`, `task`, and `chore` subcommands.
7. Add `--json`, `--quiet`, and `--no-input`.
8. Add interactive prompting for create/update flows.

### Phase 3: Polish

9. Add `move` and `reorder` ergonomics.
10. Add engine lifecycle subcommands.
11. Add shell completions and better table output.
12. Add end-to-end docs and examples for humans and agents.

## Design Decisions

- The public CLI binary should be `boss`, not `boss-engine`.
- The CLI should talk to the engine over the local socket by default.
- Work-only commands must not require ACP auth.
- Interactive prompts are a CLI concern, not an engine concern.
- JSON output and stable exit codes are mandatory for agent use.
- Product/project selectors should use `id` or `slug`; task/chore selectors
  should use `id` initially.
- Board-friendly verbs like `move --to review` should map onto canonical
  storage statuses.

