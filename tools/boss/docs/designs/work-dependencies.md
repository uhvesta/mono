# Boss: Work-Item Dependencies

## Problem

Boss's planning model has products, projects, tasks, and chores, but no way to say *"don't start B until A is done."* Today the only ordering signal is intra-project: tasks within a project are sorted by `ordinal` and `work.rs:462` makes only the first incomplete task `ready`, which collapses every other task into `waiting_dependency` regardless of whether it actually depends on the leader. Anything else — a chore that should follow another chore, a task in project P2 that needs a deliverable from project P1, a project that gates on a small bit of plumbing — has to be enforced by humans pacing the engine.

`status: blocked` exists as a kanban column and a `TaskStatus` variant (`cli/src/main.rs:408`), but it is a *manual* flag. The engine never sets it, never honours it for dispatch (the auto-dispatcher just looks at `work_executions.status`), and never clears it. So in practice it functions as a sticky note for humans, not a scheduling primitive.

This doc proposes an explicit, edge-based dependency model: a separate `work_item_dependencies` table, an automatic `blocked` ↔ `todo` transition driven by prerequisite completion, a cycle-safe CLI surface, and dispatch / kanban behaviour that respects edges. The aim is the smallest change to the existing model that turns "B depends on A" into a first-class declaration the engine understands, while leaving cross-product / multi-repo work to a separate design (`proj_18a2bbe20fc03718_8`).

## Goals

- Declare typed edges between two work items of any kind (task, chore, project) within the same product.
- Engine refuses to dispatch a dependent while any prerequisite is incomplete.
- Engine auto-toggles a dependent's kanban status to `blocked` while gated, and back to `todo` (or whatever the prior status was) when unblocked.
- CLI verbs to add / remove / list edges and to filter `*-list` views by edge.
- Cycle prevention enforced at edge creation time.
- Show the dep graph on `boss <kind> show` and on the kanban card detail.
- Migration is a no-op for existing data — every row starts with no edges.

## Non-Goals

- **Cross-product / multi-repo edges.** Filed under `proj_18a2bbe20fc03718_8`. Storage column will allow it physically (`prerequisite_id` is just an opaque id) but CLI / engine reject it for v1.
- **Soft dependencies, "relates-to", "duplicates", FYI links.** v1 ships exactly one edge type: `blocks`. Other kinds get a column for forward compatibility but no codepath beyond storage.
- **Backfill of inferred edges from project parent-child structure.** The existing intra-project ordering by `ordinal` keeps working as-is for projects that don't add explicit edges; we don't try to materialise it as concrete rows.
- **Dependency-aware ordering of the kanban column** (i.e. sorting Todo so dependents appear after their prerequisites). Out of scope; the existing ordinal sort is fine.
- **Engine-side propagation of `done` *across* repositories.** A dependent in repo X cannot have a prerequisite in repo Y until cross-product is built.
- **Per-edge metadata / comments / weights / lag times.** Edges are pure (id → id, kind).

## Naming

