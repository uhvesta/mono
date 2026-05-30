# Spike: What Does Collapsing the `chore`/`project_task` Distinction Actually Simplify?

**Date:** 2026-05-30
**Parent project:** Unify work-item kinds (flavors) — design `unify-work-item-kinds-flavors.md`, tracking issue [spinyfin/mono#731](https://github.com/spinyfin/mono/issues/731) (Boss row T638).
**Status:** Investigation writeup. **No code changes** — this is a read-only audit + analysis. The deliverable is this doc.

## TL;DR

I enumerated **every** site across the engine, CLI, protocol, macOS app, and SQL that treats `chore` differently from `project_task`. There are **~30 distinct sites**. **Every single one** is either:

- **(a) pure project-membership** — it diverges only because a chore has no `project_id` and a project_task has one. These collapse to a direct `project_id.is_some()` / `IS NULL` check (or disappear when two list buckets merge into one filtered query); **or**
- **a display/identifier label derived from membership** — a noun in an error string, a JSON envelope key, a kanban card label, a metric name. These don't branch *behavior*; they emit a *name*.

**I found zero (b) non-membership / latent-conflation sites** — no place branches chore-vs-project_task for a reason other than project presence. The hypothesis in the brief is **confirmed**.

The strongest evidence is historical: the *one* time the engine ever treated chore and project_task differently in real behavior (the merge poller swept only `kind = 'chore'`), it was logged as a **bug** (2026-05-07, `merge_poller.rs:2697`) and fixed by treating them the *same*. The codebase has already been actively erasing the distinction wherever it mattered.

What the collapse buys, concretely:

- **The headline bug fix is structural, not incidental.** Three divergent list queries (`list_tasks`, `list_chores`, the `get_work_tree` split) become one filtered query. `boss task list` today excludes chores **and** revisions (`workitems.rs:524/533`) — the PR#959→T852 invisibility class — purely because the partition is keyed on `kind` instead of `project_id`. Collapsing chore/pt is what removes the partition.
- **Two insert paths become one.** `insert_chore_in_tx` and `insert_task_in_tx` differ *only* in `kind` literal + whether `project_id`/`ordinal` are set — and `kind` is 100% determined by `project_id` at the insert (`insert_helpers.rs:160-204`).
- **~5 `match`/`if` branches** on the chore/pt split fold into a `project_id` test or disappear.
- **~9 macOS `isChore` checks** reduce to `projectID == nil`.

The cost is **not** behavioral risk — it's a small, enumerable set of **derived identifiers** that the collapse (and the eventual `kind` removal) would change: JSON envelope keys (`{"chore": …}` vs `{"task": …}`), the `kind` JSON field, the `chore_implementation`/`task_implementation` execution-kind label, the `total_chores` metric, and the macOS "Chore" card chrome. §6 lists each. None is load-bearing engine logic; all are cosmetics or consumer-facing names that must be *deliberately* preserved or migrated, not silently changed.

**Recommendation:** the collapse is safe to proceed with. The implementation must be explicit in code that `kind` is being deprecated/removed (the design doc's "derive `kind` from `(flavor, project_id)`" lever is the right transition), **not** kept as a permanent derived field — keeping it permanently just re-persists the conflation this project exists to remove.

---

## 1. Method & scope

**Model under test.** Boss stores leaf work items in one `tasks` table with a free-form `kind TEXT NOT NULL` column (no `CHECK`; validated in Rust — `schema_init.rs:56`). Values: `chore`, `project_task`, `design`, `investigation`, `revision`. `project_id TEXT NULL` already exists. The proposed collapse: `chore` and `project_task` both become one behavior value (`flavor = 'normal'`), distinguished *only* by `project_id` — `chore ≡ (normal, project_id IS NULL)`, `project_task ≡ (normal, project_id IS NOT NULL)`.

**Scope guard (per the brief).** This is a spike focused narrowly on the **`chore` vs `project_task`** distinction and what removing it simplifies. `design`/`investigation`/`revision` behavior is audited *only* where those arms sit in the same `match`/query as chore/pt and are relevant to the collapse. I did not re-audit the full flavor behavior set (the design doc's §6 already does that).

**What counts as a discrimination site.** A `match`/`if` where `chore` and `project_task` land in different arms (including one explicit, the other in `_`); a SQL `kind = '…'` or `kind IN (…)` that puts the two values in *different* buckets; any place the two are otherwise treated differently (different noun, label, card type, routing). A `kind IN ('chore','project_task',…)` list that contains *both* is **not** a discrimination — they're treated identically there — but I note those separately because they're part of the collapse's blast radius and are themselves evidence (see §3, "grouped-same").

**Verification.** Findings below were read directly from current `main` (`4e38eb0`), not taken from the design doc's §6 table (which I treated as a claim to check, and which covers all five kinds rather than the chore/pt pair specifically). Test files are excluded from the site census but noted where they hard-code the strings (blast radius).

---

## 2. The master enumeration

Legend — **Class**: `M` = pure membership (collapses to a `project_id` check or disappears); `L` = display/identifier *label* derived from membership (emits a name, doesn't branch behavior); `N` = non-membership/latent conflation (the risky kind — **none found**); `≈` = grouped-same (not a discrimination, listed for context).

### 2.1 Engine — dispatch & execution

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| E1 | `work/exec_status_helpers.rs:21-26` (`execution_kind_for_work_item`) | `match kind` (chore explicit, pt via `_`) | execution-kind **string** (`chore_implementation` vs `task_implementation`) | M + L | `if project_id.is_some() { "task_implementation" } else { "chore_implementation" }` — string is a label (see §6) |
| E2 | `work/executions_runs.rs:424-503` (dispatch loop) | `match kind` (chore arm vs `project_task \| design` arm) | chore dispatches **independently + immediately**; project_task is **serialized per-project** by ordinal | **M** | `project_id.is_none()` → independent; `is_some()` → per-project serialize |
| E3 | `work/exec_status_helpers.rs:214-221` (`next_task_ordinal`) | SQL `kind = 'project_task'` | next ordinal slot within a project | **M** | `flavor='normal' AND project_id = ?` |

### 2.2 Engine — lists, tree, reorder, insert (the partition surface)

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| E4 | `work/workitems.rs:257-289` (`get_work_tree`) | SQL: `tasks` = `kind IN ('project_task','design','investigation','revision')`; `chores` = `kind = 'chore'` | splits chores into a separate `chores` field | **M** (partition) | one query; split for display on `project_id IS NULL` |
| E5 | `work/workitems.rs:499-538` (`list_tasks` → `ListTasks` RPC) | SQL `kind IN ('project_task','design','investigation')` | `boss task list` corpus — **excludes chore AND revision** | **M** (partition) + **bug** | unified `flavor`/`project_id` filter |
| E6 | `work/workitems.rs:621-645` (`list_chores` → `ListChores` RPC) | SQL `kind = 'chore'` | `boss chore list` corpus | **M** (partition) | `--no-project --type normal` filter |
| E7 | `work/workitems.rs:312-343` (`reorder_project_tasks`) | SQL `kind = 'project_task'` | reorder validation set | **M** | `project_id = ? AND flavor='normal'` |
| E8 | `work/insert_helpers.rs:160-204` (`insert_task_in_tx` vs `insert_chore_in_tx`) | two `INSERT`s | `'project_task'` + `project_id`/`ordinal` set vs `'chore'` + both `NULL` | **M** | one insert; derive `kind` from `project_id` |

### 2.3 Engine — cosmetic nouns & display wrappers

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| E9 | `work/dispatch_helpers.rs:150-167` (`repo_unresolved_kind_label`) | `match kind` (chore explicit, pt via `_`) | error-message noun: `boss chore update` vs `boss task update` | M + L | `if project_id.is_none() { "chore" } else { "task" }` |
| E10 | `work/dep_helpers.rs:117-123` | `match kind` (chore explicit, pt via `_`) | dependency-edge `kind` label rendered in CLI | M + L | same |
| E11 | `work/audit_misc.rs:334-340` (`task_to_item`) | `if kind == "chore"` | wraps row as `WorkItem::Chore` vs `::Task` | M + L | `if project_id.is_none()` |
| E12 | `work/exec_tail.rs:213-228` (`list_chores_for_audit`) | SQL `kind = 'chore'` | per-product chore corpus for the effort audit | M + L | `project_id IS NULL` |
| E13 | `audit_effort.rs:83,181` (`total_chores`) | `chores.len()` | metric field `total_chores` | **L** | count of free-floating `normal` rows |

### 2.4 Engine — "grouped-same" (NOT discriminations; context only)

| # | Site | Construct | Note | Class |
|---|------|-----------|------|-------|
| G1 | `work/pr_flow.rs` `list_chores_pending_merge_check` | `kind IN ('chore','project_task','design','investigation')` | chore + pt treated **identically** (real intent: "not revision") | ≈ → `flavor != 'revision'` |
| G2 | `work/blocking.rs` `list_chores_blocked_on_merge_conflict` | same `IN (…)` list | same | ≈ → `flavor != 'revision'` |
| G3 | `merge_poller.rs:2696-2730` (regression test) | historical | **Evidence:** the 2026-05-07 bug where the poller matched only `kind = 'chore'` and stranded project_tasks in `in_review`. Fixed by sweeping both. | — |

### 2.5 CLI (`tools/boss/cli/src/main.rs` unless noted)

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| C1 | `:104-110` `Commands::Task` / `Commands::Chore` | enum | top-level noun routing | M (surface) | one noun (`boss chore` stays as alias) |
| C2 | `:282` `TaskCommand` / `:387` `ChoreCommand` | enums | two parallel verb trees | M (surface) | shared verbs (already mostly kind-agnostic) |
| C3 | `:2815` `run_task_command` / `:2956` `run_chore_command` | fns | route to `ListTasks`/`ListChores`, `CreateTask`/`CreateChore` | M | unify behind filters |
| C4 | `:3292` `label_titlecase` `"chore" => "Chore"` | `match` | display label | **L** | derive from `project_id` |
| C5 | `:6287` `expect_leaf_work_item` `WorkItem::Chore => "chore"` | `match` | display label | **L** | — |
| C6 | `:2994` `print_entity(json!({ "chore": chore }))` | JSON key | **JSON envelope key** `chore` vs `task` (and `chores`/`tasks` for lists) | **L (identifier-risk)** | see §6 |

### 2.6 Protocol (`tools/boss/protocol/src/`)

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| P1 | `wire.rs:132 ListTasks` / `:145 ListChores` | RPC variants | two list RPCs partitioned by kind | M (partition) | one RPC + filters |
| P2 | `wire.rs:188 CreateTask` / `:192 CreateChore` (+ `CreateChoreInput` vs `CreateTaskInput`) | RPC variants / input structs | `CreateChoreInput` has **no** `project_id`; `CreateTaskInput` has one | **M** | one input + optional `project_id` |
| P3 | `wire.rs:1120,1126` + `types.rs:1085` `chores: Vec<Task>` | struct field | work-tree response carries a separate `chores` collection | M + L | — |
| P4 | `types.rs:192` `pub kind: String` + JSON `"kind"` field | field | the discriminator itself, exposed on the wire | **L (identifier-risk)** | derived; see §6 |
| P5 | `types.rs:706,760-762` `total_chores: u32` | field | metric surfaced to clients | **L** | — |

### 2.7 macOS app (`tools/boss/app-macos/Sources/`)

| # | Site | Construct | What it decides | Class | Collapses to |
|---|------|-----------|-----------------|-------|--------------|
| M1 | `Models.swift:532` `isChore { kind == "chore" }` | computed var | feeds the 8 call sites below | M (mostly) | `projectID == nil` |
| M2 | `Models.swift:539-545` `kindLabel` switch | switch | card-popover label "Chore" | **L** | — |
| M3 | `EngineClient.swift:1571-1576` `parseWorkItem` `"chore" → .chore` | switch on `item_type` | display enum case | **L** | mirrors CLI envelope |
| M4 | `EngineClient.swift:109,968-977` workTree `tasks`/`chores` arrays | parsing | mirrors engine partition | M (partition) | — |
| M5 | `ContentView.swift:2414,2445` `!task.isChore` | `if` | show ordinal/reorder affordance only for project tasks | **M** | `projectID != nil` |
| M6 | `ChatViewModel.swift:2394` `if isChore { "Chores" }` | `if` | kanban section header label | **L** | — |
| M7 | `ChatViewModel.swift:2405` `first(where { !$0.isChore })?.projectID` | filter | resolve a section's project id | **M** | `projectID != nil` |
| M8 | `ChatViewModel.swift:942,2746` `isChore ? .chore : .task` | ternary | edit-request target / card type | **L** | — |
| M9 | `ChatViewModel.swift:1184,2185` `!isChore` / `isChore` | `if` | misc gating (project-context-only paths) | **M** | `projectID != nil` |

**Census total (non-test, distinct sites):** 13 engine (E1–E13) + 2 grouped-same (context) + 6 CLI + 5 protocol + 9 macOS ≈ **33 sites**, of which **0 are class N**. Every behavioral branch is `M`; the remainder are `L` labels.

---

## 3. Classification: (a) pure membership vs (b) latent conflation

### 3.1 The hypothesis holds — every discrimination is membership

Walking the sites, the chore/pt split is *always* one of these three shapes:

1. **"Does it belong to a project?" → branch behavior.** E2 (independent vs per-project serialized dispatch), E3/E7 (ordinal/reorder only exist within a project), E4/E5/E6 (the list/tree partition), E8 (insert sets `project_id`+`ordinal` iff project_task), M5/M7/M9 (project-only UI affordances). All of these are *literally* `project_id IS NULL` vs `IS NOT NULL` wearing a `kind` costume.

2. **"Does it belong to a project?" → emit a noun.** E9/E10/E11 (error-message noun, dep-edge label, `WorkItem` wrapper), C4/C5 (CLI labels), M2/M6/M8 (card label, section header, card type). These don't change behavior; they pick a human-readable name. Post-collapse the *name* can still be derived from `project_id` (or the new `flavor`), so they survive as cosmetics.

3. **"Count/collect the free-floating ones" → a named bucket.** E12/E13 (`list_chores_for_audit`, `total_chores`), P3 (`chores: Vec<Task>`), M4. The bucket *is* the no-project set; the name "chores" is incidental.

### 3.2 Why E2 (the dispatch loop) is membership, not conflation

E2 is the most behavior-divergent-looking site and deserves the scrutiny, because a naive read says "chores and project_tasks *do* dispatch differently, so the distinction is load-bearing." It isn't. The divergence is:

- **chore** → `reconcile_work_item_execution(..., "chore_implementation", "ready")` immediately.
- **project_task | design** → bucketed into `project_tasks: HashMap<project_id, Vec<Task>>`, sorted by `ordinal`, and only the *first incomplete* row in each project is set `"ready"` (the rest `"waiting_dependency"`).

The divergence is **per-project serialization**: project members run one-at-a-time in ordinal order; free-floating chores have no project to serialize against, so they run independently. That is exactly `project_id.is_some()` → "serialize within `project_id`'s lane" vs `is_none()` → "no lane, run now." A chore promoted to a project_task *should* start serializing; a project_task demoted to a chore *should* start running independently. Both are the correct, desired outcome of a `project_id` flip — which is precisely what makes the collapse safe rather than dangerous. (The `design` arm shares the project_task branch because a design *is* a project member; that's the design-flavor coupling, orthogonal to the chore/pt question.)

### 3.3 The one historical "conflation" was a bug — and was already fixed

The closest thing to a class-N site that has *ever* existed is documented in `merge_poller.rs:2696`:

> Regression for the bug where the poller's SQL filter only matched `kind = 'chore'`, leaving Performance project_tasks stuck in `in_review` after their PRs landed (2026-05-07). A `kind = 'project_task'` row with a merged PR must be promoted by the same sweep that handles chores.

In other words: the *only* time the engine treated chore and project_task differently for a non-membership reason, it was a defect, and the fix (`list_chores_pending_merge_check` now uses `kind IN ('chore','project_task','design','investigation')`, G1) was to **stop discriminating**. The codebase's own history is evidence for the thesis: where the chore/pt distinction leaked into behavior, it was wrong.

---

## 4. What the collapse quantifiably simplifies

Scoped to the **chore/pt collapse specifically** (not the full flavor refactor — `design`/`investigation`/`revision` arms remain):

| Simplification | Before | After | Effect |
|---|---|---|---|
| **List partition** (the headline bug) | 3 divergent queries: `list_tasks` (`IN ('project_task','design','investigation')`), `list_chores` (`= 'chore'`), `get_work_tree` (two SELECTs into `tasks`+`chores`) | 1 parametric query `WHERE product_id=? [AND project_id …][AND flavor IN …]` | `boss task list` stops silently excluding chores **and** revisions (PR#959→T852 class); the partition becomes a filter |
| **Insert paths** | `insert_task_in_tx` + `insert_chore_in_tx` (~45 lines each), differing only in `kind` literal + `project_id`/`ordinal` | 1 insert; `kind` derived from `project_id` | ~one whole function removed |
| **`match`/`if` arms folding** | chore vs pt branches at E1, E2, E9, E10, E11 | each becomes a `project_id` test (or the arm merges into the project branch) | ~5 branch sites simplified |
| **Protocol RPC pairs** | `ListTasks`/`ListChores`, `CreateTask`/`CreateChore`, `CreateChoreInput`/`CreateTaskInput` | unified RPC + input with optional `project_id` | 2 RPC pairs + 1 input-struct pair collapse |
| **CLI verb trees** | `TaskCommand` + `ChoreCommand`, `run_task_command` + `run_chore_command` | shared (chore retained as a thin filter alias) | the bulk of `run_chore_command` becomes a delegation |
| **macOS `isChore` checks** | ~9 `kind == "chore"` call sites | ~5 become `projectID == nil`; ~4 stay as label cosmetics | the membership ones lose the `kind` dependency |
| **Promotion (chore↔task)** | delete-and-recreate (loses short_id, status, PR bindings, history) | `UPDATE tasks SET project_id = ?` | a destructive operation becomes a field write |

**Approximate net lines removed by the chore/pt collapse alone:** on the order of **80–150 lines** (one insert function, two query bodies merged into one, a handful of match arms), *before* counting the RPC/CLI consolidation which is larger but partly back-compat-shim-offset. The point is less the line count than the **structural** wins: a partition becomes a filter, a destructive promotion becomes a field update, and a documented bug class (`task list` invisibility) is removed by construction rather than patched.

---

## 5. Before / after — the illustrative sites

### 5.1 The list partition (E4/E5/E6) — the bug, made structural

**Before** — three queries that disagree even with each other (`list_tasks` omits revision; `get_work_tree` includes it; both omit chore):

```rust
// list_tasks (ListTasks RPC) — boss task list — workitems.rs:524/533
WHERE product_id = ?1 AND kind IN ('project_task', 'design', 'investigation') {deleted_clause}

// list_chores (ListChores RPC) — boss chore list — workitems.rs:639
WHERE product_id = ?1 AND kind = 'chore' {deleted_clause}

// get_work_tree — workitems.rs:267 (tasks) + :281 (chores), two SELECTs
WHERE product_id = ?1 AND kind IN ('project_task','design','investigation','revision') AND deleted_at IS NULL
WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
```

A chore is invisible to `boss task list`; a revision is invisible to *both* list RPCs. The partition is keyed on `kind`, so "show me every leaf item" is impossible.

**After** — one filtered query; chore/pt is a `project_id` predicate, flavor is an optional filter:

```rust
// one query backing a flavor-complete `boss task list`
WHERE product_id = ?1
  {AND project_id IS NULL        -- when --no-project  (the old "chores")}
  {AND project_id = ?2           -- when --project P}
  {AND flavor IN (?, …)          -- when --type given}
  {deleted_clause}
ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC
```

`boss chore list` becomes `--no-project --type normal`; nothing is structurally excludable.

### 5.2 The two insert paths (E8) — `kind` is just `project_id`, spelled out

**Before** — `insert_helpers.rs:164/199`, the two paths differ only in the highlighted columns:

```rust
// insert_task_in_tx  →  project_task
INSERT INTO tasks (… project_id, kind, … ordinal …)
VALUES (?1, ?2, /*project_id*/ ?3, 'project_task', …, /*ordinal*/ ?6, …)

// insert_chore_in_tx  →  chore
INSERT INTO tasks (… project_id, kind, … ordinal …)
VALUES (?1, ?2, /*project_id*/ NULL, 'chore', …, /*ordinal*/ NULL, …)
```

`kind` is *literally* a function of whether `project_id` is `NULL` at insert time. They are never set independently.

**After** — one insert; `kind`/`flavor` derived:

```rust
let flavor = "normal";
let kind = if project_id.is_some() { "project_task" } else { "chore" }; // derived; see design doc derive_kind()
let ordinal = if project_id.is_some() { Some(next_task_ordinal(conn, project_id)?) } else { None };
INSERT INTO tasks (… project_id, flavor, kind, … ordinal …) VALUES (…);
```

### 5.3 The dispatch loop (E2) — divergence is the project lane

**Before** — `executions_runs.rs:425/460`:

```rust
match task.kind.as_str() {
    "chore" => { /* dispatch now, independently */
        reconcile_work_item_execution(&tx, &mut result, &task.id, "chore_implementation", "ready")?; }
    "project_task" | "design" => { /* serialize within the project */
        if let Some(project_id) = &task.project_id {
            project_tasks.entry(project_id.clone()).or_default().push(task); } }
    …
}
// later: only the first incomplete row per project_id gets "ready"
```

**After** — the chore arm folds into the membership test:

```rust
match flavor {
    "normal" => match &task.project_id {
        None      => reconcile_work_item_execution(&tx, …, exec_label(&task), "ready")?, // was chore
        Some(pid) => project_tasks.entry(pid.clone()).or_default().push(task),           // was project_task
    },
    "design" => { /* project member, serialized */ … }
    …
}
```

Behavior is byte-identical; the `kind` discriminator is replaced by the thing it was always standing in for.

### 5.4 The execution-kind label (E1) — a name, not a branch

**Before** — `exec_status_helpers.rs:21`:

```rust
match task.kind.as_str() {
    "chore"    => "chore_implementation".to_owned(),
    "design"   => "project_design".to_owned(),
    "revision" => "revision_implementation".to_owned(),
    _          => "task_implementation".to_owned(),  // project_task
}
```

This is the *only* place the chore/pt split produces a value that gets **persisted** (`work_executions.kind`) and surfaced to the worker session. But — see §6 — **no code branches on `chore_implementation` vs `task_implementation`**. It's a telemetry/display label. Post-collapse it's still derivable (`project_id.is_none()` → `chore_implementation`), but the doc flags it as the one derived identifier with the widest reach.

---

## 6. `kind`-as-stable-identifier risks

These are the things the collapse — and especially the *eventual removal of `kind`* — would change that are **not** behavior branches. They are consumer-facing or persisted names. None blocks the collapse; each must be **deliberately preserved or migrated**, not silently dropped.

| Risk | Where | Who depends on it | Disposition |
|---|---|---|---|
| **Execution-kind label** `chore_implementation` vs `task_implementation` | E1; persisted in `work_executions.kind`; printed into the worker session context ("execution kind: …") | **Nothing branches on it** — the only `execution.kind ==` comparisons in the engine are for `"revision_implementation"` and `"ci_remediation"` (`worker_setup.rs:361`, `completion.rs:1132/1954/2152/2552`, `coordinator.rs:2227`, `runner.rs:334`). It's a telemetry/display label. | Safe to derive from `project_id`. If telemetry **groups by** execution kind, collapsing changes which label a no-project `normal` row gets — confirm no dashboard treats the two as distinct identities before dropping the distinction. |
| **JSON envelope keys** `{"chore": …}` / `{"task": …}` and `{"chores": [...]}` / `{"tasks": [...]}` | C6 (`main.rs:2994`), list outputs; documented in agent guidance (`BossPaneModel.swift:408,410,420` → `boss chore show --json` ⇒ `{chore: {…}}`, `boss chore list --json` ⇒ `{chores: […]}`) | External scripts and the macOS direct-developer agent that `jq '.chore'` / `.chores[]`. | Keep `boss chore *` as a back-compat alias emitting the same envelope; the unified `boss task` surface adds `flavor`/`project_id` without removing `kind` from JSON (design doc §5). |
| **`kind` JSON field** (`"kind": "chore"` / `"project_task"`) | P4 (`types.rs:192`), Models.swift decode | Any consumer reading `kind` to label or filter. | Retain in JSON through the transition; the design doc's "derive `kind`" lever keeps it correct. **But** — per the brief — the implementation must be explicit that this is *deprecation*, with a telemetry-gated drop, not a permanent derived field. |
| **`total_chores` metric** + `list_chores_for_audit` corpus | E12/E13, P5 (`audit_effort.rs`, `types.rs:760`) | The effort-audit report. | The metric *is* the free-floating-normal count; rename/redefine as `project_id IS NULL AND flavor='normal'` and keep the field name for report stability. |
| **macOS card chrome** — `kindLabel` "Chore", section header "Chores", `.chore` card type | M2/M3/M6/M8 | The Work-tab kanban UI (card popover, section grouping). | Derive the label from `(flavor, project_id)`; "Chore" stays the display name for `(normal, no project)`. Card-type identity is cosmetic. |
| **`chores: Vec<Task>` protocol collection** | P3 (`wire.rs:1120/1126`, `types.rs:1085`), macOS `workTree` | Clients that read the separate `chores` array. | Either keep the split collection as a derived convenience or migrate clients to filter `tasks` by `project_id`. |

**Not a risk:** short ids. `T<n>` is unique on `(product_id, short_id)` with no relationship to `kind` or `project_id`, so promotion/demotion preserves it for free (consistent with the design doc's data-preservation guarantee). There is no `C<n>` chore id format anywhere (`BossPaneModel.swift:433`).

---

## 7. Conclusion

**Hypothesis confirmed.** Across ~33 distinct sites, **every** `chore`-vs-`project_task` discrimination is project-membership (collapses to `project_id.is_some()` / `IS NULL`) or a display/identifier label derived from membership. **No site branches the two for a non-membership behavioral reason.** The single historical case where they *were* treated differently in behavior (the merge-poller chore-only sweep) was a logged bug, already fixed by treating them the same — so the codebase has been actively converging on this model on its own.

The collapse therefore carries **no behavioral risk** and delivers concrete simplification: a list partition becomes a filter (removing the `boss task list` invisibility-bug class by construction), two insert paths become one, a destructive promotion becomes a `project_id` field write, ~5 match arms and ~9 macOS checks lose their `kind` dependency, and two RPC/CLI pairs consolidate.

The *only* care required is around a small, fully-enumerated set of **derived identifiers** (§6) — JSON envelope keys, the `kind` JSON field, the execution-kind label, the `total_chores` metric, macOS card chrome — which the implementation must preserve or migrate consciously.

**Recommendation:** proceed with the flavor direction. Use the design doc's "derive `kind` from `(flavor, project_id)`" transition as the zero-break lever — **but** make the implementation explicit in code and comments that `kind` is being *deprecated and removed*, gated on a telemetry check that nothing reads it from JSON. Keeping `kind` as a permanent derived field would simply re-persist the very conflation this project exists to delete; the derived column is a transition aid, not the end state.

### Open questions for the implementing tasks

1. **Execution-kind telemetry grouping.** Does any dashboard/report treat `chore_implementation` and `task_implementation` as distinct identities (not just labels)? If so, the collapse re-labels no-project `normal` executions and that surface needs a migration note. (No *code* branch depends on it — verified.)
2. **JSON `kind` drop gate.** What is the concrete signal that "no external script reads `kind`"? Until that exists, `kind` stays in JSON — but that retention should be visibly time-boxed/deprecated, per the brief, not silently permanent.
3. **`chores: Vec<Task>` collection.** Keep the derived split collection in the work-tree response (convenience) or migrate macOS/clients to filter `tasks` by `project_id`? Affects M4/P3.
