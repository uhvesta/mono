# Boss: Auto-Remediation for In-Review PRs (Merge Conflicts and CI Failures)

This design covers two parallel auto-remediation flows for in-review PRs that share a model, a schema, and a state machine. The merge-conflict flow is the original spine; the CI-failure flow extends every section in-place. Both flows are triggered by the same `gh pr view` poll, both write into a shared multi-signal blocked-state side table, both spawn a single-turn worker through the pre-leased spawn path, and both auto-retire the parent back to `in_review` when the signal clears. A single parent can be blocked for both reasons simultaneously and each signal clears independently.

## Problem

The kanban's `Review` column is supposed to mean "this PR is settled state — the worker is done, the human is the bottleneck." Once a worker pushes its branch, opens the PR, and exits, the chore sits in `in_review` until the merge poller observes the merge (`merge_poller.rs:122`) and flips it to `done`. The contract is *the engine watches; the human reviews.*

That contract quietly breaks the moment `main` moves under the open PR's feet. Some unrelated PR lands that touches a file the in-review PR also touches; GitHub re-evaluates the PR and reports `mergeable = CONFLICTING`. The reviewer clicks the merge button, sees the red "this branch has conflicts that must be resolved" banner, and bounces — the PR is no longer review-ready. Today nothing in Boss notices. The chore stays in `in_review`, the kanban happily shows a clean Review card, the merge poller's `state, mergedAt` probe says "still OPEN, not merged" forever, and the user is the one who eventually pings the worker to come back and rebase.

This is the close cousin of [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md), but the trigger and idempotency model are different:

- **Auto-rebase fires on `base PR merged`.** It scans `gh pr list --base feat-A` for dependents and rebases each onto `main`. The trigger is a specific upstream event the engine *sees*.
- **This design fires on `the in-review PR is no longer mergeable against its current base`.** The trigger isn't tied to a particular upstream merge — `main` may have moved several times, possibly through PRs Boss never tracked (manual `gh` merges, web UI clicks, external contributors). The signal is GitHub's own evaluation of the diff.

The two flows converge on the same *action*: lease a workspace, run `jj rebase`, push the result. They diverge on *who notices the need* and on *whether one work-item-shaped record should represent the rebase request* (auto-rebase: yes, a `rebase_attempts` row plus possibly an escalated chore for conflicts; this design: see Q3).

The project description anticipates an **ephemeral work item** concept: a record that is auto-created when the conflict appears, auto-completed when the PR is mergeable again, and tied to a parent. The challenge — explicit in the project framing — is to design it so it doesn't conflate with the existing chore / task / project_task taxonomy. Tasks are *planned* work. Chores are *standalone* work. Project_tasks are *ordered, design-driven* work. None of those describe "the engine spotted that PR #243 stopped being mergeable, spun up a worker for 90 seconds, then made the symptom go away." Making that a `tasks` row pollutes the kanban, the dispatcher's pickup query, and the `boss task list` surface with engine plumbing that the human doesn't manage.

This doc proposes a shape parallel to `rebase_attempts`: a side table `conflict_resolutions` that records the engine's *attempts* to clear conflicts on an in-review PR, with full worker-spawn handoff for the conflict-resolution turn, and tight coordination with the existing `auto-rebase-stacked-prs` and the in-flight `review-feedback` projects (`proj_18ad7d43affb0370_2a`). The parent chore / project_task gets a new `blocked_reason = 'merge_conflict'` state plus a pointer to the active attempt, so the human sees *why* the card is suddenly in Doing-with-blocked-badge and can drill in. Auto-completion is the engine's job; the user is told *after the fact* by an activity-feed entry, not interrupted while the engine is still trying.

### The parallel signal: CI on the PR turns red

The same contract is broken by a parallel signal: **CI on the PR turns red.** A worker pushes its branch, opens the PR, exits. Buildkite runs against the head ref. A test fails. The PR is no longer review-ready — the reviewer who clicks merge will see a red required-check and bounce — but today nothing in Boss notices. The chore sits in `in_review`. The merge poller's `state, mergedAt, mergeable` probe is happy (the PR is `OPEN`, `MERGEABLE`, not merged); the kanban shows a clean Review card. The user is the one who eventually pings the worker to come back and fix the test.

The motivating real-world examples sit in this project's own chore history: `task_18ae9a0d732db7e8_31` (the merge-conflict case) and `task_18ae9a26abd65dc0_36` (a CI failure on flunge#647). The user filed both manually. The system this doc specifies should have handled both without the user filing chores at all.

The framing is identical to the merge-conflict case: a *mechanical* (not reviewer-driven) regression to review-readiness, on the same kanban column, with the same "auto-spawn a worker, snap back when fixed" remediation shape. Everything the merge-conflict flow builds — the `blocked_reason` discriminator, the `blocked_attempt_id` soft-FK, the side-table-not-tasks-row attempt model, the merge-poller probe extension, the activity-feed surface, the unified `boss engine attempts ...` CLI — is load-bearing for the CI flow too. The CI flow adds a parallel attempt subsystem (`ci_remediations`) and parallel `blocked_reason` values (`ci_failure` / `ci_failure_exhausted`), composes the state machine, and answers the open questions specific to CI as a signal.

The crucial difference from merge-conflict is the framing the user emphasised: **CI failures are not always tractable for a worker.** A merge conflict is a closed mechanical problem — `jj rebase` either succeeds or the worker resolves the conflicts. A failing test could be a typo, a flaky integration test, an infra outage, a semantic regression that needs design judgment, or a CI config bug that has nothing to do with this PR. The worker must try at least once — that is the whole point of automating this — but a worker that keeps spinning on a test it fundamentally can't fix burns cube leases, accumulates noisy force-pushes on the PR, and trains the user to ignore the activity feed. **An attempt budget caps that risk.** After N attempts, the engine stops trying, leaves the parent `blocked: ci_failure_exhausted`, and pages the user.

The CI flow's `ci_remediations` row follows the same ephemerality rule as `conflict_resolutions`: auto-created when CI goes red, may auto-complete when CI is green again, tied to a parent — same shape, same anti-taxonomy-pollution argument. The parent's `blocked_attempt_id` points at the active CI remediation just as it points at an active conflict resolution. The two attempt subsystems coexist on the same parent state machine, clear independently, and the parent returns to `in_review` only when *all* active blocking signals clear.

## Goals