- The relation row goes in **`work_item_dependencies`** — `dependent_id` (the row that's gated) and `prerequisite_id` (the row that gates it). I keep the words `dependent` and `prerequisite` consistent throughout: one clear vocabulary, no ambiguous "depends" / "blocks" overloading. "Prerequisite is done → dependent unblocks" reads correctly in both directions.
- Edge type is **`relation`**, default `'blocks'`. We deliberately do not call this column `kind` because `tasks.kind` already exists and means task-vs-chore.
- The kanban status stays **`blocked`**. We don't introduce `auto_blocked` or `dep_blocked` — fewer states, and the user's mental model of "blocked" already aligns.

---

## Design Question 1 — Edge Storage

### Options

| | (a) `depends_on: Vec<id>` column on `tasks` | (b) join table `work_item_dependencies` |
|---|---|---|
| Cardinality | One column carries 0..N ids (TEXT JSON or comma-separated) | Standard many-to-many row-per-edge |
| Indexable lookups (`who depends on X?`) | Painful (LIKE / JSON1) | Cheap (index on `prerequisite_id`) |
| Cycle detection | Have to parse the column on every check | Trivial recursive CTE |
| Schema for projects | Need to add the column to `projects` too — duplicated logic | One table covers all kinds |
| Edge-type field | Awkward (parallel array, or per-element JSON object) | Natural — extra column |
| Migration cost | One column on each of `tasks`, `projects` | One new table |

### Discussion

(a) is what was originally suggested in the chore. It is appealing because the dependent row "owns" its declaration. But every interesting query is *reverse* — "what depends on X, can X be archived, does completing X unblock anyone?" — and those become full table scans plus JSON parsing. We'd also have to apply the same column twice (tasks and projects), and keep their semantics in sync.

(b) is the standard shape. One row per edge, two foreign keys, cheap lookups in either direction, room for an edge-type column without contortions. The main cost is one more table, but Boss already has nine and the new one fits the pattern.

The interaction with the existing `project_id` parent-child column on `tasks` is conceptually distinct: `project_id` says *which project a task belongs to* (containment), while a dependency edge says *which item gates this one* (ordering). They can coexist — a task can sit in project P and also depend on a task in project Q within the same product — but I do not unify them. Containment is one-to-many and structural; dependency is many-to-many and behavioural. Mashing them together would force every task to either nominate one parent project or pretend its project is "just another prerequisite," and would make the kanban-by-project grouping and the dep-graph rendering collide in ugly ways.

### Recommendation

**Pick (b).** A `work_item_dependencies` table with `(dependent_id, prerequisite_id, relation)` as the primary key.

```sql
CREATE TABLE IF NOT EXISTS work_item_dependencies (
    dependent_id     TEXT NOT NULL,
    prerequisite_id  TEXT NOT NULL,
    relation         TEXT NOT NULL DEFAULT 'blocks',
    created_at       TEXT NOT NULL,
    PRIMARY KEY (dependent_id, prerequisite_id, relation),
    CHECK (dependent_id <> prerequisite_id)
);

CREATE INDEX IF NOT EXISTS work_item_dependencies_prereq_idx
    ON work_item_dependencies(prerequisite_id, relation);
CREATE INDEX IF NOT EXISTS work_item_dependencies_dependent_idx
    ON work_item_dependencies(dependent_id, relation);
```

Two indexes because both directions are hot: completion of a prerequisite needs *"who depends on me?"* and the kanban / `show` view needs *"what gates this one?"*

`dependent_id` and `prerequisite_id` are not declared as foreign keys to a single physical table because the referent can be a row in `tasks` *or* `projects`. We enforce existence and cross-kind validity in the engine's edge-create path (Q3) rather than at the schema level. Cascade on delete is also in the engine — a dependent or prerequisite that is deleted (`deleted_at` set, or hard-removed from the rare `projects` delete path) drops all its edges in the same transaction.

### Self-references and cycles

- The `CHECK` rejects the trivial `A → A` self-loop at the schema level.
- Multi-step cycles are checked in the engine on `dependency add`: before insert, run a recursive CTE starting from the proposed `prerequisite_id` and walk forward; if `dependent_id` is reachable, refuse with `CliError::Conflict("creating this edge would form a cycle: A → B → ... → A")`. A recursive CTE on a few hundred edges is fast and the path is non-hot (edge create).

```sql
WITH RECURSIVE forward(id) AS (
    SELECT prerequisite_id FROM work_item_dependencies WHERE dependent_id = ?1
  UNION
    SELECT d.prerequisite_id
    FROM work_item_dependencies d
    JOIN forward f ON d.dependent_id = f.id
)
SELECT 1 FROM forward WHERE id = ?2 LIMIT 1;
```

We run this with `?1 = proposed_prerequisite_id` and `?2 = proposed_dependent_id`; a row means the new edge would close a cycle.

---

## Design Question 2 — Dependency Types

### Options

- `blocks` — hard gate. Dependent cannot dispatch while any prerequisite is incomplete.
- `relates-to` — informational link, no scheduling effect.
- `duplicates` — informational, with maybe an "archive on completion of the other" rule later.
- Soft block — a warning at dispatch, not a refusal.

### Recommendation

**v1 ships only `blocks`.** Add the `relation` column with a default of `'blocks'` so we don't have to migrate again to introduce siblings; reject any other value at the CLI layer until we ship them.

Reasons:

- Soft block adds a warning surface that has to be designed (CLI, kanban tooltip, JSON output). With one block type and no soft variant, the rule is crisp: `done → ungated`, anything else → gated.
- `relates-to` and `duplicates` are nice-to-haves, but the right time to ship them is when we have a UI affordance (the dep-graph view) that can render non-blocking edges visibly distinct.
- v1 is small enough that a clear "no, refuse" is better UX than a clear "warn, let through."

### How to apply

The CLI accepts `--relation blocks` and validates against an enum that today only has one valid variant. Storage already allows for more. Engine-side dispatch logic *only* checks `relation = 'blocks'` rows when computing whether a dependent is gated, so adding a `relates-to` row in a future migration has zero behavioural effect.

---

## Design Question 3 — Cross-Scope Edges

### Options

- **(i)** Same-project only.
- **(ii)** Cross-project within a product.
- **(iii)** Cross-product (covered by `proj_18a2bbe20fc03718_8` — out of scope).

### Recommendation

**Pick (ii). Same-product, cross-project, cross-kind.**

`(i)` is too restrictive: a chore that gates an entire project, or a project A that must precede project B, cannot be expressed. Both are real cases I've hit while planning the bossctl/macos work.

`(ii)` carries the storage cost of one row per edge regardless of where the endpoints sit, which is identical to `(i)`. The UX win is real — the dep graph can express "feature A depends on the auth refactor (different project)" without inventing a fake parent project to host both.

`(iii)` requires repository remoting (the engine on repo X does not see rows in repo Y) and a coordinator decision about which side owns the edge. Filed under cross-product.

### How to apply

Edge create validates `product_id_for_work_item(dependent) == product_id_for_work_item(prerequisite)`. If they differ, refuse with `CliError::Conflict("dependency edges must stay within a single product; cross-product edges are tracked in proj_18a2bbe20fc03718_8")`. Across kinds (project-→-task, chore-→-task, etc.) is allowed — the rules in Q4 work uniformly because they read each side's `status` field.

---

## Design Question 4 — Status Mechanics

### The rule

A dependent is **gated** iff there exists at least one edge `(dependent_id = D, prerequisite_id = P, relation = 'blocks')` such that `P` is not in a *satisfied* status.

Satisfied statuses:

- For tasks/chores: `done`.
- For projects: `done` or `archived`. (See Q10 for why `archived` counts.)
- `in_review` is **not** satisfied. The signal is "the work is finished and merged"; an unmerged review can still bounce. Once review concludes, the move to `done` (manual or automatic) is the unblock trigger.

While gated, the dependent's `status` is forced to `blocked`. When the last gating prerequisite transitions to a satisfied status, the dependent's `status` flips out of `blocked` to `todo`.

### Is the auto-flip on gate symmetric and clean?

Two edge cases need explicit answers:

**(1) The user manually moves a gated dependent to `active` (or any non-`blocked`) status.** Refuse the manual move while edges remain gating. Surface the reason in the CLI output: `cannot move task_X to active: gated by [task_Y, task_Z] (use boss task depend rm to remove)`. This keeps the kanban-vs-engine model honest. The alternative — letting the user override and run anyway — recreates the current ambiguous "blocked" flag and we've already learned that's bad UX.

**(2) The user manually moves a non-gated item to `blocked`.** Allowed. We respect the manual flag — the engine considers an item gated if either (a) any prerequisite is incomplete, or (b) the user has explicitly set status to `blocked`. The auto-unblock path only flips `blocked → todo` when *both* (a) and (b) are false; if the user manually blocked it, the rule (a) clearing is a no-op.

To make (2) work cleanly without a separate column, we let the existing `tasks.status` carry the manual signal and add a derived "is gated" predicate driven by edges. A small `manual_blocked` bit on `tasks` would be cleaner but it doesn't pay for itself — the rule above handles it without a schema change. Specifically, `clear_auto_block` only fires when both:

- the work item's current `status` is `blocked`, and
- the most recent kanban move recorded for this item was the engine itself (we already write `updated_at` on every move; we add a tiny `last_status_actor` column in the same migration to disambiguate engine vs. human moves).

If `last_status_actor = 'engine'` and edges are clear, auto-unblock to `todo`. Otherwise, leave alone.

### Diagram

```
                                  manual block
                                  ┌──────────┐
                                  ▼          │
       (no edges)              ┌──────┐  manual unblock (allowed
   todo ───────────► active ──►│ blocked├────► todo / active
    │ ▲                ▲       └──┬───┘  if no gating edges)
    │ │                │          ▲
    │ │  human moves   │  edges   │  edges become satisfied
    │ │  to in_review  │  become  │  (last gater → done/archived)
    │ │  / done        │  unsat'd │  AND last_status_actor = 'engine'
    │ │                │  (any    │
    │ │                │  prereq  │
    │ │                │  not     │
    │ │                │  satis.) │
    │ │                │          │
    │ │              ┌──────────┐ │
    │ └──────────────│ in_review│─┘
    │                └────┬─────┘
    │                     │ done / merge
    │                     ▼
    │                  ┌──────┐
    └─────────────────►│ done │
                       └──────┘
                          │
                          ▼
                       archived (manual)
```

Edges that point at *projects* read the project's status the same way; the diagram applies, and a project entering `done` or `archived` ungates anything depending on it.

### Why not refuse manual `active`?

We already do — see (1) above.

### `in_review` vs `done`

Treating `in_review` as satisfied would be friendlier to "unblock the next task as soon as the PR is up," but it's wrong in the bad case: a PR that gets bounced for substantive reasons may never land. The dependent would already be running on a foundation that hasn't actually shipped. `done` is the merge moment; that's the right gate.

### Q5 escape hatch

If a row's status is stuck (the chore_18ad2632b2b998f0_6 PR-detection bug) but its PR is merged, dependents shouldn't silently stall — see the next question.

---

## Design Question 5 — PR-Mediated Completion

### The bug context

`chore_18ad2632b2b998f0_6` documents a real failure mode: PR detection occasionally misses a merge, so a work item's `status` stays at `in_review` forever even though the PR closed. Today this is annoying but contained — the kanban shows it in the wrong column. With dependencies in play, every dependent waits forever, and nothing notifies the human.

### Options

- **A)** Treat `status = done` as the only signal. Rely on humans / the PR-detection fix to keep status accurate.
- **B)** Treat `pr_url` set + PR merged as "effectively done" for the purposes of unblocking, even if `status` hasn't transitioned.
- **C)** A hybrid: `status = done` is the canonical signal; if `status != done` but `pr_url` is merged AND merged > 24h ago, surface a *warning* (`bossctl agents probe` style) but don't unblock.

