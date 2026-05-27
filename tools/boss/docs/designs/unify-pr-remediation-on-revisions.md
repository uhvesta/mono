# Boss: Unify merge-conflict and CI-failure remediation on the revision mechanism

## Problem

Boss has three flows that all do the same shape of work — *add a commit to an existing in-review PR* — but only one of them knows it.

1. **Revision tasks** ([`revision-tasks.md`](revision-tasks.md), "P654"). A first-class `tasks.kind = 'revision'` bound to a parent task via `parent_task_id`. Its deliverable is *a new commit on the parent's existing PR branch* — not a new PR. It dispatches a `revision_implementation` execution into (ideally) the parent's warm workspace, checks out the parent branch by name, pushes back, is hard-guarded against `gh pr create`, rides the parent PR's lifecycle to `done`, and is numbered `R<n>` within its chain. Operator-triggered today; explicitly designed so future *producers* (the GitHub comment-triage UI, "Source B") are "thin producers on top of it" calling `CreateRevision` with a `created_via` pointer.

2. **Merge-conflict handling** ([`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md), "P188"). When the merge poller observes an in-review PR is `CONFLICTING`/`DIRTY`, `conflict_watch::on_conflict_detected` flips the parent chore to `blocked: merge_conflict`, inserts a `conflict_resolutions` attempt row (keyed `(work_item_id, base_sha_at_trigger)`), runs a `git merge-tree` diagnosis, and dispatches a bespoke `conflict_resolution` execution whose prompt (`runner.rs::compose_conflict_resolution_prompt`) tells the worker to rebase, resolve, push the branch, and **open no new PR**. When the PR is clean again, `on_resolved` snaps the parent back to `in_review`.

3. **CI-failure handling** (same design doc, CI half). When the poller observes required checks failing at clean mergeability (or a merge-queue `FAILED_CHECKS` rebounce), `ci_watch::on_ci_failure_detected` flips the parent to `blocked: ci_failure`, inserts a `ci_remediations` attempt row (keyed `(work_item_id, head_sha_at_trigger, attempt_kind)`), fetches a CI log excerpt, and dispatches a bespoke `ci_remediation` execution whose prompt (`runner.rs::compose_ci_remediation_prompt`) tells the worker to rebase, fix, push the branch, and **open no new PR**. Budget-capped (default 3 fix attempts/PR); clears on green.

Flows 2 and 3 are conceptually special cases of flow 1: *"add a commit to the existing PR to fix X."* They each carry their own execution kind, their own `compose_*_prompt`, their own "don't open a PR" instruction, their own warm-workspace targeting, and their own completion handling — all re-implementing what the revision mechanism already does. The divergence is not incidental: when P188 was designed it *deliberately* chose a side-table attempt model over the task-row model ([`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) Q on `blocked_reason`, option A vs the side-table pick), because at that time there was no task-kind whose deliverable was "another commit on an existing PR." P654 then created exactly that kind. The two designs were never reconciled.

This doc proposes the reconciliation: **make merge-conflict and CI-fix remediation thin producers on the revision substrate** — engine-triggered siblings of the operator and comment-triage producers — so the *fix-delivery vehicle* is one code path (`kind=revision` + `revision_implementation`), while the *signal-specific bookkeeping* (detection, idempotency keys, diagnosis, budgets, retire-on-clean) stays where it belongs.

