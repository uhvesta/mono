# Boss: Auto-Populate Project Tasks on Design-PR Merge

## Problem

Every Boss project has exactly one `kind = 'design'` task, auto-created at `ordinal = 0` when the project is created (`engine/src/work/revision_helpers.rs:383`). That task dispatches with execution kind `project_design` (`engine/src/work/executions_runs.rs:493`), the worker writes a design doc under `tools/boss/docs/designs/<slug>.md`, opens a normal PR, and the PR merges. The merge poller (`engine/src/merge_poller.rs`) notices the merge via `gh pr view` polling, calls `mark_chore_pr_merged` (`engine/src/work/pr_flow.rs:293`) which flips the design task to `done` and cascades dependents, and then — for `kind = 'design'` rows — calls `design_detector::on_design_pr_merged` (`engine/src/design_detector.rs:166`), which scans the PR for the single `tools/boss/docs/designs/*.md` file and records it as the project's `design_doc_path` pointer.

What happens next is entirely manual. The *real* payoff of a design doc is the pile of implementation tasks it enumerates — almost every doc in `tools/boss/docs/designs/` ends with a "Proposed implementation task breakdown" / "Follow-up Implementation Chores" section. Today the human coordinator reads that section by hand, infers the task graph (names, descriptions, kinds, effort, and the dependency edges that let work proceed in parallel), and types it into a sequence of `boss task create` / `boss task depend add` calls. P707, P757, and P754 were each populated this way this week. The design author writes the work plan, a human reads it, a human retypes it.

This document proposes closing that loop: when a project's design-task PR merges, the engine **automatically generates the project's implementation tasks** — with their dependency edges — by reading the merged design doc through a reusable LLM **Planner** (a "mini-coordinator"). The infer step (LLM reads prose, proposes a typed task graph) is cleanly separated from the apply step (the engine deterministically writes rows through the existing `boss task create` / `boss task depend add` paths), which is what makes the feature testable, idempotent, and safe.

### What is already built vs. what this adds

| Already implemented | This project adds |
|---|---|
| Design-task PR merge detection (`merge_poller::mark_merged`) | A planner invocation hooked into that same merge path |
| `mark_chore_pr_merged` flips design → `done` | An idempotent "populate this project once" gate |
| `on_design_pr_merged` sets `project.design_doc_path` | The Planner: doc-prose → typed task-graph proposal |
| `create_task` / `add_dependency` write paths + cycle detection | A deterministic materializer that applies a proposal through those paths |
| Direct Anthropic API substrate (`live_status.rs`) | A durable planner-run audit trail + operator review/undo surface |

## Reconciliation with existing work

### P6 — "Planner agent for project planning and task extraction" (`proj_18a2bb9a1f7068d8_4`)

P6's stated goal was to "enable automated project planning and task breakdown through a dedicated Planner agent that produces structured markdown plans and populates project task lists." **P6 is archived** — it was never built, and there is no planner agent in the codebase today. This design does not build a parallel planner; it *carries P6's mandate forward* and makes the Planner the central reusable deliverable of this project. Concretely: the "mini-coordinator" this project requires **is** the P6 planner agent. The design-PR-merge trigger is its first consumer, not the planner itself — the planner is defined with a caller-agnostic contract (see *The Planner*) so that operator-invoked planning, replanning, and large-chore decomposition reuse it unchanged.

### `design-producing-tasks.md` — the manifest approach that was *not* taken

The earlier [`design-producing-tasks`](design-producing-tasks.md) design proposed that the design worker emit a structured `<slug>.tasks.json` **manifest** alongside the doc, that the doc be pushed direct-to-`main` (no PR), and that an `ApproveDesign` RPC materialize the manifest on explicit operator approval (its Q8 chose option *(δ)*: worker-emitted manifest as primary, an extractor worker as fallback). **That path was never implemented.** The flow that actually shipped is simpler: design docs ship as ordinary PRs and the engine records a `design_doc_path` pointer on merge. There is no manifest, no `ApproveDesign`, no renderer-approve surface in the code.

This project deliberately picks the *extractor* branch of that earlier taxonomy — `design-producing-tasks.md` Q8 option *(β)*, "spawn something that reads the doc and emits the structured breakdown" — and elevates it from a fallback into the first-class, reusable Planner. The reasons we read the merged prose rather than require a worker-emitted manifest:

- **The doc already ships as prose via PR.** Requiring the worker to *also* maintain a hand-written `.tasks.json` re-introduces a second artifact to keep in sync with the prose, and a second thing for the worker to get wrong. GitHub is already the source of truth for the merged doc.
- **The breakdown is exactly what an LLM is good at extracting.** The section exists in prose in every real example; turning it into a typed graph is a bounded transform.
- **Reusability.** A manifest is bound to the design-authoring worker. A Planner that ingests any design doc + project context is reusable by operator commands and replanning, which is P6's whole point.

Where the two designs *do* converge is the **apply** half: the typed task-graph proposal this design defines (local handles + `depends_on` edges, kind, effort) is structurally the same shape as `design-producing-tasks.md`'s manifest, and the materializer here is the deterministic, transactional, dedup-and-cycle-checked apply step that doc's Q8 sketched. If the manifest flow is ever revived, both can feed the same materializer.

