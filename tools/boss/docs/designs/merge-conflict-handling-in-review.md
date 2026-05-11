# Boss: Merge-Conflict Handling for In-Review PRs

## Problem

The kanban's `Review` column is supposed to mean "this PR is settled state — the worker is done, the human is the bottleneck." Once a worker pushes its branch, opens the PR, and exits, the chore sits in `in_review` until the merge poller observes the merge (`merge_poller.rs:122`) and flips it to `done`. The contract is *the engine watches; the human reviews.*

That contract quietly breaks the moment `main` moves under the open PR's feet. Some unrelated PR lands that touches a file the in-review PR also touches; GitHub re-evaluates the PR and reports `mergeable = CONFLICTING`. The reviewer clicks the merge button, sees the red "this branch has conflicts that must be resolved" banner, and bounces — the PR is no longer review-ready. Today nothing in Boss notices. The chore stays in `in_review`, the kanban happily shows a clean Review card, the merge poller's `state, mergedAt` probe says "still OPEN, not merged" forever, and the user is the one who eventually pings the worker to come back and rebase.

This is the close cousin of [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md), but the trigger and idempotency model are different:

- **Auto-rebase fires on `base PR merged`.** It scans `gh pr list --base feat-A` for dependents and rebases each onto `main`. The trigger is a specific upstream event the engine *sees*.
- **This design fires on `the in-review PR is no longer mergeable against its current base`.** The trigger isn't tied to a particular upstream merge — `main` may have moved several times, possibly through PRs Boss never tracked (manual `gh` merges, web UI clicks, external contributors). The signal is GitHub's own evaluation of the diff.

The two flows converge on the same *action*: lease a workspace, run `jj rebase`, push the result. They diverge on *who notices the need* and on *whether one work-item-shaped record should represent the rebase request* (auto-rebase: yes, a `rebase_attempts` row plus possibly an escalated chore for conflicts; this design: see Q3).

The project description anticipates an **ephemeral work item** concept: a record that is auto-created when the conflict appears, auto-completed when the PR is mergeable again, and tied to a parent. The challenge — explicit in the project framing — is to design it so it doesn't conflate with the existing chore / task / project_task taxonomy. Tasks are *planned* work. Chores are *standalone* work. Project_tasks are *ordered, design-driven* work. None of those describe "the engine spotted that PR #243 stopped being mergeable, spun up a worker for 90 seconds, then made the symptom go away." Making that a `tasks` row pollutes the kanban, the dispatcher's pickup query, and the `boss task list` surface with engine plumbing that the human doesn't manage.

This doc proposes a shape parallel to `rebase_attempts`: a side table `conflict_resolutions` that records the engine's *attempts* to clear conflicts on an in-review PR, with full worker-spawn handoff for the conflict-resolution turn, and tight coordination with the existing `auto-rebase-stacked-prs` and the in-flight `review-feedback` projects (`proj_18ad7d43affb0370_2a`). The parent chore / project_task gets a new `blocked_reason = 'merge_conflict'` state plus a pointer to the active attempt, so the human sees *why* the card is suddenly in Doing-with-blocked-badge and can drill in. Auto-completion is the engine's job; the user is told *after the fact* by an activity-feed entry, not interrupted while the engine is still trying.

## Goals