This is large and crosses three subsystems and reverses a prior deliberate decision. **It should be promoted to a project with a design-seed task** (see [Recommendation](#recommendation-promote-to-a-project)); it was filed as a chore per operator request. This doc is written to be that project's seed.

## Goals

- **One fix-delivery vehicle.** Merge-conflict and CI-fix remediation dispatch a `kind=revision` task / `revision_implementation` execution instead of bespoke `conflict_resolution` / `ci_remediation` execution kinds. Delete the duplicated dispatch, warm-workspace, no-new-PR-guard, and completion plumbing.
- **A provenance model for non-operator revisions.** `created_via` distinguishes operator feedback, comment-triage, merge-conflict, and CI-fix origins, so the kanban, audit, and editorial-controls gate can tell *why* a revision exists without reading its description.
- **Auto-generated revision intent.** The short `description` (Review-lane affordance text) and the full worker directive (diagnosis blocks, log excerpts, step-by-step commands) are generated from the attempt row, not authored by an operator — without bloating the card text.
- **Keep the signal-specific bits that are genuinely signal-specific.** Detection, SHA-keyed idempotency, churn guard, CI budget, `git merge-tree` diagnosis, CI log fetch, opt-out gates, retire-on-clean, and the `blocked_reason` PR-health discriminator stay; they are not duplication, they are per-signal policy.
- **Preserve every externally observable behaviour** of P188/P654 that operators rely on: the kanban "this PR needs attention" signal, the `boss engine conflicts|ci …` CLIs, the attention items, the budget exhaustion stop, the opt-out label/flag.
- **Coordinate, don't conflict, with P188 and P654.** Both are largely landed. This is a refactor *on top of* their primitives, sequenced after they stabilize.

## Non-Goals

- **Changing detection.** The merge poller's three-state probe (`Open{mergeability, ci}` / `Merged` / `ClosedUnmerged`) and the merge-queue rebounce timeline read are unchanged. This doc only changes what happens *after* a signal fires.
- **Folding CI `retrigger` into revisions.** A `retrigger` attempt re-runs flaky/infra checks and produces **no commit**; a revision's entire contract is "produce a commit." Retrigger stays a lightweight bespoke action (see [Q6](#design-question-6--the-cases-that-do-not-map-cleanly)).
- **Removing the attempt side-tables.** `conflict_resolutions` and `ci_remediations` remain as the per-signal *trigger ledger* (idempotency keys, diagnosis JSON, budget/churn counters, mark-failed reasons). They gain a soft-FK to the revision they spawned; they lose the role of "the thing that owns the worker execution."
- **A new kanban column or card type for engine-triggered revisions.** They render as revision cards; `created_via` may drive a subtle chrome distinction, no more.
- **Auto-merging** after remediation, or auto-applying anything without the existing opt-out gates.
- **Unifying auto-rebase of stacked PRs** ([`auto-rebase-stacked-prs.md`](auto-rebase-stacked-prs.md)) in this pass. Auto-rebase is adjacent (it also pushes to an existing branch) and is a natural fourth producer, but it has its own attempt table and ordering constraints; fold it in a later phase only after the conflict/CI convergence proves out.

## Naming

- **Producer.** A code path that decides a revision should exist and calls the revision-create entry point. Today: the operator (Source A) and the deferred comment-triage UI (Source B). This doc adds **Source C (merge-conflict)** and **Source D (CI-fix)**, both *engine-triggered*.
- **Engine-triggered revision.** A `kind=revision` task whose `created_via` marks it as produced by the engine in response to a mechanical PR-health signal rather than human feedback. Same row shape, same dispatch, same lifecycle as an operator revision.
- **Trigger ledger.** The `conflict_resolutions` / `ci_remediations` side-tables in their post-unification role: per-signal idempotency + diagnosis + budget, plus `revision_task_id` pointing at the revision they spawned.
- **`created_via` provenance grammar** (existing free-form `tasks.created_via` TEXT, extended): `operator` · `pr-comment:<repo>#<pr>:<cid>` · **`merge-conflict:<crz_id>`** · **`ci-fix:<crm_id>`**. The attempt id is the pointer; the `(repo, pr#)` is recoverable from the chain root's `pr_url`, mirroring how Source B carries the comment pointer rather than mirroring it into columns.

---

## Background: the three mechanisms as they stand (read-only map)

Concrete anchors for the implementer. All line numbers are as of this writing and will drift.

**Revision (P654)** — `tasks.kind='revision'`, `tasks.parent_task_id` (soft FK), projections `revision_seq` / `revision_parent_pr_url`. Create: `WorkDb::create_revision` → `assert_parent_revisable_and_insert` → `insert_revision_in_tx` (`work.rs`), gated on chain-root PR being Open. Dispatch: revision arm in the reconcile loop → `reconcile_revision_execution` (`work.rs`) creates a `revision_implementation` execution with `pr_url = chain_root.pr_url`, `prefer_is_soft = true`, `preferred_workspace_id` from the chain root's last run. Worker: `compose_revision_directive` (`runner.rs`) + a `gh pr create` PreToolUse guard registered only for `revision_implementation` (`worker_setup.rs`). Lifecycle: rides the parent — `mark_chore_pr_merged` flips in-review revisions to `done`; merged-mid-flight → `blocked: parent_pr_closed` + attention item. `CreateRevisionInput.created_via` already carries arbitrary provenance.

**Merge-conflict (P188)** — detect in `merge_poller::sweep_one` → `conflict_watch::on_conflict_detected`. Side-table `conflict_resolutions` (UNIQUE `(work_item_id, base_sha_at_trigger)`; churn guard 3/3600s; `conflict_diagnosis` JSON from `conflict_diagnosis::collect` via `git merge-tree`). Parent flip `mark_chore_blocked_merge_conflict` (`in_review`→`blocked`, `blocked_reason='merge_conflict'`, `blocked_attempt_id=<crz>`). Execution `kind='conflict_resolution'`; prompt `compose_conflict_resolution_prompt`. Retire `on_resolved` → `clear_chore_blocked_merge_conflict_for_attempt` + `mark_conflict_resolution_succeeded` + lease release. Stop reasons via `boss engine conflicts mark-failed <id> --reason {obsolescence_suspected|product_decision_required|architectural_mismatch}`.

**CI-fix (P188 CI half)** — detect in `merge_poller::sweep_one` → `ci_watch::on_ci_failure_detected` (plus `on_merge_queue_rebounce_detected`). Pre-triage `classify_pre_triage` → `retrigger` vs `fix`. Side-table `ci_remediations` (UNIQUE `(work_item_id, head_sha_at_trigger, attempt_kind)`; `failed_checks` JSON; `log_excerpt` from `ci_log_reader`; `failure_kind` ∈ `pr_branch_ci`|`merge_queue_rebounce`; `before_commit_sha` for rebounce). Budget: `tasks.ci_attempts_used` vs `effective_ci_budget` (default 3; `fix` consumes, `retrigger` free; rebase-only success refunds). Parent flip `mark_chore_blocked_ci_failure` (`blocked_reason='ci_failure'`; exhaustion → `ci_failure_exhausted`). Execution `kind='ci_remediation'`; prompt `compose_ci_remediation_prompt`. Side tables `ci_failure_suppressions`, `ci_inflight_observations`, `task_blocked_signals`.

**Shared opt-out** (both P188 flows): product flag `products.auto_pr_maintenance_enabled` + PR label `boss/no-auto-rebase`, checked in `auto_pr_maintenance_disabled` / `pr_labels_opt_out`.

---

## Design Question 1 — What converges onto the revision substrate, and what stays signal-specific?

The unification is not "delete the side-tables and make everything a revision." It is a *layering*: the revision mechanism owns the **fix-delivery vehicle**; the detection subsystems own the **signal policy**; the side-tables become the **trigger ledger** that bridges them.

### Converges (delete the bespoke copy, call the revision path)

| Concern | Bespoke today | After |
|---|---|---|
| Execution kind | `conflict_resolution`, `ci_remediation` | `revision_implementation` (one kind) |
| Dispatch | `conflict_watch`/`ci_watch` each call `create_execution` directly | producers call the revision-create entry; `reconcile_revision_execution` dispatches |
| Warm-workspace targeting | re-derived per flow | `prefer_is_soft` + chain-root `preferred_workspace_id` (already in revision dispatch) |
| "Open no new PR" | a paragraph in each prompt | the `revision_implementation` `gh pr create` PreToolUse hard-guard |
| Branch-by-name checkout / push-back | each prompt spells it out | `compose_revision_directive`'s fetch+edit+push recipe |
| Completion / roll-up | bespoke retire marks attempt succeeded | revision rides parent lifecycle; rolls up under the parent card as `R<n>` |

### Stays signal-specific (not duplication — per-signal policy)

- **Detection** (merge poller probe; rebounce timeline) — unchanged.
- **Idempotency keys.** Conflict dedups on `base_sha_at_trigger`; CI dedups on `(head_sha, attempt_kind)`. The revision create-time "recent duplicate" guard is coarser and PR-merge-state-based; it cannot express "same base SHA." The attempt tables remain the dedup ledger and the producer consults them *before* creating a revision.
- **Diagnosis & log injection.** `git merge-tree` output and the CI `log_excerpt` + `failed_checks` are fetched at detection and stored on the attempt row; they are injected into the revision worker's directive at dispatch (see [Q3](#design-question-3--auto-generated-intent-vs-injected-directive)).
- **Budget (CI) and churn guard (conflict).** Caps on how many engine-triggered revisions a PR may accrue from one signal. Enforced at the producer, before revision-create.
- **Retire-on-clean and the `blocked_reason` PR-health discriminator** (see [Q4](#design-question-4--keep-the-blocked_reason-health-signal)).
- **Opt-out gates** — evaluated at the producer.
- **`mark-failed` stop reasons / CI triage classes** (see [Q6](#design-question-6--the-cases-that-do-not-map-cleanly)).

### Decision

**Layer, don't collapse.** The revision mechanism becomes the single fix-delivery vehicle; the detection subsystems become *producers* that call the revision-create entry point; the attempt side-tables persist as the per-signal trigger ledger and gain a `revision_task_id` soft-FK. This deletes the two bespoke execution kinds and their `compose_*_prompt` while keeping every per-signal policy intact. *Rationale: the duplication is entirely in the fix-delivery vehicle (dispatch/targeting/no-PR-guard/completion); the signal policy is genuinely distinct per signal and collapsing it would lose idempotency precision, budgets, and diagnosis. The revision design already anticipated exactly this — "thin producers on top of it."*

---

## Design Question 2 — Provenance for engine-triggered revisions (the core modeling decision)

Neither a merge-conflict nor a CI failure is operator feedback. The revision row must record *that* and *what triggered it*, without a human in the loop.

### Options

- **(a) Extend `created_via` grammar.** Reuse the existing free-form `tasks.created_via` TEXT. Add `merge-conflict:<crz_id>` and `ci-fix:<crm_id>`. The attempt id is the back-pointer; `(repo, pr#)` is recoverable from the chain root.
- **(b) New `revision_source` enum column.** A dedicated typed column (`operator`/`comment`/`conflict`/`ci`).
- **(c) Infer from the linked attempt table.** No provenance on the revision; join to whichever side-table references it.

### Decision

**(a), mirroring Source B exactly.** `revision-tasks.md` already established `created_via` as the provenance carrier and `pr-comment:<repo>#<pr>:<cid>` as the pointer grammar; engine triggers are `merge-conflict:<crz_id>` and `ci-fix:<crm_id>`. *Rationale:* (b) doubles the discriminator and forces a migration + every-call-site update for a distinction `created_via` already encodes; (c) makes "why does this revision exist" a join and breaks for revisions whose attempt row was GC'd. `canonicalize_created_via` (already in `work.rs`) is the one place to extend. The kanban/audit/editorial-controls gate read `created_via` prefix to brand the card and decide whether to post the in-PR breadcrumb comment (engine-triggered revisions likely *suppress* the "R<n>: <desc>" comment, since the conflict/CI flows already post their own tailored PR comment — a Q for the editorial-controls gate, [`revision-tasks.md`](revision-tasks.md) OQ on T577).

### Reverse link

The attempt row gains `revision_task_id TEXT` (soft FK to `tasks.id`), set when the producer creates the revision. This is the join the trigger ledger needs to answer "did my signal get a fix vehicle, and what happened to it" — replacing today's `blocked_attempt_id`-on-the-task with a symmetric pair (`tasks.blocked_attempt_id → attempt`, `attempt.revision_task_id → revision task`).

---

## Design Question 3 — Auto-generated intent vs injected directive

A revision's `description` is the Review-lane affordance text and *must stay short* ([`revision-tasks.md`](revision-tasks.md) Naming). The conflict/CI worker needs a *long* directive (diagnosis tables, log excerpts, per-provider fetch commands, stop conditions).

### Decision

**Split the two, exactly as today's split between `tasks.description` and `compose_*_prompt`.**

- **`description` (short, generated):** one line, e.g. `Resolve merge conflict against main` or `Fix failing CI: ci/test, ci/lint` (top N check names). Drives the card and `revision_name_from_description`.
- **Directive (long, injected at dispatch):** `compose_revision_directive` grows a *provenance-aware preamble*. When `created_via` is `merge-conflict:*` / `ci-fix:*`, it looks up the linked attempt row and prepends the existing diagnosis/log/failed-checks rendering (lift the body of `compose_conflict_resolution_prompt` / `compose_ci_remediation_prompt` into directive *fragments*, keyed off the attempt). The base revision directive (fetch → edit branch → push → no new PR) is shared; the signal-specific fragment is appended. *Rationale:* the worker still gets exactly the guidance it gets today, but the "push to the existing branch, open no PR" spine is the one shared revision recipe rather than three near-identical copies. This is the only place `runner.rs` keeps signal-specific text, and it is *injection into the shared directive*, not a parallel composer.

---

## Design Question 4 — Keep the `blocked_reason` health signal?

Today the parent chore flips to `blocked: merge_conflict` / `blocked: ci_failure` while remediation runs, then snaps back to `in_review`. With remediation as a child revision in Backlog/Doing, do we still flip the parent?

### Options

- **(a) Keep the flip.** Parent shows `blocked:<reason>` (kanban "needs attention"); the revision child is the worker vehicle; retire-on-clean clears the parent block. Complementary signals.
- **(b) Drop the flip.** Rely on the presence of an in-flight engine-triggered revision card. Parent stays `in_review`.

### Decision

**(a) Keep the `blocked_reason` discriminator.** It is the operator's "this in-review PR is not actually mergeable right now" signal and is already built and cheap. Dropping it would leave a red-CI / conflicting PR looking healthy in the Review lane until someone notices the child card. The revision is the *vehicle*; `blocked_reason` is the *PR-health state*. They compose: producer flips parent `blocked:<reason>` **and** creates the revision; retire-on-clean clears the parent block; the revision rides to `done` with the parent (or lands `blocked: parent_pr_closed` if the PR closed first — the revision mechanism already handles that). *Rationale:* this preserves every externally observable P188 behaviour while swapping only the hidden worker-dispatch mechanism. *Open sub-question for the project:* whether a child revision that itself reaches `in_review` (commit pushed, awaiting re-check) should hold the parent in `blocked` until the poller re-confirms clean — today's retire-on-clean already keys off the poller, so the answer is likely "yes, the poller is the single arbiter," but state-machine interactions between the parent block and the child revision lifecycle must be enumerated explicitly (this is the riskiest correctness surface; see Risks).

---

## Design Question 5 — Idempotency: the attempt table is the trigger ledger

Revisions created from the *same* signal must not stack. The poller fires every cadence while a PR stays conflicting/red.

### Decision

**The producer consults the attempt table's UNIQUE key before creating a revision, and the create is conditional on a fresh ledger row.** Concretely: `on_conflict_detected` / `on_ci_failure_detected` keep their existing `INSERT … (UNIQUE …)` semantics; **only when the insert produces a genuinely new attempt row** (new `base_sha_at_trigger` / new `(head_sha, attempt_kind)`) does the producer call the revision-create path and stamp `attempt.revision_task_id`. A repeat probe at the same SHA hits the existing row, finds `revision_task_id` already set, and is a no-op. This keeps SHA-precise dedup (which the revision create-time guard cannot express) while making revision creation idempotent per signal. Budget (CI) and churn (conflict) are checked in the same producer step, before create — an exhausted budget flips the parent to `ci_failure_exhausted` and creates **no** revision, exactly as today.

---

## Design Question 6 — The cases that do not map cleanly

Surfacing these is half the value of the investigation.

- **CI `retrigger` is not a revision.** It produces no commit; it asks CI to re-run flaky/infra checks. A revision must produce a commit (its completion detector and `gh pr create` guard assume one). **Retrigger stays a bespoke, worker-light action** (or, better, an engine-side action that re-dispatches the check without a worker at all — out of scope here). Only `attempt_kind='fix'` maps to a revision.
- **Rebase-only conflict resolution still maps.** When `main` merely moved and there is no semantic conflict, the worker rebases and force-pushes — that *is* a new commit state on the branch, so it is revision-shaped. The "rebase-only success refunds the CI budget" rule ([`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) / reconciled CI design) stays on the ledger, not the revision.
- **`mark-failed` stop reasons and CI triage classes are ledger concepts, not revision concepts.** `obsolescence_suspected`, `product_decision_required`, `architectural_mismatch`, `tractable`/`flaky_or_infra`/`unfixable` describe *the signal's outcome*, not the revision's. They stay on the attempt row; the revision simply ends (pushed → rides parent; or no-push → its own blocked/attention path). The `boss engine conflicts|ci …` CLIs keep operating on the ledger.
- **Merge-queue rebounce** keys its attempt on a synthetic merge SHA (`before_commit_sha`) and fetches logs from there. The revision worker still pushes to the *real* head branch; the synthetic SHA is purely diagnostic context injected into the directive (Q3). No change to the targeting.

---

## Design Question 7 — Lifecycle, completion, retire-on-clean

The revision lifecycle is already defined: `in_review` when the commit is pushed (rolled up under the parent), `done` when the chain root reaches `done`, `blocked: parent_pr_closed` if the PR closes mid-flight. The remediation flows add one reconciliation:

- **Retire-on-clean clears the *parent's* `blocked_reason`** (the poller sees `Clean`/green), and marks the *ledger row* succeeded (and releases its lease, if the revision held one). The *revision task* is unaffected by retire-on-clean directly — it follows its own lifecycle: it reaches `in_review` when its commit lands, and `done` when the parent PR merges. So a successful remediation produces: parent un-blocked → `in_review`; ledger row → `succeeded`; revision → `in_review` (then `done` at parent merge). These three transitions must be ordered so the poller's clear path and the revision-completion path do not race. **Decision:** the poller remains the single arbiter (`revision-tasks.md` "do not introduce a parallel polling loop"); the revision-completion detector only advances the revision row, and the poller's clear path only touches the parent block + ledger. They write disjoint columns, so they compose without a lock beyond the existing per-row WHERE-guards.

---

## Design Question 8 — Coordination with P188 and P654; sequencing

Both P188 and P654 are **largely landed** (code + design docs exist; recent commits e.g. `8e6c520 invalidate revisions when parent PR merges mid-flight`, the conflict/CI watchers, the `ci_remediations`/`conflict_resolutions` tables). This work is a **refactor on top of both**, not parallel construction. No open PR currently touches these paths (checked at authoring time). Sequencing:

1. **Do not start until P188 and P654 have stabilized on `main`** (no open PRs mutating `conflict_watch`/`ci_watch`/the revision dispatch arm). Re-check `gh pr list` for `boss/*` branches touching `work.rs` revision/remediation code before each phase.
2. **Land additively first** (provenance grammar, `revision_task_id` column) so the bespoke flows keep working while the revision path grows the injected-directive fragments.
3. **Cut over one signal at a time** (conflict first — simpler, no budget), behind the existing `auto_pr_maintenance_enabled` gate, with the old `conflict_resolution` dispatch kept dormant for one release as a fallback, then removed.

---

## Decisions (summary)

1. **Layer, don't collapse** (Q1): revision = fix-delivery vehicle; detection subsystems = producers; attempt side-tables = trigger ledger with `revision_task_id`.
2. **Provenance via `created_via`** (Q2): add `merge-conflict:<crz_id>` and `ci-fix:<crm_id>`; one extension point (`canonicalize_created_via`).
3. **Short generated `description`, long injected directive** (Q3): `compose_revision_directive` grows a provenance-aware fragment; delete `compose_conflict_resolution_prompt` / `compose_ci_remediation_prompt` as standalone composers.
4. **Keep `blocked_reason`** (Q4): parent PR-health signal composes with the child revision vehicle.
5. **Attempt table is the idempotency ledger** (Q5): revision created only on a genuinely-new attempt row; budget/churn checked before create.
6. **Retrigger stays bespoke; ledger-only concepts stay on the ledger** (Q6).
7. **Poller remains the single arbiter** (Q7): disjoint-column writes, no new lock.
8. **Refactor-on-top, one signal at a time, gated** (Q8).

## Schema deltas

Additive only (consistent with the `migrate_*` `ALTER TABLE ADD COLUMN` family; no CHECK constraints, app-layer invariants):

- `conflict_resolutions.revision_task_id TEXT` — soft FK to `tasks.id`; the revision this attempt spawned. NULL until created.
- `ci_remediations.revision_task_id TEXT` — same.
- (no change to `tasks`: `created_via` is already free-form TEXT; `parent_task_id`, `blocked_reason`, `blocked_attempt_id` already exist.)

Removed *after* cutover (separate cleanup phase): the `conflict_resolution` and `ci_remediation` values of `work_executions.kind` and their dispatch arms; `compose_conflict_resolution_prompt` / `compose_ci_remediation_prompt`; the `revision_implementation` PreToolUse guard and warm-workspace logic are *reused*, not removed.

## Implementation phases

1. **Provenance + reverse link (additive, no behaviour change).** Extend `canonicalize_created_via` for `merge-conflict:*` / `ci-fix:*`; add `revision_task_id` columns + migrations; render engine-triggered revision chrome in the kanban projection. *Acceptance:* a revision created with `created_via='merge-conflict:<id>'` round-trips and renders with the right badge; bespoke flows untouched.
2. **Injected directive fragments.** Refactor the bodies of the two `compose_*_prompt` into directive fragments that `compose_revision_directive` appends based on `created_via` + linked attempt. *Acceptance:* a `revision_implementation` worker created with a conflict provenance receives the same diagnosis/steps text it gets today.
3. **Conflict producer cutover.** `on_conflict_detected`: on a new attempt row, create an engine-triggered revision (parent = the chore) and stamp `revision_task_id`, instead of creating a `conflict_resolution` execution. Keep flipping `blocked: merge_conflict`. Wire retire-on-clean to clear the parent block + mark ledger succeeded (revision rides its own lifecycle). Old `conflict_resolution` dispatch dormant. *Acceptance:* end-to-end on a test PR — conflict → revision spawns into the warm workspace → pushes the rebased branch → no new PR → poller sees clean → parent back to `in_review`, ledger `succeeded`, revision `in_review`; parent merge flips revision `done`. Churn guard still caps at 3/3600s.
4. **CI producer cutover.** Same for `on_ci_failure_detected` (`fix` kind only; retrigger unchanged). Budget enforced before create; exhaustion → `ci_failure_exhausted`, no revision. Rebase-only refund preserved. *Acceptance:* CI fail → revision → green; budget exhaustion still blocks; retrigger still works without a revision.
5. **Remove the dormant bespoke paths** once 3+4 prove out for a release: delete the two execution kinds, their dispatch arms, and the standalone composers.
6. *(Stretch, separate effort)* fold auto-rebase in as a fourth producer.

## Risks

- **R1 — Parent-block ↔ child-revision state-machine races** (Q4/Q7). The interaction between the parent's `blocked_reason` clear (poller) and the child revision's lifecycle (completion detector) is the riskiest surface. *Mitigation:* disjoint-column writes, poller-as-sole-arbiter, an explicit truth table of (parent status × revision status × poll result) enumerated in the seed task before coding.
- **R2 — Reversing a deliberate decision.** P188 chose side-tables over task-rows on purpose. This doc does **not** undo that — the side-tables stay as the ledger — but the implementer must resist the temptation to "finish the job" and migrate the ledger into `tasks`. The ledger's SHA-keyed idempotency and budget counters have no good home on `tasks`.
- **R3 — Coordinating with in-flight P188/P654 follow-ups.** A worker cannot see other workspaces' uncommitted work. *Mitigation:* the sequencing gate in Q8 (re-check open PRs per phase; land additively first).
- **R4 — Duplicate-PR regression.** Revisions are the sanctioned exception to one-PR-per-task; a mis-wired engine producer could spawn a revision against a task with no open PR (e.g. a chore still in Doing). *Mitigation:* the revision create-time gate (`assert_parent_revisable`) already refuses a parent without an open PR — reuse it; do not bypass it for engine producers.
- **R5 — Description bloat / Review-lane unreadability.** Auto-generated descriptions must stay one line (Q3). *Mitigation:* generate from check-names / "Resolve merge conflict", never from the diagnosis body.

## Recommendation: promote to a project

This work reverses (partially) a deliberate prior architecture decision, spans three subsystems (`merge_poller`/`conflict_watch`/`ci_watch`, the revision dispatch + `runner.rs` directive, the attempt side-tables), requires the provenance modeling decision above, and must be sequenced against two other efforts. That exceeds a single chore. **Recommend promoting "Re-use the revision mechanism for merge-conflict and CI-failure handling" to a project**, with this document as its design-seed artifact and the six phases above as its initial task breakdown. Phase 1 (additive provenance + reverse link) is independently shippable and a good first task.

## Cross-references

- [`revision-tasks.md`](revision-tasks.md) — the substrate; Source A/B producer model, `created_via` grammar, dispatch/lifecycle/guard this doc reuses.
- [`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) — P188; the side-table attempt model, opt-out gates, CLI surface, and the deliberate task-row-vs-side-table decision this doc layers onto.
- [`auto-rebase-stacked-prs.md`](auto-rebase-stacked-prs.md) — adjacent fourth producer, folded only as a stretch phase.
- [`chore-lifecycle-pr-closed-unmerged.md`](chore-lifecycle-pr-closed-unmerged.md) — the three-state poller probe shape reused by detection.
- [`work-taxonomy.md`](work-taxonomy.md) / [`design-producing-tasks.md`](design-producing-tasks.md) — `kind` discriminator precedent.