## Goals

- When a `kind = 'design'` task's PR merges, automatically generate that project's implementation tasks — names, descriptions, kinds, effort levels, and the dependency edges between them — from the merged design doc, with no human retyping.
- Build the **Planner** as a first-class, reusable component with a typed input/output contract, usable beyond this trigger (operator "plan this project", replanning, decomposing a large chore).
- **Separate infer from apply.** The LLM produces a *structured* proposal; a deterministic engine materializer applies it. The LLM never writes rows directly.
- **Fire exactly once per project.** A hard idempotency gate prevents duplicate tasks on re-runs, retries, or concurrent triggers.
- **Fail safe and leave a durable, inspectable record.** Because the operator cannot watch the Planner run, every invocation persists its input, raw output, validation result, and materialization result, and surfaces an attention item.
- **Never create a partial or broken graph.** Validation (schema, non-empty, cap, acyclicity) gates the apply; the apply is a single all-or-nothing transaction.
- Reuse the existing `boss task create` / `boss task depend add` write paths (their engine-internal `create_task` / `add_dependency` equivalents); do not invent a parallel creation path that bypasses their gates (including cycle detection).
- Encode the coordinator's effort-estimation heuristic and kind conventions in the Planner, and emit `[effort-classification]` audit lines per [`effort-and-model-estimation`](effort-and-model-estimation.md).

## Non-Goals

- **Replacing the human design-review step.** The PR review of the design doc itself is unchanged; auto-populate runs only *after* the design PR has merged.
- **Mirroring the design doc into Boss.** The doc is fetched live from GitHub at the merged ref. Boss stores only the pointer (already implemented) plus the planner-run audit record. The doc is never copied into the DB.
- **A new task-creation write path.** Materialization goes through `create_task` / `add_dependency`. We do not duplicate their validation, dedup, or cycle-detection logic.
- **Editing the design doc.** The Planner reads; it never writes back to the doc or proposes doc edits.
- **Auto-populating non-design merges, or chores.** The trigger is scoped to `kind = 'design'` rows with a `project_id`. (Operator-invoked planning of an arbitrary project is a *separate caller* of the same Planner, not this trigger.)
- **Cross-product / cross-repo task graphs.** All generated tasks land in the design task's project, in the same product. Cross-product edges are out of scope (tracked elsewhere).
- **Learned task estimation.** Effort follows the existing rules-based heuristic; an ML estimator is a separate future concern.
- **A bespoke review UI.** The review surface is a thin client over engine-owned state (an attention item + the existing kanban + a `release` action). The engine owns the trigger, materialization, idempotency, and audit; the UI renders them.

## Naming

- **Planner** — the reusable LLM mini-coordinator. Entry point `Planner::plan(PlannerInput) -> Result<PlannerOutput>`. (This is the P6 "planner agent.")
- **Task-graph proposal** — the Planner's structured output: a list of proposed tasks each with a `handle` (proposal-local id), plus a set of dependency edges referencing tasks by handle.
- **Materializer** — the deterministic engine step that applies a validated proposal by calling `create_task` / `add_dependency` in one transaction.
- **Populator** — the auto-populate orchestration triggered on design-PR merge: idempotency gate → fetch doc → Planner → validate → Materializer → audit → surface. The Populator is *a* caller of the Planner; it is not the Planner.
- **`planner_runs`** — the durable audit ledger; one row per Planner invocation, also serving as the per-project idempotency gate.
- **Staged task** — a generated task created with `autostart = false` so it exists and is graph-wired but does not dispatch a worker until an operator *releases* it.

---

## Alternatives considered

### Alternative A — Engine-side convention parser (no LLM)

The engine reads the merged doc, finds the `## Proposed implementation task breakdown` (or similar) heading, parses the numbered list for task names/descriptions, and infers dependencies from "*Depends on T-N*" annotations.

**Rejected.** This is `design-producing-tasks.md` Q8 option *(α)* and it hits the brittleness wall that doc already flagged. The five docs in `tools/boss/docs/designs/` that have a breakdown use at least three different heading texts ("Proposed implementation task breakdown", "Follow-up Implementation Chores", "Implementation Plan"), three different per-task layouts (bold-name + inline desc; `T-N — name (crate)` + paragraph; numbered with separate "Acceptance:" clauses), and three different dependency notations ("*Depends on T-6*", "*Depends on: T-1, T-2*", "Critical path: T-1 → T-2 → ..."). A parser would lag doc reality forever and silently misparse, which is the worst failure mode for something the operator can't watch. Effort estimation and kind inference also can't be done by a regex.

### Alternative B — Require a worker-emitted manifest (the `design-producing-tasks.md` path)

Make the design worker write a `<slug>.tasks.json` manifest alongside the doc; the engine applies it on merge.

**Rejected for this trigger** (discussed at length under *Reconciliation*). It re-introduces a second hand-maintained artifact, depends on every design worker remembering to emit it, and produces a component bound to the authoring worker rather than a reusable Planner. The merged *prose* is the source of truth we already have. (The materializer, however, is shared with this approach should it be revived.)

### Alternative C — A headless interactive worker that runs the CLI itself