### Recommendation

**Pick (A) for v1, with a follow-up to fix `chore_18ad2632b2b998f0_6` first.**

(B) is tempting, but it widens the behavioural surface significantly. The "PR merged" check requires a GitHub API call (or a cached state) on every dependency satisfaction probe; it muddies the rule (now `done` *or* `pr_url merged` means satisfied); and it papers over the bug rather than fixing it. (C) compounds (B)'s API cost without the unblock-on-merge benefit.

The simpler answer is: **fix PR detection.** As long as `status = done` flips reliably when a PR merges, dependencies-on-status is the right model. If we ship dependencies before the PR-detection fix, we add observability — see "Stuck-prerequisite warning" below — but the unblock rule stays simple.

### Stuck-prerequisite warning (lightweight)

If any prerequisite is in `in_review` for more than 48 hours, the dependent's kanban card shows a small "prereq in review > 48h" badge in the blocked lane. This is implemented in the UI layer reading existing fields (`updated_at` on the prereq); no engine change. It's purely informational and gives the human a nudge to investigate without changing dispatch behaviour.

---

## Design Question 6 — CLI Surface

### Verbs

```text
boss <kind> depend add    <dependent> <prerequisite> [--relation blocks]
boss <kind> depend rm     <dependent> <prerequisite> [--relation blocks]
boss <kind> depend list   <selector>  [--direction prereqs|dependents|both]
```

