# Unify merge-conflict and CI-failure remediation on one blocking-signal abstraction

Status: design (precedes the refactor). Cross-references
[`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md),
[`unify-pr-remediation-on-revisions.md`](unify-pr-remediation-on-revisions.md),
and [`revision-tasks.md`](revision-tasks.md). Supersedes the per-path framing of
T911 (cleared-signal retire) and absorbs the #1007 "CI-failure parity note".

## Problem

The engine has two near-duplicate auto-remediation paths that turn a blocking
signal on an `in_review` PR into a spawned `kind=revision` fix task:

- **merge-conflict**: signal `merge_conflict`, attempt ledger
  `conflict_resolutions` (`crz`), detection in `conflict_watch.rs`, probe in
  `merge_poller.rs` (`mergeable != CONFLICTING`).
- **CI-failure**: signal `ci_failure`, attempt ledger `ci_remediations`
  (`cir`), detection in `ci_watch.rs`, probe in `merge_poller.rs`
  (required checks green).

Parent-state transitions, revision spawn/retire, re-arm reconciliation, and
cleared-signal handling are written twice. Every fix has to be made in both
places and one half keeps getting forgotten:

- **#1007** ("keep the revision's parent in Review while the fix is in flight")
  implemented the *parent-stays-`in_review`* model — but only for the conflict
  path. The CI path still flips the parent to `blocked: ci_failure` and leaves
  it there while the CI-fix revision runs (live proof at design time: T869 /
  PR #1032 sat at `blocked: ci_failure` while its CI-fix revision T918 was in
  flight). #1007's own doc carries a "CI-failure parity note" acknowledging the
  gap.
- **T911** (cleared-signal retire should run at dispatch, not only at
  clean-stop) spans both ledgers — same defect, both paths.
- **#992** (stop-boundary PROBE_NO_PR misfire on revision execs) — same
  "handled in one path, forgotten in the other" shape.

## What is already shared (do not rebuild)

- `task_blocked_signals` side table: `(work_item_id, reason, attempt_id,
  created_at, cleared_at)`. Helpers `upsert_task_blocked_signal`,
  `active_blocked_signals`, `task_blocked_reason`
  (`tools/boss/engine/src/work/blocking.rs`).
- The merge poller's **polymorphic clear** dispatch iterates active signals /
  the scalar `blocked_reason` and routes each to its retire probe.
- Both attempt ledgers expose the same lifecycle:
  `insert_*`, `active_*_for_work_item`, `latest_*_for_work_item`,
  `mark_*_running`, `mark_*_succeeded`, `mark_*_failed`, `mark_*_abandoned`,
  `list_*`, `retry_*`, plus a `revision_task_id` soft-FK to the spawned
  revision (so the dormant pre-revision dispatch skips revision-backed rows).
- The revision substrate (`create_revision`, revision watch/retire) is already
  common to both producers.

## What diverges

| Concern | conflict | ci_failure | Verdict |
| --- | --- | --- | --- |
| upfront flip | `mark_chore_blocked_merge_conflict` | `mark_chore_blocked_ci_signal` | identical logic, different reason literal |
| retire flip | `clear_chore_blocked_merge_conflict[_for_attempt]` | `clear_chore_blocked_ci_failure` | identical logic |
| keep-parent-`in_review` while revision in flight | `record_merge_conflict_in_flight` + `clear_*_signal_only` | **missing** | **parity gap (#1007)** |
| re-arm a pre-blocked parent that has an active revision | `rearm_blocked_merge_conflict_signal` | **missing** | **parity gap** |
| cleared probe | `mergeable != CONFLICTING` | required checks green | signal-specific hook |
| attempt budget / exhaustion | n/a | `effective_ci_budget`, `ci_attempts_used`, `ci_failure_exhausted` | CI-only, preserve |
| manual-move suppression | n/a | `is_ci_failure_suppressed` | CI-only, preserve |
| never-starts in-flight alerts | n/a | `observe_ci_in_flight`, `mark_ci_inflight_alert_level` | CI-only, preserve |
| attempt kinds | single | `fix` / `retrigger` / `merge_queue_rebounce` | CI-only, preserve |

## Unified state machine

One driver, parameterised by a `SignalStrategy` (kind = `merge_conflict` |
`ci_failure`). Signal-specific behaviour is confined to small strategy hooks;
CI-only concerns (budget, suppression, in-flight alerts, attempt kinds) are
pre/post hooks the CI adapter wraps around the shared core, *not* branches
inside it.

Strategy hooks:

- `reason() -> &str` — the `blocked_reason` / `task_blocked_signals.reason`
  literal.
- `is_cleared(probe) -> bool` — `mergeable != CONFLICTING` vs checks-green.
- `active_attempt(work_item) -> Option<Attempt>` and `insert_attempt(...)` —
  over the kind's own ledger (idempotency key, churn guard live here).
- `spawn_revision(attempt) -> Result<Spawned, CreateRevisionRefused>`.

Shared `on_signal_detected(strategy, candidate, probe)`:

1. If `strategy.is_cleared(probe)` → no-op (the retire path owns clearing).
2. *(CI adapter pre-hook: budget/suppression/in-flight observe; an exhausted
   budget short-circuits to the terminal `blocked: ci_failure_exhausted`.)*
3. Attempt the upfront WHERE-guarded flip `in_review -> blocked: <reason>`.
   - **Guard miss** (parent already `blocked: <reason>`, or human-moved):
     reconcile. If there is an active fix revision, clear the parent back to
     `in_review` and `record_signal_in_flight` (the brief blocked state is
     invisible to the sweep); else re-arm the side-table signal so the retire
     path stays wired; a human-moved row is left alone.
   - **Guard applied**: insert/find the attempt (idempotent). If the churn cap
     is tripped or `spawn_revision` is refused → **keep** `blocked: <reason>`
     (terminal, human attention). On a successful spawn → **clear the flip back
     to `in_review`** and `record_signal_in_flight`.

Shared `on_signal_resolved(strategy, candidate, probe)` (the polymorphic clear):

1. If `strategy.is_cleared(probe)`: mark the active attempt `succeeded`,
   `clear_signal_only(reason)`, and — if the parent is still `blocked:
   <reason>` — `clear_chore_blocked_<reason>` back to `in_review`.
2. *(CI adapter post-hook: `reset_ci_attempts_used`,
   `clear_ci_inflight_observations`.)*

Net behaviour: the #1007 parent-state model applies to CI for free, and T911's
retire-at-dispatch is written once.

## Refactor plan

1. **DB layer** (`work/blocking.rs`): factor the parent-state helpers into
   kind-parameterised private impls and add the CI public wrappers that are
   missing — `record_ci_failure_in_flight`, `clear_ci_failure_signal_only`,
   `rearm_blocked_ci_failure_signal` — each mirroring its `merge_conflict`
   sibling. The existing conflict wrappers delegate to the shared impl so their
   behaviour is unchanged. DB mapper functions stay struct-literal (per
   `CLAUDE.md`).
2. **Driver** (`blocking_signal.rs`, new): the `SignalStrategy` trait + shared
   `on_signal_detected` / `on_signal_resolved`.
3. **Adapters**: `conflict_watch` and `ci_watch` build their strategy and
   delegate; CI keeps its budget/suppression/in-flight/attempt-kind hooks
   around the shared core. `on_ci_failure_detected` adopts the in_review model.
4. **Polymorphic clear** in `merge_poller` dispatches both kinds through
   `on_signal_resolved`.

## Tests

Parameterised over `{merge_conflict, ci_failure}` so a conflict-only or
ci-only regression cannot hide. Each kind must cover:

1. parent stays `in_review` on a successful revision spawn;
2. a pre-`blocked` parent with an active revision reconciles back to
   `in_review`;
3. a cleared signal retires the attempt and snaps the parent back;
4. churn-cap-exhausted / `create_revision`-refused stays terminally
   `blocked: <reason>`.

## Constraints preserved

- Merge-protection invariant: no engine path auto-merges a CONFLICTING or
  red-CI PR.
- Human-attention terminal: churn-cap-exhausted / `create_revision`-refused
  still surfaces as `blocked: <reason>` (`ci_failure_exhausted` for CI).
- Conflict-path behaviour from #1007 is preserved exactly; CI gains the same.