Spawn a normal Claude worker (like any other execution) whose prompt is "read the merged doc and run `boss task create` / `boss task depend add` to populate the project."

**Rejected.** This collapses infer and apply into one un-gated step. The worker writes rows as side effects with no structured proposal to validate first, no atomic transaction, no clean idempotency key, and no point at which the engine can reject a cyclic or over-large graph before damage is done. It is also the *least* inspectable option — the operator can't see the interaction, and a half-finished worker run leaves a half-built graph. The whole value of separating "propose" from "apply" (testability, idempotency, no-partial-graph safety) is lost. An interactive worker is also slower and costlier than a single structured-output API call for what is fundamentally a prose-to-JSON transform.

### Alternative D (chosen) — Engine-internal LLM Planner with structured output + deterministic materializer

The engine fetches the merged doc live, calls the Planner (a direct Anthropic API call returning a schema-validated task-graph proposal — *infer*), validates the proposal, then materializes it through `create_task` / `add_dependency` in one transaction (*apply*). The Planner is reusable; the trigger is one of several callers. This is the rest of this document.

---

## Chosen approach

### Architecture overview

```
                          merge_poller::mark_merged  (kind == "design", PR merged)
                                        │
                                        ▼
                          ┌──────────────────────────┐
                          │  Populator (orchestrator) │
                          └──────────────────────────┘
   1. idempotency gate ───┤  claim planner_runs row for project_id (UNIQUE)
                          │      already populated / pre-seeded? → skip + log
   2. fetch doc live  ────┤  gh api /repos/.../contents/<path>?ref=<merged ref>
   3. INFER (LLM)     ────┤  Planner::plan(PlannerInput) ──► PlannerOutput
   4. validate        ────┤  schema · non-empty · cap · acyclic · confidence
   5. APPLY (engine)  ────┤  Materializer: create_task + add_dependency  (1 txn)
   6. audit           ────┤  persist input/raw-output/validation/result → planner_runs
   7. surface         ────┤  attention item on project + kanban shows staged tasks
                                        │
                                        ▼
              tasks exist, graph-wired, autostart=false (staged)
              operator reviews → `release` → dispatch begins   (undo: delete batch)
```

The engine owns steps 1, 2, 4, 5, 6, 7. Step 3 (the Planner) is the only LLM step and is the reusable component. The UI is a thin client over the attention item and the staged tasks.

### 1. Trigger & idempotency

**Where it fires.** The hook is `merge_poller::mark_merged` (`engine/src/merge_poller.rs:2095`), in the existing `if updated.kind == "design"` block at lines 2131–2143, immediately after `on_design_pr_merged` has recorded the `design_doc_path`. `mark_chore_pr_merged` has already idempotently flipped the design task to `done` (it returns `Ok(None)` if the row is already `done`/`archived`, so the merge poller never re-enters this block for an already-merged design — the first idempotency layer is free). At this point the engine knows `project_id`, `product_id`, `pr_url`, and `base_ref_name` (the merged ref) — everything the Populator needs.

The merge poller must not block on a multi-second LLM call, so `mark_merged` *enqueues* a Populator job (the design follows the same background-task pattern the poller itself uses) rather than running the Planner inline. The enqueue is cheap and synchronous; the work runs on a worker task.

**Idempotency key = `project_id`.** A new `planner_runs` table (see *Durable audit trail*) carries a `UNIQUE` partial index ensuring at most one `outcome IN ('applied','staged')` row per `project_id`. The Populator's first action is to *claim* the project by inserting a `planner_runs` row in state `running` with `project_id` as the conflict target:

- If the insert succeeds, this invocation owns the populate.
- If it conflicts (a prior `running`/`staged`/`applied` row exists), the Populator **skips** and logs. This makes concurrent triggers, poller restarts (the startup sweep re-runs `run_one_pass`), and manual retries all safe — exactly one populate per project.

A crashed `running` row (engine died mid-populate) is reclaimable after a TTL: a `running` row older than the cost/latency budget (below) is considered stale and may be re-claimed, since no tasks were committed (the apply is transactional — a crash before commit creates nothing).

**Project already has implementation tasks (operator pre-seeded some).** Belt-and-suspenders beyond the `planner_runs` gate: before claiming, the Populator checks for any non-design task under the project (`kind IN ('project_task','task')`, not deleted). If any exist, the Populator **refuses (skips) and raises an attention item** rather than merging:

> *Skipped auto-populate of project «P»: it already has N implementation task(s). The design PR merged, but the project was pre-seeded. Run `boss project plan P --force` to add the planner's tasks anyway (existing tasks are preserved by slug dedup).*

Refuse-not-merge is the safe default because the Planner cannot reason about *why* the operator pre-seeded — merging risks duplicates and contradictory dependency edges the operator didn't intend. The escape hatch (`--force`, which routes through the same Planner via the operator caller and relies on the Materializer's `(name, project_id)` dedup) lets a human opt into merge when they know it's right.

### 2. The Planner (mini-coordinator) — the core reusable component

The Planner is a pure transform: design-doc prose + project/product context **in**, a typed task-graph proposal **out**. It performs no writes and has no knowledge of the trigger that invoked it.

#### Interface contract