- Detect when an in-review PR becomes unmergeable (conflict against `main`) automatically, on the same poll cadence as merge detection. No new background loop.
- Flip the parent work item from `in_review → blocked` with `blocked_reason = 'merge_conflict'`, so the kanban and inspector make the reason for blocking visible without overloading the dependency-graph blocked semantics.
- Auto-spawn a worker to resolve the conflict, with the workspace pre-leased and the PR's conflict diagnosis pre-loaded into the spawn prompt — same playbook the auto-rebase escalation path already uses.
- Once the worker pushes and GitHub re-evaluates the PR as `mergeable = MERGEABLE`, retire the conflict-resolution attempt, flip the parent back to `in_review` automatically, and emit an activity-feed entry. The human's only interruption is "by the way, while you were away, the engine cleared a conflict on this PR."
- Stay coherent with the in-flight [review-feedback workflow](https://github.com/spinyfin/mono/issues?q=proj_18ad7d43affb0370_2a) (`proj_18ad7d43affb0370_2a`). Both flows are "an `in_review` item temporarily leaves its settled state and snaps back." They should share the `blocked_reason` enum, the parent-side state shape, and the engine activity feed; they differ only in trigger and worker prompt.
- Stay coherent with [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md). When a base PR merges and the dependent develops a conflict, auto-rebase's escalation path should *not* also trigger this design's attempt — they would collide. See Q7.
- Make ephemeral *operationally invisible* (no kanban card for the resolve-conflicts turn) but *operationally inspectable* (activity feed entry, attempt record retained for history, retry verb).

## Non-Goals

- **Resolving conflicts the engine can't push for.** PRs from a fork (`headRepositoryOwner != owner`) are out, same exclusion as [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md) Q2. The engine can't force-push to a fork branch.
- **Detecting non-conflict review-readiness regressions.** CI going red after a passing review is not a `blocked: merge_conflict`. It's an unrelated signal (often handled by the user re-pushing or asking the worker to look). This design narrowly handles the *git-state-conflicting-with-base* case.
- **Pre-merge mechanical-vs-conflict split.** In `auto-rebase-stacked-prs.md` the engine first attempts a *mechanical* in-engine rebase and only escalates to a worker on conflict. That makes sense when the trigger is "I know main moved because A merged" and the upstream diff is small. Here the trigger is *already* "conflict exists" — GitHub has computed that no straight three-way merge succeeds. There is no mechanical path left; we go straight to the worker. (See Q4 for the edge case where main has moved again *during* our resolution and the conflict has self-cleared — we *do* re-probe and skip in that case.)
- **Conflicts inside the *base* branch.** A PR based on `feat-X` (a stacked PR) developing a conflict against `feat-X` is auto-rebase-stacked-prs' territory once `feat-X` merges. Until `feat-X` merges, GitHub considers the dependent mergeable-vs-its-own-base regardless of what's happening on `main`, so this design's probe doesn't trip.
- **Aggregating multiple conflict episodes into one attempt.** If the worker resolves the conflict, pushes, then `main` moves *again* an hour later and produces a new conflict, that's a second attempt. The history is "this PR conflicted twice."
- **Auto-merging once the conflict is resolved.** Auto-merge is explicitly deferred elsewhere in the codebase; we restore the PR to `in_review` and leave the merge button to the human.
- **Cross-product / cross-repo PRs.** Same exclusion as `auto-rebase-stacked-prs.md`. Boss's merge poller operates per-product; this design extends the same per-product probe and stays within it.
- **Watching GitHub branch-protection settings to predict whether the engine's force-push will dismiss approvals.** Same answer as `auto-rebase-stacked-prs.md` Q9: it will; we post a comment explaining it. No prediction logic.

## Naming

- The new module is **`conflict_watch`** (`engine/src/conflict_watch.rs`). It owns the detection-trigger pipeline. The actual rebase work is done by a worker via the existing spawn flow; the module orchestrates detection, attempt creation, handoff, and retirement.
- The unit of work it tracks is a **conflict resolution** (the action) recorded as a **conflict-resolution attempt** (the row). Status values: `pending`, `running`, `succeeded`, `superseded`, `failed`, `abandoned`.
- The persisted record (Q3) is the **`conflict_resolutions`** row. It is *not* a work item — see Q3 for why; it lives in its own table, alongside `rebase_attempts`. Both are "engine-tracked attempts at a mechanical correction"; they share an activity-feed surface.
- The new column on `tasks` is **`blocked_reason TEXT`**. Values are an open-ended set (initially `'merge_conflict'`; the review-feedback project adds `'review_feedback'`; the existing dependency-graph blocked path adopts `'dependency'`). `NULL` for chores blocked by the original opaque reason (rows pre-migration; see Q9).
- The new column on `tasks` is **`blocked_attempt_id TEXT`**, a soft FK to either `conflict_resolutions.id` or (future) the review-feedback attempt's id. Discriminated by `blocked_reason`. Allows the inspector to one-click "show me the attempt that's holding this card."
- The parent work item is the **target PR** or **target chore** (the in-review item whose PR conflicted). The resolution worker is referred to as the **resolution worker** to distinguish from the original chore's worker (which has long since exited).
- The CLI surface verbs are **`boss engine conflicts list`** / **`boss engine conflicts show <id>`** / **`boss engine conflicts retry <id>`**, parallel to `boss engine rebase-attempts list ...` from `auto-rebase-stacked-prs.md` Q7. A unified `boss engine attempts ...` verb that lists both is an obvious follow-up once a second attempt-shaped surface exists; see Q11.

---

## Design Question 1 — Detection Trigger and Signal

### What `gh pr view` returns that the merge poller doesn't use today

The merge poller currently queries `--json state,mergedAt` (`merge_poller.rs:66-115`). GitHub's PR JSON includes two more fields that answer "is this PR mergeable?":

- **`mergeable`**: `MERGEABLE | CONFLICTING | UNKNOWN`. The mergeability of the head ref into the base ref *as currently computed by GitHub*. `UNKNOWN` is the transient state while GitHub recomputes after a base move; eventually it resolves to `MERGEABLE` or `CONFLICTING`.
- **`mergeStateStatus`**: `CLEAN | BLOCKED | BEHIND | HAS_HOOKS | DIRTY | UNKNOWN | UNSTABLE | DRAFT`. A finer breakdown of merge-readiness. The two values relevant here are `DIRTY` (= conflicts) and `BEHIND` (= the head ref is behind the base; mergeable but a rebase is recommended). `BEHIND` is *not* a conflict — GitHub still considers the PR mergeable via merge commit — and we explicitly do not act on it (rebasing-because-behind is a different feature, not in scope).

The two fields are correlated but `mergeable=CONFLICTING` is the load-bearing signal. `mergeStateStatus=DIRTY` is the more precise variant of the same answer; we read both and require them to agree before acting.

### Options

- **(a) New polling loop.** A separate background task that periodically lists open PRs and re-probes mergeability. Independent of `merge_poller`.
- **(b) Extend the merge poller's existing probe.** Add `mergeable,mergeStateStatus` to the `--json` projection that `merge_poller::sweep_one` already runs. Add a new branch alongside the merge-detection branch.
- **(c) Subscribe to GitHub webhooks.** Push-based. Same problem as everywhere else: the home-machine engine isn't internet-reachable.

### Recommendation

**Pick (b).** Reuses the same `gh pr view` round-trip per pending-merge-check candidate — the poller is already querying every `in_review`-with-`pr_url` chore at its cadence, and the marginal cost of two extra JSON fields is zero (GitHub returns them whether we ask or not). Extending the probe to a three-state output is the same shape `chore-lifecycle-pr-closed-unmerged.md` Q2 already proposed for the close-unmerged variant.

Concretely, evolve `PrMergeState` (and the rename it picked up via `chore-lifecycle-pr-closed-unmerged.md` to `PrLifecycleProbe`) one step further:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrMergeability),
    Merged,
    ClosedUnmerged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPrMergeability {
    /// `mergeable=MERGEABLE` (or `=UNKNOWN` — treat as mergeable until
    /// GitHub disagrees; we don't act on UNKNOWN).
    Clean,
    /// `mergeable=CONFLICTING` *and* `mergeStateStatus=DIRTY`.
    Conflict,
}
```

The `--json` query gains `mergeable,mergeStateStatus`:

```rust
"--json", "state,mergedAt,closedAt,mergeable,mergeStateStatus",
"--jq",   r#"[
                (.state // ""),
                (.mergedAt // ""),
                (.closedAt // ""),
                (.mergeable // ""),
                (.mergeStateStatus // "")
              ] | @tsv"#,
```

`sweep_one` becomes a three-way dispatch (composing with `chore-lifecycle-pr-closed-unmerged.md` Q2):

```rust
match probe.state {
    PrLifecycleState::Open(OpenPrMergeability::Clean)    => maybe_clear_blocked(...).await,
    PrLifecycleState::Open(OpenPrMergeability::Conflict) => mark_blocked_merge_conflict(...).await,
    PrLifecycleState::Merged                             => mark_merged(...).await,
    PrLifecycleState::ClosedUnmerged                     => mark_closed_unmerged(...).await,
}
```

`maybe_clear_blocked` is the symmetric path: a chore currently `blocked` with `blocked_reason='merge_conflict'` whose PR is back to mergeable should flip back to `in_review`. See Q5.

### Why not (a)

A second poller hitting `gh` on its own cadence doubles the request rate to GitHub for the same answers. The merge poller already iterates `list_chores_pending_merge_check` candidates; the conflict probe rides on the same loop. There's no useful difference in cadence between "is this PR merged?" and "is this PR mergeable?" — they answer the same time-resolution question.

### Why not (c)

Same answer as the other PR-state designs: home-machine engine isn't internet-reachable. If/when a hosted engine exists, webhooks are a strict latency upgrade; the handler signature here is identical.

### Probe-pool extension

`list_chores_pending_merge_check` (`work.rs:2021`) currently returns chores in `status='in_review' AND pr_url IS NOT NULL`. We need to also probe chores in `status='blocked' AND blocked_reason='merge_conflict'` (to detect resolution). Add a parallel `list_chores_blocked_on_merge_conflict()` query and call both per sweep:

```rust
let in_review_candidates  = work_db.list_chores_pending_merge_check()?;
let blocked_conflict      = work_db.list_chores_blocked_on_merge_conflict()?;
for c in in_review_candidates.iter().chain(blocked_conflict.iter()) {
    sweep_one(work_db, probe, publisher, conflict_watch, c).await;
}
```

`list_chores_blocked_on_merge_conflict` is a small new method; SQL is the natural `WHERE status='blocked' AND blocked_reason='merge_conflict' AND pr_url IS NOT NULL`. The duplicate-iteration cost is bounded by the small N of blocked chores (typically ≤ a handful at any moment).

### Transient `UNKNOWN` and flapping

`mergeable=UNKNOWN` is GitHub's "I'm still computing" state. Common timing: a base merge happens, GitHub asynchronously re-evaluates open PRs against the new base, and the eval takes seconds to minutes. We must not flap a chore between `in_review` and `blocked` while GitHub is mid-recompute.

Recommended: treat `UNKNOWN` as Clean for the purposes of this design — i.e. do *not* trigger a conflict-resolution attempt on `UNKNOWN`. The poller will re-probe on its next pass; if mergeability resolves to `CONFLICTING`, the next sweep catches it. The cost of waiting one poller interval (~30s) for `UNKNOWN → CONFLICTING` is preferable to false-positive blocked transitions during normal base-merge churn.

To mitigate the very-rare case where `mergeable` is stuck at `UNKNOWN` for an extended period (sometimes GitHub gets confused), the engine logs `tracing::warn!(pr_url=…, "PR mergeable=UNKNOWN for >5 minutes; will not auto-flag conflict")` after the third consecutive UNKNOWN probe. This is observability, not action — we still wait for a definitive `CONFLICTING`.

---

## Design Question 2 — Parent State Model

### What `blocked` already means

The `tasks.status='blocked'` state today is set by the dependency-graph machinery (`work.rs:3450-3478` and `work-dependencies.md` Q4): when a prereq is non-terminal, the dependent flips to `blocked` and `last_status_actor='engine'`. The state is *uniform* — the only way to know *why* a chore is blocked is to inspect the work-dependencies graph and find a non-`done` prereq. There's no `blocked_reason` discriminator today.

This design needs to add `merge_conflict` as a second cause of `blocked`. The review-feedback project will add a third (`review_feedback`). And the dependency-graph case is implicitly the fourth (`dependency`). Without a discriminator, the UI has no way to render different "why" badges for each, and the engine has no way to know whose retirement logic owns a given blocked row.

### Options

- **(A) Reuse the dependency-graph mechanism.** Model the conflict-resolution attempt as a phantom prereq: insert a fake `work_dependencies` row with the attempt as the prereq, flip the parent to `blocked` via the existing `auto_block_on_prereq` path. When the attempt resolves, mark the phantom prereq `done`, the dependency-graph auto-unblock path flips the parent back. No new column.
- **(B) Add `blocked_reason TEXT` on `tasks`.** Open-ended string. The conflict-resolution path sets it to `'merge_conflict'`; the dep-graph path adopts `'dependency'`; review-feedback adopts `'review_feedback'`. The unblock side-effect is owned by whichever subsystem set the reason.
- **(C) Add a `blocked_by_attempt_id` FK column alongside the existing dep-graph blocking.** No new reason field; the presence of an attempt id is the discriminator. Dep-graph blocking has the FK as NULL.

### Discussion

**(A) is conceptually clean** — "the resolve-conflicts attempt is a prereq of the parent's merge" is a true statement — but the implementation is awful. The dependency-graph machinery is wired for `tasks` rows on both sides; making it accept a `conflict_resolutions` row as a prereq either requires a polymorphic prereq table (big refactor) or creating a *fake* `tasks` row for each attempt (the exact taxonomy pollution this design is supposed to avoid). The kanban and dispatcher would have to learn to filter out the fake rows everywhere. No.

**(C) is the smallest schema change** but conflates two pieces of information into one column. "Is this chore blocked because of an attempt?" is a yes/no the FK answers; "what kind of attempt?" is a separate question the FK doesn't answer without a join. For the inspector to render "Blocked: merge conflict (resolution worker running)" vs "Blocked: review feedback (response worker running)", it'd have to join through whichever table the FK points at — and there's no way to know which table without an explicit type tag. Three different attempt subsystems (conflict, review-feedback, future) means three speculative joins per render. Bad.

**(B) cleanly separates** "why blocked" from "what attempt is fixing it." `blocked_reason` is the typed discriminator; `blocked_attempt_id` (added separately, see below) is the optional pointer to the row that's doing the fixing. For dep-graph-driven blocking, `blocked_reason='dependency'` and `blocked_attempt_id=NULL` (the prereqs themselves are queried via `work_dependencies`). For conflict resolution, `blocked_reason='merge_conflict'` and `blocked_attempt_id` points at the `conflict_resolutions` row. For review feedback, `blocked_reason='review_feedback'` and the attempt id points at whatever that project's table ends up being.

The "smaller column on tasks" trade-off becomes "two columns instead of one," but each column has a single, clear meaning. Both nullable; both default to NULL.

### Recommendation

**Pick (B).** Two new columns:

```sql
ALTER TABLE tasks ADD COLUMN blocked_reason     TEXT;  -- NULL OK; values: 'dependency' | 'merge_conflict' | 'review_feedback'
ALTER TABLE tasks ADD COLUMN blocked_attempt_id TEXT;  -- NULL OK; soft FK, discriminated by blocked_reason
```

Whenever the dep-graph machinery flips a row to `blocked`, it also writes `blocked_reason='dependency'`. The migration backfills existing `blocked` rows by joining against `work_dependencies` (any blocked row with a non-`done` prereq gets `'dependency'`; any blocked row without is left NULL as legacy).

For this design's path: when the engine flips a chore from `in_review` to `blocked` for a conflict, it writes:

```sql
UPDATE tasks SET
    status            = 'blocked',
    blocked_reason    = 'merge_conflict',
    blocked_attempt_id = ?conflict_resolution_id,
    last_status_actor = 'engine',
    updated_at        = ?now
WHERE id = ?id AND status = 'in_review' AND pr_url = ?pr_url;
```

The guarded WHERE clause is the idempotency lock — the same probe sweep firing twice for the same `CONFLICTING` state finds the row already in `blocked` and the UPDATE matches zero rows.

### Soft-FK rationale

`blocked_attempt_id` is a soft FK (no `REFERENCES` constraint) because the target table differs by `blocked_reason`. SQLite supports CHECK constraints that could enforce the (reason, target table) pair, but it would require an INSTEAD-OF trigger to do the right thing; not worth the complexity for v1. Run-time validation in `WorkDb` is sufficient.

### Why not a sub-status

We considered modelling this as `status='blocked:merge_conflict'` (colon-separated). It's compact but every consumer of `tasks.status` would have to learn the parser. The kanban-column projection in `work-kanban.md` already maps `blocked → Doing`; a multi-tier status would force the projection to learn the prefix. A clean column is friendlier.

### Kanban presentation

The kanban projection rule from [`work-kanban.md`](work-kanban.md#status-mapping) is `blocked → Doing` with a blocked badge. This design preserves that — a `blocked: merge_conflict` chore still appears in `Doing`, still surfaces as blocked, but the badge text reads "merge conflict" rather than the generic "blocked." The frontend reads `blocked_reason` from the protocol's `Task` row and renders the badge accordingly:

```text
┌────────────────────────────────────────┐
│ 🔧 engine-app rpc: bind to socket      │
│ chore_18ad…f3                          │
│ ────────────────────────────────────── │
│ 🟧 blocked: merge conflict             │
│    resolution worker running           │
│ feat-engine-app-rpc · in chore         │
└────────────────────────────────────────┘
```

The badge's secondary line is presence-driven: if a resolution worker is `running`, show "resolution worker running"; if it's `pending` (workspace lease unavailable), show "queued"; if `failed`, show "needs attention" (the failure surfacing path, Q6).

A subtle but important point: the parent card *does not get a separate card for the resolution attempt*. The attempt is invisible on the kanban. The user sees the parent card with a richer badge; clicking it opens the inspector, which shows the attempt's state inline. This is the operational shape of "ephemeral" in this design — the side-record exists, is inspectable, but does not produce its own kanban card.

### Engine status actor

The `last_status_actor` convention (from `work-dependencies.md` Q4) is `'engine'` when the engine flips the row and `'human'` when the user does. Conflict-resolution writes `'engine'`. This keeps the existing rule — *the engine can only undo engine moves; human moves are sticky* — intact: a human who manually moves a `blocked: merge_conflict` chore to `active` is asserting authority, and the engine's resolution path will refuse to silently flip it back to `in_review` on a successful resolution (it'll log the divergence; see Q6).

---

## Design Question 3 — The Ephemeral Concept

### Restate the requirement

The project description names an "ephemeral 'resolve conflicts' work item" that the engine auto-creates and auto-completes. The framing is *carefully* — "design it so it doesn't conflate with the existing chore/task/project_task taxonomy."

### Options

- **(α) New `tasks.kind = 'resolve_conflict'`** alongside `chore`, `project_task`. Hidden from the kanban by a status-filter or kind-filter. Reuses the worker spawn flow via the normal kind-aware dispatcher.
- **(β) New side table `conflict_resolutions`** (parallel to `rebase_attempts` from `auto-rebase-stacked-prs.md` Q3). NOT a work item. Worker spawn uses a "pre-leased" path the auto-rebase escalation already proposes. No kanban presence.
- **(γ) A flag `tasks.is_resolving = TRUE`** on the parent itself. No new row at all — the parent just temporarily owns the worker. State: the parent has both an open PR *and* an active worker, which today is not a legal combination.

### Discussion

**(α) is the kind-extension shape this user already considered for `system` work in `auto-rebase-stacked-prs.md` Q3.** That doc explicitly rejected it for v1 ("widens the taxonomy and forces every kanban / boss task list consumer to learn a `system` filter") and recommended the side-table shape (D). The argument is the same here. Even with kind-aware filtering, every consumer of `tasks` rows — kanban projection, `boss task list`, work-dependencies graph, project rollups — has to learn to ignore `resolve_conflict` rows. The footprint is broad, and the failure mode "forgot to filter" is silent. A small dedicated kind also doesn't give us the *attempt-history* semantics naturally; a chore has `status` (kanban lane), `pr_url` (the produced PR), `priority` — none of which map cleanly to "this is a single rebase attempt with an outcome." A `pr_url` on a resolve_conflict task points where? At the parent's PR? Then `boss task list --status in_review` returns the resolve_conflict rows when their PR is the parent's. Confusing.

**(γ) collapses the data model** but the unstated invariant "an in-review chore has no active worker" is load-bearing in several places (the kanban-doing column has a contract about live workers, `merge_poller`'s pickup query assumes no worker is touching `pr_url` mid-flight, and the dispatch rehydrate on startup assumes `in_review` chores aren't dispatch candidates). Breaking that invariant to hold one extra bit on the parent is more disruption than a side table costs.

**(β) is the natural shape.** A `conflict_resolutions` row has its own lifecycle, its own status enum, its own foreign key into the parent's `tasks.id`, and its own lease/workspace pointer. It does not appear in any `tasks` query. It surfaces via the parent's `blocked_attempt_id` (Q2) and via the activity feed / `boss engine conflicts list`. When the attempt succeeds, the row stays (it's history); when it fails, the row stays (the human inspects it). The taxonomy stays clean.

### Recommendation

**Pick (β).** Mirror the `rebase_attempts` shape from `auto-rebase-stacked-prs.md` Q3.

```sql
CREATE TABLE IF NOT EXISTS conflict_resolutions (
    id                  TEXT PRIMARY KEY,         -- 'conflict_<ulid>'
    product_id          TEXT NOT NULL,
    work_item_id        TEXT NOT NULL,            -- FK to tasks.id (the in-review chore / project_task)
    pr_url              TEXT NOT NULL,            -- snapshot of tasks.pr_url at trigger time
    pr_number           INTEGER NOT NULL,
    head_branch         TEXT NOT NULL,
    base_branch         TEXT NOT NULL,            -- the PR's base ref (usually 'main')
    base_sha_at_trigger TEXT,                     -- sha of base ref when conflict was first observed
    head_sha_before     TEXT,                     -- head ref sha before the resolution worker pushes
    head_sha_after      TEXT,                     -- post-push, NULL on failure
    status              TEXT NOT NULL,            -- 'pending'|'running'|'succeeded'|'superseded'|'failed'|'abandoned'
    failure_reason      TEXT,                     -- non-null when status='failed'
    cube_lease_id       TEXT,                     -- lease the resolution worker holds
    cube_workspace_id   TEXT,
    worker_id           TEXT,                     -- agent id of the resolution worker, when running
    conflict_diagnosis  TEXT,                     -- structured JSON, mirrors auto-rebase's diagnosis collector
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    UNIQUE (work_item_id, base_sha_at_trigger)    -- idempotency, see Q6
);

CREATE INDEX conflict_resolutions_status_idx       ON conflict_resolutions(status);
CREATE INDEX conflict_resolutions_work_item_idx    ON conflict_resolutions(work_item_id);
CREATE INDEX conflict_resolutions_product_idx      ON conflict_resolutions(product_id);
```

Bump `metadata.schema_version`.

### Why the unique key is `(work_item_id, base_sha_at_trigger)` and not `(work_item_id, pr_url)`

`auto-rebase-stacked-prs.md` uses `(dependent_pr_url, base_pr_url)` because the upstream PR is what makes each attempt unique — same dependent + same base-PR-merge = same attempt. Here, the trigger isn't a specific upstream merge; it's "main moved and now we conflict." If `main` moves twice (two different conflict-producing events) on the same PR, those should be two separate attempts. The discriminator is the base ref's sha at the moment we observed the conflict. `base_sha_at_trigger` is captured from `gh pr view --json baseRefOid` during the probe.

### Why the row is retained after success

When status flips to `succeeded`, we don't delete the row. It's the history record. `boss engine conflicts list --status succeeded` shows "the engine cleared a conflict on PR #243 on 2026-05-11." A future activity-feed view aggregates these. Hard-deleting them would erase that signal. (Soft-deletion via a `deleted_at` column is an option if storage growth ever matters, but the volume is tiny.)

### What about chores that never reach this state

The vast majority of `in_review` chores never see a conflict — they merge cleanly. They have no `conflict_resolutions` row. The table is sparse by design; it grows only when `main` happens to step on an in-review PR's toes.

### Relationship to `rebase_attempts`

Two side tables, similar shape, different trigger. They can coexist in v1. The unification angle:

- `rebase_attempts` is "we noticed PR A merged; we tried to rebase its dependents."
- `conflict_resolutions` is "we noticed PR X conflicts with main; we tried to resolve."

Both record one rebase attempt. The natural follow-up is a single `engine_attempts` table with a discriminator column once we have a third such subsystem. v1 stays with two tables to avoid premature generalisation; the unification is a one-shot schema migration when it becomes necessary. The CLI surface `boss engine attempts ...` is the user-facing convergence (Q11).

---

## Design Question 4 — Worker Spawn and the Conflict-Resolution Playbook

### The handoff

When `conflict_watch::on_conflict_detected(work_item_id, pr_url)` fires, the engine must:

1. **Compute base_sha_at_trigger.** One `gh pr view <url> --json baseRefOid,headRefName,headRefOid` call gets the base ref sha and the head branch. Cheap.
2. **Insert a `conflict_resolutions` row** with `status='pending'`. The unique key on `(work_item_id, base_sha_at_trigger)` is the idempotency guard — a second probe for the same `(item, base_sha)` finds the row already pending and the INSERT fails. Treat that as "an attempt is already in flight; do nothing."
3. **Update the parent `tasks` row** atomically with the INSERT — flip `status='blocked'`, `blocked_reason='merge_conflict'`, `blocked_attempt_id=<new id>`. (The atomic update is via the same SQLite transaction; the INSERT is `INSERT OR IGNORE` and the UPDATE is the WHERE-guarded one from Q2.)
4. **Broadcast** a `WorkItemChanged` plus a new `FrontendEvent::ConflictResolutionStarted` typed event (Q8).
5. **Enqueue a worker spawn request** through the `ExecutionCoordinator`. The execution kind is a new `conflict_resolution` (under `work-execution.md`'s `kind` taxonomy). The coordinator handles pool capacity and lease acquisition.
6. **When the coordinator picks up the request and acquires a lease**, the engine updates the `conflict_resolutions` row's `cube_lease_id`, `cube_workspace_id`, `worker_id`; flips status `pending → running`; spawns the worker pane with a tailored prompt (below).
7. **Before spawning**, the engine runs the conflict-diagnosis collector — same code path as `auto-rebase-stacked-prs.md` Q11 — and stores the result in `conflict_resolutions.conflict_diagnosis` (JSON). The worker prompt reads from this.

### Worker prompt template

```
## Conflict resolution: PR #243 has merge conflicts against `main`

**PR**: <pr_url>
**Branch**: `<head_branch>` based off `main`
**Base sha at conflict detection**: `<base_sha_at_trigger>` (current `main` may be ahead)
**Workspace**: fresh lease on `<head_branch>`. The conflict has NOT been pre-loaded;
your first step is to fetch and rebase.

This PR was in code review when `main` moved under it. The PR's diff against
the current `main` does not apply cleanly. Your job is to resolve the conflicts,
push the resolved branch, and stop. **You are not adding new work to this PR.**

### Steps

1. `jj git fetch`
2. `jj edit <head_branch>`
3. `jj rebase -d main -b <head_branch>`
4. If the rebase reports a conflict:
   - Inspect with `jj st`, `jj resolve --list <file>`.
   - Resolve each conflict. Read the conflict diagnosis below for what was
     touched on the `main` side.
   - For the project's tests, run `<test_command>` if configured. If green,
     continue; if red and the failure is rebase-induced, fix it. If red and
     the failure was pre-existing, stop and surface it.
5. `jj git push --bookmark <head_branch>`
6. `gh pr comment <pr_number> --body "<post-resolution comment template>"`
7. Stop. Do not change the PR base, do not change the PR title or description,
   do not push new commits beyond the resolved rebase.

### Conflict diagnosis (from the engine's pre-spawn pass)

<conflict_diagnosis rendered as markdown — file list, per-file shape, upstream
 footprint, dependent commits in the rebased range>

### Stop conditions

Same three patterns as auto-rebase escalation (see `auto-rebase-stacked-prs.md` Q11):

1. **Semantic obsolescence** — the upstream change accomplished what this PR
   was trying to do. Comment on the PR explaining; do NOT push; ask the user
   whether to close the PR. (The engine flips the attempt to `failed` with
   `failure_reason = 'obsolescence_suspected'` and surfaces it.)
2. **Product decision required** — the conflict needs a human choice between
   two valid resolutions. Comment, ask, do not push. Engine flips attempt to
   `failed` with `failure_reason = 'product_decision_required'`.
3. **Architectural mismatch** — the upstream removed an abstraction this PR
   was extending. Comment, ask, do not push. Engine flips attempt to `failed`
   with `failure_reason = 'architectural_mismatch'`.

Do NOT close the PR yourself. Closing is the human's call.

### Post-resolution PR comment template

🤖 boss resolved merge conflicts on this PR after `main` moved.

Resolutions:
- <per-file resolution summary>

`<test_command>` is green. Branch force-pushed; per branch protection, prior
approvals have been dismissed. Re-review when ready.

Original head: <head_sha_before>; new head: <head_sha_after>.
```

### Why no pre-load of the conflict into the workspace

`auto-rebase-stacked-prs.md` Q5 hands off a pre-loaded conflict (engine ran `jj rebase`, conflict markers are already in the working copy, worker continues from there). That makes sense when the engine has *also* attempted a mechanical rebase. Here, the engine never attempted one — we go straight to the worker — so there's no pre-load to hand off. A fresh lease with the branch checked out is cheaper and lets the worker control the rebase strategy (e.g. `jj rebase -s` vs `jj rebase -b` for split-commit cases).

A future optimisation could pre-run the rebase in the engine just to populate the diagnosis collector with a real conflict tree, then hand it off pre-loaded. v1 skips that — the diagnosis collector can also run against a not-yet-rebased state by computing the would-be conflicts via `git merge-tree`, which is what `auto-rebase-stacked-prs.md` Q11 already does. Reuse that code path.

### Reuse of `auto-rebase-stacked-prs.md` infrastructure

This design intentionally reuses everything `auto-rebase-stacked-prs.md` builds:

- **Conflict-diagnosis collector** (Q11) — verbatim. Runs against the post-merge head sha vs the current `main`. Output is the JSON in `conflict_resolutions.conflict_diagnosis`.
- **Pre-leased spawn path** in `spawn_flow.rs` — the same path the auto-rebase escalation uses to hand a worker an existing lease. The variant difference is the prompt template and the `execution_kind`.
- **Force-push semantics** — same `jj git push --bookmark` shape.
- **Post-resolution PR comment template** — adapted (different first line: "after `main` moved" vs "after #238 merged"; otherwise identical).

If `auto-rebase-stacked-prs.md` is implemented first (and it is, in the merge-poller integration), this design is a thin addition: one new probe field, one new detection branch, one new attempt table, one new execution kind, one new spawn-prompt template, one new auto-retire path.

### Worker authority

Same answer as `auto-rebase-stacked-prs.md` Q5: the engine and the worker share the user's `gh` config (single-user, multi-persona). The resolution worker may be a different agent persona from the original PR author; same token has push rights to the same branch. No auth changes for v1.

---

## Design Question 5 — Auto-Retire and Re-In-Review

### The retire signal

After the worker pushes, GitHub re-evaluates the PR. On the next merge-poller sweep, the probe returns `state=OPEN, mergeable=MERGEABLE, mergeStateStatus=CLEAN`. That's the retire signal.

(Possible transient: `mergeable=UNKNOWN` immediately after the push while GitHub recomputes. As in Q1, treat `UNKNOWN` as not-yet-resolved — wait for definitive `MERGEABLE`.)

### What "retire" means

1. **Update the parent** atomically:
   ```sql
   UPDATE tasks SET
       status            = 'in_review',
       blocked_reason    = NULL,
       blocked_attempt_id = NULL,
       last_status_actor = 'engine',
       updated_at        = ?now
   WHERE id = ?id
     AND status = 'blocked'
     AND blocked_reason = 'merge_conflict'
     AND blocked_attempt_id = ?attempt_id;
   ```
   The fully-qualified WHERE clause ensures we only flip back rows we *put* in this state. A human who manually moved the chore elsewhere during the resolution stays where they put it; the engine logs the divergence and abandons the attempt without forcing.

2. **Update the attempt row**:
   ```sql
   UPDATE conflict_resolutions SET
       status         = 'succeeded',
       head_sha_after = ?new_head_sha,
       finished_at    = ?now
   WHERE id = ?attempt_id AND status = 'running';
   ```

3. **Release the workspace lease** (the resolution worker has already exited; the engine holds the lease metadata for the post-mortem; on `succeeded`, we explicitly release via `cube workspace release --lease <id>`).

4. **Broadcast** the typed event `FrontendEvent::ConflictResolutionSucceeded { product_id, work_item_id, attempt_id, pr_url }` and a generic `WorkItemChanged`.

5. **Activity-feed entry**: "Engine cleared a merge conflict on PR #243; PR is back in review."

### What if the worker pushes but the PR still isn't mergeable

Two sub-cases:

- **The push succeeded but the resolved branch still conflicts.** Rare but possible if the worker resolved against an older `main` sha (e.g. `main` moved again during the rebase). The next probe returns `CONFLICTING` again with a new `base_sha`. The current attempt is `running`; the engine compares its `base_sha_at_trigger` to the new `base_sha`, sees they differ, and either (a) marks the current attempt `superseded` and creates a new one, or (b) waits for the worker to push again. (a) is the cleaner shape — the worker has exited; (b) requires the worker to know to retry, which it doesn't. Use (a).
- **The worker exited without pushing** (stop-condition fired, see Q4). The attempt is still `running` from the engine's view because nothing told it the worker is done. The completion path (`completion.rs`'s on-Stop handler) needs a new branch: when a worker exits and was bound to a `conflict_resolutions` row, examine the row's expected outcome. If `head_sha_after IS NULL` (no push happened), flip to `failed` with whatever reason the worker recorded via the chore-comment mechanism, or `failed` with `failure_reason='no_push_no_stop_condition'` as the catch-all. The activity-feed entry surfaces it loudly.

### Manual override

If the user manually flips a `blocked: merge_conflict` chore back to `in_review` (e.g. they fixed it themselves), the WHERE-guarded UPDATE above no longer matches (the chore is no longer `blocked`). The attempt stays `running` until the worker exits, at which point the completion path notices the parent has been manually advanced and marks the attempt `abandoned` with `failure_reason='manually_resolved_externally'`.

A `boss engine conflicts abandon <id>` verb (Q11) lets the coordinator explicitly mark an attempt abandoned, e.g. when the underlying PR has been closed.

### What if `main` moves *during* the resolution but the rebase still applies cleanly

The worker rebased against the `main` it saw at lease time. `main` moves once more before the push lands. GitHub's re-evaluation against the *new* `main` may say `MERGEABLE` (the new main commits don't touch the resolved files) or `CONFLICTING` (they do).

- If `MERGEABLE`: normal retire path. No special handling.
- If `CONFLICTING`: superseded path above — new attempt with the new base sha.

We don't try to pre-empt this with a "rebase against current main right before push" worker step — too easy for race; the next probe handles it.

---

## Design Question 6 — Failure Modes

### Resolution worker fails to push (auth, network)

The worker tries `jj git push --bookmark <branch>` and the push is rejected. Two sub-cases parallel to `auto-rebase-stacked-prs.md` Q4:

- **Concurrent human push.** The user rebased the branch by hand and pushed while the worker was working. The worker's local branch is now stale. The right behaviour: the worker detects the rejection, fetches, *does not* try to force-overwrite the human's push (the human's resolution is canonical), and stops. The engine marks the attempt `superseded` (a new `mergeable` probe will pick up whether the human's push fixed the conflict; if so, the parent goes back to `in_review` naturally; if not, a new attempt fires).
- **Push genuinely rejected** (auth, branch protection). Engine marks the attempt `failed` with `failure_reason='push_rejected'`. The parent stays `blocked`. UI surfaces it loudly via the activity feed. Auto-retry does *not* fire on auth failures — human intervention required (same policy as `auto-rebase-stacked-prs.md` Q4).

### Worker crashes / pane dies

Same handling as any worker crash. The completion path notices the worker exited without a successful Stop event and marks the attempt `failed` with `failure_reason='worker_crashed'`. The parent stays `blocked`. A retry verb (`boss engine conflicts retry <id>`) creates a fresh attempt that re-leases a workspace and re-spawns.

### Cube lease unavailable at spawn time

Same as `auto-rebase-stacked-prs.md` Q4 sub-case 2. The attempt stays in `pending` (not `failed`); the merge-poller's next pass re-probes; if the conflict still exists, the existing `pending` attempt is consulted (idempotency unique key). The coordinator's lease loop is the natural retry mechanism.

A `pending` attempt older than 30 minutes is logged at `warn` so the operator can investigate ("cube pool exhausted, no resolution worker started"). No auto-escalation; the pool is the constraint, not the trigger.

### Parent PR closed while resolution is in flight

The user (or someone) closes the PR on GitHub. The next merge-poller probe returns `state=CLOSED, mergedAt=NULL`. The `chore-lifecycle-pr-closed-unmerged.md` path fires — the parent goes from `blocked` to `needs_attention`. The resolution attempt's `work_item_id` still points at the parent; on the completion path or on the next conflict-watch sweep, the engine notices the parent is no longer in `blocked: merge_conflict` and marks the attempt `abandoned` with `failure_reason='parent_pr_closed'`. The worker is stopped (via the existing `bossctl work cancel` path) if still running.

### Parent PR merged while resolution is in flight (extreme race)

GitHub somehow merges the PR while it's `blocked: merge_conflict`. This shouldn't happen — a CONFLICTING PR isn't mergeable — but the user can force-merge if branch protection allows. The next probe returns `state=MERGED`. The merge-poller's existing `mark_chore_pr_merged` path transitions the parent to `done`. The resolution attempt is no longer relevant; it's marked `abandoned` with `failure_reason='parent_pr_merged_externally'`. Worker is cancelled.

### Conflict reappears after a successful resolution (within the same poller cycle)

Worker pushed; engine flipped parent to `in_review`. On the very next poller pass, `main` has moved again and the PR is `CONFLICTING` again. A new attempt is created (unique key allows it — new `base_sha_at_trigger`). The parent flips back to `blocked`. The activity feed shows two entries; the operator sees the churn.

If the churn rate is excessive (>3 conflict cycles for one PR in a short window), the engine should *not* keep auto-resolving — at that point the PR is fundamentally racing main and needs human attention. A simple guard: count `conflict_resolutions WHERE work_item_id = ? AND created_at > ?-1h`; if ≥ 3, the new attempt is created with `status='abandoned'` immediately and a `failure_reason='churn_threshold_exceeded'` annotation. The parent stays `blocked` and surfaces an attention item. The user can `boss engine conflicts retry --force <id>` to override.

### Worker takes too long

`auto-rebase-stacked-prs.md` Q4 doesn't propose a timeout; rebase work is typically minutes, not hours. Same here. We don't time the resolution worker out, but the operator-facing view shows the elapsed time, which becomes a soft signal ("this resolution has been running for 4 hours, probably stuck").

### Worker resolves the conflict in a way that breaks the PR's intent

Same risk as `auto-rebase-stacked-prs.md` Q11 ("conflicts the worker resolves 'incorrectly'"). Same mitigation: the worker's post-resolution PR comment enumerates per-file resolutions; the human reviewer is the second line of defence. The auto-retire path *does* flip the parent back to `in_review` automatically, so the reviewer's first action — clicking the PR — surfaces the worker's resolution comment immediately.

### Resolution succeeds but local tests were not run

The worker prompt instructs the worker to run `<test_command>` before pushing. If the product has no configured test command, the prompt omits the step and the worker pushes after the rebase compiles. CI re-running on push is the safety net. Same trade-off as `auto-rebase-stacked-prs.md` Q11.

---

## Design Question 7 — Coordination with `auto-rebase-stacked-prs`

### When the two flows can both want to fire

PR A (base = main) merges. PR B (base = feat-A) has its base swept up by auto-rebase. Auto-rebase's engine-direct attempt fails on conflict; auto-rebase's escalation path creates a chore (`auto-rebase-stacked-prs.md` Q5) and hands off a pre-loaded conflict workspace.

Meanwhile, this design's merge-poller probe of PR B *also* observes `mergeable=CONFLICTING` (it's the same conflict, after all). Without coordination, this design's path would *also* fire, creating a `conflict_resolutions` row, spawning a *second* worker, and racing the auto-rebase escalation chore.

### Options

- **(i) Suppress this design's path when the conflict is from a base-PR-merge.** Detect by joining `rebase_attempts WHERE dependent_pr_url = ? AND status IN ('running', 'escalated')`; if a row exists, don't create a `conflict_resolutions` row.
- **(ii) Suppress auto-rebase's path when this design has an active attempt.** Symmetric. Auto-rebase already runs first (it's the trigger that *put* the PR in this state), so this is unnatural.
- **(iii) Run both** and let the unique-key on `conflict_resolutions` plus the cube-lease contention naturally serialise them. Inefficient (two workers race for one fix) but correct.

### Recommendation

**Pick (i).** Concretely: `conflict_watch::on_conflict_detected` checks for an active `rebase_attempt` first:

```rust
async fn on_conflict_detected(work_item_id: &str, pr_url: &str) {
    if work_db.has_active_rebase_attempt_for_pr(pr_url)? {
        tracing::debug!(pr_url, "conflict detected but rebase_attempt is active; skipping");
        return;
    }
    // ... existing path: create conflict_resolutions, spawn worker
}
```

`has_active_rebase_attempt_for_pr` is a cheap query against `rebase_attempts WHERE dependent_pr_url = ? AND status IN ('pending', 'running', 'escalated')`. If it returns true, the auto-rebase path owns this PR's conflict and we defer.

When the auto-rebase escalation chore lands its push and the PR returns to mergeable, the auto-rebase path retires the rebase_attempt (`succeeded`) and the parent's `blocked: dependency` returns to `in_review` (auto-rebase's own retire path). No `conflict_resolutions` row was ever created — clean.

If the auto-rebase escalation chore *fails* (failure_reason set, status `failed`), the auto-rebase path leaves the parent in whatever state it was in (typically `blocked` with a dependency reason). The next conflict-watch sweep will see no active rebase attempt and *will* fire — picking up where auto-rebase left off. The handoff is clean.

### The reverse: PR comes back to merge poller and conflict is "free-floating"

If a PR conflicts against `main` and no `rebase_attempt` is associated (i.e. the conflict didn't come from a base merge — it's just main drifting), this design fires unconditionally. That's the primary case.

### What about PRs auto-rebase explicitly skipped (forks, opted-out)

Auto-rebase skips fork PRs and opted-out products (`auto-rebase-stacked-prs.md` Q8). For fork PRs, this design also skips (same auth limitation; engine can't push to a fork). For opted-out products, this design should also skip — the opt-out is a "leave my PRs alone" signal, and triggering conflict resolution would violate it.

Recommendation: the opt-out is *unified*. `products.auto_rebase_enabled` becomes `products.auto_pr_maintenance_enabled` (rename), governing both flows. Both auto-rebase and conflict-resolution check this flag before firing. If a user wants finer control, the per-PR label opt-out (auto-rebase Q8) extends similarly: a label `boss/no-auto-rebase` also suppresses conflict resolution for that PR.

This is the cleanest opt-out story: one product setting, one PR label, both flows respect both.

---

## Design Question 8 — Coordination with `review-feedback`

### What the sibling project does

`proj_18ad7d43affb0370_2a` handles the *reviewer-driven* round trip: a reviewer leaves comments on an `in_review` PR; Boss notices the new comments; the parent flips from `in_review` to a temporarily-out-of-settled-state column; a worker iterates on the feedback; the parent returns to `in_review`. Same shape as this design — `in_review` items briefly leaving and snapping back — different trigger (mechanical/main-moved vs. reviewer-comment-arrived).

### Where they should agree

These design choices should be shared:

1. **The parent's state model.** Both flows write `status='blocked'` with a `blocked_reason`. This design picks `'merge_conflict'`; the review-feedback project picks `'review_feedback'`. The `blocked_reason` column is single, shared.
2. **The `blocked_attempt_id` pointer.** Both flows populate it with the id of *their* attempt row. The inspector renders the right detail by switching on `blocked_reason`.
3. **The activity feed surface.** Both surface entries to the same engine-activity stream. The list view `boss engine attempts list` (proposed Q11) shows both kinds.
4. **The auto-retire pattern.** Both flows watch for a "resolved" signal (mergeable=MERGEABLE here; new commits / comments-addressed signal in review-feedback) and flip parent back via the same WHERE-guarded UPDATE.
5. **The "engine put me here, only engine can take me back" rule** via `last_status_actor='engine'`.

### Where they differ

- **Trigger source.** This design: merge-poller probe of `mergeable`. Review-feedback: comment poller of `gh pr view --comments` / a new probe.
- **Worker prompt.** This design: rebase against main. Review-feedback: address comments.
- **Attempt table.** This design: `conflict_resolutions`. Review-feedback: TBD by that project — likely `review_iterations` or similar.
- **Idempotency key.** This design: `(work_item_id, base_sha_at_trigger)`. Review-feedback: probably `(work_item_id, latest_comment_id)`.

### Recommended coordination

The review-feedback design should reuse, verbatim:

- the `blocked_reason` column (Q2 above),
- the `blocked_attempt_id` column (Q2 above),
- the `FrontendEvent::ConflictResolution*` event shape, generalised (e.g. `FrontendEvent::ParentResolutionStarted { reason: 'merge_conflict' | 'review_feedback', ... }`),
- the engine-activity CLI surface (Q11),
- the merge-poller's three-state extension (extend further to four-state if needed — review-feedback may add `Open(NeedsAttention)` or similar).

The two designs should ship in either order; whichever ships second picks up the columns/events as already-present infrastructure.

### Recommendation

**Shared column and event names, separate attempt tables, separate trigger probes, parallel auto-retire paths.** The two designs converge on the parent's data model and the activity-feed UX; they diverge on the per-attempt mechanics. This keeps each project independently shippable while ensuring the user sees a coherent "engine is helping with this PR" story.

### `FrontendEvent` shape

```rust
// Initial v1 — conflict-specific events
ConflictResolutionStarted   { product_id: String, work_item_id: String, attempt_id: String, pr_url: String },
ConflictResolutionSucceeded { product_id: String, work_item_id: String, attempt_id: String, pr_url: String },
ConflictResolutionFailed    { product_id: String, work_item_id: String, attempt_id: String, pr_url: String, failure_reason: String },
ConflictResolutionAbandoned { product_id: String, work_item_id: String, attempt_id: String, pr_url: String, failure_reason: String },
```

When review-feedback lands, these can be generalised to `ParentResolution*` with a `reason` field, with the conflict-specific names kept as aliases for one release for back-compat. A wire-level refactor is fine when the second consumer exists.

---

## Design Question 9 — Migration

### Existing `blocked` rows

Any chore currently in `status='blocked'` has no `blocked_reason`. The migration should backfill what it can.

```rust
fn migrate_merge_conflict_handling(conn: &Connection) -> Result<()> {
    // Add the columns.
    if !table_has_column(conn, "tasks", "blocked_reason")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN blocked_reason TEXT", [])?;
    }
    if !table_has_column(conn, "tasks", "blocked_attempt_id")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN blocked_attempt_id TEXT", [])?;
    }

    // Create the side table.
    conn.execute_batch(/* schema from Q3 */)?;

    // Backfill: blocked rows with at least one open prereq are 'dependency'.
    conn.execute(
        "UPDATE tasks
            SET blocked_reason = 'dependency'
          WHERE status = 'blocked'
            AND blocked_reason IS NULL
            AND id IN (
                SELECT dependent_id FROM work_dependencies wd
                JOIN tasks t ON t.id = wd.prereq_id
                WHERE t.status NOT IN ('done', 'archived')
            )",
        [],
    )?;

    Ok(())
}
```

Rows that remain `blocked_reason IS NULL` are legacy — typically chores moved to `blocked` by a human via `boss chore move ... --to blocked` for reasons the engine doesn't track. The UI renders these with a generic "blocked" badge (no reason); the inspector lets the user set a reason manually if they want.

Bump `metadata.schema_version`.

### Existing in-review chores whose PRs are conflicting *right now*

When the engine starts after the migration, the merge-poller's startup sweep (mirroring `chore-lifecycle-pr-closed-unmerged.md` Q9) will run `run_one_pass` once. The new conflict-detection branch fires for any in-review chore whose PR is currently `CONFLICTING`. Those chores transition to `blocked: merge_conflict`, attempts are created, workers spawn (subject to pool capacity).

This is the right behaviour — chores that should already have been picked up while the engine was offline catch up in one pass. If the pool is small and many chores have conflicted, the attempts queue (`status='pending'`) and the coordinator drains them. The activity feed records each transition.

### Existing `auto_rebase_enabled` opt-out

If `auto-rebase-stacked-prs.md`'s `products.auto_rebase_enabled` already exists at the time this design ships, the migration renames it to `auto_pr_maintenance_enabled`:

```rust
if table_has_column(conn, "products", "auto_rebase_enabled")?
    && !table_has_column(conn, "products", "auto_pr_maintenance_enabled")?
{
    conn.execute(
        "ALTER TABLE products RENAME COLUMN auto_rebase_enabled TO auto_pr_maintenance_enabled",
        [],
    )?;
}
```

If `auto_rebase_enabled` does *not* exist yet (this design ships first), the column is created directly as `auto_pr_maintenance_enabled BOOLEAN NOT NULL DEFAULT 1`.

The CLI verb `boss product update <selector> --auto-rebase on|off` is preserved as an alias for `--auto-pr-maintenance` for back-compat; documentation moves to `--auto-pr-maintenance`.

---

## Design Question 10 — Idempotency and Ordering

### The hazards

- Two probe sweeps observe the same `CONFLICTING` state for the same PR before the first one has finished writing the `blocked` transition. (Race within the poller; bounded by the poller's serial sweep.)
- The probe observes `CONFLICTING`, the engine creates an attempt, the worker pushes successfully, the *next* probe (before retire ran) still sees `MERGEABLE`. The retire path fires; the attempt completes. So far so good.
- The probe observes `CONFLICTING` *during* an active attempt's window — main moved again, conflict regenerated. The unique-key `(work_item_id, base_sha_at_trigger)` differentiates: same base_sha = same attempt (no-op); different base_sha = new attempt.

### The invariants

1. **At most one `conflict_resolutions` row in `status='running'` for any given `work_item_id` at any moment.** Enforced by the WHERE-guarded INSERT-then-UPDATE pattern: when creating a new attempt, the engine first sets any prior `running` row for the same `work_item_id` to `superseded`, then INSERTS the new one. SQLite transaction wraps both writes.
2. **The parent's `blocked_attempt_id` always points at a `pending` or `running` attempt, or is NULL.** Enforced by the auto-retire path nulling the FK when the attempt terminates.
3. **The retire path only flips a parent it put in `blocked`.** Enforced by the WHERE clause including `blocked_attempt_id=?attempt_id` and `last_status_actor='engine'`.

### Ordering with merge-poller

The merge poller's sweep order is `list_chores_pending_merge_check` ASC by `updated_at`. Order of probing doesn't matter for this design — each chore's `conflict_resolutions` row is independent. There's no cross-chore conflict ordering to enforce (unlike `auto-rebase-stacked-prs.md` Q6 where the base-merge order matters for stacking).

### Ordering with auto-rebase

If a PR is being auto-rebased *and* its base is itself conflicting, both flows have opinions. The recommendation in Q7 — defer to auto-rebase when its row is active — gives a clean precedence: auto-rebase owns the slot until its attempt terminates; only then does this design potentially fire.

---

## Design Question 11 — CLI Surface

### Verbs

```text
boss engine conflicts list   [--product <selector>] [--status running|succeeded|failed|...] [--limit 50]
boss engine conflicts show   <attempt-id>
boss engine conflicts retry  <attempt-id>                # only valid for status=failed|abandoned
boss engine conflicts abandon <attempt-id> [--reason]    # mark a running attempt abandoned (e.g. PR closed externally)
```

The shape mirrors `auto-rebase-stacked-prs.md` Q7's `boss engine rebase-attempts list ...`. Both are `boss engine <thing>-list`-style surfaces.

### Unified `boss engine attempts list`

Once review-feedback (`proj_18ad7d43affb0370_2a`) ships its own attempts table, the natural follow-up is a unified verb:

```text
boss engine attempts list [--kind conflict|rebase|review-feedback] [--product ...] [--status ...]
```

This is a thin SQL wrapper that UNIONs across the three tables with a `kind` column projected from the table name. It does not require a schema unification; it's a CLI convenience. Defer until the third subsystem exists.

### macOS app surface

The macOS app gets a new "Engine" tab (per `auto-rebase-stacked-prs.md` Q7's recommendation) showing both `rebase_attempts` and `conflict_resolutions` in a single table view, with a `Kind` column. Same UX shape as the rebase-attempts list; rendering driven by the unified `boss engine attempts list` once available.

The PR-card badge from `auto-rebase-stacked-prs.md` Q7 (a small "↶ rebased" chip on cards whose PR was the dependent of a recent rebase) extends here: a "🔧 conflict cleared" chip on cards whose PR was the target of a recent successful conflict resolution, ageing out after 24h.

### Reference doc updates

`boss reference` gains:

> A chore in `status='blocked'` with `blocked_reason='merge_conflict'` is one
> whose PR was in code review but developed a merge conflict against its base
> (typically because `main` moved). The engine has auto-spawned a resolution
> worker that's running in the background; once the conflict is cleared and
> the PR is mergeable again, the chore returns to `in_review` automatically.
> Inspect the current attempt with `boss engine conflicts show <attempt-id>`
> (the attempt id is on the chore's `blocked_attempt_id` field).

---

## Sequence Diagrams

### Happy path: detect → resolve → retire

```
┌──────────┐  ┌───────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────┐  ┌─────────────┐  ┌──────────────┐
│ GitHub   │  │ merge_poll│  │ conflict_    │  │ coordinator  │  │ cube │  │ resolution  │  │ tasks /      │
│ (main    │  │           │  │ watch        │  │              │  │      │  │  worker     │  │ conflict_res │
│  moved)  │  │           │  │              │  │              │  │      │  │             │  │              │
└────┬─────┘  └─────┬─────┘  └──────┬───────┘  └──────┬───────┘  └──┬───┘  └──────┬──────┘  └──────┬───────┘
     │  gh pr view  │               │                 │             │              │                │
     │◄─────────────┤               │                 │             │              │                │
     │ state=OPEN   │               │                 │             │              │                │
     │ mergeable=   │               │                 │             │              │                │
     │ CONFLICTING  │               │                 │             │              │                │
     │──────────────►               │                 │             │              │                │
     │              │ on_conflict_  │                 │             │              │                │
     │              │   detected    │                 │             │              │                │
     │              │ ──────────────►                 │             │              │                │
     │              │               │ check rebase_   │             │              │                │
     │              │               │ attempts for PR │             │              │                │
     │              │               │ ──────────────► │             │              │                │
     │              │               │   (none active) │             │              │                │
     │              │               │ ◄────────────── │             │              │                │
     │              │               │ INSERT conflict_resolutions   │              │                │
     │              │               │ (status=pending)              │              │                │
     │              │               │ ──────────────────────────────┼──────────────┼────────────────►
     │              │               │ UPDATE tasks SET status=      │              │                │
     │              │               │   blocked, blocked_reason=    │              │                │
     │              │               │   'merge_conflict', ...       │              │                │
     │              │               │ ──────────────────────────────┼──────────────┼────────────────►
     │              │               │ RequestExecution(conflict_    │              │                │
     │              │               │   resolution, attempt_id)     │              │                │
     │              │               │ ──────────────► │             │              │                │
     │              │               │                 │ lease ws    │              │                │
     │              │               │                 │ ──────────► │              │                │
     │              │               │                 │ ws_id       │              │                │
     │              │               │                 │ ◄────────── │              │                │
     │              │               │                 │ UPDATE attempt status=     │                │
     │              │               │                 │ running, lease_id, ws_id   │                │
     │              │               │                 │ ───────────────────────────┼────────────────►
     │              │               │                 │ spawn(prompt + diagnosis)  │                │
     │              │               │                 │ ──────────────────────────►              │  │
     │              │               │                 │             │ jj git fetch │                │
     │              │               │                 │             │ jj rebase    │                │
     │              │               │                 │             │ jj git push  │                │
     │ (force push) │               │                 │             │              │                │
     │ ◄────────────┼───────────────┼─────────────────┼─────────────┼──────────────│                │
     │              │               │                 │             │ Stop         │                │
     │              │               │                 │ ◄───────────┼──────────────│                │
     │ (next sweep) │               │                 │             │              │                │
     │ gh pr view   │               │                 │             │              │                │
     │ ◄────────────┤               │                 │             │              │                │
     │ mergeable=   │               │                 │             │              │                │
     │ MERGEABLE    │               │                 │             │              │                │
     │ ─────────────►               │                 │             │              │                │
     │              │ on_resolved   │                 │             │              │                │
     │              │ ──────────────►                 │             │              │                │
     │              │               │ UPDATE tasks SET status=      │              │                │
     │              │               │   in_review, blocked_reason=  │              │                │
     │              │               │   NULL, blocked_attempt_id=   │              │                │
     │              │               │   NULL                        │              │                │
     │              │               │ ──────────────────────────────┼──────────────┼────────────────►
     │              │               │ UPDATE conflict_resolutions   │              │                │
     │              │               │ SET status=succeeded          │              │                │
     │              │               │ ──────────────────────────────┼──────────────┼────────────────►
     │              │               │ release lease                 │              │                │
     │              │               │ ──────────────────────────────►              │                │
```

### Failure path: worker stops with no push

```
... (detection same as happy path) ...

resolution worker:
    inspects diagnosis
    decides this is "product decision required"
    posts PR comment with the question
    exits without pushing

completion handler:
    observes worker Stop with attempt_id bound
    queries conflict_resolutions WHERE id=? — head_sha_after IS NULL
    decides this is a no-push exit; needs failure_reason from worker

    (the worker is expected to write failure_reason via the PR-comment
     reasoning + a structured boss-cli call, e.g. `boss engine conflicts
     mark-failed <attempt-id> --reason product_decision_required`)

    UPDATE conflict_resolutions SET status=failed, failure_reason=...
    parent stays blocked: merge_conflict (the activity feed surfaces the
    failure; the human acts on it)
```

---

## Schema Summary

```sql
-- New columns on existing tables
ALTER TABLE tasks    ADD COLUMN blocked_reason     TEXT;             -- 'dependency'|'merge_conflict'|'review_feedback'|NULL
ALTER TABLE tasks    ADD COLUMN blocked_attempt_id TEXT;             -- soft FK, discriminated by blocked_reason
ALTER TABLE products RENAME COLUMN auto_rebase_enabled TO auto_pr_maintenance_enabled;
-- (or, if auto-rebase-stacked-prs hasn't shipped:)
-- ALTER TABLE products ADD COLUMN auto_pr_maintenance_enabled BOOLEAN NOT NULL DEFAULT 1;

-- New side table
CREATE TABLE conflict_resolutions (
    id                  TEXT PRIMARY KEY,
    product_id          TEXT NOT NULL,
    work_item_id        TEXT NOT NULL,
    pr_url              TEXT NOT NULL,
    pr_number           INTEGER NOT NULL,
    head_branch         TEXT NOT NULL,
    base_branch         TEXT NOT NULL,
    base_sha_at_trigger TEXT,
    head_sha_before     TEXT,
    head_sha_after      TEXT,
    status              TEXT NOT NULL,
    failure_reason      TEXT,
    cube_lease_id       TEXT,
    cube_workspace_id   TEXT,
    worker_id           TEXT,
    conflict_diagnosis  TEXT,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    UNIQUE (work_item_id, base_sha_at_trigger)
);

CREATE INDEX conflict_resolutions_status_idx    ON conflict_resolutions(status);
CREATE INDEX conflict_resolutions_work_item_idx ON conflict_resolutions(work_item_id);
CREATE INDEX conflict_resolutions_product_idx   ON conflict_resolutions(product_id);
```

All timestamps stored as Unix epoch seconds in decimal-string form, per [`work-taxonomy.md`](work-taxonomy.md#timestamp-format).

---

## Implementation Phases

Sized as bite-sized, independently mergeable chores. Each row in this table should land as one PR.

### Phase 1: Schema and types

1. **DB migration**. Add `tasks.blocked_reason`, `tasks.blocked_attempt_id`, the `conflict_resolutions` table, the `products.auto_pr_maintenance_enabled` rename. Backfill `blocked_reason='dependency'` for existing blocked-with-active-prereq rows. Acceptance: migration runs idempotently on a fresh DB and on a DB with pre-existing `auto_rebase_enabled` rows; unit tests for backfill correctness.

2. **Protocol types**. Add `blocked_reason: Option<String>`, `blocked_attempt_id: Option<String>` to the `Task` wire shape. Add `ConflictResolution` type to `tools/boss/protocol/src/types.rs`. Acceptance: round-trip serialisation tests pass; macOS app builds with the new fields rendered as no-op decoration.

### Phase 2: Detection and trigger

3. **Probe extension**. Extend `PrMergeState`/`PrLifecycleProbe` to the four-state enum from Q1. Update `gh pr view` flags to fetch `mergeable,mergeStateStatus,baseRefOid`. Add `OpenPrMergeability` mapping. Acceptance: unit tests over each combination of `state × mergeable × mergeStateStatus`.

4. **`conflict_watch` module**. Wire `on_conflict_detected(work_item_id, pr_url)` and `on_resolved(work_item_id)` into the merge-poller sweep dispatch. Implement the WHERE-guarded transitions for both directions. Implement the active-rebase-attempt check from Q7. Acceptance: integration tests with stubbed probe driving conflict-flip-resolve-flip cycles; idempotency tests for repeated probes.

5. **`list_chores_blocked_on_merge_conflict`**. New `WorkDb` method; called alongside `list_chores_pending_merge_check` per sweep. Acceptance: query returns only chores in `blocked: merge_conflict` with a `pr_url`.

### Phase 3: Worker spawn and resolution

6. **Execution kind: `conflict_resolution`**. Add to the `work_executions.kind` enum (per `work-execution.md`). Coordinator routes these to the pre-leased spawn path (same path `auto-rebase-stacked-prs.md` Q5 introduces). Acceptance: an execution of this kind requests a lease, gets one, and binds a worker pane.

7. **Conflict diagnosis collector**. If `auto-rebase-stacked-prs.md` Q11 has shipped, reuse its function verbatim. If not, implement it as part of this phase — same code, called from a different trigger. Acceptance: a synthetic conflict (rebased branch, conflicted files) produces a populated JSON diagnosis with file shapes and upstream footprint.

8. **Spawn prompt template**. Implement the conflict-resolution worker prompt (Q4) in `spawn_flow.rs` / `compose_spawn_prompt`. Reads `conflict_resolutions.conflict_diagnosis` and the per-product `test_command`. Acceptance: spawned worker pane receives the templated prompt with diagnosis embedded.

9. **Worker → engine markers**. Add `boss engine conflicts mark-failed <attempt-id> --reason <r>` CLI verb for workers to surface stop-condition failures. Acceptance: worker calls flip the attempt status and `failure_reason`.

### Phase 4: Auto-retire and completion

10. **Resolved-side transition**. Implement `on_resolved` in `conflict_watch`: WHERE-guarded UPDATE on parent `tasks`, UPDATE on `conflict_resolutions`, lease release, broadcast. Acceptance: a successful push → next probe → retire path runs end-to-end in an integration test.

11. **Completion-path branch**. Extend `completion.rs` to inspect outgoing workers for `attempt_id` bindings. On a worker Stop with an unresolved attempt (`head_sha_after IS NULL` and no `mark-failed` call), default to `failure_reason='no_push_no_stop_condition'`. Acceptance: synthetic worker exits without pushing → attempt is `failed` with the catch-all reason.

12. **Frontend events**. Emit `FrontendEvent::ConflictResolution{Started,Succeeded,Failed,Abandoned}` from the appropriate transitions. macOS app subscribes and renders activity-feed entries. Acceptance: integration test asserts events arrive in order with correct payloads.

### Phase 5: CLI and observability

13. **`boss engine conflicts` verbs**. Implement `list`, `show`, `retry`, `abandon`, `mark-failed` (the last one used by workers, but available to humans). JSON and text output. Acceptance: snapshot tests on output shape.

14. **macOS Engine tab**. New tab listing both `rebase_attempts` and `conflict_resolutions` in a single sortable table. Reuses the activity-feed infrastructure from `auto-rebase-stacked-prs.md` Q7. Acceptance: tab renders both row kinds; click → detail view; refresh on event.

15. **PR-card badge**. Render "🔧 conflict cleared" chip on parent cards whose PR was a successful conflict resolution target in the last 24h. Acceptance: visual regression test on a card with a 6h-old vs 36h-old successful attempt.

### Phase 6: Edge cases and hardening

16. **Churn guard**. Implement the `≥3 resolutions for one PR in 1h` heuristic (Q6). New attempts in that condition are created immediately as `abandoned`. Acceptance: integration test driving 4 conflict-resolve cycles in <1h; 4th attempt is abandoned with the correct reason.

17. **Startup sweep**. Ensure `run_one_pass` runs once at engine startup (the existing one-shot path in `chore-lifecycle-pr-closed-unmerged.md` Q9 may already do this). Verify it picks up conflict transitions for chores whose PRs became conflicting while the engine was offline.

18. **Opt-out**. Implement the `auto_pr_maintenance_enabled` per-product flag plus the `boss/no-auto-rebase` per-PR label check. Both gate both flows (this design + auto-rebase). Acceptance: an opted-out product's conflict-watch path is a no-op.

---

## Design Decisions

- The trigger is the merge poller's existing per-PR `gh pr view` round-trip, extended with `mergeable` and `mergeStateStatus`. No new background loop.
- The parent's blocked-reason is encoded as a dedicated `tasks.blocked_reason` column. Values are an open set; `'merge_conflict'`, `'dependency'`, and `'review_feedback'` are reserved.
- The parent's `blocked_attempt_id` is a soft FK whose target table is discriminated by `blocked_reason`.
- The ephemeral resolve-conflicts unit is recorded as a row in a new `conflict_resolutions` side table, *not* as a `tasks` row. This avoids contaminating the chore / task / project_task taxonomy.
- The kanban does *not* render an extra card for the conflict-resolution attempt. The parent card surfaces the state via a richer blocked badge. The attempt is inspectable via the inspector, the activity feed, and the CLI.
- The engine spawns a worker for every conflict — there is no engine-direct mechanical path (in contrast to `auto-rebase-stacked-prs.md`, which is engine-direct first and worker-on-conflict). Justification: the trigger is "conflict already exists by GitHub's evaluation," so the engine has no mechanical path left to attempt.
- Coordination with `auto-rebase-stacked-prs.md`: this design suppresses its trigger when an active `rebase_attempts` row covers the same PR. Auto-rebase escalation owns the slot until terminal.
- Coordination with `review-feedback`: both flows share `blocked_reason`, `blocked_attempt_id`, the activity feed surface, and the `last_status_actor` rules. They differ in trigger, attempt table, and worker prompt.
- Opt-out is unified: a single `products.auto_pr_maintenance_enabled` flag and a single `boss/no-auto-rebase` label govern both auto-rebase and conflict-resolution flows.
- Auto-retire is automatic and unguarded: when GitHub reports `MERGEABLE` for a parent we put in `blocked: merge_conflict`, the engine flips back to `in_review` without further prompting. The user is informed via the activity feed; no interruption.

## Open Questions

- **Does the resolution worker need to read review comments?** A PR with conflict may also have pending reviewer feedback that the worker, while in the workspace, *could* address — but the project description scopes this design to mechanical conflict resolution. The recommendation here is to defer that to the `review-feedback` flow; the resolution worker does only the rebase. But: if the same chore is *also* `blocked: review_feedback`, we have two attempts on the same parent. The state model in Q2 allows only one `blocked_attempt_id`. Resolve by ordering: this design's flow defers to review-feedback if one of its attempts is active for the same parent (same shape as Q7's deferral to auto-rebase). Confirm with the review-feedback design.

- **Should we support `boss engine conflicts cancel <id>`?** Stopping an in-flight resolution worker mid-rebase. Today the user can `bossctl work cancel <agent-id>`, which would also fail the attempt via the completion path. A dedicated verb is sugar. Defer to Phase 6 if it's worth it; not critical for v1.

- **What's the engine's behaviour if `gh pr view` consistently returns `mergeable=UNKNOWN` for a PR for hours?** Q1 says "wait." But there's a degenerate case where GitHub is genuinely stuck (rare; happens during their own outages). Should the engine eventually fall back to a definitive guess via `git merge-tree` locally? Probably yes, after a long timeout (e.g. 1h of consecutive UNKNOWN). Not in v1 — first ship the GitHub-trust path; revisit if real-world telemetry shows it matters.

- **`tasks.test_command` storage.** The worker prompt (Q4) embeds the product's test command. The `tasks` table doesn't have this today; `products` doesn't either. Where does it live? Suggest a new `products.test_command TEXT` column, set via `boss product update <selector> --test-command <cmd>`. Same field is useful for `auto-rebase-stacked-prs.md` Q11, so the column should be added by whichever design lands first. Confirm.

- **PR-card badge ageing.** Q11's 24h ageing matches `auto-rebase-stacked-prs.md`. Is 24h right for conflict resolution? Probably; revisit if users say the badge is sticky / distracting.

- **Does the conflict-watch's `on_resolved` need to consult the auto-rebase row?** If auto-rebase put the parent into `blocked: dependency` and its retire path returns the parent to `in_review`, the conflict-watch path observing `MERGEABLE` later is a no-op (the parent isn't `blocked: merge_conflict`). Confirmed safe — no extra coordination logic needed.

## Related Designs

- [`work-taxonomy`](work-taxonomy.md) — domain model for products, projects, tasks, chores.
- [`work-execution`](work-execution.md) — execution / run / lease lifecycle.
- [`work-kanban`](work-kanban.md) — kanban projection and the `Doing`/`Review` column contracts.
- [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md) — closely-related design; shares the conflict-diagnosis collector, pre-leased spawn path, and `boss engine attempts` surface.
- [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) — adjacent merge-poller extension; the probe enum evolution composes with that work.
- [`work-dependencies`](work-dependencies.md) — owns `blocked: dependency`; this design adds the `blocked_reason` column it implicitly assumed.
- review-feedback workflow (`proj_18ad7d43affb0370_2a`) — sibling design that should share the `blocked_reason`, `blocked_attempt_id`, and engine-activity surfaces.
