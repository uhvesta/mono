# Boss: Unify Work-Item Kinds into One Task Entity with Flavors

## Problem

Boss splits leaf work items into five `kind`s — `chore`, `project_task`, `design`, `investigation`, `revision` — stored in `tasks.kind` (a free-form `TEXT` column, no `CHECK`, validated in Rust). The engine has *already* half-unified the model: `boss reference` says "a chore is a kind of task," and the kind-agnostic verbs (`show`, `update`, `move`, `delete`, `restore`, `depend`, `bind-pr`, `link-external`, `unlink-external`) accept any leaf id under either `boss task` or `boss chore`. But `create` / `create-many` / `list` / `reorder` stay split by kind, and the split `list` verbs partition the id-space incompletely:

- `boss task list` → `WHERE kind IN ('project_task', 'design', 'investigation')` (`engine/src/work/workitems.rs:462,471`)
- `boss chore list` → `WHERE kind = 'chore'` (`engine/src/work/workitems.rs:577`)
- **`revision` rows appear in neither RPC** — they surface only inside `get_work_tree()` (`workitems.rs:267`), so no `list`-based lookup finds them.

This is not a cosmetic split. The single `kind` enum conflates two genuinely-independent axes:

1. **Deliverable / behavior** — what the work *is* and how it completes (a normal code change opens its own PR; a `design` seeds a project; an `investigation` produces a doc PR; a `revision` commits to a parent's PR).
2. **Project membership** — whether the row belongs to a project. This is just `project_id IS NULL` and is orthogonal to the deliverable axis.

Conflating them is why "promote a chore into a project" has no clean path (the only option today is delete-and-recreate, throwing away short id, history, status, and PR bindings), and why the list surface is a partition rather than a filter. The partition already caused a real coordinator failure: mapping PR #959 → T852 (a chore) was impossible via any `task list`-based lookup because the row was structurally absent from every list RPC.

This doc resolves the model and lays out a phased, zero-break implementation. Full motivation lives in the tracking issue: https://github.com/spinyfin/mono/issues/731 (Boss row T638).

## Goals

- A single leaf work-item entity with a **`flavor`** attribute (the deliverable/behavior axis) and **project membership as an orthogonal nullable `project_id`** (not a kind).
- A **single flavor-complete `list` surface**: one noun returns *every* leaf work item regardless of flavor (chore, project_task, design, investigation, revision), with `--type` / `--project` / `--no-project` filters. No flavor silently excluded from any list verb.
- **Promotion as a trivial field update**: `boss task update --project <P> <id>` / `--unset-project` reparent a row by flipping `project_id`, with a hard data-preservation guarantee.
- **Zero-break compatibility**: existing `boss chore *` verbs, split `create` verbs, and existing scripts keep behaving as documented; `T<n>` short ids stay stable across the transition and across project (de)assignment.
- **Flavor-behavior preservation**: `design` still seeds a project, `investigation` still produces a doc-PR pointer, `revision` still commits to its parent's PR gated on the chain root's open PR — unchanged.
- A **phased path**: derive `kind` from `(flavor, project_id)` first so nothing breaks; collapse `kind` branching and (optionally) drop `kind` later.

## Non-goals

- **A big-bang rewrite that drops `kind` in the first pass.** `kind` stays as a derived column through the transition; dropping it is explicitly deferred (see Alternatives, and the task breakdown's `future` items).
- **A new top-level noun (`boss work`).** We make the existing `boss task` noun flavor-complete rather than introducing a third synonym for "leaf work item" (see Alternative B).
- **Changing flavor semantics.** This project does not alter what a `design`/`investigation`/`revision` *does*; it only changes how the five values are stored and surfaced. Behavior preservation is a hard requirement, audited in §Flavor-behavior preservation.
- **Reparenting flavors other than `normal` in v1.** `--project`/`--unset-project` target the chore↔task case (`flavor=normal`). Moving a `design`/`investigation`/`revision` between projects has flavor-specific invariants and is scoped out of v1 (see §Promotion).
- **Cross-product moves.** Reparenting changes `project_id` within one product; moving a row to a *different product* (and its short-id sequence) is out of scope.
- **Removing the `boss chore *` aliases or the split `create-*` verbs.** Deprecation is a later, telemetry-gated step, not a v1 deliverable.
- **A "flavor" kanban column.** Flavors flow through the existing Backlog/Doing/Blocked/Review/Done columns; only card chrome distinguishes them (already true today).

## Alternatives considered

### Alternative A — Fold project-membership into the flavor enum (rename `kind` → `flavor`)

Keep one enum but rename it: `flavor ∈ {chore, task, design, investigation, revision}`, where `chore` vs `task` still encodes project membership. This is the smallest diff — a column rename.

**Rejected.** It re-commits the original mistake: the deliverable axis and the membership axis stay conflated in one value. Promotion would still mean *changing the flavor* (`chore` → `task`) rather than flipping a `project_id`, so the "trivial field update" goal is lost — every promotion mutates the discriminator that other code matches on. It also doesn't fix the list partition cleanly, because "list all normal-deliverable items regardless of project" still requires OR-ing two flavor values. The whole point of the project is to *separate* these axes, not relabel their conflation.

### Alternative B — A new neutral noun `boss work list`

Leave `boss task` / `boss chore` as-is and add a third noun, `boss work`, as the flavor-complete surface.

**Rejected.** It adds a third synonym for the same entity, fragmenting muscle memory and scripts further rather than consolidating. The engine already frames the model as "a chore is a kind of task," and the kind-agnostic verbs already live under `boss task`. Making `boss task list` complete (and keeping `boss chore list` as a narrowing alias) matches that existing framing, costs callers nothing, and means the *fix* for the invisibility bug is "the noun you already use now shows everything." A brand-new noun would leave the old nouns as lingering partial views — the exact trap we're removing.

### Alternative C — Separate table per flavor (`designs`, `revisions`, …)

Give each deliverable shape its own table foreign-keyed to a base row.

**Rejected** — already litigated twice in this repo (`design-producing-tasks.md` Q1, `revision-tasks.md` Q1) and rejected both times. Executions, runs, transcripts, attention items, dependency edges, and short ids all key on `tasks.id`. A per-flavor table forces every join to become a `UNION ALL` or denies those flavors first-class plumbing. The whole codebase is built around one `tasks` table; splitting it is strictly more work for strictly less capability.

### Alternative D — Drop `kind` immediately and compute everywhere

Add `flavor`, delete `kind`, and rewrite every `match kind` site in one PR.

**Rejected for v1.** The audit (§Flavor-behavior preservation) found ~14 distinct branch sites across `completion.rs`, `runner`-adjacent helpers, `executions_runs.rs`, `pr_flow.rs`, `revision_helpers.rs`, `chain_helpers.rs`, and several SQL queries. Touching all of them at once is a high-blast-radius change with no safe intermediate state and a painful rollback. The phased "derive first, collapse incrementally, drop last" path (Chosen approach) keeps every site green at each step.

## Chosen approach

**Two orthogonal axes, a derived `kind`, and a flavor-complete `boss task` noun.** Project membership stays where it already is — a nullable `project_id`. The deliverable/behavior axis becomes a new `flavor` column. `kind` survives the transition as a *derived, denormalized display hint* computed from `(flavor, project_id)` on every write, so all existing `kind`-matching code keeps working byte-for-byte until it is migrated deliberately.

### 1. The flavor model (orthogonal axes)

Two axes, stored independently on `tasks`:

| Axis | Storage | Values |
|------|---------|--------|
| Deliverable / behavior | `flavor TEXT NOT NULL` | `normal`, `design`, `investigation`, `revision` |
| Project membership | `project_id TEXT NULL` (exists today) | NULL (free-floating) or a project id |

`flavor` has **four** values, not five: the legacy `chore` and `project_task` kinds *both* collapse to `flavor = 'normal'` and are distinguished purely by `project_id`. "Chore" becomes the display name for `(normal, project_id IS NULL)`; "task" is `(normal, project_id IS NOT NULL)`.

The two axes are orthogonal *in storage* but constrained by **flavor-specific invariants** on which combinations are legal — these are real today and must be preserved (enforced in Rust at the insert/update boundary, consistent with the existing no-`CHECK` house style):

| `flavor` | `project_id` | `parent_task_id` | `pr_url` | Legacy `kind` |
|----------|--------------|------------------|----------|---------------|
| `normal` | NULL or set | NULL | own PR | `chore` if NULL, else `project_task` |
| `design` | **required** | NULL | own PR | `design` |
| `investigation` | optional | NULL | own PR | `investigation` |
| `revision` | inherited from parent | **required** | **NULL** (parent owns it) | `revision` |

Notes:
- `design` always seeds a project, so `project_id` is required (it is the project the design is *for*). This is the one place the two axes are coupled, and the coupling is intrinsic to the design flavor, not to the model.
- `revision` inherits `project_id` from its parent chain and carries `parent_task_id`; its `pr_url` stays NULL because the chain root owns the PR (`pr_flow.rs:64`).
- `investigation` is the only flavor that is genuinely free on the membership axis (standalone or under a project) today.

**Why orthogonal, not folded:** the issue brief recommends the orthogonal-axis model "unless there's a concrete reason not to," and there isn't one. Promotion is a `project_id` write that never touches `flavor`; the list surface becomes a filter over two independent dimensions instead of a partition over one conflated enum; and engine code that only cares about membership (`is this free-floating?`) tests `project_id IS NULL` without consulting the deliverable axis.

### 2. `kind` disposition: derive now, drop later

Keep `kind` as a **derived, denormalized column**, recomputed from `(flavor, project_id)` on every insert and every update that can change either input, via one shared helper:

```rust
fn derive_kind(flavor: &str, project_id: Option<&str>) -> &'static str {
    match flavor {
        "design" => "design",
        "investigation" => "investigation",
        "revision" => "revision",
        "normal" => if project_id.is_some() { "project_task" } else { "chore" },
        other => other, // forward-compat: unknown flavor passes through
    }
}
```

The invariant "`kind` is always consistent with `(flavor, project_id)`" is enforced in the application layer (the write path computes `kind`; callers never set it directly), matching how every other `tasks` invariant is enforced — the schema deliberately carries no `CHECK` constraints so migrations stay `ALTER TABLE ADD COLUMN`-shaped and error messages stay in Rust (see `revision-tasks.md` Q1 for the established rationale).

**This is the load-bearing zero-break lever.** Because `kind` stays populated and correct, every `match task.kind` site (§Flavor-behavior preservation) keeps reading the same values it reads today. The engine collapse (rewriting those sites to read `flavor` + `project_id`) and the eventual `kind`-drop become *separate, independently-schedulable* tasks rather than prerequisites — and the system is shippable after each one.

`boss task show <id>` will surface `(flavor, project_id, kind)` together; the derivation guarantees they are always consistent.

### 3. CLI surface: make `boss task` flavor-complete

`boss task` becomes the single flavor-complete leaf-work-item noun. `boss chore *` and the split `create-*` verbs remain as thin back-compat aliases.

**`boss task list` returns every flavor by default** (chore, project_task, design, investigation, revision), with filters to slice:

- `--type <flavor>` (repeatable / comma-list; `--flavor` accepted as a synonym) — filter by deliverable axis. Values: `normal`, `design`, `investigation`, `revision`.
- `--project <P>` — only rows in project P (the existing flag, semantics unchanged).
- `--no-project` — only free-floating rows (`project_id IS NULL`).
- Existing `--status` / `--priority` / `--match` / `--repo` / `--id` / `--deleted` / dependency filters compose unchanged.

The query collapses to one parametric `SELECT` over `tasks WHERE product_id = ? AND deleted_at IS NULL` with optional `flavor IN (…)` and `project_id` predicates — replacing the three divergent hard-coded `kind IN (…)` lists at `workitems.rs:462/471/577` and the tree query at `workitems.rs:267/281`.

Back-compat aliases (behavior identical to today):
- `boss chore list` ≡ `boss task list --no-project --type normal`.
- `boss chore create` ≡ `boss task create` with no `--project`.
- `boss task create-investigation` / `create-revision` stay; a unified `boss task create --type <flavor>` is added alongside them (the split verbs become aliases that set `--type`).

The unified, flavor-complete `list` is the **highest-value early carve-out** and is the first implementation deliverable (task **T-A** below). It can land *before* the schema work by mapping `--type` values onto the existing `kind` set and simply UNION-ing the currently-partitioned queries — closing the chores/revisions-invisible bug immediately, independent of the flavor column.

### 4. Promotion (reparenting)

`boss task update --project <P> <id>` and `boss task update --unset-project <id>` are the reparenting surface. No bespoke `promote` verb — promotion is a `project_id` write.

**Data-preservation guarantee (hard requirement):** reparenting changes only `project_id` (and project-side bookkeeping below). Everything else lives on the same row and is untouched: `short_id`, `status`, `last_status_actor`, `pr_url`, `effort_level`, dependency edges (`work_item_dependencies` key on `tasks.id`), `description`, and external links (`link-external` bindings). In particular, `short_id` is unique on `(product_id, short_id)` (`friendly-numeric-ids-for-work-items.md`, indexes at lines 280-283) and has **no relationship to `project_id`**, so `T<n>` is stable across (de)assignment *for free* — no special handling required.

**Project-side bookkeeping** (the only writes beyond `project_id`):
- On `--project <P>`: assign `ordinal = MAX(ordinal) + 1` among project P's `project_task` rows (the slot the existing ordinal query at `exec_status_helpers.rs:217` already computes), placing the promoted row at the end of P's task list. `kind` recomputes `chore → project_task`.
- On `--unset-project`: clear `ordinal` (set NULL) and `project_id`. `kind` recomputes `project_task → chore`.

**Scope guard (v1):** `--project`/`--unset-project` apply to `flavor = normal` only. Reparenting a `design` (whose `project_id` is intrinsic), an `investigation`, or a `revision` (whose membership follows its parent chain) has flavor-specific rules and is deferred — the command rejects those flavors with a clear message rather than silently doing something surprising.

### 5. Migration & back-compat

**Schema:** add `flavor` via `ALTER TABLE tasks ADD COLUMN flavor TEXT` in a new `migrate_tasks_flavor_column()` (same shape as the existing `migrate_tasks_*` family, e.g. `migrate_tasks_investigation_doc_columns` and `migrate_tasks_parent_task_id_column`). Backfill in the same migration with one `UPDATE` per legacy kind:

```sql
UPDATE tasks SET flavor = 'normal'        WHERE kind IN ('chore', 'project_task');
UPDATE tasks SET flavor = kind            WHERE kind IN ('design', 'investigation', 'revision');
```

This is a single pass over the existing 700+ rows, cheap and idempotent. After backfill, `flavor` is logically `NOT NULL` (enforced in Rust; the column is added nullable because SQLite `ADD COLUMN NOT NULL` without a constant default on a populated table is awkward — backfill then treat-as-required is the house pattern). Add a partial index mirroring the existing `tasks_product_idx`: `CREATE INDEX tasks_product_flavor_idx ON tasks(product_id, flavor, deleted_at)` to keep the new `list` query fast.

**`kind` is retained and kept derived** (§2). No `match kind` site changes in the schema PR — they keep reading the derived value.

**Back-compat:** `boss chore *` and split `create-*` verbs stay as aliases (§3). JSON `list` output gains a `flavor` field but keeps the existing `kind` field, so no consumer breaks. Scripts that pass `--kind`-style filters or read `kind` from JSON keep working.

**`T<n>` stability:** unaffected by any of the above (short_id is independent of flavor and project_id).

**Deprecation (later, out of v1):** once the engine collapse (T-D) lands and telemetry shows nothing reads `kind`, drop the column; once usage telemetry shows the aliases are unused, deprecate `boss chore *`. Both are explicitly `future / not a v1 blocker`.

### 6. Flavor-behavior preservation (audit)

Every site that branches on `kind` today, and how it maps onto `(flavor, project_id)`. In **Phase 1 none of these change** — they keep reading the derived `kind`. The engine-collapse task (T-D) rewrites them to read `flavor`/`project_id` directly; this table is its spec.

| Site (`engine/src/...`) | Today branches on | Decides | Post-collapse predicate |
|---|---|---|---|
| `work/audit_misc.rs:335` | `kind == "chore"` | `WorkItem::Chore` vs `::Task` for kanban | `flavor == "normal" && project_id IS NULL` |
| `work/exec_status_helpers.rs:21-26` | `match kind` | task→execution kind (`chore_implementation`/`project_design`/`revision_implementation`/`task_implementation`) | `match flavor` (+ membership for the `normal` split if needed) |
| `work/exec_status_helpers.rs:171-191` | `kind == "design"` / `"investigation"` | repo routing (`design_repo` / `docs_repo`) | `flavor == "design"` / `"investigation"` |
| `work/exec_status_helpers.rs:217` | `kind = 'project_task'` (SQL) | next ordinal in project | `flavor = 'normal' AND project_id = ?` |
| `work/executions_runs.rs:424-504` | `match kind` | dispatch gating (independent vs per-project serialized vs revision-gated) | `flavor` + `project_id` (design/normal-with-project serialize per project) |
| `work/revision_helpers.rs:29,186,199,…` | `kind = 'revision'` | revision chain walking / sequence | `flavor = 'revision'` |
| `work/chain_helpers.rs:42,106` | `kind = 'revision'` | parent-chain root walk; child lookup | `flavor = 'revision'` |
| `work/dispatch_helpers.rs:159-162` | `kind == "chore"` | error-message noun (`boss chore` vs `boss task update`) | cosmetic; `flavor=normal && no project` |
| `work/pr_flow.rs:64` | `kind == "revision"` | keep `pr_url` NULL | `flavor == "revision"` |
| `work/pr_flow.rs:145`, `work/blocking.rs:18` | `kind IN ('chore','project_task','design','investigation')` | merge/conflict pollers (exclude revision) | `flavor != 'revision'` |
| `completion.rs:1545` | `kind == "design"` | fire design-doc detector | `flavor == "design"` |
| `work/workitems.rs:267,281,462,471,577` | `kind IN (…)` (list/tree) | list partition | replaced by unified `flavor`/`project_id` filter (T-A/T-B) |
| `work/workitems.rs:321` | `kind = 'project_task'` | reorder validation | `flavor = 'normal' AND project_id = ?` |

Confirmation of the three behaviors the acceptance criteria call out:
- **`design` seeds a project** — `completion.rs:1545` keys on the flavor; unchanged.
- **`investigation` produces a doc-PR pointer** — repo routing (`exec_status_helpers.rs:171-191`) and `set-investigation-doc` are flavor-keyed; unchanged.
- **`revision` commits to parent's PR, gated on chain-root open PR** — `revision_helpers`/`chain_helpers`/`pr_flow.rs:64` all key on the flavor and `parent_task_id`; unchanged. The reconciliation gate in `executions_runs.rs` (`reconcile_revision_execution`) is untouched.

No engine logic was found that depends on the chore/task distinction *beyond* `project_id IS NULL` and the listed flavor behaviors — confirming the issue's premise that little such logic exists.

## Risks / open questions

1. **`flavor` value naming: `normal` vs `task` vs `code`.** This doc picks `normal` for the default deliverable (a code change that opens its own PR). Alternatives: `task` (collides with the noun) or `code`. *Recommendation to land:* `normal`. Reviewer should confirm before T-B fixes the string in the backfill.
2. **`--type` vs `--flavor` as the primary flag name.** The project title says `--type`; the column is `flavor`. This doc makes `--type` primary with `--flavor` as a synonym. Confirm we want the CLI flag and the storage column to differ in name (precedent: `kind` was an internal term too).
3. **Should `--unset-project` ever be allowed for `investigation`?** Investigations are the one flavor genuinely free on the membership axis today. v1 restricts reparenting to `flavor=normal` for safety; a reviewer may want investigations included from the start. Low risk either way; deferring is the conservative call.
4. **Forward-compat for unknown flavors.** `derive_kind` passes unknown flavors through verbatim. If a future flavor needs a *derived* kind different from its own name, this helper is the single place to extend — acceptable, flagged so it isn't forgotten.
5. **Dropping `kind` (Phase 3) needs a telemetry gate.** We cannot safely drop the column until we're confident no external script reads `kind` from JSON. The plan keeps `kind` in JSON output indefinitely until that's established; the drop task is `future` precisely because the gate is a judgment call for a human.
6. **`design` membership coupling.** `design` requires `project_id`, the one place the two axes touch. If a future "free-floating design" use case appears, the invariant must relax. Out of scope now; noted so it isn't mistaken for an oversight.

## Proposed implementation task breakdown

Dependency-ordered. Effort hints: `trivial | small | medium | large`. Items marked `future / not a v1 blocker` are deferred but listed so the rejected/postponed scope is explicit, not silently omitted.

### Depth 0 — may run in parallel

**T-A: Flavor-complete `boss task list` + `--type`/`--project`/`--no-project` filters**
*Scope:* Replace the three divergent partitioned list queries (`workitems.rs:462/471/577`) and the tree query (`workitems.rs:267/281`) with one parametric query returning every flavor by default, and add `--type` (comma-list, `--flavor` synonym), `--no-project`, and the existing `--project` as composable filters. Make `boss chore list` an alias for `--no-project --type normal`. Add `flavor`/membership to the printed columns and JSON. This is the bug-fix carve-out and ships independently of the schema work by mapping `--type` values onto the existing `kind` set. Closes the chores/revisions-invisible failure mode (PR #959 → T852 class).
*Effort:* small.
*Dependencies:* none (only this design's CLI shape).

**T-B: Schema + protocol — add `flavor`, backfill, centralize derived `kind`**
*Scope:* Add `migrate_tasks_flavor_column()` (`ALTER TABLE ADD COLUMN` + backfill UPDATEs over the 700+ existing rows + `tasks_product_flavor_idx`). Add `flavor: String` to the `Task` protocol struct (`protocol/src/types.rs`) and to JSON wire output alongside the retained `kind`. Introduce the single `derive_kind(flavor, project_id)` helper and route every insert/update write path through it so `kind` is always derived (callers stop setting `kind` directly). Enforce the legal `(flavor, project_id, parent_task_id)` matrix in Rust at the write boundary.
*Effort:* medium.
*Dependencies:* none.

### Depth 1 — after T-B; T-C, T-D, T-E, T-F may run in parallel

**T-C: `boss task update --project / --unset-project` reparenting**
*Scope:* Add the two flags to `TaskUpdateArgs`, restricted to `flavor=normal`. On assign, set `project_id` + end-of-list `ordinal`; on unassign, clear both. Recompute `kind` via `derive_kind`. Enforce the data-preservation guarantee (short_id, status, last_status_actor, pr_url, effort, deps, description, external links untouched — all on the same row). Reject non-`normal` flavors with a clear message.
*Effort:* small.
*Dependencies:* T-B.

**T-D: Engine `kind`-branch collapse to `(flavor, project_id)`**
*Scope:* Rewrite the ~14 branch sites in §Flavor-behavior preservation to read `flavor`/`project_id` instead of the derived `kind`. Purely mechanical, site-by-site; the audit table is the spec. May be split into smaller PRs (e.g. one per file/subsystem) if reviewer prefers. No behavior change — `kind` stays derived so each step is independently verifiable against current behavior.
*Effort:* medium (large if not split).
*Dependencies:* T-B.

**T-E: Unified `boss task create --type <flavor>` + create-verb aliases**
*Scope:* Add `--type`/`--project` to `boss task create`; make `boss chore create`, `boss task create-investigation`, `boss task create-revision` thin aliases that set `--type` (and the revision parent flag). Preserve every existing flag and behavior of the split verbs.
*Effort:* small.
*Dependencies:* T-B.

**T-F: Display — kanban / macOS app / CLI columns surface flavor + membership**
*Scope:* Show flavor and project membership consistently in `boss task show`/`list` and the macOS app's Work tab so the derived `kind` is no longer the only signal. Card chrome already distinguishes flavors; this ensures `(flavor, project_id)` is the displayed source of truth.
*Effort:* small-medium.
*Dependencies:* T-B.

### Depth 2 — future / not a v1 blocker

**T-G: Drop the derived `kind` column**
*Scope:* Once T-D lands and telemetry confirms nothing reads `kind` (including external scripts via JSON), remove the column and `derive_kind`, and drop `kind` from JSON output.
*Effort:* medium.
*Dependencies:* T-D, plus a human telemetry gate (Risk 5). **future / not a v1 blocker.**

**T-H: Deprecate `boss chore *` aliases and split `create-*` verbs**
*Scope:* After usage telemetry shows the aliases are unused, emit deprecation warnings then remove them.
*Effort:* small.
*Dependencies:* T-A, T-E, plus usage telemetry. **future / not a v1 blocker.**

**T-I: Extend reparenting to `investigation` (and a documented rule for `design`/`revision`)**
*Scope:* Lift the `flavor=normal`-only restriction on `--project`/`--unset-project` for investigations, and document/implement the intended behavior (likely "rejected") for design and revision.
*Effort:* small.
*Dependencies:* T-C. **future / not a v1 blocker** (Risk 3).

**Parallelism summary:** T-A and T-B start immediately and run in parallel (T-A delivers the bug fix without waiting on the schema). Once T-B merges, T-C / T-D / T-E / T-F all run in parallel. The depth-2 items are sequenced behind their depth-1 prerequisites and gated on human/telemetry judgment.