```rust
// boss-protocol — shared so every caller (and tests) speaks the same shape.

pub struct PlannerInput {
    pub design_doc: String,            // full merged doc content, fetched live
    pub design_doc_ref: DocRef,        // repo_remote_url + branch/sha + path (provenance)
    pub project: ProjectContext,       // id, name, slug, description, goal
    pub product: ProductContext,       // id, slug, name, repo_remote_url
    pub existing_tasks: Vec<TaskBrief>, // names already in the project (dedup hint for the LLM)
    pub max_tasks: usize,              // hard guardrail surfaced to the model
}

pub struct PlannerOutput {
    pub tasks: Vec<ProposedTask>,
    pub edges: Vec<ProposedEdge>,      // dependency edges by handle
    pub confidence: Confidence,        // High | Medium | Low
    pub breakdown_found: bool,         // false ⇒ no task-breakdown section in the doc
    pub notes: String,                 // free-text rationale, persisted for the operator
    pub effort_audit: Vec<String>,     // one `[effort-classification] ...` line per task
}

pub struct ProposedTask {
    pub handle: String,                // proposal-local id, e.g. "schema-migration"
    pub name: String,
    pub description: String,
    pub kind: TaskKind,                // project_task (default) | investigation
    pub effort: EffortLevel,           // trivial | small | medium | large  (never `max`)
    pub ordinal: i64,                  // soft ordering hint
}

pub struct ProposedEdge {
    pub dependent: String,             // handle of the task that is gated
    pub prerequisite: String,          // handle of the task that gates it
}

pub enum Confidence { High, Medium, Low }
```

This is a **typed, structured-output schema, not free-form prose the engine re-parses.** The Planner is forced to return exactly this shape (see *Execution model*), so the engine receives validated data, never markdown. Dependencies reference tasks by `handle`; the Materializer resolves handles to real task ids at apply time, mirroring the `external_id → id` resolution `design-producing-tasks.md` Q8 described.

`breakdown_found = false` is a first-class signal, distinct from "found a breakdown but it was empty/garbage" — it lets the Populator no-op cleanly when the doc simply has no task list (a pure design-rationale doc), without treating it as an error.

#### Encodes coordinator policy

The Planner's system prompt encodes the policy a human coordinator applies by hand:

- **Effort heuristic.** The rules-based heuristic from [`effort-and-model-estimation`](effort-and-model-estimation.md) Q4 (rules 1–8, first match wins; emits `trivial | small | medium | large`, never `max`). For every proposed task the Planner emits an `[effort-classification]` audit line in the exact format the coordinator/app use (`engine` ... see `BossPaneModel.swift`):

  ```
  [effort-classification] level=`medium` matched-rule=`rule 4 (multi-subsystem hint)` reasons="names engine + protocol surfaces"
  ```

  These lines are persisted in `planner_runs.effort_audit` *and* appended to each created task's description (separated by a blank line), exactly as a hand-filed task carries its classification today. The Materializer sets each task's `effort_level` from `ProposedTask.effort`; the dispatcher then picks model/effort per the existing mapping. The Planner never sets `model_override` (per that doc's Q3 — model is a property of the level).
- **Kind conventions.** Generated tasks are `project_task` by default (they belong to a project and map to one PR each, per `work-taxonomy.md`). A proposed item framed as research/audit ("investigate", "audit", "diagnose") is emitted as `kind = 'investigation'` and classified `large` by rule 2. The Planner never emits `kind = 'design'` (one design per project, already exists) or `kind = 'chore'` (chores are product-direct, not project-scoped).
- **Dependency edges maximize safe parallelism.** The Planner is instructed to add an edge only for a *true* prerequisite (B cannot start until A lands — e.g. "protocol types" before "engine RPC handler"), and to leave independently-startable tasks unedged so they dispatch in parallel. This mirrors how P707/P757/P754 were wired (a schema/protocol task as a shared root, then a fan-out of independent consumers, then an integration task that depends on the fan-out). `ordinal` is a soft ordering hint only; real gating is the edge set — the same separation `work-dependencies.md` draws.

#### Execution model

- **Engine-internal direct API call, not a worker spawn.** The engine already has exactly the right substrate: `live_status.rs` POSTs to `https://api.anthropic.com/v1/messages` via a shared `reqwest` client (`engine/src/live_status.rs:52`), reading the API key from config and returning a *typed outcome* (`NoApiKey`, `ApiError { .. }`, success) so callers distinguish "no key" from "model 429" from "succeeded". The Planner reuses this client and outcome model. A direct API call is the right tool because the Planner needs no filesystem and no tools — it is a prose-to-JSON transform — and an interactive worker can't return structured output without re-introducing a manifest/sentinel (Alternative C).
- **Structured output is enforced**, not requested. The call forces the model to return the `PlannerOutput` schema (a single forced tool call whose input schema is `PlannerOutput`, or the equivalent JSON-schema-constrained output). The engine deserializes directly into the Rust type; a deserialization failure is a validation failure (retry, then fail safe), never a parse-and-hope.
- **Model selection.** Planning quality matters and the call is infrequent (once per project), so the Planner defaults to a strong model (Opus) rather than the Haiku that `live_status` uses for its cheap one-liner. The model is a single constant, tunable without a schema change (the open questions note this). Effort/`max_tokens` are bounded (see guardrails).
- **The doc is fetched live at the merged ref**, never mirrored: `gh api /repos/<owner>/<repo>/contents/<path>?ref=<base_ref_name>` (the merged ref the poller already has), reusing the same `gh`-based fetch shape `design_detector::do_scan_pr` uses. GitHub remains the source of truth for the PR-shipped artifact.

#### Reusability

`Planner::plan(PlannerInput) -> Result<PlannerOutput>` is caller-agnostic. Defined consumers:

1. **The Populator** (this project) — on design-PR merge.
2. **Operator command** `boss project plan <project> [--force] [--dry-run]` — "plan this project now" / re-plan, building `PlannerInput` from the project's stored `design_doc_path` (fetched live). `--dry-run` runs infer + validate and prints the proposal *without* materializing — the natural way for an operator to preview what the trigger would do. `--force` bypasses the pre-seeded refusal.
3. **Replanning** — re-running against an updated doc; the Materializer's dedup makes it additive (existing tasks by `(name, project_id)` are skipped, new ones added), never destructive.
4. **Decompose a large chore** (future) — same contract with a chore description in place of a design doc.

All four share one contract, one validation path, one Materializer, and one audit ledger.

### 3. The deterministic materializer (apply)

`Materializer::apply(project_id, &PlannerOutput) -> ApplyResult` runs in a single SQLite transaction and is the *only* thing that writes rows:

1. **Resolve every edge's endpoints to known handles.** An edge referencing an unknown handle is a validation failure (reject the whole proposal — see below); we never silently drop edges.
2. **Topologically sort tasks by the edge set; reject cycles** before any insert. (Defense in depth: `add_dependency` itself calls `would_create_cycle` at `engine/src/work_dependencies.rs:194`, so even if validation missed a cycle the edge insert refuses it and the transaction rolls back.)
3. **For each task**, dedup by `(name, project_id)`: if a non-deleted task with that name already exists in the project, skip it but still record its `handle → id` mapping so edges resolve. Otherwise call `create_task(CreateTaskInput { product_id, project_id, name, description (incl. the effort-audit line), effort_level, kind, autostart: false, created_via: "engine_auto", .. })` (`engine/src/work/create_entities.rs:153`) and remember the new id.
4. **For each edge**, call `add_dependency(AddDependencyInput { dependent_id, prerequisite_id, relation: "blocks" })` (`engine/src/work/workitems.rs:609`), resolving handles via the map. `INSERT OR IGNORE` semantics in `insert_edge` make duplicate edges a no-op (re-apply safe).
5. **Commit.** Any error in steps 1–4 rolls the whole transaction back — **no partial graph is ever created.** The `planner_runs` row is updated to `failed` with the error.
6. Return `ApplyResult { created: Vec<TaskId>, skipped: Vec<String>, edges_created: usize }` for the audit record and the operator summary.

Reusing `create_task` / `add_dependency` means the materializer inherits their existing gates (same-product check, cycle detection, `INSERT OR IGNORE` edge dedup) for free, per the project constraint to not bypass those write paths.

### 4. Graceful failure & observability (first-class)

Because the operator **cannot watch the Planner run**, this is treated as a core requirement, not an afterthought. Every invocation either commits a complete graph or commits nothing, and always leaves a durable, inspectable record.

#### Validation of the structured proposal

Between infer and apply, the proposal is validated. Any failure is **no-op-safe** — nothing is written:

- **Deserialization** into `PlannerOutput` must succeed (enforced by the structured-output call).
- **`breakdown_found == false`** → clean no-op (not an error). Record `outcome = 'no_breakdown'`, raise an informational attention item ("Design doc for «P» had no task-breakdown section; no tasks created. Plan manually or add a breakdown and re-run `boss project plan P`.").
- **Empty `tasks`** with `breakdown_found == true` → treated as low-signal; no-op + attention item.
- **Task cap.** `tasks.len() > max_tasks` → **do not truncate.** Silent truncation must never read as success. Record `outcome = 'rejected_too_many'`, raise an attention item quoting the count, and stage nothing. The operator re-runs with a higher cap or splits the project.
- **Acyclicity** (step 2 above) → a cyclic proposal is rejected whole; `outcome = 'rejected_cycle'` + attention item.
- **Confidence == Low** → still materialize (staged, see review checkpoint) but the attention item is escalated in prominence and says explicitly that the planner was uncertain.
- **Unknown handle in an edge / duplicate handle** → reject whole; attention item.

#### Retries, fallbacks, and the fail-safe mode

- **Doc fetch fails** (GitHub 5xx, transient `gh` error, token issue): retry with bounded backoff (small fixed number of attempts). On exhaustion → `outcome = 'fetch_failed'`, attention item, no-op. The `design_doc_path` pointer is unaffected, so a later `boss project plan P` can retry once GitHub is healthy.
- **Doc fetch 404** (file moved/renamed since merge): no retry; `outcome = 'doc_missing'` + attention item naming the path.
- **LLM call fails** (`ApiError`, timeout, or output fails schema validation): retry once, then fail safe (`outcome = 'planner_failed'` + attention item). `NoApiKey` is a distinct outcome — the feature degrades to "design pointer set, tasks not auto-created" with an attention item telling the operator to configure the key, exactly as `live_status` degrades.
- **The cardinal rule:** the only state-mutating step is the single materializer transaction. Every failure mode before commit leaves the project exactly as it was (design task `done`, pointer set, zero tasks created).

#### Durable audit trail

A new `planner_runs` table is the operator's after-the-fact window into an interaction they didn't witness:

```sql
CREATE TABLE planner_runs (
  id              TEXT PRIMARY KEY,         -- run_<...>
  project_id      TEXT NOT NULL,
  product_id      TEXT NOT NULL,
  design_task_id  TEXT,
  caller          TEXT NOT NULL,            -- 'merge_trigger' | 'operator' | 'replan'
  doc_ref         TEXT,                     -- repo|ref|path the doc was fetched from
  model           TEXT,                     -- model slug used
  input_summary   TEXT,                     -- doc length, project/product, existing-task count
  raw_output      TEXT,                     -- the model's full structured JSON (verbatim)
  effort_audit    TEXT,                     -- the [effort-classification] lines
  notes           TEXT,                     -- planner's rationale
  outcome         TEXT NOT NULL,            -- running|staged|applied|no_breakdown|
                                            --   rejected_too_many|rejected_cycle|
                                            --   fetch_failed|doc_missing|planner_failed|
                                            --   skipped_pre_seeded|skipped_already_populated
  result_summary  TEXT,                     -- created/skipped task ids, edge count, errors
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
-- One successful populate per project (idempotency gate):
CREATE UNIQUE INDEX planner_runs_one_per_project
  ON planner_runs(project_id)
  WHERE outcome IN ('running','staged','applied');
```

Storing `raw_output` verbatim means that even when the apply succeeds, an operator can later read exactly what the model proposed and why. The doc itself is *not* stored (it lives in GitHub at `doc_ref`); only the planner's interaction with it is. `boss project plan-runs <project>` (and an engine RPC for the app) expose these rows.

#### Operator review checkpoint — recommendation

**Recommendation: auto-create the tasks in a *staged* (non-dispatching) state, surface a prominent attention item, and require one operator `release` action to begin dispatch. Provide undo regardless.**

This is human-in-the-loop on the *irreversible/expensive* step (spawning workers) but full-auto on the *tedious* step (extract + create + wire). Concretely, the Materializer creates tasks with `autostart = false`, so they appear on the kanban, fully graph-wired, but the dispatcher's "first-incomplete-is-ready" chain (`executions_runs.rs`) does not promote them to `ready` — no worker spawns. The operator reviews on the kanban (and can read the planner's rationale via `planner_runs`), then runs `boss project release <project>` (or flips autostart in the app) to set `autostart = true` and let dispatch begin.