- Detect when an in-review PR becomes unmergeable (conflict against `main`) automatically, on the same poll cadence as merge detection. No new background loop.
- Flip the parent work item from `in_review → blocked` with `blocked_reason = 'merge_conflict'`, so the kanban and inspector make the reason for blocking visible without overloading the dependency-graph blocked semantics.
- Auto-spawn a worker to resolve the conflict, with the workspace pre-leased and the PR's conflict diagnosis pre-loaded into the spawn prompt — same playbook the auto-rebase escalation path already uses.
- Once the worker pushes and GitHub re-evaluates the PR as `mergeable = MERGEABLE`, retire the conflict-resolution attempt, flip the parent back to `in_review` automatically, and emit an activity-feed entry. The human's only interruption is "by the way, while you were away, the engine cleared a conflict on this PR."
- Stay coherent with the in-flight [review-feedback workflow](https://github.com/spinyfin/mono/issues?q=proj_18ad7d43affb0370_2a) (`proj_18ad7d43affb0370_2a`). Both flows are "an `in_review` item temporarily leaves its settled state and snaps back." They should share the `blocked_reason` enum, the parent-side state shape, and the engine activity feed; they differ only in trigger and worker prompt.
- Stay coherent with [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md). When a base PR merges and the dependent develops a conflict, auto-rebase's escalation path should *not* also trigger this design's attempt — they would collide. See Q7.
- Make ephemeral *operationally invisible* (no kanban card for the resolve-conflicts turn) but *operationally inspectable* (activity feed entry, attempt record retained for history, retry verb).
- **Detect when an in-review PR's required checks fail** automatically, on the same poll cadence as merge detection, sharing the same `gh pr view` round-trip. No new background loop for CI either.
- **Flip the parent from `in_review → blocked` with `blocked_reason = 'ci_failure'`** (and `'ci_failure_exhausted'` after budget exhaustion). The kanban and inspector make the reason visible and distinguishable from `merge_conflict`.
- **Auto-spawn a worker to fix CI**, capped by a per-PR attempt budget (default 3, per-product configurable). The worker tries at least once even on suspicious failures; the budget prevents perpetual spin.
- **Distinguish re-trigger from fix.** A failure that looks like infra/flake is *re-triggered* (CI re-run, no code change) and *does not* consume a budget slot. A failure that looks code-shaped is *fixed* (worker pushes), and consumes one slot.
- **Auto-retire on green.** Once the required checks pass, retire the attempt, flip the parent back to `in_review`, post an activity-feed entry. Same UX shape as the merge-conflict resolve-then-snap-back.
- **Compose cleanly within this doc.** A PR can be `blocked` for `merge_conflict`, `ci_failure`, or *both*. The parent unblocks only when both clear. Order the resolution flows so they don't race (merge-conflict first, then CI re-check).
- **Provide a manual reset.** `boss engine ci retry <work-item-id>` resets the per-PR budget and re-fires the auto-fix flow. Cheap escape hatch when the user wants to override the engine's "I give up" decision.

## Non-Goals

- **Resolving conflicts the engine can't push for.** PRs from a fork (`headRepositoryOwner != owner`) are out, same exclusion as [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md) Q2. The engine can't force-push to a fork branch.
- **Detecting non-conflict review-readiness regressions other than failing CI.** Going stale because a reviewer left comments but didn't approve is handled by the in-flight `review-feedback` workflow (`proj_18ad7d43affb0370_2a`), not here. This design's two flows narrowly handle the *git-state-conflicting-with-base* case and the *required-CI-check-failing* case.
- **Auto-fixing failing CI on items NOT in `in_review`.** A `done` item that just got reverted may have failing pre-merge CI; that is a different concern (reactivation lifecycle) and is briefly addressed in Q10 but deferred to its own design.
- **Re-architecting CI itself.** This design assumes the CI provider (Buildkite, GitHub Actions) is the source of truth; we read its outputs, we don't try to second-guess them.
- **Fixing flaky tests at the test level.** The CI worker re-triggers flake-looking failures but does not modify the test to mark it `@flaky`, retry-decorate it, or skip it. That kind of fix is human-judgment territory; if the worker recognises chronic flake, it bails and pages the user.
- **Detecting CI on a base PR that hasn't merged yet.** A stacked PR's failing CI on its base is the base's problem; this design probes the in-review item's own checks against the head ref of that item.
- **Pre-empting CI that is still pending.** A `PENDING` rollup is the wait-state; we don't act on it. Q1 covers the rollup-state mapping.
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

### Naming for the CI-failure flow

- The new module is **`ci_watch`** (`engine/src/ci_watch.rs`). Owns the detection-trigger pipeline for CI failures. Parallel to `conflict_watch`.
- The unit of work is a **CI remediation** (the action) recorded as a **CI-remediation attempt** (the row). Status values mirror `conflict_resolutions`: `pending`, `running`, `succeeded`, `superseded`, `failed`, `abandoned`. Plus one CI-specific kind: an attempt may be a **fix attempt** (consumes budget) or a **retrigger attempt** (does not consume budget); see Q3.
- The persisted record is the **`ci_remediations`** row, parallel to `conflict_resolutions`. Same anti-taxonomy-pollution rationale (Q3): not a `tasks` row.
- New values on `tasks.blocked_reason`: **`'ci_failure'`** (auto-fix flow is still trying) and **`'ci_failure_exhausted'`** (budget exhausted; engine has given up). The two-value split avoids re-decoding "is the engine still trying?" from a per-PR counter at every kanban render. The kanban inspector resolves both to user-visible labels: `ci failing` and `ci failing (exhausted)`.
- New side table for composing multiple simultaneous blocked signals: **`task_blocked_signals`** (Q2). Scalar `tasks.blocked_reason` and `tasks.blocked_attempt_id` remain as the denormalised "primary reason" cache for the UI.
- New columns on `tasks`: **`ci_attempt_budget`** (per-PR override, NULL = use product default) and **`ci_attempts_used`** (counter, default 0). The product-level default lives in **`products.ci_attempt_budget`** (default 3).
- The CLI surface verbs for CI are **`boss engine ci list`** / **`boss engine ci show <id>`** / **`boss engine ci retry <id>`** (resets the counter for the PR + re-fires) / **`boss engine ci abandon <id>`**. Parallel to `boss engine conflicts ...`. The unified `boss engine attempts ...` grows a third kind, `ci`.
- The provider abstraction is **`CiLogReader`** (Rust trait), with concrete impls **`BuildkiteLogReader`** and **`GithubActionsLogReader`**. Q4 ("CI provider abstraction" subsection).

---

## Design Question 1 — Detection Trigger and Signal

### What `gh pr view` returns that the merge poller doesn't use today

The merge poller currently queries `--json state,mergedAt` (`merge_poller.rs:66-115`). GitHub's PR JSON includes two more fields that answer "is this PR mergeable?":

- **`mergeable`**: `MERGEABLE | CONFLICTING | UNKNOWN`. The mergeability of the head ref into the base ref *as currently computed by GitHub*. `UNKNOWN` is the transient state while GitHub recomputes after a base move; eventually it resolves to `MERGEABLE` or `CONFLICTING`.
- **`mergeStateStatus`**: `CLEAN | BLOCKED | BEHIND | HAS_HOOKS | DIRTY | UNKNOWN | UNSTABLE | DRAFT`. A finer breakdown of merge-readiness. The two values relevant here are `DIRTY` (= conflicts) and `BEHIND` (= the head ref is behind the base; mergeable but a rebase is recommended). `BEHIND` is *not* a conflict — GitHub still considers the PR mergeable via merge commit — and we explicitly do not act on it (rebasing-because-behind is a different feature, not in scope).

The two fields are correlated but `mergeable=CONFLICTING` is the load-bearing signal. `mergeStateStatus=DIRTY` is the more precise variant of the same answer; we read both and require them to agree before acting.

### What `gh pr view` returns for the CI signal

GitHub's PR JSON also includes:

- **`statusCheckRollup`**: an array of leaf check entries, each with `name`, `status` (`COMPLETED | IN_PROGRESS | QUEUED | PENDING | WAITING`), `conclusion` (`SUCCESS | FAILURE | NEUTRAL | CANCELLED | SKIPPED | TIMED_OUT | ACTION_REQUIRED | STALE | STARTUP_FAILURE | null` while in-progress), `targetUrl` (the provider's job page), and `isRequired` (whether it gates merge per branch protection).

`mergeStateStatus` overloads several signals: `UNSTABLE` means "mergeable but failing optional checks"; `BLOCKED` can mean "failing required checks" but also covers "requires N approvals" and other branch-protection reasons. The summary is not load-bearing for CI; we read the full rollup to know *which* check failed and route to the right provider's log reader. `mergeStateStatus` is at most a cheap pre-filter (skip the rollup walk on `CLEAN`).

#### The CI failure predicate

A PR has *required-checks failing* iff:

- there is at least one rollup leaf with `isRequired = true` AND `status = 'COMPLETED'` AND `conclusion ∈ {FAILURE, CANCELLED, TIMED_OUT, STARTUP_FAILURE, ACTION_REQUIRED}`.

Notes on each conclusion value:

- `FAILURE` — straightforward red.
- `CANCELLED` — a prior job was cancelled (user, or a re-run superseded it). Conservative: treat as failure; the re-run, if it happened, will produce a fresh `IN_PROGRESS` leaf that supersedes this one in the rollup. We only flag when the *latest* leaf for a given check name is in this state.
- `TIMED_OUT` — count as failure; usually a real problem.
- `STARTUP_FAILURE` — provider could not start the job (image missing, dependency unavailable). Treat as a *re-trigger candidate* in Q4, not a fix-budget consumer.
- `ACTION_REQUIRED` — GitHub Actions waits for a manual approval. Out of scope; flag the parent as `blocked: ci_failure` but mark the attempt `failed` immediately with `failure_reason='manual_action_required'`. The worker can't approve workflows.
- `NEUTRAL` and `SKIPPED` — not failures.
- `STALE` — the check ran on an old head sha; not a current-head failure. Don't flag.

Non-required checks (`isRequired = false`) failing do *not* gate merge per branch protection, so they do not trip this design's flag. The kanban inspector still shows them ("optional check X failing") as informational; no auto-remediation fires.

A check `IN_PROGRESS`, `QUEUED`, `PENDING`, `WAITING` is the wait-state. Treat as Clean for triggering purposes — do not act. Q6 covers the "checks never start" edge case.

#### Latest-leaf-per-name rule

`statusCheckRollup` can contain multiple entries for the same check name if the job has been re-run. The shape is provider-specific; both Buildkite and GitHub Actions expose every run, not just the latest. The detection predicate must collapse by name (taking the most recent leaf per name by `startedAt` / GitHub's natural ordering) before applying the failure predicate. Concretely:

```rust
fn current_required_failures(rollup: &[CheckLeaf]) -> Vec<&CheckLeaf> {
    let mut latest: BTreeMap<&str, &CheckLeaf> = BTreeMap::new();
    for leaf in rollup {
        latest.entry(leaf.name.as_str())
              .and_modify(|cur| if leaf.started_at > cur.started_at { *cur = leaf; })
              .or_insert(leaf);
    }
    latest.into_values()
        .filter(|l| l.is_required && l.status == "COMPLETED")
        .filter(|l| matches!(l.conclusion.as_deref(),
            Some("FAILURE" | "CANCELLED" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED")))
        .collect()
}
```

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

### Composing the CI signal into the same probe

The CI signal slots into the same probe extension. The `--json` projection grows by one field (`statusCheckRollup`); the `--jq` clause stays the same (the rollup is a nested array we parse in Rust, not project through jq). The probe types evolve once more:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrLifecycleState {
    Open(OpenPrStatus),
    Merged,
    ClosedUnmerged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPrStatus {
    pub mergeability: OpenPrMergeability,        // from the conflict path
    pub ci:           OpenPrCiStatus,            // new
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenPrCiStatus {
    /// All required checks are pending, neutral, or success.
    Clean,
    /// At least one required check is failing per the predicate above.
    Failing { failures: Vec<RequiredCheckFailure> },
    /// At least one required check is still in-flight; treat as Clean for triggering,
    /// but record so the auto-retire path knows not to fire prematurely (Q5).
    InFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheckFailure {
    pub name: String,
    pub conclusion: String,
    pub target_url: String,
    pub provider: CiProvider,                    // 'buildkite' | 'github_actions' | 'other'
    pub provider_job_id: Option<String>,         // parsed from target_url; None if unparseable
}
```

`sweep_one` becomes a richer dispatch combining both signals:

```rust
match (probe.mergeability(), probe.ci()) {
    // Both clean → either still in_review, or coming out of blocked
    (Clean, Clean | InFlight)     => maybe_clear_blocked(...).await,

    // Conflict, with or without CI failure → conflict path owns the slot first
    (Conflict, _)                 => mark_or_keep_blocked_merge_conflict(...).await,

    // No conflict, CI failing → CI watch
    (Clean, Failing { failures }) => ci_watch::on_ci_failure_detected(..., failures).await,
}
```

`maybe_clear_blocked` is now polymorphic over the active blocked-reason set (see Q2's `task_blocked_signals` side table and Q5's `maybe_clear_blocked` implementation).

### Why not (a)

A second poller hitting `gh` on its own cadence doubles the request rate to GitHub for the same answers. The merge poller already iterates `list_chores_pending_merge_check` candidates; the conflict probe rides on the same loop. There's no useful difference in cadence between "is this PR merged?" and "is this PR mergeable?" — they answer the same time-resolution question.

### Why not (c)

Same answer as the other PR-state designs: home-machine engine isn't internet-reachable. If/when a hosted engine exists, webhooks are a strict latency upgrade; the handler signature here is identical.

### Probe-pool extension

`list_chores_pending_merge_check` (`work.rs:2021`) currently returns chores in `status='in_review' AND pr_url IS NOT NULL`. We need to also probe chores in `status='blocked' AND blocked_reason='merge_conflict'` (to detect resolution). Add a parallel `list_chores_blocked_on_merge_conflict()` query and call both per sweep:

```rust
let in_review_candidates  = work_db.list_chores_pending_merge_check()?;
let blocked_conflict      = work_db.list_chores_blocked_on_merge_conflict()?;
let blocked_ci            = work_db.list_chores_blocked_on_ci_failure()?;
for c in in_review_candidates.iter()
            .chain(blocked_conflict.iter())
            .chain(blocked_ci.iter())
{
    sweep_one(work_db, probe, publisher, conflict_watch, ci_watch, c).await;
}
```

`list_chores_blocked_on_merge_conflict` is a small new method; SQL is the natural `WHERE status='blocked' AND blocked_reason='merge_conflict' AND pr_url IS NOT NULL`. The duplicate-iteration cost is bounded by the small N of blocked chores (typically ≤ a handful at any moment).

`list_chores_blocked_on_ci_failure` is the parallel CI-side method:

```sql
SELECT ... FROM tasks
 WHERE status = 'blocked'
   AND blocked_reason IN ('ci_failure', 'ci_failure_exhausted')
   AND pr_url IS NOT NULL
```

Worth noting: `blocked: ci_failure_exhausted` items are still probed, because the *symmetric* exit (CI eventually went green on its own, or a human pushed a fix) should auto-clear them too. Probing exhausted items does *not* re-fire the auto-fix flow; it only watches for the clear signal.

### Transient `UNKNOWN` and flapping

`mergeable=UNKNOWN` is GitHub's "I'm still computing" state. Common timing: a base merge happens, GitHub asynchronously re-evaluates open PRs against the new base, and the eval takes seconds to minutes. We must not flap a chore between `in_review` and `blocked` while GitHub is mid-recompute.

Recommended: treat `UNKNOWN` as Clean for the purposes of this design — i.e. do *not* trigger a conflict-resolution attempt on `UNKNOWN`. The poller will re-probe on its next pass; if mergeability resolves to `CONFLICTING`, the next sweep catches it. The cost of waiting one poller interval (~30s) for `UNKNOWN → CONFLICTING` is preferable to false-positive blocked transitions during normal base-merge churn.

To mitigate the very-rare case where `mergeable` is stuck at `UNKNOWN` for an extended period (sometimes GitHub gets confused), the engine logs `tracing::warn!(pr_url=…, "PR mergeable=UNKNOWN for >5 minutes; will not auto-flag conflict")` after the third consecutive UNKNOWN probe. This is observability, not action — we still wait for a definitive `CONFLICTING`.

### Transient `null` conclusions and head-sha staleness on the CI side

A check that has just started will have `status='IN_PROGRESS'` and `conclusion=null`. The latest-leaf rule filters these out via the `status='COMPLETED'` requirement. Good.

A trickier case: a push happens, GitHub re-evaluates, the rollup briefly contains only the *old* head-sha's leaves (because new jobs haven't started yet) before the new head's jobs appear. Solution: tag every probe with the rollup's per-leaf `headSha`; only consider leaves whose `headSha` matches the PR's current `headRefOid`. A leaf whose `headSha != current_head_sha` is a stale leaf from the prior head and is ignored. Concretely:

```rust
let current_head = probe.head_ref_oid;
let relevant = rollup.iter().filter(|l| l.head_sha == current_head);
// then apply latest-leaf-per-name to `relevant`
```

If, after filtering by head_sha, the rollup is empty for the current head, treat as `InFlight` (Q6's "checks never start" timer applies).

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

### Composing multiple simultaneous signals on the same parent

The CI flow forces an extension to the parent state model: **the same parent can be blocked for two engine reasons simultaneously.** A PR may both conflict with `main` AND have a failing required check. Both signals must clear before the parent returns to `in_review`, and each must clear independently — the conflict resolver's push may also fix the CI failure (great) or may leave it failing (separate concern).

#### Why scalar `blocked_reason` is insufficient on its own

If we only have a single `blocked_reason`, we'd have to pick one to surface — typically the more "fundamental" (merge conflict) — and silently demote the other. That breaks two contracts: (a) the user can't see at a glance that both signals are red; (b) the engine can't independently retire each signal.

#### Options revisited

- **(I) Allow multiple `blocked_reason` values, comma-joined.** Compact; every consumer parses the field. Bad: scalar typing was the whole point of option (B) above.
- **(II) Promote `blocked_reason` to a side table.** A row per (work_item_id, reason) pair; scalar `tasks.blocked_reason` is dropped or becomes a denormalised "primary reason" cache.
- **(III) Keep `tasks.blocked_reason` scalar but back it with a side table; a *priority order* on reasons picks which one the scalar holds.** Compatible with the columns (B) introduces and adds one side table for the multi-signal case.

**Pick (III).** Preserves the column API surface from earlier in this DQ; adds one side table for the (currently uncommon) case where two engine-managed signals coexist. The scalar `tasks.blocked_reason` is "what the UI shows by default"; the side table is the authoritative set.

#### Priority order

```
dependency               > all engine-auto signals
review_feedback          > ci_failure
merge_conflict           > ci_failure
ci_failure_exhausted     > ci_failure
```

Rationale: merge conflicts gate the rest of the world (the conflict resolver is the only path that can rewrite the branch); review-feedback represents a human signal that supersedes mechanical ones; dependency is the original semantics and stays at the top of the heap. Within the CI family, `exhausted` outranks `ci_failure` (an exhausted attempt is a more important UI signal than "still trying").

`blocked_attempt_id` becomes a derived field: it points at the attempt for whichever reason currently sits in `tasks.blocked_reason`. When the highest-priority signal retires, the engine reads the next-priority signal from the side table, promotes it to `tasks.blocked_reason`, and updates `blocked_attempt_id` accordingly.

#### The side table

```sql
CREATE TABLE IF NOT EXISTS task_blocked_signals (
    work_item_id   TEXT NOT NULL,
    reason         TEXT NOT NULL,         -- 'dependency'|'merge_conflict'|'ci_failure'|'ci_failure_exhausted'|'review_feedback'
    attempt_id     TEXT,                  -- soft FK, discriminated by reason; NULL for 'dependency'
    created_at     TEXT NOT NULL,
    cleared_at     TEXT,                  -- NULL while active; set on clear
    PRIMARY KEY (work_item_id, reason)
);

CREATE INDEX task_blocked_signals_active_idx
    ON task_blocked_signals(work_item_id, reason)
    WHERE cleared_at IS NULL;
```

The `(work_item_id, reason)` PK is the natural idempotency lock — re-observing the same signal is an upsert, not a duplicate row. The `cleared_at` column retains history (alongside `conflict_resolutions` and `ci_remediations`).

Two helper functions on `WorkDb`:

```rust
fn add_blocked_signal(&self, item: &str, reason: &str, attempt_id: Option<&str>) -> Result<()>;
fn clear_blocked_signal(&self, item: &str, reason: &str) -> Result<TaskStatusAfterClear>;
```

`clear_blocked_signal` is the load-bearing one. Atomically:

1. Set `cleared_at = now` on the signal row.
2. Query remaining active signals for the same `work_item_id`.
3. If none remain: promote parent to `in_review` (the natural return state — see "Pre-block status snapshot" below).
4. If at least one remains: re-promote `tasks.blocked_reason` and `tasks.blocked_attempt_id` to the highest-priority remaining signal.

The return value `TaskStatusAfterClear` tells the caller "the parent is back in `in_review`" vs "the parent is still blocked, but now under a different reason" so the broadcast can emit the right event.

#### Pre-block status snapshot

When the engine flips a parent from `in_review` to `blocked`, it stamps `tasks.last_status_actor='engine'`. When all signals clear, the natural return state is `in_review` — there's no other status this design plausibly puts an item into. We don't need a `pre_blocked_status` column; the auto-retire always lands at `in_review`. (Edge: if the human manually moved the chore to `active` during the block, `last_status_actor='human'` and the retire path's WHERE-guard refuses to flip. Same rule as Q5.)

#### Migration shape

The columns `blocked_reason` and `blocked_attempt_id` are added regardless. The side table is built lazily; existing scalar values are mirrored into a corresponding side-table row on first engine sweep after migration. See Q9 for the migration sequence.

### Composed kanban presentation (both signals possible)

When both signals are live, the badge shows the highest-priority one plus a smaller indicator that another signal is also active:

```text
🟧 blocked: merge conflict
   + ci failing (queued)
   resolution worker running
```

After CI-budget exhaustion:

```text
🛑 blocked: ci failing (exhausted)
   3 / 3 attempts used; engine has stopped
   last failure: <check name>
```

The 🛑 vs 🟧 colour distinction is intentional: 🟧 = "engine is still trying"; 🛑 = "engine has given up, user attention needed."

The detail-view inspector renders all active signals as a list, each with its attempt id, status, and elapsed time.

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

Two side tables, similar shape, different trigger. They can coexist. The unification angle:

- `rebase_attempts` is "we noticed PR A merged; we tried to rebase its dependents."
- `conflict_resolutions` is "we noticed PR X conflicts with main; we tried to resolve."

Both record one rebase attempt. The natural follow-up is a single `engine_attempts` table with a discriminator column. The CI flow adds a *third* such subsystem (`ci_remediations`, below), which is the threshold at which the unification CLI surface (`boss engine attempts ...`, Q11) becomes worth shipping — schema-level unification stays a follow-up.

### The CI remediations side table

Mirroring `conflict_resolutions` directly:

```sql
CREATE TABLE IF NOT EXISTS ci_remediations (
    id                  TEXT PRIMARY KEY,         -- 'ci_<ulid>'
    product_id          TEXT NOT NULL,
    work_item_id        TEXT NOT NULL,
    pr_url              TEXT NOT NULL,
    pr_number           INTEGER NOT NULL,
    head_branch         TEXT NOT NULL,
    head_sha_at_trigger TEXT NOT NULL,            -- head ref sha when failure was first observed
    head_sha_after      TEXT,                     -- post-push, NULL on failure or for retrigger-only
    attempt_kind        TEXT NOT NULL,            -- 'fix' | 'retrigger'
    consumes_budget     INTEGER NOT NULL,         -- 1 if attempt_kind='fix' AND it actually proceeded; 0 otherwise
    failed_checks       TEXT NOT NULL,            -- JSON: [{name, conclusion, provider, target_url, provider_job_id}]
    triage_class        TEXT,                     -- 'tractable' | 'flaky_or_infra' | 'unfixable' (worker's call after log read)
    log_excerpt         TEXT,                     -- the tail the worker read; saved for audit
    status              TEXT NOT NULL,            -- 'pending'|'running'|'succeeded'|'superseded'|'failed'|'abandoned'
    failure_reason      TEXT,
    cube_lease_id       TEXT,
    cube_workspace_id   TEXT,
    worker_id           TEXT,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)
);

CREATE INDEX ci_remediations_status_idx    ON ci_remediations(status);
CREATE INDEX ci_remediations_work_item_idx ON ci_remediations(work_item_id);
CREATE INDEX ci_remediations_product_idx   ON ci_remediations(product_id);
```

Bump `metadata.schema_version`.

#### Why the unique key is `(work_item_id, head_sha_at_trigger, attempt_kind)`

`conflict_resolutions` uses `(work_item_id, base_sha_at_trigger)` because the trigger is "main moved" and base sha discriminates events. CI failure's discriminator is `head_sha` — the same head sha producing the same failure is the same event. The triplet's third component (`attempt_kind`) is what makes a re-trigger and a fix on the same head sha distinct attempts: a re-trigger that didn't fix it followed by a fix attempt should be two rows, not a unique-key conflict.

#### Why `attempt_kind` and `consumes_budget` are separate columns

`attempt_kind` is the worker's *initial* classification. `consumes_budget` is the engine's *post-hoc* answer to "did this count against the budget?" They diverge in one common case: the worker classifies a failure as `flaky_or_infra` and re-triggers; CI then fails again with the same conclusion on the re-run. At that point the worker may pivot to `fix`. The original attempt row's `attempt_kind='retrigger'` and `consumes_budget=0` is correct historically; a *new* attempt row is created for the fix attempt, with `attempt_kind='fix'` and `consumes_budget=1`.

The budget counter on the parent (`tasks.ci_attempts_used`) is incremented only when a fix attempt actually progresses past the worker's go/no-go decision. A worker that reads the log and immediately bails (`triage_class='unfixable'`) does *not* consume a budget slot, because no force-push was attempted; the attempt is marked `failed` with `failure_reason='triage_bailout'` and the parent transitions to `ci_failure_exhausted` only if the budget was already exhausted. Otherwise the parent remains `ci_failure`, waiting — there's nothing the engine can do; the user must intervene.

An explicit rule worth restating: **a worker that triages and bails without pushing does not decrement the budget *and* does not re-trigger another attempt for the same head sha.** The unique-key constraint enforces this — the engine cannot create a second `('fix', same_head_sha, same_item)` row. If the user pushes a different head sha or runs `boss engine ci retry`, the budget unlocks. Q4 details the worker's decision-tree.

### The attempt budget and its counters

Storage:

```sql
ALTER TABLE tasks    ADD COLUMN ci_attempt_budget INTEGER;            -- NULL = use product default
ALTER TABLE tasks    ADD COLUMN ci_attempts_used  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE products ADD COLUMN ci_attempt_budget INTEGER NOT NULL DEFAULT 3;
```

- **Default**: 3 attempts per PR.
- **What counts as one attempt**: a *fix* attempt that proceeds past the worker's go/no-go (i.e. the worker decided this is tractable, tried to edit and push). Re-triggers do not count; triage-bailouts (worker decided unfixable before pushing) do not count.

The "do not count" cases are not free — each spawns a worker pane and burns a cube lease — so they are bounded separately by the unique-key constraint on `ci_remediations`.

#### Per-PR vs per-failure-signature

If the worker pushes a fix and CI runs again, a *different* check fails — is that attempt 2 for the PR or attempt 1 for a fresh failure?

**Recommendation: per-PR.** Cleaner; harder to game. A fix that flipped check A's failure into check B's failure (e.g. fixed the lint, broke the test) is two attempts; not one and one. The per-failure-signature variant encourages workers to play whack-a-mole — fix one check, push, see the next check fail, fix that one, push — until every check has been hit once. The user's feedback "do not spin forever" is precisely the prohibition on whack-a-mole.

#### Budget reset rules

The budget resets when the PR successfully transitions back to `in_review` *and stays there* (i.e. the parent's `task_blocked_signals` is empty of CI signals for at least one full poll cycle):

```rust
// When transitioning parent from blocked → in_review (last signal cleared)
if reason_cleared == "ci_failure" {
    work_db.reset_ci_attempts(work_item_id)?;  // sets ci_attempts_used = 0
}
```

A `blocked: ci_failure_exhausted` parent does *not* auto-reset on its own. Three paths out:

1. **The user runs `boss engine ci retry <work-item-id>`.** Resets `ci_attempts_used=0`, clears the `ci_failure_exhausted` signal, re-fires the auto-fix flow.
2. **The user pushes their own fix.** CI re-runs; if green, the auto-retire path fires; the budget is reset on transition back to `in_review`.
3. **The user manually moves the chore back to `in_review`.** `last_status_actor='human'`; engine doesn't second-guess. Budget is reset on the human move.

#### Per-product override

- `budget = 0` — auto-fix is disabled; the engine flags failures but never spawns a fixer. Parent goes straight to `blocked: ci_failure_exhausted` on first failure (the "notify only" mode).
- `budget = 1` — try once, then give up. For high-cost-of-error products where the user wants tight control.
- `budget = 3` (default) — the headline value.
- `budget > 3` — capped in code at 10 (hard limit) to prevent a misconfigured product from spinning for an hour.

Per-PR override via `tasks.ci_attempt_budget`: same range, NULL = inherit product default.

#### Why default = 3

A pragmatic balance:

- 1 attempt is too pessimistic — the worker might succeed on the first try; cutting it off after a single failure denies the headline value.
- ≥ 5 attempts is too noisy — each attempt produces a force-push, a CI run, a PR comment. Five tries is enough to train the user to filter the activity feed.
- 3 attempts gives the worker room to: (a) try a fix, observe new failure, (b) try a refined fix, observe new failure, (c) try once more if confident, then stop. Most successful auto-fixes will land on attempt 1; attempt 3 is a safety net.

Default is a hypothesis; revisit with telemetry. If success-rate is heavily attempt-1 with negligible attempt-2/3, lower the default. If attempt-3 is materially successful, keep or raise.

#### Budget exhaustion path

When `ci_attempts_used >= budget` and a new failure is detected:

```rust
// inside ci_watch::on_ci_failure_detected
let used    = work_db.get_ci_attempts_used(work_item_id)?;
let budget  = work_db.effective_ci_budget(work_item_id)?;

if used >= budget {
    work_db.add_blocked_signal(work_item_id, "ci_failure_exhausted", None)?;
    // do NOT spawn a worker; do NOT create an attempt row
    publisher.broadcast(FrontendEvent::CiRemediationExhausted { ... }).await;
    return Ok(());
}

// Normal path: create attempt, spawn worker
```

The activity feed entry is emphatic: "🛑 CI auto-fix exhausted on PR #647 after 3 attempts. The PR is blocked pending human attention. Last failure: `flunge/server`. `boss engine ci retry chore_18ad…77` to reset and try again."

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

### CI worker spawn and the fix-CI playbook

The CI-flow handoff parallels the conflict-resolution handoff in shape but diverges in pre-spawn data collection and prompt content.

#### The handoff (CI)

When `ci_watch::on_ci_failure_detected(work_item_id, pr_url, failures)` fires:

1. **Pre-flight checks.**
   - Confirm `tasks.ci_attempts_used < effective_budget`. If not, transition to `ci_failure_exhausted` (Q3) and return.
   - Confirm no active `conflict_resolutions` row for this PR (Q7). If yes, defer.
   - Compute `head_sha_at_trigger` from the probe payload.
2. **Insert a `ci_remediations` row** with `status='pending'` and `attempt_kind` chosen by the engine pre-spawn from a lightweight pre-triage of the rollup's failure set: if every failure has `conclusion ∈ {STARTUP_FAILURE, CANCELLED}` (engine-discernible infra signals), kind = `'retrigger'`. Otherwise kind = `'fix'`. Rationale: `STARTUP_FAILURE` is unambiguously infra (the job never executed user code); we don't need to read the log; we can re-trigger immediately. For all other conclusions, the worker reads the log.
3. **Update the parent**: add a `task_blocked_signals` row with `reason='ci_failure'`, `attempt_id=<new id>`; promote `tasks.blocked_reason='ci_failure'` and `tasks.blocked_attempt_id=<new id>` (only if no higher-priority signal is active; see Q2's priority order).
4. **Broadcast** `WorkItemChanged` plus `FrontendEvent::CiRemediationStarted`.
5. **Enqueue a worker spawn** via `ExecutionCoordinator`. New execution kind: `ci_remediation` under `work-execution.md`'s `kind` taxonomy.
6. **Coordinator acquires lease; spawns worker.** Updates `ci_remediations.cube_lease_id`, `cube_workspace_id`, `worker_id`; flips to `running`.
7. **Pre-spawn: fetch log excerpt** via the provider's `CiLogReader` for the worst failing check (default order: `FAILURE` > `TIMED_OUT` > `CANCELLED` > others). Store in `ci_remediations.log_excerpt` (tail 200 lines). The worker prompt embeds this; if the worker needs more, it shells out for the full log.

#### Worker prompt template (`attempt_kind='fix'`)

```
## CI failure on PR #647 — auto-fix attempt {used+1} of {budget}

**PR**: <pr_url>
**Branch**: `<head_branch>`
**Head sha at failure**: `<head_sha_at_trigger>`
**Workspace**: fresh lease on `<head_branch>`. Branch is already checked out at <head_sha_at_trigger>.

CI ran on this branch and at least one required check failed. Your job is to:
1. Triage the failure (re-trigger if it looks like infra/flake; fix if it's a real code problem; bail if it's unfixable from inside this PR).
2. If fixing, make the SMALLEST POSSIBLE CODE CHANGE that turns the failing check green.
3. Push.
4. Stop.

You are NOT adding new functionality, refactoring, addressing TODOs you happen to notice, or expanding scope. The PR was in code review; the only thing the engine has authorised you to change is what's needed to get CI back to green.

### Failed checks

<list of failed checks: name, conclusion, target_url; for each, the provider and provider_job_id>

### Log excerpt (worst failing check, tail 200 lines)

```
<log_excerpt>
```

For the full log: `bk job log <provider_job_id>` (Buildkite) or `gh run view --log-failed --job <provider_job_id>` (GitHub Actions).

### Triage decision tree

Apply in order:

1. **Infra markers** in the log → classify `flaky_or_infra`:
   `Connection refused`, `i/o timeout`, `pull access denied`, `manifest unknown`,
   `agent lost`, `agent timeout`, `node lost contact`,
   `The hosted runner lost communication`, `The runner has received a shutdown signal`,
   HTTP 5xx against package mirrors.
   → Run `boss engine ci classify <attempt-id> --as flaky_or_infra`.
   → Re-trigger: `bk build retry <build-id> --failed-only` or `gh run rerun <run-id> --failed`.
   → Exit. No code change. No push.

2. **Chronic-flake markers** (`@FlakyTest`, this check is in the product's known-flake list) → classify `flaky_or_infra`; re-trigger; exit.

3. **Hard-gate markers** ("PR exceeds N-line limit", missing CHANGELOG, DCO failed, licence-check policy violation, compile errors in files this PR did NOT touch) → classify `unfixable`; comment on the PR explaining what's wrong; exit.

4. **Otherwise** → classify `tractable`. Read more of the log if needed (`bk job log <id>` for the full body). Identify the smallest fix:
   - Lint / formatter / import — run the formatter; commit the formatter's output.
   - Type error — fix the type, do not weaken it. (Adding `any` to silence a type-check is *not* allowed.)
   - Test assertion mismatch — first decide whether the test or the code is wrong. If the code matches the PR's stated intent and the test is stale, update the test. If the code is wrong, fix the code. If you can't tell, bail (classify `unfixable`).
   - Codegen — re-run the codegen step; commit the output.
5. Run the project's `<test_command>` locally if configured. If green, push. If red, decide whether your fix is partially right (try one more refinement) or wrong (classify `unfixable`; comment; exit).

6. `jj git push --bookmark <head_branch>`
7. `gh pr comment <pr_number> --body "<post-fix comment template>"`
8. Stop. **Do not change the PR base, title, description, or scope.**

### Stop conditions

Same shape as the conflict-resolver stop conditions earlier in this DQ — comment on the PR, classify as `unfixable`, do NOT push:

- **Test is stale and updating it requires product knowledge** (e.g. the assertion encodes an old contract you can't tell whether to honour).
- **The fix requires changes to files this PR did not touch** in a way that would expand scope beyond what's reviewable.
- **The failure suggests a base-branch problem**, not a PR problem (compile errors in files outside the diff). Suggest re-running with a fresh `jj rebase -d main` first; if that doesn't help, bail.

### Post-fix PR comment template

🤖 boss auto-fixed CI on this PR.

Failing check: `<check_name>` — `<conclusion>`
Root cause (worker's best read): <one line>
Fix: <per-file summary, ≤3 bullets>

`<test_command>` is green locally. Force-pushed to `<head_branch>`. Per branch protection, prior approvals have been dismissed. Re-review when ready.

Original head: <head_sha_before>; new head: <head_sha_after>.
Attempt <used+1> of <budget>.
```

#### Worker prompt template (`attempt_kind='retrigger'`)

```
## CI failure on PR #647 — engine-pre-triaged retrigger

**PR**: <pr_url>
**Branch**: `<head_branch>`
**Head sha**: `<head_sha_at_trigger>`

The engine has pre-classified this failure as infra (all failing checks have `STARTUP_FAILURE` or unambiguous infra conclusion). No log read or code change is needed.

### Steps

1. For each failed check, re-trigger via the provider:
   `bk build retry <build-id> --failed-only` (Buildkite)
   `gh run rerun <run-id> --failed` (GitHub Actions)
2. `boss engine ci classify <attempt-id> --as flaky_or_infra`
3. Exit. Do not push.

If a re-trigger fails to enqueue (the provider rejects):
- `boss engine ci classify <attempt-id> --as unfixable --reason retrigger_failed`
- Comment on the PR with the provider's error.
- Exit.
```

#### When re-trigger is allowed

A re-trigger consumes no budget but is still bounded:

- **At most one re-trigger per `(work_item_id, head_sha)`.** Enforced by the unique-key triplet. A second re-trigger for the same head sha is blocked; the worker must classify as `tractable` or `unfixable` instead.
- **The re-trigger must succeed in actually re-running the job.** If the re-run fails to enqueue (auth, provider outage), the attempt is `failed` with `failure_reason='retrigger_failed'`; budget unchanged; parent stays `ci_failure`.
- **The re-trigger's outcome reopens triage.** If the re-run also fails on the same check, the worker (in a fresh attempt — fix kind this time) classifies as `tractable` or `unfixable`. If the re-run passes, the auto-retire path fires; no fix attempt is created.

Concretely, a re-trigger attempt's lifecycle:

```
worker classifies head_sha=H, check=C as flaky_or_infra
worker invokes provider's retry API
worker exits; attempt row status=succeeded (with consumes_budget=0)
... next probe ...
if CI now passing → maybe_clear_blocked → parent back to in_review (budget reset on cycle)
if CI failing on same head_sha (no new push) → ci_watch sees the failure; creates a FIX attempt (kind='fix', new row, consumes_budget=1)
if CI failing on different check but same head_sha → also a FIX attempt (per-PR budget)
```

#### Why the pre-triage path is separate

Retrigger-only attempts are cheap and don't need the full prompt context. Splitting the prompt template avoids loading the worker with decision-tree complexity it doesn't need; the engine has already decided. Separating retrigger from fix also makes the audit trail in `ci_remediations` clearer.

#### Reuse of the conflict-resolution infrastructure

- **Pre-leased spawn path** — same `spawn_flow.rs` path the conflict resolver uses. Variant difference: prompt template + execution kind.
- **Force-push semantics** — `jj git push --bookmark`. Same shape.
- **Post-resolution PR comment** — adapted (different first line: "auto-fixed CI" vs "resolved conflicts"; otherwise identical structure).
- **Worker authority** — same `gh` token; same push rights; multi-persona OK.

#### Worker → engine markers (CI)

New CLI verbs the CI worker uses:

- `boss engine ci classify <attempt-id> --as tractable|flaky_or_infra|unfixable [--reason <r>]`
- `boss engine ci mark-failed <attempt-id> --reason <r>` (parallel to `boss engine conflicts mark-failed`)
- `boss engine ci mark-retriggered <attempt-id>` (records that the re-run was successfully enqueued)

These map to UPDATEs on `ci_remediations`. The engine knows to expect them via the bound `attempt_id`; the completion path uses their presence/absence to decide the terminal status when the worker exits.

### CI provider abstraction

Mono builds on Buildkite. Flunge builds on Buildkite *and* GitHub Actions. The signal source for *detection* is provider-agnostic: GitHub's `statusCheckRollup` aggregates both. The signal source for *fixing* is provider-specific: reading the failing job's log requires `bk` (Buildkite) or `gh run view` (GitHub Actions).

#### The trait

```rust
#[async_trait::async_trait]
pub trait CiLogReader: Send + Sync {
    /// Read the tail of the failing job's log.
    async fn read_log_tail(&self, job_id: &str, n_lines: usize) -> Result<String>;

    /// Read the full job log (for the worker's deeper dive).
    async fn read_log_full(&self, job_id: &str) -> Result<String>;

    /// Re-trigger a failed job. Returns the new run/build id, or an error if the
    /// provider rejected the re-trigger.
    async fn retrigger(&self, job_id: &str) -> Result<String>;

    /// Identifier the worker should pass to the provider's CLI for the fix prompt.
    fn worker_cli_invocation_hint(&self, job_id: &str) -> String;
}

pub enum CiProvider { Buildkite, GithubActions, Other }
```

Two concrete impls, plus a fallback:

- **`BuildkiteLogReader`** — wraps `bk job log <id>`, `bk build retry <build-id>`. The engine shells out; the worker also shells out (same `bk` binary, same auth via env). Job-id parsing from `targetUrl` (`https://buildkite.com/<org>/<pipeline>/builds/<n>#<job-uuid>`).
- **`GithubActionsLogReader`** — wraps `gh run view --log-failed --job <id>`, `gh run rerun <run-id> --failed`. Same multi-call pattern. Run-id parsing from `targetUrl` (`https://github.com/<owner>/<repo>/actions/runs/<run-id>/job/<job-id>`).
- **`UnknownProviderReader`** — fallback for `Other`. Returns an error from every method. The worker's triage classifies these as `unfixable` immediately. A real third provider ships its own reader.

Provider inference is from `targetUrl` host:

```rust
fn provider_of(target_url: &str) -> CiProvider {
    match Url::parse(target_url).ok().and_then(|u| u.host_str().map(|s| s.to_owned())) {
        Some(h) if h == "buildkite.com" || h.ends_with(".buildkite.com") => CiProvider::Buildkite,
        Some(h) if h == "github.com"                                     => CiProvider::GithubActions,
        _                                                                => CiProvider::Other,
    }
}
```

The engine and the worker route the same way. The abstraction is thin: four methods; the concrete impls each wrap two CLI calls. Adding a third provider is one new impl plus one new arm in `provider_of`.

Explicitly *not* abstracted in v1: re-running a workflow vs re-running a single job (providers differ; `retrigger` papers over the difference per-impl) and provider-specific failure metadata (Buildkite's "blocked step", GHA's environment-protection holds — out of scope; would be classified `unfixable`).

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

### Composed retire for both signals (polymorphic `maybe_clear_blocked`)

With `task_blocked_signals` (Q2) the retire path is no longer signal-specific. The merge poller's `maybe_clear_blocked` iterates the active signal set and clears each that matches its probe condition:

```rust
async fn maybe_clear_blocked(
    work_db: &WorkDb,
    publisher: &FrontendPublisher,
    chore: &Chore,
    probe: &PrLifecycleProbe,
) -> Result<()> {
    let active_signals = work_db.active_blocked_signals(chore.id)?;

    for signal in active_signals {
        let should_clear = match signal.reason.as_str() {
            "merge_conflict"           => matches!(probe.mergeability(), Clean),
            "ci_failure"               => matches!(probe.ci(), Clean),
            "ci_failure_exhausted"     => matches!(probe.ci(), Clean),  // also clears if user/external fixes it
            "review_feedback"          => /* per review-feedback design */,
            "dependency"               => /* per work-dependencies */,
            _                          => false,
        };
        if should_clear {
            let after = work_db.clear_blocked_signal(chore.id, &signal.reason)?;
            update_attempt_row_for_reason(&signal)?;
            publisher.broadcast(retire_event_for(&signal, after)).await?;
        }
    }
}
```

For `ci_failure` and `ci_failure_exhausted`, the side-effects on clear are:

1. Mark the corresponding `ci_remediations` row as `succeeded` if it was `running`; this is the *most recent* row for the work item (queried by `created_at DESC LIMIT 1`).
2. Reset `tasks.ci_attempts_used` to 0 (Q3's reset rule — completed cycle = reset).
3. Release the worker's cube lease.
4. Broadcast `FrontendEvent::CiRemediationSucceeded` plus the generic `WorkItemChanged`.
5. Activity-feed entry: "Engine cleared a CI failure on PR #647; PR is back in review."

#### What if the worker pushed but a *different* check now fails

The push succeeded; the previous check went green; a new check went red. The next probe sees `ci=Failing` with a different `failed_checks` set. The previous `ci_remediations` row is `running` (the worker exited but the engine hadn't yet observed green). The flow:

1. Probe observes failure on new head sha.
2. `on_ci_failure_detected` for the new head sha is called.
3. Pre-check: is there an active running attempt for an *older* head sha? If yes, mark it `superseded` (the new push superseded it; the worker exited cleanly, but the outcome on the new sha isn't green).
4. Create a new `ci_remediations` row for the new head sha. `consumes_budget=1` (this is attempt 2 for the PR).
5. Spawn a new fixer worker.

The transition `running → superseded` does *not* decrement the budget — the worker did consume one slot when it ran; we just observe its outcome retroactively as "did not produce green."

#### What if the worker pushed and CI was already green on the new head

Push lands; some checks ran instantly (lint), passed; others (long test suites) still queued. Probe at this moment sees mixed: some `SUCCESS`, some `IN_PROGRESS`. Per Q1, `ci=InFlight` (Clean for triggering purposes, but we don't auto-retire while in-flight).

Wait one more sweep. If eventually all required checks are `SUCCESS` → retire. If a check goes `FAILURE` while others are still running → flip back to fixing.

The auto-retire is conservative: it requires *all* required checks at `SUCCESS` (latest-leaf-per-name). Any `InFlight` blocks retire; any new `FAILURE` reopens.

#### Manual override (CI)

If the user manually flips `blocked: ci_failure` to `in_review`, the WHERE-guarded UPDATE refuses to second-guess. The next probe will, however, observe the chore as `in_review` with still-failing CI. To avoid an immediate re-flip-to-blocked loop, the manual move resets the budget and inserts a *suppression record*:

```sql
CREATE TABLE IF NOT EXISTS ci_failure_suppressions (
    work_item_id   TEXT NOT NULL,
    head_sha       TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    PRIMARY KEY (work_item_id, head_sha)
);
```

A row in this table tells `ci_watch::on_ci_failure_detected` to suppress for the given `(work_item, head_sha)`. The suppression is cleared automatically when the head sha changes (a new push invalidates the override). Lifespan is one head sha.

If the user wants permanent suppression for a check, that's a branch-protection-level decision they make on GitHub, not in Boss.

The suppression record is a thin escape hatch. It is *not* used by the `boss engine ci retry` verb — retry explicitly *re-enables* auto-fix; manual move *disables* auto-fix. The two verbs are mirrored.

#### Exhausted parent goes green organically

A `blocked: ci_failure_exhausted` parent is still being probed. If the user pushed their own fix (or CI flake resolved itself), the next probe sees `ci=Clean`. The retire path fires; the `ci_failure_exhausted` signal clears; the parent goes back to `in_review`; budget resets. This is the "user fixed it, engine learns" path.

The activity feed entry on this transition is celebratory: "✅ CI green on PR #647; engine was exhausted but the human (or CI itself) cleared it. PR back in review."

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

The worker prompt instructs the worker to run `<test_command>` before pushing. If the product has no configured test command, the prompt omits the step and the worker pushes after the rebase compiles. CI re-running on push is the safety net. Same trade-off as `auto-rebase-stacked-prs.md` Q11. (CI failures on this newly-pushed head will then route through the CI-failure flow on the next probe.)

### CI-specific failure modes

#### CI fix-worker fails to push (auth, network)

Mirrors the conflict resolver's two sub-cases:

- **Concurrent human push.** User pushed their own fix while the fixer was working; the fixer's local branch is stale. On `jj git push`, the push is rejected. The fixer detects the rejection, fetches, observes the new head, classifies as `superseded`, exits without forcing. The engine marks the attempt `superseded`. **Does not consume budget.** The next probe re-evaluates CI on the user's head sha.
- **Push genuinely rejected** (auth, branch protection). Engine marks the attempt `failed` with `failure_reason='push_rejected'`. The parent stays `ci_failure`. UI surfaces it. Auto-retry does *not* fire on auth failures.

#### Two fixer workers on the same PR

The unique-key constraint on `ci_remediations` plus the `pending → running` state machine means at most one row in a non-terminal state per `(work_item_id, head_sha_at_trigger, attempt_kind)`. A second `on_ci_failure_detected` call for the same triplet finds the row already present and is a no-op.

If the engine restarts mid-attempt: the startup sweep observes the row in `running` (or `pending`); the coordinator re-attaches if the worker pane was preserved across restart (per `work-execution.md`); otherwise the orphaned attempt is marked `failed` with `failure_reason='engine_restart_orphan'` and budget is unchanged (the attempt didn't get to push). Next probe re-fires; new attempt.

#### CI provider outage

`bk` or `gh run view` returns transient errors when reading logs. The fixer worker retries the provider call (up to 3 immediate retries with exponential backoff). If still failing, the worker classifies the attempt as `flaky_or_infra` (the *provider* is the flake) and bails; no budget consumed. The engine logs a warning; the next probe re-fires once the rollup updates (which won't happen during the outage, so the system naturally waits).

#### Parent PR closed while fixer is mid-flight

Same handling as the conflict-resolver case ("Parent PR closed while resolution is in flight" above). Probe returns `state=CLOSED`. The `chore-lifecycle-pr-closed-unmerged.md` path transitions the parent. The fixer's `ci_remediations` row is marked `abandoned` with `failure_reason='parent_pr_closed'`. Worker is cancelled.

#### Parent PR merged while fixer is mid-flight

A required-checks-failing PR shouldn't be mergeable, but force-merge through branch protection is possible. Probe returns `state=MERGED`. Existing merge-poller path transitions parent to `done`. Fixer attempt is `abandoned` with `failure_reason='parent_pr_merged_externally'`. Worker is cancelled.

#### Two consecutive pushes by the worker

One push per attempt is the contract. The worker pushes a fix and exits; it does not loop on its own push outcomes. Multi-push fixers are a future complexity.

#### Worker force-pushes to an unrelated SHA (bug)

The worker should *not* do this — its prompt forbids scope-expansion. If it does, the next probe sees a head sha mismatch from what the engine snapshotted at attempt time. The attempt is *still* the relevant one; we don't fail it on sha mismatch. The probe just re-evaluates CI on the new head and the flow continues.

#### Worker resolves CI in a way that breaks the PR's intent

Same risk and mitigation as the conflict resolver's incorrect-resolution case above: the worker's PR comment enumerates per-file fixes; the human reviewer is the second line of defence. Auto-retire snaps the parent back to `in_review` so the reviewer's first action surfaces the worker's fix comment immediately.

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

Recommendation: the opt-out is *unified*. `products.auto_rebase_enabled` becomes `products.auto_pr_maintenance_enabled` (rename), governing all auto-remediation flows. Auto-rebase, conflict-resolution, and CI-remediation check this flag before firing. If a user wants finer control, the per-PR label opt-out (auto-rebase Q8) extends similarly: a label `boss/no-auto-rebase` also suppresses conflict resolution and CI fixing for that PR.

This is the cleanest opt-out story: one product setting, one PR label, all auto-remediation flows respect both.

### Composed ordering: rebase > conflict > ci

When auto-rebase, conflict resolution, and CI fixing might all want to fire for the same PR, the precedence is:

```
rebase_attempts   >  conflict_resolutions  >  ci_remediations
```

Implemented as `ci_watch::on_ci_failure_detected` defer-and-skip when a higher-priority attempt is active:

```rust
async fn on_ci_failure_detected(work_item_id: &str, pr_url: &str, failures: Vec<RequiredCheckFailure>) {
    if work_db.has_active_conflict_resolution_for(work_item_id)? {
        tracing::debug!(pr_url, "CI failure observed but conflict resolution is active; deferring");
        return;
    }
    if work_db.has_active_rebase_attempt_for_pr(pr_url)? {
        tracing::debug!(pr_url, "CI failure observed but rebase attempt is active; deferring");
        return;
    }
    // ... existing path: create ci_remediations, spawn worker
}
```

The deferral is silent (debug-level log; no activity-feed entry). Once the higher-priority flow completes and the PR is back to `in_review`, the next probe's sweep will re-observe the CI failure (if it still exists) and fire normally.

The reverse — conflict resolution running while CI is failing — does *not* require any change. The conflict resolver only cares about the merge-conflict signal; it pushes when the rebase is clean; CI re-runs on the new head; the next probe picks up the new CI state.

#### Auto-rebase escalation produces a push that fails CI

Auto-rebase escalates a conflict to a worker; the worker pushes the resolved rebase; CI then runs on the new head and fails. This is *new* CI failure data; the auto-rebase attempt's row is `succeeded` (it cleared the conflict). The chore went `blocked: dependency → in_review`. The next merge-poller probe sees `mergeable=MERGEABLE, ci=Failing` and `ci_watch::on_ci_failure_detected` fires normally — no special handling.

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

The CI flow adds parallel events:

```rust
CiRemediationStarted    { product_id: String, work_item_id: String, attempt_id: String, pr_url: String, attempt_kind: String },
CiRemediationSucceeded  { product_id: String, work_item_id: String, attempt_id: String, pr_url: String },
CiRemediationFailed     { product_id: String, work_item_id: String, attempt_id: String, pr_url: String, failure_reason: String },
CiRemediationAbandoned  { product_id: String, work_item_id: String, attempt_id: String, pr_url: String, failure_reason: String },
CiRemediationExhausted  { product_id: String, work_item_id: String, pr_url: String, last_failure_check: String },
```

With three subsystems now active (conflict, ci, future review-feedback), the unified generalisation `EngineRemediation{Started,Succeeded,Failed,Abandoned}` with a `kind` field becomes the natural shape; the per-flow event names can be aliases for one release.

### Composition with review-feedback (CI)

`proj_18ad7d43affb0370_2a` will land its own `blocked_reason='review_feedback'` value and its own attempt table. The composition rules:

- Both signals can be live simultaneously on the same parent (the `task_blocked_signals` side table from Q2 supports it).
- Priority order: `review_feedback > ci_failure` (a reviewer asking for changes is a human signal that should be addressed first). When the review-feedback worker is iterating, the CI fixer defers (mirror of the merge-conflict deferral from Q7).
- The unified `boss engine attempts list` (Q11) spans three subsystems.

The review-feedback design should adopt the `task_blocked_signals` side table verbatim (this design introduces it; review-feedback inherits) and the generalised `EngineRemediation*` event names.

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

### Migration additions for the CI flow

The same migration adds, in one bundled step:

```rust
fn migrate_auto_remediation(conn: &Connection) -> Result<()> {
    // (merge-conflict columns — as above)

    // CI multi-signal side table
    conn.execute_batch(r#"
        CREATE TABLE IF NOT EXISTS task_blocked_signals (
            work_item_id TEXT NOT NULL,
            reason       TEXT NOT NULL,
            attempt_id   TEXT,
            created_at   TEXT NOT NULL,
            cleared_at   TEXT,
            PRIMARY KEY (work_item_id, reason)
        );
        CREATE INDEX IF NOT EXISTS task_blocked_signals_active_idx
            ON task_blocked_signals(work_item_id, reason)
            WHERE cleared_at IS NULL;
    "#)?;

    // CI attempt counters on tasks + product default
    if !table_has_column(conn, "tasks", "ci_attempt_budget")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN ci_attempt_budget INTEGER", [])?;
    }
    if !table_has_column(conn, "tasks", "ci_attempts_used")? {
        conn.execute("ALTER TABLE tasks ADD COLUMN ci_attempts_used INTEGER NOT NULL DEFAULT 0", [])?;
    }
    if !table_has_column(conn, "products", "ci_attempt_budget")? {
        conn.execute("ALTER TABLE products ADD COLUMN ci_attempt_budget INTEGER NOT NULL DEFAULT 3", [])?;
    }

    // CI remediation attempts side table
    conn.execute_batch(/* ci_remediations DDL from Q3 */)?;

    // Manual-override suppression
    conn.execute_batch(/* ci_failure_suppressions DDL */)?;

    // Backfill task_blocked_signals from existing scalar tasks.blocked_reason
    conn.execute(
        "INSERT OR IGNORE INTO task_blocked_signals
            (work_item_id, reason, attempt_id, created_at)
         SELECT id, blocked_reason, blocked_attempt_id, COALESCE(updated_at, CAST(strftime('%s','now') AS TEXT))
           FROM tasks
          WHERE blocked_reason IS NOT NULL",
        [],
    )?;

    Ok(())
}
```

If the merge-conflict portion of this design has already shipped (`tasks.blocked_reason` and the dependency backfill exists), the CI migration is purely additive and the backfill step idempotently mirrors any in-flight scalar reason into the side table. If both portions ship together, the backfill runs once.

### Existing in-review chores with failing CI *right now*

Mirrors the merge-conflict case. The merge-poller's startup sweep (run_one_pass) probes once after migration. The new CI-detection branch fires for any in-review chore with failing required checks. Those chores transition to `blocked: ci_failure`, attempts are created, workers spawn (subject to pool capacity and budget). The activity feed records each transition.

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

### CI-specific idempotency and race conditions

#### A re-trigger and a fix race on the same head sha

The engine's pre-triage in Q4 chooses `attempt_kind='retrigger'` if every failure has unambiguous infra conclusion; else `'fix'`. The unique-key `(work_item_id, head_sha_at_trigger, attempt_kind)` allows *both* row types to exist for the same head sha — but only sequentially. If a retrigger fired and exited (`succeeded` with `consumes_budget=0`), and CI subsequently re-runs and still fails on the same head sha, the next probe's sweep is allowed to create a fix attempt for the same head sha (different `attempt_kind`). The unique key permits this.

What is *not* allowed: a fix attempt followed by another fix attempt on the same head sha. The unique key blocks it. To create a new fix attempt, either the head sha must change (new push) or the user must `boss engine ci retry` (which the engine treats as authorisation to bump and creates an attempt with a synthetic `head_sha_at_trigger` suffix; see Q11).

#### CI invariants

1. **At most one `ci_remediations` row in `status='running'` for any given `work_item_id` at any moment.** Enforced by the WHERE-guarded INSERT-then-UPDATE pattern: when creating a new attempt, prior `running` rows for the same `work_item_id` (older `head_sha`) are marked `superseded` in the same SQLite transaction as the new INSERT.
2. **The parent's `blocked_attempt_id` always points at an active CI attempt when `blocked_reason='ci_failure'`.** Maintained by the side-table's priority promotion logic on signal clear.
3. **The retire path only flips a parent it put in `blocked`.** Same WHERE-clause guard as the conflict path: includes `last_status_actor='engine'` and the attempt id.

#### Ordering with merge-poller cadence (CI)

Within a sweep, sweep_one for each PR is atomic with respect to writes to `task_blocked_signals` / `ci_remediations`. There's no within-pass race. Across passes (~30s cadence): if a fixer worker is faster than the poll cadence (rare; the worker reads the log, edits, runs tests, pushes — usually >30s), the same probe may observe the in-flight attempt's effects on the next sweep. The unique-key constraint and the `running → succeeded` state machine handle this without special logic.

### Composed edge cases

#### Checks that never start (timeout)

A push happens; CI is supposed to run; nothing in the rollup ever appears (provider outage, mis-config). Without a timeout, the parent could sit forever in `in_review` despite an effectively-failing CI gate.

**Recommendation.** Add a soft timeout on the *post-push wait*:

```rust
// In the merge poller, when we observe `ci=InFlight` for a head sha:
// - If we've been seeing InFlight for this same head sha for >30 minutes
//   without any rollup leaves appearing, log a warning.
// - If for >2 hours, fire a soft alert (activity feed: "CI hasn't started on PR #N for 2h; check provider").
// No auto-action — the engine doesn't know whether to spawn a fixer when no log exists.
```

Storage: derive from the most recent push timestamp (commit author date on `head_sha`). A side table `ci_inflight_observations` keyed by `(work_item_id, head_sha)` is an alternative if the derivation proves insufficient.

#### PR re-targeted to a different base mid-fix

The user changes the PR's base from `main` to `feat-X` while a fixer is running. The fixer's local branch is unchanged; the push still lands on `head_branch`. But the PR's `mergeable` / `statusCheckRollup` is now evaluated against the new base. Two effects:

- The original CI failures may no longer apply (different test set runs).
- A merge conflict may newly appear (different base diff).

The engine sees these on the next probe. The existing `ci_remediations` row is in `running`; its outcome is determined by the worker's push (or no-push). Once the worker exits:

- If pushed: probe re-evaluates on the new base. Old failures may be gone (probe sees `ci=Clean` or different `Failing`). The flow continues naturally.
- If didn't push (bailed): the attempt is `failed` (or `superseded` if the worker recognises the re-targeting); the parent stays blocked; the next probe re-evaluates against the new base.

We don't try to detect re-targeting per se. The probe naturally reflects whichever base the PR currently targets, and the state machine converges.

#### Branch deleted by user

User deletes `head_branch`. PR is auto-closed by GitHub. Treated as "PR closed" (Q6).

#### CI failure on a PR whose original chore is in `done`

If a `done` chore is reactivated (the PR was re-opened or a follow-up commit lands), its CI may fail. The merge-poller doesn't probe `done` chores today. Briefly: the reactivation lifecycle is a separate concern; the kanban / inspector should surface the reactivation as a state change first, at which point this design's flow naturally picks up. Not in v1 scope.

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

### CI verbs

```text
boss engine ci list   [--product <selector>] [--status running|succeeded|failed|...] [--limit 50]
boss engine ci show   <attempt-id>
boss engine ci retry  <work-item-id-or-attempt-id>     # resets budget, re-fires auto-fix
boss engine ci abandon <attempt-id> [--reason]         # mark a running attempt abandoned
boss engine ci classify <attempt-id> --as ... [--reason ...]   # worker-facing; humans can call too
boss engine ci mark-failed <attempt-id> --reason <r>           # worker-facing
boss engine ci mark-retriggered <attempt-id>                   # worker-facing

boss engine ci budget show    <work-item-id>           # shows used/budget for a chore
boss engine ci budget set     <work-item-id> <n>       # per-PR override; pass NULL via --clear to inherit
```

The `retry` verb is the critical user-escape-hatch:

```bash
$ boss engine ci retry chore_18ad…77
✓ Reset ci_attempts_used for chore_18ad…77 (was: 3/3 exhausted, now: 0/3)
✓ Cleared blocked_reason='ci_failure_exhausted'
→ Parent will re-enter in_review on next probe; engine will auto-fix on detection of failure.
```

When invoked on an *attempt id*, the engine resolves the attempt to its `work_item_id` and acts on the parent. The dual signature is for ergonomics; the user usually has the chore id, not the attempt id.

`retry` does *not* immediately spawn a worker — it resets the parent's state and lets the next merge-poller sweep do its thing. This keeps the retry behaviour aligned with normal sweeps and avoids a separate "force immediate spawn" code path. Worst-case latency is one poller interval (~30s); acceptable.

### Unified `boss engine attempts list` (three subsystems)

With three subsystems live the unification proposed earlier becomes worth shipping:

```text
boss engine attempts list [--kind conflict|rebase|ci|review-feedback] [--product ...] [--status ...]
```

The SQL is a `UNION ALL` across `conflict_resolutions`, `rebase_attempts`, `ci_remediations`, and (when it lands) the review-feedback attempts table, projecting a `kind` column. No schema unification needed.

### macOS app surface (CI additions)

The Engine tab grows a `ci` row kind. PR-card badges:

- **🟧 ci failing (1/3)** — engine still trying; small numeric badge showing budget usage.
- **🛑 ci failing (exhausted)** — engine has given up.
- **✅ ci auto-fixed** — chip on the card for 24h after a successful auto-fix (paralleling the "🔧 conflict cleared" chip).

A detail view per chore shows the failed checks, the worker's triage decision, the log excerpt, the per-file fix summary, the elapsed time. This is the user's "what did the engine do?" panel.

### Reference doc updates (CI)

`boss reference` also gains:

> A chore in `status='blocked'` with `blocked_reason='ci_failure'` is one whose
> PR was in code review but CI on the head ref turned red (typically because a
> test or lint check failed). The engine has auto-spawned a fix-CI worker that
> reads the failing job's log, attempts the smallest possible fix, pushes, and
> exits. Once CI is green again the chore returns to `in_review` automatically.
> A per-PR attempt budget (default 3) caps how many fix attempts the engine
> will try; after the budget is exhausted, the chore stays blocked with
> `blocked_reason='ci_failure_exhausted'` and the user must investigate.
> Inspect the current attempt with `boss engine ci show <attempt-id>`; reset
> the budget and retry with `boss engine ci retry <chore-id>`.

---

## Composed State Machine

Both signals (`merge_conflict` and `ci_failure`) can be active on the same parent simultaneously. The retire path (`maybe_clear_blocked`) iterates `task_blocked_signals`; each signal clears independently when its specific probe condition holds. The parent transitions back to `in_review` only when no engine-managed signal is active.

```
                ┌─────────────────────────────────────────────────────────┐
                │                                                         │
                │                       in_review                         │
                │                                                         │
                └─┬───────┬───────────────────────────────────────┬───────┘
                  │       │                                       │
                  │       │ probe: ci=Failing                     │ probe:
                  │       │ AND no active conflict_resolutions    │ mergeable=
                  │       │     no active rebase_attempt          │ CONFLICTING
                  │       │                                       │
                  │       ▼                                       ▼
                  │   ┌───────────────────────┐         ┌──────────────────────┐
                  │   │ blocked:              │         │ blocked:             │
                  │   │   ci_failure          │ ◀──────▶│   merge_conflict     │
                  │   │   (attempts < budget) │         │                      │
                  │   │                       │         │ (auto-spawn          │
                  │   │ ▶ auto-spawn fix or   │         │  resolver worker)    │
                  │   │   retrigger worker    │         │                      │
                  │   └─┬───────────┬─────────┘         └──────────┬───────────┘
                  │     │           │                              │
                  │     │ CI green  │ CI red                       │ mergeable
                  │     │ on next   │ on next probe                │ on next
                  │     │ probe     │ (still attempts left)        │ probe
                  │     │           │                              │
                  │     │           │ ┌─ attempts == budget ─┐     │
                  │     │           │ ▼                      │     │
                  │     │           │ ┌──────────────────────┐    │
                  │     │           │ │ blocked:             │    │
                  │     │           │ │   ci_failure_        │    │
                  │     │           │ │   exhausted          │    │
                  │     │           │ │                      │    │
                  │     │           │ │ ▶ no auto-spawn      │    │
                  │     │           │ │   user attention     │    │
                  │     │           │ │   required           │    │
                  │     │           │ └─┬────────────────────┘    │
                  │     │           │   │                         │
                  │     │           │   │ CI green (user fix,     │
                  │     │           │   │ or boss engine ci retry)│
                  │     │           │   │                         │
                  └─────┴───────────┴───┴─────────────────────────┘
                  (all signals cleared → return to in_review;
                   budget resets when ci_failure or ci_failure_exhausted clears)
```

Both signals can be active simultaneously. When they are, the kanban badge shows the highest-priority signal (per Q2's priority order) with the others indicated alongside.

Priority for the scalar `tasks.blocked_reason` (UI surfacing):

```
dependency > review_feedback > merge_conflict > ci_failure_exhausted > ci_failure
```

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

### Happy path (CI): detect fix → push → retire

```
┌──────────┐  ┌───────────┐  ┌─────────┐  ┌─────────────┐  ┌──────┐  ┌──────────┐  ┌─────────┐
│ GitHub   │  │ merge_poll│  │ ci_watch│  │ coordinator │  │ cube │  │ fix-CI   │  │ tasks / │
│          │  │           │  │         │  │             │  │      │  │ worker   │  │ side    │
│ CI red   │  │           │  │         │  │             │  │      │  │          │  │ tables  │
└────┬─────┘  └─────┬─────┘  └────┬────┘  └──────┬──────┘  └──┬───┘  └────┬─────┘  └────┬────┘
     │ gh pr view   │             │              │            │           │              │
     │ ◄────────────┤             │              │            │           │              │
     │ ci=Failing   │             │              │            │           │              │
     │ ────────────►│             │              │            │           │              │
     │              │ pre-checks: │              │            │           │              │
     │              │  budget ok? │              │            │           │              │
     │              │  no active  │              │            │           │              │
     │              │  conflict?  │              │            │           │              │
     │              │ ───────────►│              │            │           │              │
     │              │             │ INSERT ci_remediations    │           │              │
     │              │             │ (attempt_kind=fix,        │           │              │
     │              │             │  status=pending)          │           │              │
     │              │             │ ──────────────────────────┼───────────┼──────────────►
     │              │             │ add_blocked_signal(...)   │           │              │
     │              │             │ ──────────────────────────┼───────────┼──────────────►
     │              │             │ UPDATE tasks SET          │           │              │
     │              │             │  status=blocked,          │           │              │
     │              │             │  blocked_reason=ci_failure│           │              │
     │              │             │  blocked_attempt_id=<id>  │           │              │
     │              │             │  ci_attempts_used += 1    │           │              │
     │              │             │ ──────────────────────────┼───────────┼──────────────►
     │              │             │ RequestExecution(         │           │              │
     │              │             │  ci_remediation, <id>)    │           │              │
     │              │             │ ────────────►             │           │              │
     │              │             │              │ lease ws   │           │              │
     │              │             │              │ ──────────►│           │              │
     │              │             │              │ ws_id      │           │              │
     │              │             │              │ ◄──────────│           │              │
     │              │             │              │ spawn(prompt with log excerpt)        │
     │              │             │              │ ──────────────────────►              │
     │              │             │              │            │ classify --as tractable  │
     │              │             │              │            │ (worker)                 │
     │              │             │              │            │ ──────────────────────────►
     │              │             │              │            │ jj edit ...              │
     │              │             │              │            │ jj git push              │
     │              │ (force push)│              │            │           │              │
     │ ◄────────────┼─────────────┼──────────────┼────────────┼───────────│              │
     │              │             │              │            │ Stop      │              │
     │              │             │              │ ◄──────────┼───────────│              │
     │ (next sweep) │             │              │            │           │              │
     │ gh pr view   │             │              │            │           │              │
     │ ◄────────────┤             │              │            │           │              │
     │ ci=Clean     │             │              │            │           │              │
     │ ────────────►│             │              │            │           │              │
     │              │ maybe_clear │              │            │           │              │
     │              │  _blocked   │              │            │           │              │
     │              │ ───────────►│              │            │           │              │
     │              │             │ clear_blocked_signal(ci_failure)      │              │
     │              │             │ ──────────────────────────────────────┼─────────────►│
     │              │             │ UPDATE ci_remediations                │              │
     │              │             │  SET status=succeeded                 │              │
     │              │             │ ──────────────────────────────────────┼─────────────►│
     │              │             │ reset ci_attempts_used                │              │
     │              │             │ ──────────────────────────────────────┼─────────────►│
     │              │             │ release lease                         │              │
     │              │             │ ──────────────────────────────────────►              │
```

### Re-trigger path: engine-pre-triaged

```
... probe sees ci=Failing with all conclusions ∈ {STARTUP_FAILURE} ...

ci_watch decides attempt_kind = 'retrigger'

INSERT ci_remediations (kind=retrigger, consumes_budget=0)
add_blocked_signal(ci_failure, attempt_id)
ci_attempts_used NOT incremented

spawn worker with retrigger prompt:
    worker calls `bk build retry` or `gh run rerun --failed`
    worker calls `boss engine ci classify --as flaky_or_infra`
    worker exits

attempt status → succeeded (kind=retrigger; no push happened)

(next sweep)
ci=InFlight while re-run executes
... eventually ...
ci=Clean → retire path → parent back to in_review → budget remains at 0
```

### Budget-exhaustion path

```
... attempt 1 fails (worker pushed, CI still red on different check) ...
... attempt 2 fails (same shape) ...
... attempt 3 fails ...

ci_attempts_used = 3, budget = 3

next probe: ci=Failing on (new) head sha
ci_watch::on_ci_failure_detected:
    used >= budget → add_blocked_signal(ci_failure_exhausted)
    do NOT create ci_remediations row
    do NOT spawn worker

UPDATE tasks SET
  blocked_reason = 'ci_failure_exhausted'
  blocked_attempt_id = NULL  (or points at last attempt; choose: NULL is cleaner)
  last_status_actor = 'engine'

broadcast FrontendEvent::CiRemediationExhausted

activity feed:
🛑 CI auto-fix exhausted on PR #647 after 3 attempts.
   The PR is blocked pending human attention. Last failure: `flunge/server`.
   `boss engine ci retry chore_18ad…77` to reset and try again.
```

---

## Schema Summary

One combined `ALTER TABLE` / migration step covers both flows:

```sql
-- New columns on existing tables (both flows)
ALTER TABLE tasks    ADD COLUMN blocked_reason     TEXT;             -- 'dependency'|'merge_conflict'|'ci_failure'|'ci_failure_exhausted'|'review_feedback'|NULL
ALTER TABLE tasks    ADD COLUMN blocked_attempt_id TEXT;             -- soft FK, discriminated by blocked_reason
ALTER TABLE tasks    ADD COLUMN ci_attempt_budget  INTEGER;          -- NULL = use product default
ALTER TABLE tasks    ADD COLUMN ci_attempts_used   INTEGER NOT NULL DEFAULT 0;
ALTER TABLE products ADD COLUMN ci_attempt_budget  INTEGER NOT NULL DEFAULT 3;
ALTER TABLE products RENAME COLUMN auto_rebase_enabled TO auto_pr_maintenance_enabled;
-- (or, if auto-rebase-stacked-prs hasn't shipped:)
-- ALTER TABLE products ADD COLUMN auto_pr_maintenance_enabled BOOLEAN NOT NULL DEFAULT 1;

-- Multi-signal side table (authoritative set; tasks.blocked_reason remains as denormalised cache)
CREATE TABLE task_blocked_signals (
    work_item_id   TEXT NOT NULL,
    reason         TEXT NOT NULL,
    attempt_id     TEXT,
    created_at     TEXT NOT NULL,
    cleared_at     TEXT,
    PRIMARY KEY (work_item_id, reason)
);

CREATE INDEX task_blocked_signals_active_idx
    ON task_blocked_signals(work_item_id, reason)
    WHERE cleared_at IS NULL;

-- Merge-conflict attempts
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

-- CI remediation attempts
CREATE TABLE ci_remediations (
    id                  TEXT PRIMARY KEY,
    product_id          TEXT NOT NULL,
    work_item_id        TEXT NOT NULL,
    pr_url              TEXT NOT NULL,
    pr_number           INTEGER NOT NULL,
    head_branch         TEXT NOT NULL,
    head_sha_at_trigger TEXT NOT NULL,
    head_sha_after      TEXT,
    attempt_kind        TEXT NOT NULL,         -- 'fix' | 'retrigger'
    consumes_budget     INTEGER NOT NULL,
    failed_checks       TEXT NOT NULL,
    triage_class        TEXT,
    log_excerpt         TEXT,
    status              TEXT NOT NULL,
    failure_reason      TEXT,
    cube_lease_id       TEXT,
    cube_workspace_id   TEXT,
    worker_id           TEXT,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    UNIQUE (work_item_id, head_sha_at_trigger, attempt_kind)
);

CREATE INDEX ci_remediations_status_idx    ON ci_remediations(status);
CREATE INDEX ci_remediations_work_item_idx ON ci_remediations(work_item_id);
CREATE INDEX ci_remediations_product_idx   ON ci_remediations(product_id);

-- Manual-override suppression (CI only)
CREATE TABLE ci_failure_suppressions (
    work_item_id   TEXT NOT NULL,
    head_sha       TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    PRIMARY KEY (work_item_id, head_sha)
);
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

18. **Opt-out**. Implement the `auto_pr_maintenance_enabled` per-product flag plus the `boss/no-auto-rebase` per-PR label check. Both gate all auto-remediation flows (conflict resolution + auto-rebase + CI fixing). Acceptance: an opted-out product's conflict-watch and ci-watch paths are no-ops.

### Phase 7: CI schema and types

19. **DB migration (CI).** Add `task_blocked_signals`, `ci_remediations`, `ci_failure_suppressions`, `tasks.ci_attempt_budget`, `tasks.ci_attempts_used`, `products.ci_attempt_budget`. Backfill `task_blocked_signals` from existing `tasks.blocked_reason` scalar values. Acceptance: idempotent migration on a fresh DB and on a DB carrying the merge-conflict schema; unit tests for backfill.

20. **Protocol types (CI).** Add `CiRemediation`, `BlockedSignal`, `ci_attempt_budget`, `ci_attempts_used` to `tools/boss/protocol/src/types.rs`. Extend the `Task` wire shape with the multi-signal list. Acceptance: round-trip serialisation tests.

### Phase 8: CI detection

21. **Probe extension (CI).** Extend `PrLifecycleProbe` to carry `OpenPrStatus { mergeability, ci }`. Add `statusCheckRollup` to `gh pr view` JSON projection. Implement the latest-leaf-per-name + required-check failure predicate (Q1). Acceptance: unit tests over each `(state × mergeability × ci-leaf-set)` combination.

22. **`ci_watch` module.** Wire `on_ci_failure_detected` and `on_ci_resolved` into the merge-poller sweep dispatch. Implement the pre-check deferrals against active `conflict_resolutions` / `rebase_attempts` (Q7 composed ordering). Implement budget exhaustion path (Q3). Acceptance: integration tests driving probe → attempt → push → retire cycles; idempotency on repeated probes; deferral works.

23. **`list_chores_blocked_on_ci_failure`.** New `WorkDb` method. Acceptance: query returns only rows in `blocked: ci_failure` or `ci_failure_exhausted` with a `pr_url`.

### Phase 9: CI worker spawn and triage

> **Reconciled 2026-05-17 amendment — rebase-first inside the `fix` path.** Many CI failures on long-running PRs are caused by `main` moving (a fix landed, a dep bumped, an env changed). Rebasing onto base HEAD alone may produce green CI for free; we shouldn't burn a fix-attempt budget slot to discover that. So inside the `fix` path the dispatched worker's **first** action is `jj rebase -d main -b <branch>` followed by a force-push, then it waits for the re-run's required checks to settle. If post-rebase CI is green the worker calls `boss engine ci mark-succeeded-via-rebase <attempt-id>`; the engine flips the attempt to `succeeded` with `consumes_budget = 0` and decrements `tasks.ci_attempts_used` so the detection-side bump is refunded. Only when post-rebase CI is still red does the worker proceed to a code fix (the budget slot is then genuinely consumed, as today). The pre-spawn triage from #28 is unchanged: `retrigger`-kind attempts still bypass this path entirely.
>
> Wire surface added in this phase to support the refund path: `FrontendRequest::MarkCiRemediationSucceededViaRebase { attempt_id }`. CLI surface: `boss engine ci mark-succeeded-via-rebase --attempt-id <id>`.

24. **Execution kind: `ci_remediation`.** Add to `work_executions.kind` enum. Coordinator routes to pre-leased spawn path. Acceptance: an execution of this kind acquires a lease and binds a worker pane.

25. **`CiLogReader` trait + Buildkite impl.** Trait surface from Q4. `BuildkiteLogReader` wrapping `bk job log` / `bk build retry`. Acceptance: integration tests against a mock Buildkite returning canned responses.

26. **`CiLogReader` GitHub Actions impl.** `GithubActionsLogReader` wrapping `gh run view --log-failed` / `gh run rerun --failed`. Acceptance: mock-GHA tests.

27. **Engine pre-spawn: log-excerpt fetch.** Before spawning a fix worker, the engine reads the failing job's log tail and stores it in `ci_remediations.log_excerpt`. Acceptance: a synthetic failure with a Buildkite target_url produces a populated excerpt; GHA likewise.

28. **Engine pre-triage: fix vs retrigger.** If every failing check has `conclusion=STARTUP_FAILURE`, pre-classify as `retrigger`; else `fix`. Acceptance: unit tests over conclusion-set permutations.

29. **CI spawn prompt templates.** Implement both prompts (Q4). `spawn_flow.rs` reads `ci_remediations.log_excerpt`, `failed_checks`, `attempt_kind`, and the per-product `test_command`. The `fix` prompt instructs the worker to rebase onto base HEAD first (see amendment above) — concrete `jj git fetch / jj edit / jj rebase -d main / jj git push` commands are embedded inline. Acceptance: spawned worker pane receives templated prompt.

30. **Worker → engine markers (CI).** Implement `boss engine ci classify`, `boss engine ci mark-failed`, `boss engine ci mark-retriggered`, and (per the amendment above) `boss engine ci mark-succeeded-via-rebase`. Acceptance: workers' calls produce expected UPDATEs, and the rebase-only path leaves `tasks.ci_attempts_used` unchanged (refunded) for a `fix`-kind attempt.

### Phase 10: CI auto-retire and completion

31. **`maybe_clear_blocked` polymorphism.** Refactor the merge-poller's clear path to iterate `task_blocked_signals` and clear per-reason (Q5). Acceptance: integration test with a chore that has both `merge_conflict` and `ci_failure` signals; each clears independently when its probe condition holds.

32. **Budget reset on cycle completion.** When `ci_failure` (or `ci_failure_exhausted`) clears via the retire path, reset `tasks.ci_attempts_used = 0`. Acceptance: full cycle (failure → fix → green) leaves counter at 0.

33. **Completion-path branch (CI).** Extend `completion.rs` to inspect outgoing workers for `ci_remediations.attempt_id` bindings. On worker Stop with no push and no `mark-failed` call, default `failure_reason='no_push_no_classification'`. Acceptance: synthetic exits.

34. **Frontend events (CI).** Emit `FrontendEvent::CiRemediation{Started,Succeeded,Failed,Abandoned,Exhausted}` from the transitions. macOS app subscribes. Acceptance: integration test asserts events arrive with correct payloads.

### Phase 11: CI CLI and observability

35. **`boss engine ci` verbs.** Implement `list`, `show`, `retry`, `abandon`, `classify`, `mark-failed`, `mark-retriggered`, `budget show`, `budget set`. JSON and text. Acceptance: snapshot tests on output.

36. **Unified `boss engine attempts list` v2.** Extend the merge-conflict verb to include the `ci` kind. Acceptance: list shows entries from all three subsystems with correct `kind` column.

37. **macOS Engine tab (CI).** Extend the tab to render `ci_remediations` alongside `conflict_resolutions` and `rebase_attempts`. PR-card badges per Q11. Acceptance: visual regression tests.

### Phase 12: CI edge cases and hardening

38. **Manual-override suppression.** Implement `ci_failure_suppressions` write on manual move from `blocked: ci_failure` to other status. `ci_watch::on_ci_failure_detected` consults the suppression table. Acceptance: a manually-moved chore does not auto-flip back to blocked on the same head sha.

39. **CI never-starts soft alert.** Track first-observed-InFlight per `(work_item, head_sha)` and emit a warn at 30min, a soft alert at 2h. Acceptance: integration test with a head sha that stays InFlight for >2h emits the alert.

40. **CI churn guard.** Cross-design with item 16: count `ci_remediations WHERE work_item_id=? AND created_at > now-1h`; if ≥ 5 attempts (well above budget; the only path here is repeated manual `boss engine ci retry`), the next retry is rate-limited with an explicit prompt. Acceptance: integration test.

41. **Cross-flow ordering correctness.** Integration test driving: PR develops conflict AND CI failure simultaneously → conflict resolver fires first, completes, then CI fixer fires. Acceptance: side-table reflects both signals being active and clearing in the right order.

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
- The CI failure predicate is the latest-leaf-per-name on required checks (`isRequired=true`) with a closed set of failure conclusions. Pending/in-flight/skipped/neutral do not fire.
- A new `task_blocked_signals` side table makes the parent's blocked state a *set* of reasons rather than a scalar. The `tasks.blocked_reason` scalar remains as the denormalised "primary reason" cache for UI rendering, with an explicit priority order.
- New `tasks.blocked_reason` values: `'ci_failure'` (auto-fix in flight) and `'ci_failure_exhausted'` (budget exhausted, user attention needed). The two-value split avoids re-decoding budget state at render time.
- Per-PR attempt budget (`tasks.ci_attempts_used` / `tasks.ci_attempt_budget`) with a product-level default (`products.ci_attempt_budget`, default 3). Per-PR budget = 0 is "notify only"; budget ≥ 10 is rejected.
- Budget reset rule: a complete success cycle (failure → fix → green → parent back to `in_review`) resets `ci_attempts_used` to 0. Manual `boss engine ci retry` also resets.
- `attempt_kind` distinguishes `fix` from `retrigger`. Re-triggers do not consume budget; the engine's pre-spawn triage chooses retrigger for unambiguous infra signals (`STARTUP_FAILURE`); the worker may also classify a fix-kind attempt as `flaky_or_infra` at triage time and pivot to retriggering.
- The CI worker is briefed with an explicit triage decision tree (Q4) — infra markers, chronic flake, hard gates, then tractable. The worker's triage decision is recorded in `ci_remediations.triage_class`.
- Composed ordering across all auto-remediation flows: `rebase_attempts > conflict_resolutions > ci_remediations`. CI watch defers when an active higher-priority attempt covers the same PR.
- Opt-out remains unified under `products.auto_pr_maintenance_enabled` and the per-PR `boss/no-auto-rebase` label. A single switch governs auto-rebase, conflict resolution, and CI fixing.
- CI provider abstraction is a thin `CiLogReader` trait with concrete Buildkite and GHA impls. Provider inferred from `targetUrl` host.
- Auto-retire for CI requires *all* required checks at `SUCCESS` for the current head sha. `InFlight` blocks retire; any new `FAILURE` reopens the flow.
- Manual `boss engine ci retry <work-item-id>` is the user's escape hatch from `ci_failure_exhausted`. Resets counter; clears exhaustion; lets the next probe re-fire.

## Open Questions

- **Does the resolution worker need to read review comments?** A PR with conflict may also have pending reviewer feedback that the worker, while in the workspace, *could* address — but the project description scopes this design to mechanical conflict resolution. The recommendation here is to defer that to the `review-feedback` flow; the resolution worker does only the rebase. But: if the same chore is *also* `blocked: review_feedback`, we have two attempts on the same parent. The state model in Q2 allows only one `blocked_attempt_id`. Resolve by ordering: this design's flow defers to review-feedback if one of its attempts is active for the same parent (same shape as Q7's deferral to auto-rebase). Confirm with the review-feedback design.

- **Should we support `boss engine conflicts cancel <id>`?** Stopping an in-flight resolution worker mid-rebase. Today the user can `bossctl work cancel <agent-id>`, which would also fail the attempt via the completion path. A dedicated verb is sugar. Defer to Phase 6 if it's worth it; not critical for v1.

- **What's the engine's behaviour if `gh pr view` consistently returns `mergeable=UNKNOWN` for a PR for hours?** Q1 says "wait." But there's a degenerate case where GitHub is genuinely stuck (rare; happens during their own outages). Should the engine eventually fall back to a definitive guess via `git merge-tree` locally? Probably yes, after a long timeout (e.g. 1h of consecutive UNKNOWN). Not in v1 — first ship the GitHub-trust path; revisit if real-world telemetry shows it matters.

- **`tasks.test_command` storage.** The worker prompts (Q4) embed the product's test command. The `tasks` table doesn't have this today; `products` doesn't either. Where does it live? Suggest a new `products.test_command TEXT` column, set via `boss product update <selector> --test-command <cmd>`. Same field is useful for `auto-rebase-stacked-prs.md` Q11 and both flows in this doc; the column should be added by whichever piece lands first. Confirm.

- **PR-card badge ageing.** Q11's 24h ageing matches `auto-rebase-stacked-prs.md`. Is 24h right for conflict resolution and CI fixing? Probably; revisit if users say the badge is sticky / distracting.

- **Does the conflict-watch's `on_resolved` need to consult the auto-rebase row?** If auto-rebase put the parent into `blocked: dependency` and its retire path returns the parent to `in_review`, the conflict-watch path observing `MERGEABLE` later is a no-op (the parent isn't `blocked: merge_conflict`). Confirmed safe — no extra coordination logic needed.

- **Does the CI fixer need to interact with the conflict-diagnosis collector?** Some CI failures are downstream of an actual merge-conflict (a compile error in a file the PR touched, whose contents depend on a `main` change the PR didn't merge in). The triage decision tree (Q4 step 4) bails on compile errors in files outside the diff, which approximates "this is a rebase problem, not a fix problem." If telemetry shows this misfires, we'd need to teach the worker to consult `mergeStateStatus=BEHIND` and recommend the user `jj rebase` themselves. Defer; revisit with data.

- **Should `STARTUP_FAILURE` re-trigger pre-triage have its own budget?** The current design treats re-triggers as free (no budget). An adversarial provider that keeps returning STARTUP_FAILURE forever would burn cube leases. Recommendation: a soft cap (≥5 retriggers in 1h triggers a churn alert; no auto-action). Defer to Phase 12 churn guard (item 40).

- **Per-product log-tail size.** The default is 200 lines. Some products produce verbose logs (especially with `-v` test runners) where the relevant signal is buried deeper. A per-product `products.ci_log_tail_lines INTEGER DEFAULT 200` is cheap to add; defer until users ask.

- **Multi-job-failure aggregation.** A PR may have multiple required checks fail simultaneously. The current design picks the "worst" one for the log excerpt and asks the worker to fix all. If two failures are root-caused differently, the worker may fix one and the next probe sees the other and creates attempt 2. That's two attempts for one logical PR failure, which can quickly exhaust budget. Alternative: aggregate all failures into one prompt and ask the worker to fix the lot in a single push. v1 keeps the simple shape — the worker prompt already lists all failed checks in the failed_checks JSON, and the test-run step catches all of them. Revisit if budget-exhaustion telemetry shows this misfires.

- **Engine-direct mechanical fixes (formatter, lint).** A small subset of CI failures (run the formatter, commit the output) could be done by the engine directly without spawning a worker. Auto-rebase does engine-direct-then-escalate; merge-conflict goes straight to worker. For CI, an engine-direct pre-attempt would be cheap and would not consume budget. Defer; v1 sends everything to the worker for simplicity. Likely worth a follow-up design.

- **Notification channels for exhaustion.** Today `ci_failure_exhausted` surfaces only via the activity feed and a kanban badge. Some users may want a desktop notification or a Slack ping when the engine has given up on auto-fixing a PR. Defer to a notifications-system design; not specific to this flow.

- **Test-run flake detection on the worker side.** The worker's triage step 5 runs the project's `test_command` locally and expects green before pushing. If the local test run *itself* flakes (e.g. a test that passed locally fails on CI), the worker has no signal to distinguish that from a real fix. Currently we trust the worker's judgement; the user reviews the PR comment afterwards. A future enhancement: the worker re-runs the failing test N times locally before declaring green. Defer.

## Related Designs

- [`work-taxonomy`](work-taxonomy.md) — domain model for products, projects, tasks, chores.
- [`work-execution`](work-execution.md) — execution / run / lease lifecycle.
- [`work-kanban`](work-kanban.md) — kanban projection and the `Doing`/`Review` column contracts.
- [`auto-rebase-stacked-prs`](auto-rebase-stacked-prs.md) — closely-related design; shares the conflict-diagnosis collector, pre-leased spawn path, and `boss engine attempts` surface.
- [`chore-lifecycle-pr-closed-unmerged`](chore-lifecycle-pr-closed-unmerged.md) — adjacent merge-poller extension; the probe enum evolution composes with that work.
- [`work-dependencies`](work-dependencies.md) — owns `blocked: dependency`; this design adds the `blocked_reason` column it implicitly assumed.
- review-feedback workflow (`proj_18ad7d43affb0370_2a`) — sibling design that should share the `blocked_reason`, `blocked_attempt_id`, and engine-activity surfaces.