`<kind>` is `task`, `chore`, or `project`. The same verb set lives under each, but each subcommand is a thin wrapper over the same engine call (the engine doesn't care about `kind` — it just resolves selectors to ids and validates same-product). Putting the verbs under each kind keeps the CLI grammar consistent (`boss task ...`, `boss chore ...`) without forcing the user to learn a new top-level noun.

#### Examples

```text
# A's completion gates B
boss task depend add task_18ad6475573e1a88_11 task_18ad6475573e1a88_07

# Same with selectors that aren't ids (uses existing selector resolution)
boss task depend add "ship the kanban dot fix" "land the dispatcher cap fix"

# What's gating B? What does A unblock?
boss task depend list task_18ad6475573e1a88_11 --direction prereqs
boss task depend list task_18ad6475573e1a88_07 --direction dependents
boss task depend list task_18ad6475573e1a88_11 --direction both

# Remove
boss task depend rm  task_18ad6475573e1a88_11 task_18ad6475573e1a88_07
```

`add` returns the new edge as JSON in `--json` mode and a one-line confirmation in human mode. `add` on an already-present edge is idempotent (no error). `rm` on a missing edge is a no-op (no error) — same shape as `boss task delete` on an already-archived task.

### Filters on existing list verbs

Add to `boss task list`, `boss chore list`, `boss project list`:

```text
--prerequisites-of <selector>     items that the named row depends on (its incoming edges)
--dependents-of    <selector>     items that depend on the named row (its outgoing edges)
--unblocked                       items in `todo` with no gating prerequisite
--blocked-by-deps                 items currently gated by at least one incomplete prereq
```

`--unblocked` answers the question *"what's actually dispatchable right now?"* without the human having to cross-reference the dep graph. `--blocked-by-deps` answers the inverse for triage.

### `boss <kind> show` output

`show` already prints a multi-section view (header, description, status, project, etc.). Add a **Dependencies** section at the bottom:

```text
Dependencies:
  Prerequisites (2):
    task_18ad...07  done       "land the dispatcher cap fix"
    proj_18a2...05  active     "Task and project dependency system"  ← INCOMPLETE
  Dependents (1):
    chore_18ad...22  blocked   "wire kanban dep-graph render"
```

In `--json`, the section becomes:

```json
{
  "dependencies": {
    "prerequisites": [
      {"id": "task_18ad...07", "relation": "blocks", "status": "done", "name": "..."},
      {"id": "proj_18a2...05", "relation": "blocks", "status": "active", "name": "..."}
    ],
    "dependents": [
      {"id": "chore_18ad...22", "relation": "blocks", "status": "blocked", "name": "..."}
    ]
  }
}
```

### Reference doc

The CLI reference (`boss reference`) gets a new `status_semantics` line: *"`blocked` is set automatically when a work item has at least one incomplete `blocks` prerequisite. Manual moves to non-`blocked` statuses are refused while gated."* And a workflow guidance line: *"Use `boss <kind> depend add A B` to declare 'A depends on B'."*

---

## Design Question 7 — UI Surfacing

### Kanban card

A dependent whose status is `blocked` is *already* surfaced today by the existing Blocked lane (`work-kanban.md`). Two changes to make the lane carry its weight:

1. **Card badge.** A small icon on the card — the existing card layout has space in the footer row alongside the project tag and PR link (see `WorkBoardCardView`, `app-macos/Sources/ContentView.swift`). Use a "link" or "chain" SF symbol. Hover / click → tooltip lists the gating prerequisites with their current status, e.g. *"gated by: task_…07 (in_review), proj_…05 (active)"*. The icon only appears when the block is *automatic* (driven by edges); manual blocks get no icon, only the lane.
2. **Dimming.** No. The lane already says "blocked"; dimming on top of it doubles up. Save the dim state for the worker live-status path (different concept — that's about staleness, not blockedness).

### Drag refusal

When the user drags a card from Blocked → Doing while it has an automatic gate, the kanban refuses the drop and shows an inline warning in the source lane: *"task_X is gated by 2 incomplete prerequisites — clear them or remove the edge first."* For manual blocks, the drop is allowed; the user explicitly chose `blocked`, they can choose to leave.

### Card detail — dep graph

`WorkBoardCardView` already supports an inspector / detail popover. Add a **Dependencies** subsection there, mirroring the CLI `show` output: incoming edges (prereqs) and outgoing edges (dependents) as two short lists with each row hyperlinked to the corresponding card.

For a richer graph (multi-hop), defer. We can revisit once we know whether the dep graphs people actually build are wide and shallow (a single layer of fan-out), in which case the list view is fine, or deeper, in which case we need a graph render. My read is wide and shallow; ship the list and see.

### Lane swap

No automatic lane swap on a manual move. If a user drags a card to Doing while gated, refuse (above) — don't quietly swap it back to Blocked, that is confusing. The user moved on purpose; tell them why it failed.

---

## Design Question 8 — Dispatch Behaviour

### Dispatcher gate

`auto_dispatch` (the engine path that turns `ready` executions into running runs) reads the work item's status. We add one filter: if the dependent has *any* `(relation = 'blocks')` edge whose prerequisite is not in a satisfied status, the execution is held in `waiting_dependency`, not `ready`. The existing `waiting_dependency` execution status handles this perfectly — it's already in the enum and already excluded from the dispatcher's pickup pool.

The reconcile path (`work.rs:430`-`485`) already computes `waiting_dependency` for non-leader tasks within a project. We extend it to also consider `work_item_dependencies` rows: an item that's the project's leader still gets `waiting_dependency` if it has unmet edges; a non-leader stays `waiting_dependency` regardless.

The CLI's explicit `RequestExecution` path (`bossctl work start`) bypasses the auto-dispatcher's gate today. We keep that exception narrow: explicit requests that name a gated work item should refuse with a clear error (`cannot start task_X: gated by [task_Y, ...] — use boss task depend rm <X> <Y> first or wait for it to complete`) rather than silently override. A manual override flag (`--force`) is a future-tense decision; v1 is no-override.

### Auto-start on unblock

When the last gating prerequisite is satisfied and the dependent's `autostart` flag is true, the engine flips its status from `blocked` → `todo` and reconciles a `ready` execution in the same transaction. The original create-time `autostart` was a one-shot, but the meaning generalises cleanly: `autostart = true` means *"as soon as you're eligible, run me."* The unblock moment is the second eligibility window the engine cares about. If the human flipped `autostart` to false in the meantime (no current CLI verb for this, but it's a column they could edit), respect it.

### Interaction with the dispatcher cap regression

`task_18ad6475573e1a88_11` is about the dispatcher cap not freeing slots correctly when a worker terminates. That's an orthogonal bug — once a slot is free, it picks the next `ready` execution. Dependencies don't introduce new dispatch logic; they just change which executions hit `ready`. So the cap regression is a precondition for shipping (so dispatch actually flows), not a coupling.

### What happens if a dependent has more deps added after dispatch starts?

A worker is already running on a dependent when a human runs `boss task depend add <running-dependent> <new-prereq>`. The engine accepts the edge — we don't have a hook into "is there a live run on this id" that's cheap, and even if we did, the new edge is *informational about future runs* in this case. The current run continues; if it lands and the dependent transitions to `done`, the new edge had no effect. If the run terminates without completing and a future re-dispatch is requested, the new edge gates it normally.

The CLI prints a warning when this happens: *"warning: task_X currently has a running execution; the new dependency edge will only affect future re-dispatches."* No engine-side abort.

---

## Design Question 9 — Migration

### Existing rows

Empty. `work_item_dependencies` starts with zero rows; nothing in `tasks` / `projects` is touched.

The current intra-project `ordinal`-based ordering keeps working unchanged. We don't backfill edges from project parent-child structure for two reasons:

1. **Semantic mismatch.** `ordinal` is "the human's preferred order to do these in" — it's a hint, not a hard prerequisite. Backfilling it as `blocks` edges hardcodes a soft preference into a hard constraint, and a project with five tasks would suddenly have ten edges no one asked for.
2. **Reversibility.** A migration that creates edges has to be undone if we change our mind, and "undo" is harder than "never created in the first place."

Project owners who want explicit ordering can declare it with `boss task depend add` after the migration ships.

### Schema migration

One pure additive `CREATE TABLE` (the table from Q1) plus the `last_status_actor` column on `tasks`:

```rust
fn migrate_dependencies(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_item_dependencies (
            dependent_id     TEXT NOT NULL,
            prerequisite_id  TEXT NOT NULL,
            relation         TEXT NOT NULL DEFAULT 'blocks',
            created_at       TEXT NOT NULL,
            PRIMARY KEY (dependent_id, prerequisite_id, relation),
            CHECK (dependent_id <> prerequisite_id)
         );

         CREATE INDEX IF NOT EXISTS work_item_dependencies_prereq_idx
            ON work_item_dependencies(prerequisite_id, relation);
         CREATE INDEX IF NOT EXISTS work_item_dependencies_dependent_idx
            ON work_item_dependencies(dependent_id, relation);",
    )?;

    if !table_has_column(conn, "tasks", "last_status_actor")? {
        conn.execute(
            "ALTER TABLE tasks ADD COLUMN last_status_actor TEXT NOT NULL DEFAULT 'human'",
            [],
        )?;
    }
    Ok(())
}
```

Add the same `last_status_actor` to `projects` for symmetry. Default `'human'` so existing rows keep manual-block semantics; the engine will write `'engine'` whenever it programmatically transitions.

### Schema version bump

Bump `metadata.schema_version` to `4`.

---

## Design Question 10 — Edge Cases

### Prerequisite is `archived`

Treat `archived` as a satisfied status for `projects` (a project that's been wound down should not be perpetually gating). For tasks/chores, `archived` doesn't appear in the existing `TaskStatus` enum — they go to `done` or sit in their last status. So the rule reduces to: project `done` or `archived` satisfies; task / chore `done` satisfies.

### Prerequisite has `deleted_at` set

A deleted prerequisite is a UI-only deletion (the row sticks around for history). Two legitimate reads:

- **(a) Auto-drop the edge.** Deletion = "this is no longer a thing" → the gate has no meaning. Engine removes the edge in the same transaction.
- **(b) Stay gated.** Deletion is reversible (clear `deleted_at`); preserving the edge means re-adding it later is a no-op.

I recommend **(a)**. Deleted-prereq → drop edge → unblock dependent if no other edges remain. Deletion is a deliberate user action, and a dependent stuck on a tombstone is the worst of both worlds. The CLI's `delete` path emits a one-line note: *"removed 2 dependency edges from task_X."*

### Prereq on a different product / repo

Refused at edge-create (Q3). N/A here.

### Coordinator session scoped elsewhere

Same answer — the engine for product P doesn't know about edges in product Q because cross-product is out of scope. When cross-product ships (`proj_18a2bbe20fc03718_8`), it'll need its own answer to "how does completion in repo Q propagate to repo P's engine," and that's where this question gets a real treatment.

### Dependent has more deps added after dispatch starts

Covered in Q8 — accept, warn, no abort.

### Worker creates a new edge mid-run via the CLI

Workers run in their own workspaces and have CLI access. Nothing prevents them from running `boss task depend add` against the engine. The engine accepts it like any other call — there's no special "from-worker" mode here. If a worker decides mid-run that its task should depend on something else, the warning behaviour from Q8 applies.

### Mass-edge-create through the JSON API

The engine's RPC layer should expose a single `add_dependency` and a single `remove_dependency`. No bulk variant in v1; if a user wants to add ten edges they make ten calls. The cycle check runs per-call; a hypothetical bulk variant would have to run cycle detection across all proposed edges as a batch and is not worth designing until the demand is real.

### A dependent-with-no-prereqs is moved manually to `blocked`

Allowed. `last_status_actor = 'human'`. Auto-unblock will not fire (because edges aren't gating it). The user has to manually move it back, which is the right behaviour.

### A project's dependents are tasks (cross-kind)

A project gating a task is allowed (Q3). The task watches the project's `status`; project → `done` or `archived` ungates it. Reverse direction (a task gating a project) is also allowed but rare — the typical case is a project gating tasks that follow it.

---

## Schema and Wire Summary

### New table

```sql
CREATE TABLE work_item_dependencies (
    dependent_id     TEXT NOT NULL,
    prerequisite_id  TEXT NOT NULL,
    relation         TEXT NOT NULL DEFAULT 'blocks',
    created_at       TEXT NOT NULL,
    PRIMARY KEY (dependent_id, prerequisite_id, relation),
    CHECK (dependent_id <> prerequisite_id)
);

-- both directions are hot
CREATE INDEX work_item_dependencies_prereq_idx
    ON work_item_dependencies(prerequisite_id, relation);
CREATE INDEX work_item_dependencies_dependent_idx
    ON work_item_dependencies(dependent_id, relation);
```

### Column adds

```sql
ALTER TABLE tasks    ADD COLUMN last_status_actor TEXT NOT NULL DEFAULT 'human';
ALTER TABLE projects ADD COLUMN last_status_actor TEXT NOT NULL DEFAULT 'human';
```

### Protocol additions (`tools/boss/protocol/src/types.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemDependency {
    pub dependent_id: String,
    pub prerequisite_id: String,
    #[serde(default = "default_relation")]
    pub relation: String,
    pub created_at: String,
}

fn default_relation() -> String { "blocks".to_string() }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddDependencyInput {
    pub dependent: String,    // selector or id
    pub prerequisite: String, // selector or id
    #[serde(default = "default_relation")]
    pub relation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoveDependencyInput {
    pub dependent: String,
    pub prerequisite: String,
    #[serde(default = "default_relation")]
    pub relation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListDependenciesInput {
    pub work_item: String,
    /// "prereqs" | "dependents" | "both"
    #[serde(default)]
    pub direction: Option<String>,
}
```

`Task` and `Project` gain an optional `last_status_actor: String` so the wire reflects the new column.

The kanban / `WorkTree` payload could include a precomputed dep summary per work item (count of incomplete prereqs, count of dependents) for cheap card rendering — out of scope for the design doc; left to the implementation chore to decide.

### Topic / event

`work.tree` (the existing `WorkTreeUpdated` topic) is the right vehicle. Edges are ambient state of the workspace; dependents care because their `status` flips, and the `Task` / `Project` row already broadcasts on status change. Subscribers of `work.tree` already do a full re-render on every update, so adding edge data to the snapshot is additive.

A separate `work.deps` topic is unnecessary in v1 — edge changes are infrequent and never independent of a status change worth broadcasting anyway.

### CLI verbs (Q6, restated)

```
boss task    depend add  <dependent> <prerequisite> [--relation blocks]
boss task    depend rm   <dependent> <prerequisite> [--relation blocks]
boss task    depend list <selector>  [--direction prereqs|dependents|both]
boss chore   depend ...
boss project depend ...

boss task list   --prerequisites-of <selector>
                 --dependents-of    <selector>
                 --unblocked
                 --blocked-by-deps
boss chore list ...
boss project list ...
```

### Engine module split

- `engine/src/work_dependencies.rs` — pure SQL helpers: `insert_edge`, `delete_edge`, `list_edges`, `would_create_cycle`, `gating_prereqs_for`, `dependents_of`. Unit-tested against a fixture DB.
- `engine/src/work.rs` — extend `reconcile_work_executions` to call `gating_prereqs_for` and downgrade the desired status to `waiting_dependency` if any are unmet. Extend `apply_status_change` to write `last_status_actor` and trigger auto-unblock when a satisfied transition fires.

### App / UI deltas

- Models: decode `last_status_actor`, `WorkItemDependency`.
- Card view: badge in the footer row for auto-blocked items; tooltip lists gating prereqs.
- Card detail / inspector: Dependencies subsection with prereqs and dependents.
- Drag handling: refuse drops out of `blocked` while gated; show the inline warning.

---

## State Diagram (combined)

```
Each work item carries:                 Edges live in work_item_dependencies.
  status            (kanban label)      The engine derives "is_gated" by
  last_status_actor (human|engine)      counting unsatisfied 'blocks' rows.

                           ┌─────────────────────────────┐
                           │            todo             │
                           └──┬──────────────────────────┘
       human moves to blocked│                     ▲
                             │                     │
                             ▼                     │ all gating edges
                        ┌─────────┐                │ satisfied
                        │ blocked │ ◄──────────────┤ AND last_status_actor='engine'
                        └─┬─────▲─┘                │
       human moves       │     │  any edge becomes │
       out of blocked    │     │  unsatisfied      │
       (refused if       │     │  (engine writes,  │
       still gated)      │     │  last_status_actor│
                         │     │  := 'engine')     │
                         ▼     │                   │
                       (allowed if no              │
                        gating edges)              │
                             │                     │
                             ▼                     │
                       todo / active   ───────────►│
                          │                        │
                          ▼                        │
                       in_review                   │
                          │ done                   │
                          ▼                        │
                         done ───────────► ungates dependents
                          │                  (their last gating
                          ▼                   prereq became done)
                     archived (manual)
```

Project rows follow the same picture; substitute the project status enum and treat `archived` as a satisfying state for `is_gated`.

---

## Risks

**R1 — Status-actor column doesn't capture intent.** A human sets a task to `blocked` then later runs `boss task depend add`; from then on the engine's auto-unblock won't fire (because `last_status_actor = 'human'`) and the user has to manually unblock. Mitigation: `boss task depend add` resets `last_status_actor = 'engine'` when the engine confirms the new edge is the *only* gating reason. Worst case: occasional user confusion; easy to fix with a `--force-auto` flag if it becomes a pattern.

**R2 — Cycle detection has a TOCTOU window.** Two concurrent `add` calls could each pass the cycle check and together create a cycle. Mitigation: edge inserts run inside a serializable transaction with `BEGIN IMMEDIATE`; the recursive CTE runs in the same transaction so a second writer sees the first writer's row. SQLite's default isolation handles this if we use a single connection per process (which we do).

**R3 — Per-edge cycle CTE blows up on huge graphs.** Mitigation: workspace edge counts are tiny (tens, not thousands), the recursive CTE bounds itself by visited set, and edge-create is not on a hot path. If we ever hit 10⁵ edges per workspace, a cached transitive-closure index is the next move; not now.

**R4 — `waiting_dependency` overload.** The reconcile loop today uses `waiting_dependency` to mean "next-in-project". With this change, it also means "edge-blocked". A consumer reading execution status alone can't tell which. Mitigation: keep them merged from the consumer's perspective (both mean "not yet eligible") and expose the reason on the work item via the dep-graph view rather than on the execution row. If a downstream tool needs the distinction, we add a `wait_reason` column later.

**R5 — UI lag on edge changes.** Edge add/remove only triggers a status broadcast if a status actually flipped. A no-op edge (e.g. dependent already `blocked` for other reasons) won't push, and the kanban dep-graph view will be briefly stale. Mitigation: emit a `work.tree` broadcast unconditionally on edge changes; the snapshot replace is cheap.

**R6 — Deleted prereq drops edges silently.** A user deleting a prerequisite implicitly removes its dependency edges and unblocks dependents (Q10). They might be surprised by a card jumping out of Blocked. Mitigation: the CLI delete output names the affected dependents; the engine writes a tracing log. Acceptable residual.

**R7 — Cross-kind edge tooltips wordy.** A task gated by a project shows the project's status in the tooltip; users may misread a project in `active` as "almost done" when it's actually got many open tasks. Mitigation: the tooltip can include a `(N tasks open)` count for project prereqs. v1 ships without it; revisit if it bites.

---

## Follow-up Implementation Chores (to enqueue once approved)

These are bite-sized so each fits in a single worker session.

1. **Schema + migration**: add `work_item_dependencies` table, add `last_status_actor` columns, bump `schema_version` to 4. Acceptance: a fresh init + a migration from a v3 DB both yield the v4 schema; `last_status_actor` defaults to `'human'` for existing rows.

2. **Protocol types**: `WorkItemDependency`, `AddDependencyInput`, `RemoveDependencyInput`, `ListDependenciesInput`; add `last_status_actor` to `Task` and `Project`. Mirror in `app-macos/Sources/Models.swift`. Acceptance: serde / Codable round-trips green; existing wire-shape tests still pass.

3. **Engine: edge CRUD module** (`engine/src/work_dependencies.rs`): `insert_edge`, `delete_edge`, `list_edges`, `would_create_cycle`, `gating_prereqs_for`, `dependents_of`. Unit tests cover cycle prevention, idempotent add, no-op rm, cross-kind, and the same-product check. Acceptance: tests green; no engine route wired up yet.

4. **Engine: dispatch gate**: extend `reconcile_work_executions` to demote a dependent's desired status to `waiting_dependency` when `gating_prereqs_for` returns a non-empty set. Extend the explicit `RequestExecution` path to refuse with a clear error if gated. Acceptance: integration test creates A and B with B-depends-on-A, asserts B sits at `waiting_dependency` until A → `done`, then transitions to `ready`.

5. **Engine: auto-block / auto-unblock on status transitions**: when an item's status flips to a satisfied state, walk `dependents_of` and clear `blocked` → `todo` for any whose `last_status_actor = 'engine'` and whose remaining gating set is empty. When a new gating edge becomes unsatisfied (e.g. on edge-add against a non-done prereq), set the dependent's status to `blocked` and stamp `last_status_actor = 'engine'`. Acceptance: integration tests cover both directions.

6. **Engine: status-actor write path**: every `apply_status_change` call records the actor (`'human'` from the public API, `'engine'` from internal transitions). Manual move to non-`blocked` while gated is refused with the message from Q4. Acceptance: a CLI move-while-gated returns the error; an engine-driven unblock writes `'engine'`.

7. **CLI: `boss <kind> depend add|rm|list`**: thin wrapper over the engine's edge CRUD. Reference doc updated. Acceptance: `--help` covers the verbs; `boss reference` mentions edge semantics; CLI integration test covers add → list → rm.

8. **CLI: list filters**: `--prerequisites-of`, `--dependents-of`, `--unblocked`, `--blocked-by-deps` on `boss task list`, `boss chore list`, `boss project list`. Acceptance: filter combinations work; JSON shape includes the requested rows.

9. **CLI: `boss <kind> show` Dependencies section**: render incoming and outgoing edges with the prereq's status, in human and JSON modes. Acceptance: golden-output test for a fixture with 2 prereqs and 1 dependent.

10. **macOS card badge**: chain icon in the footer row of `WorkBoardCardView` when the item is auto-blocked, with a hover tooltip listing gating prereqs. Acceptance: snapshot tests; visual review on a fixture board.

11. **macOS drag refusal**: `WorkBoardView` rejects drops out of Blocked while gated; inline warning appears next to the source card. Acceptance: an interaction test attempts the drag and observes the refusal.

12. **macOS card detail Dependencies subsection**: prereqs and dependents listed with hyperlinks to the corresponding cards. Acceptance: clicking a row navigates to the linked card; empty state collapses cleanly.

13. **bossctl `agents probe` reuse for stuck-prereq warnings (optional)**: a daily probe emits an attention item for any prereq sitting in `in_review` longer than 48h with at least one dependent gated on it. Out of v1 hard scope; file as a stretch.

---

## Out of Scope

- Cross-product / multi-repo edges. See `proj_18a2bbe20fc03718_8`.
- Soft dependencies, `relates-to`, `duplicates`. The `relation` column accepts them physically; the codepaths refuse them in v1.
- Bulk edge create / import.
- Lag times, weights, schedule offsets.
- Auto-derived edges from `ordinal` ordering.
- A dedicated graph-view UI (the list view in card detail covers v1 use cases).
- `--force` overrides on dispatch / manual move while gated.