Why staged rather than auto-create-and-dispatch:

- **The can't-see-it constraint cuts hard here.** The operator never witnessed the Planner. Auto-dispatching would spawn workers — opening PRs, consuming cube leases, possibly doing the *wrong* work — before any human laid eyes on the plan. Undo *after* workers have started is messy (live PRs, in-flight leases).
- **Staging keeps 100% of the automation value.** The retyping toil — read doc, infer graph, create rows, wire deps — is exactly what's eliminated. Review-then-release is a few seconds of operator time, not minutes of typing.
- **It reuses an existing field.** `autostart` already gates dispatch; no new task status or state machine is needed. (A dedicated `proposed` status was considered and rejected: it would touch every status-aware surface for no benefit `autostart = false` doesn't already give.)

Why not pure manual approval (don't create anything until approved): that re-introduces the manual step the project exists to remove, and there'd be nothing concrete to review.

**Undo / rollback (provided regardless of the above).** Every task in a populate carries the originating `planner_runs.id`. `boss project unpopulate <project> --run <id>` (and an app affordance) deletes exactly that batch — but only the still-untouched tasks: a staged task that has not been released and has no execution is safe to delete; a task that has already been released and dispatched is *reported, not deleted* (the operator decides), so undo never destroys in-flight work. Deleting the batch also clears the `planner_runs` idempotency row, so a corrected re-plan can run.

#### Bounding & guardrails

- **Task cap.** `max_tasks` (default ~30, a constant). Exceeding it rejects the whole proposal (no silent truncation, per above) and logs the count.
- **Cost/latency budget.** One bounded API call per project (bounded `max_tokens`, bounded effort, a wall-clock timeout). The doc fetch and LLM call have small fixed retry counts. The whole populate is one infrequent operation per project lifetime.
- **Circuit breaker against runaway creation.** The `planner_runs` UNIQUE-per-project gate *is* the circuit breaker: a project can be populated at most once automatically, so no trigger storm or poller restart can multiply tasks. A coarse global rate limit (populates per engine per minute) backstops a pathological flood of simultaneous design merges.
- **No silent drops.** Anything dropped or truncated — over-cap, low-confidence, skipped-because-pre-seeded — is `log()`-ed and surfaced as an attention item. Silence never reads as success.

#### Surfacing

The operator learns it happened without watching, via:

- **An attention item on the project** (the existing `WorkAttentionItem` surface), whose text differs by outcome: "staged N tasks, review & release", "skipped (pre-seeded)", "no breakdown found", "rejected: cyclic graph", "planner failed: «reason»". This is the primary signal.
- **The kanban**, which shows the new staged tasks immediately (a `work_items_created` batch event lets it refresh in one round-trip).
- **`planner_runs`** for the full after-the-fact record (input summary, raw output, rationale, result).

### 5. Edge cases

| Case | Handling |
|---|---|
| **No task-breakdown section** | Planner returns `breakdown_found = false` → clean no-op + informational attention item. Not an error. |
| **Ambiguous / low-confidence breakdown** | `confidence = Low` → still staged (never auto-dispatched), with an escalated attention item flagging the uncertainty for review. |
| **Multi-design project** (shouldn't exist — one design per project) | The `planner_runs` UNIQUE-per-project gate makes the *first* design merge populate and any subsequent one skip. Defensive, not relied upon. |
| **Design PR merged but doc fetch fails** | Bounded retries; on exhaustion `outcome = 'fetch_failed'` + attention item; no tasks created. `boss project plan P` retries later. |
| **Doc moved/deleted between merge and fetch** | 404 → `outcome = 'doc_missing'` + attention item naming the path; no-op. |
| **Planner proposes a cyclic graph** | Rejected at validation (topo-sort) before any write; `add_dependency`'s `would_create_cycle` is the second line of defense; `outcome = 'rejected_cycle'` + attention item; nothing created. |
| **Planner proposes too many tasks** | Over `max_tasks` → whole proposal rejected (no truncation); attention item quotes the count. |
| **Project pre-seeded with implementation tasks** | Refuse + attention item; `boss project plan P --force` opts into additive merge (dedup by `(name, project_id)`). |
| **Concurrent triggers / poller restart** | First claim of the `planner_runs` row wins; others skip. |
| **Engine crash mid-populate** | Apply is transactional → nothing committed; the stale `running` row is reclaimable after the budget TTL. |
| **No API key configured** | `outcome = 'planner_failed'` (NoApiKey) + attention item to configure the key; feature degrades gracefully, pointer still set. |

---

## Risks / open questions

**R1 — Planner output quality.** The whole feature rests on the LLM producing a sensible graph from prose. Mitigation: the staged (non-dispatching) default means a bad plan costs a review, not wasted worker runs; `--dry-run` lets operators preview; the raw output is persisted for inspection; low confidence escalates the attention item. *Open question:* what confidence threshold (if any) should force staging-with-warning vs. a no-op? Current proposal: never auto-discard on low confidence — always stage and flag, because a human reviews before release anyway.

**R2 — Effort/kind drift from the human heuristic.** The Planner encodes the Q4 rules in a prompt, so it approximates rather than executes the deterministic heuristic. Mitigation: every task carries its `[effort-classification]` line, so a reviewer sees the reasoning; the operator can edit effort before release. *Open question:* should the engine re-run the deterministic Rust heuristic (if/when it's extracted from the coordinator into engine code) over the Planner's task names as a cross-check, overriding the LLM's effort when they disagree?

**R3 — Auto-apply vs. human-in-the-loop.** Resolved in favor of *staged + release* (see review checkpoint), but this is the highest-stakes decision and the one most worth a reviewer landing on. If operators find review-then-release too heavy in practice, the alternative (auto-dispatch with prominent summary + undo) is a one-field change (`autostart = true`), so the decision is reversible.

**R4 — Doc-prose variability.** Real docs vary in how they express the breakdown. The LLM is robust to this where Alternative A's parser was not, but a doc with an unusually-structured or buried breakdown could yield a thin plan. Mitigation: `breakdown_found` + confidence signals; the operator can always re-plan or hand-fill.

**R5 — Cost.** An Opus call per project is more expensive than Haiku-tier work, but it is once per project lifetime and replaces minutes of human coordinator time. *Open question:* is Opus warranted, or is a mid-tier model sufficient for extraction? The model is a single tunable constant; start with the stronger model and downshift if quality holds.

**R6 — `planner_runs` as both audit and idempotency ledger.** Coupling the two means an audit-row cleanup could accidentally re-open the idempotency gate. Mitigation: the UNIQUE partial index is scoped to live outcomes (`running`/`staged`/`applied`); audit rows for terminal failures don't gate, and undo deliberately clears the gate. *Open question:* should idempotency live in a dedicated `projects.tasks_populated_at` column instead, with `planner_runs` purely an audit log? The single-table approach is proposed for simplicity; splitting them is a clean alternative if the coupling bites.

**R7 — Relationship to a future revived manifest flow.** If `design-producing-tasks.md`'s worker-manifest path is ever built, two producers (manifest and Planner) could feed the Materializer. Mitigation: the Materializer is producer-agnostic (it takes a validated proposal), so this is additive, not conflicting; the idempotency gate ensures only one populates a given project.

**R8 — Effort heuristic lives in the coordinator prompt, not in code.** The Q4 rules are currently applied by the coordinator LLM, not a shared Rust function. The Planner re-encodes them in *its* prompt, so there are now two prose copies of the same rules. *Open question:* extract the heuristic into a shared engine module (`engine/src/effort.rs` already exists) and have both the Planner prompt and any future deterministic check reference one source of truth.

---

## Proposed implementation task breakdown

Bite-sized; each maps to roughly one PR. Dependencies are listed explicitly; the Planner-generated version of *this* project would wire these same edges. The shared **contract** task is the root; the Planner and Materializer can then be built in parallel; the trigger integrates them; observability and operator surfaces layer on top.

1. **Protocol: Planner contract types** (`boss-protocol`). Add `PlannerInput`, `PlannerOutput`, `ProposedTask`, `ProposedEdge`, `Confidence`, and the `ApplyResult` shape. Structured-output JSON schema for `PlannerOutput`. Effort: `small`. *Depends on: none.*

2. **Engine: `planner_runs` table + migration** (`boss-engine`). Schema add per *Durable audit trail*, idempotent migration, the UNIQUE-per-project partial index, and `WorkDb` accessors (claim/update/list). Effort: `medium`. *Depends on: none.*

3. **Engine: the Planner** (`boss-engine`). `Planner::plan(PlannerInput) -> Result<PlannerOutput>` reusing the `live_status.rs` Anthropic substrate; system prompt encoding the Q4 effort heuristic, kind conventions, parallelism-maximizing edge guidance, and `[effort-classification]` emission; structured-output enforcement; bounded model/effort/timeout; typed outcomes. Effort: `large`. *Depends on: 1.*

4. **Engine: live doc fetch** (`boss-engine`). Fetch the design doc at the merged ref via `gh api /contents` (reuse the `design_detector` fetch shape), with bounded retries and 404 handling. Effort: `small`. *Depends on: none.*

5. **Engine: deterministic Materializer** (`boss-engine`). `Materializer::apply(project_id, &PlannerOutput)`: handle resolution, topo-sort + cycle reject, `(name, project_id)` dedup, single-transaction `create_task` (`autostart = false`) + `add_dependency`, `ApplyResult`. Tag created tasks with the `planner_runs.id`. Effort: `large`. *Depends on: 1, 2.*

6. **Engine: validation layer** (`boss-engine`). Schema/non-empty/cap/acyclicity/handle-integrity checks producing the no-op-safe outcomes; `breakdown_found` and confidence handling. Effort: `medium`. *Depends on: 1.*

7. **Engine: the Populator + trigger hook** (`boss-engine`). Enqueue a background Populator job from `merge_poller::mark_merged`'s `kind == 'design'` block; orchestrate idempotency claim → pre-seeded check → fetch → plan → validate → apply → audit → surface. Effort: `large`. *Depends on: 2, 3, 4, 5, 6.*

8. **Engine: attention-item + event surfacing** (`boss-engine`). Outcome-specific `WorkAttentionItem` text and the `work_items_created` batch event for the kanban. Effort: `medium`. *Depends on: 7.*

9. **CLI: operator entry points** (`boss-cli`). `boss project plan <project> [--force] [--dry-run]`, `boss project release <project>`, `boss project unpopulate <project> --run <id>`, `boss project plan-runs <project>`. These exercise the reusable Planner/Materializer from outside the trigger. Effort: `medium`. *Depends on: 3, 5, 6, 7.*

10. **macOS app: review/release/undo surface** (`app-macos`). Render the staged tasks, the planner attention item, a "release" affordance, and a planner-run inspector (raw output + rationale). Thin client over the engine RPCs. Effort: `medium`. *Depends on: 8, 9.*

11. **Tests: end-to-end fixtures** (`boss-engine`). Use P707/P757/P754 design docs as ground-truth fixtures: assert the Planner+Materializer produce the expected task set and dependency edges; cover the no-breakdown, cyclic, over-cap, pre-seeded, and fetch-failure paths; assert idempotency under double-fire. Effort: `large`. *Depends on: 5, 6, 7.*

*Critical path:* **1 → 3 → 7 → 9 → 10**, with 2/4/5/6 feeding 7 in parallel and 11 validating once 5–7 land.

---

## References

- [`design-producing-tasks`](design-producing-tasks.md) — the manifest/`ApproveDesign` design that was not built; shares the apply-step shape.
- [`effort-and-model-estimation`](effort-and-model-estimation.md) — the effort heuristic (Q4 rules) and `[effort-classification]` audit-line format the Planner encodes.
- [`work-dependencies`](work-dependencies.md) — dependency-edge semantics, `would_create_cycle`, ordinal-vs-edge separation.
- [`work-taxonomy`](work-taxonomy.md) — task/chore/project_task/design kind conventions.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) — the `design_doc_path` pointer this feature reads.
- Code anchors: `merge_poller::mark_merged` (`engine/src/merge_poller.rs:2095`), `mark_chore_pr_merged` (`engine/src/work/pr_flow.rs:293`), `design_detector::on_design_pr_merged` (`engine/src/design_detector.rs:166`), `create_task` (`engine/src/work/create_entities.rs:153`), `add_dependency` (`engine/src/work/workitems.rs:609`), `would_create_cycle` (`engine/src/work_dependencies.rs:194`), Anthropic substrate (`engine/src/live_status.rs:52`).

---

*Parent project: P783 `auto-populate-project-tasks-on-design-pr-merge` (`proj_18b3f1d464bce660_71`). Carries forward the mandate of the archived P6 "Planner agent for project planning and task extraction."*
