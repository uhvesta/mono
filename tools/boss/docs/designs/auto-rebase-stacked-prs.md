# Boss: Auto-Rebase Stacked PRs on Base Merge

## Problem

Stacked PRs are routine in this repo: a worker lands a chore, opens PR A on branch `feat-A` based off `main`, then while A is in review they queue chore B and base it on `feat-A` (PR B has `baseRefName = feat-A`). The moment A merges, B's situation collapses:

- B's base ref still points at `feat-A`, but `feat-A` is about to be deleted by GitHub (the org default), and even if it survives, "merge B into the deleted/orphaned base" is meaningless.
- B's diff now contains every commit from A as well as its own — the GitHub diff view is unreadable, CI re-runs A's tests, and reviewers can't see what's new.
- A human (typically the user) has to notice, retarget B's base to `main` with `gh pr edit --base main`, rebase B's branch on `main`, force-push, and re-poke CI. If a third PR C was stacked on B, the same dance has to repeat once B lands.

Today this is manual. The user pings the worker, the worker leases a cube workspace, runs `jj rebase`, force-pushes, and retargets — for a mechanical operation that almost always succeeds without conflicts. When there *is* a conflict, the worker is the right tool. When there isn't, spending a full Claude turn on `jj rebase --to main` is pure overhead.

This doc proposes Boss handles the maintenance automatically: when `PrDetector` (PR #237) and the merge poller see PR A merge, the engine scans for any open PR whose `baseRefName` is `feat-A`, attempts an in-engine mechanical rebase, and only spawns a worker if the rebase needs human judgment (conflict). The user stops being the rebase-noticer-of-last-resort.

## Goals

- Detect "stacked PR's base just merged" automatically as part of the existing merge-detection pass.
- For mechanical (no-conflict) rebases: do them in the engine, force-push, retarget, with no worker round-trip.
- For conflict-laden rebases: spawn a worker the way we do today, with full context about which PR merged and which downstream PR needs help.
- Keep the user surface honest — they should be able to see what auto-rebases happened, opt out, and recover when something goes wrong.
- Same-product, same-repo only for v1. Cross-repo / cross-product stacks are deferred.

## Non-Goals

- **Cross-repo / cross-product PR stacks.** A PR in repo X stacked on a PR in repo Y is a real but rare case; the engine doesn't track cross-product PR relationships and adding them is a larger design. Mention as future work.
- **Auto-merging downstream PRs.** Auto-rebase ≠ auto-merge. Once B is rebased and CI is green, B still waits for human review (the user has explicitly deferred auto-merge).
- **Multi-tier dependency graphs as first-class state.** A → B → C is handled by the recursion of "B merges, C rebases" — we don't materialise a "stack" entity.
- **Detecting stacks the engine has never seen.** A PR opened directly via `gh` outside Boss is still detected by `PrDetector` (it queries GitHub by sha) and so will get auto-rebase treatment too — that's a feature, not a special case to engineer.
- **`jj`-native cross-bookmark rebase ergonomics for users.** This is engine plumbing, not a CLI verb humans invoke.
- **Heuristics that try to avoid force-push** (e.g. detecting that the base merge was a fast-forward and the rebase is a no-op). Always force-push the rebased state; idempotency comes from comparing tree shas, not from skipping the push.

## Naming

- The new module is **`auto_rebase`** (`engine/src/auto_rebase.rs`). It owns the detection-to-action pipeline.
- The unit of work it performs is a **rebase attempt**. A rebase attempt has an outcome: `mechanical_succeeded`, `escalated_to_worker`, `failed_engine_only`, `skipped_opted_out`, `skipped_no_stack`.
- The new persisted record (Q3) is the **`rebase_attempt`** row. It's not a chore, not a task — see Q3 for why.
- The merged PR is the **base PR**; the open PR being rebased is the **dependent PR** (consistent with the dependency-edge vocabulary in `work-dependencies.md`, even though no `work_item_dependencies` row is involved here).

---

## Design Question 1 — Detection Trigger

### Options

- **(a) New watcher.** A separate background loop that periodically lists open PRs and joins them against recent merges. Independent of `PrDetector`.
- **(b) Hook into `merge_poller`.** When `merge_poller::run_one_pass` detects a merged PR (the call to `mark_chore_pr_merged`), fire an in-process event that `auto_rebase` consumes. Same loop, additive scan.
- **(c) Hook into `PrDetector`'s on-Stop path.** When `completion.rs` classifies a PR as `Merged` for the worker that just stopped, kick auto-rebase from there.

### Discussion

(a) duplicates `merge_poller`'s GitHub call cadence. Anything we'd query — "is PR X merged?" — `merge_poller` already asks. Two pollers means two `gh` round-trips per PR per minute.

(b) is the right shape. `merge_poller::run_one_pass` already iterates "chores that have a PR and might be merged," already calls `gh pr view`, and already has the moment we want: *"this PR just merged."* Today its only side effect is `mark_chore_pr_merged` + a `work_item_changed` broadcast. Adding "scan for stacked dependents and rebase them" is one more side effect at the same site.

(c) catches *some* merges but only the ones that happen during a worker's run. The whole reason `merge_poller` exists (per the doc-comment in `merge_poller.rs:1-19`) is that most merges land *after* the worker has exited. (c) would miss those, which is the bulk of cases.

### Recommendation

**Pick (b).** Extend `merge_poller::sweep_one`: when a chore's status flips to `done` because of a detected merge, call into a new `auto_rebase::on_base_merged(work_item_id, pr_url)`. The PR's branch name is already in the GitHub response (we just need to ask `gh pr view` for `headRefName` alongside the existing `state, mergedAt` fields).

`PrDetector` itself doesn't need a new emit-callback abstraction in v1. The merge poller is the trigger; the on-Stop path in `completion.rs` could fire a duplicate trigger for the rare in-run merge case, but that's optional polish, not load-bearing. (See Q10.)

#### Scoping the dependent scan

When PR A on `feat-A` merges, list open PRs in the *same repo* with `baseRefName = feat-A`:

```
gh pr list --repo <owner/repo> --state open --base feat-A \
    --json number,headRefName,baseRefName,url,headRepositoryOwner
```

- Same repo only — `--base` filter prevents fan-out across repos.
- Excludes PRs whose `headRepositoryOwner` is a fork (not in scope; force-push to a fork branch is an authority problem we're not solving in v1).
- One `gh pr list` call per merged PR. With sub-second `gh` latency and merges arriving on the order of a few per hour at busy times, this is rounding error.

#### Cross-product reach

The merge poller's `PendingMergeCheck` carries a `product_id`, so we know which product's repo the merged PR was in. The `gh pr list` call uses *that* repo only. Cross-product / cross-repo stacks are out of scope; if we wanted them, we'd need a separate index of "PRs known to Boss across all products," and that's a bigger lift. Recommend explicitly: v1 scopes the dependent scan to the merged PR's own repo, no cross-product fan-out.

---

## Design Question 2 — Execution Path

### The two tiers

- **Engine-direct rebase**: engine acquires a clone (or borrows a cube workspace), runs `jj git fetch && jj rebase -d main -b <dependent-branch> && jj git push --bookmark <dependent-branch>`, then `gh pr edit <PR#> --base main`. No worker. ~5–15s round-trip in steady state.
- **Worker-driven rebase**: engine spawns a chore (well, the new `rebase_attempt` row, see Q3) and a worker takes it on, resolves conflicts, pushes the result.

### Detecting "is this rebase mechanical?"

This is the load-bearing question. Three strategies:

1. **Try-and-fall-back.** Just attempt the rebase; if `jj rebase` reports a conflict, escalate. Cleanest, no pre-check overhead, but means the workspace state is dirty after a failed rebase and has to be cleaned up before the worker takes it.
2. **Pre-check with `git merge-tree`.** `git merge-tree --write-tree main feat-A^...feat-B-tip` returns a tree sha and a non-zero exit on conflict. Cheap, doesn't touch any working state. The `jj` equivalent is awkward — `jj` doesn't have a clean dry-run mode for a rebase, it has `jj rebase --dry-run` but the command is still in flux upstream.
3. **Speculative dry-run in a scratch directory.** Throw the operation at a temp clone, see what happens, throw the clone away. Wastes time and disk, not actually safer than (1).

### Recommendation

**Pick (1) — try-and-fall-back, on a dedicated cube workspace acquired specifically for the rebase attempt.**

Reasoning:

- `jj` is the canonical VCS in this repo. A `git merge-tree` check would have to be paired with the actual `jj rebase` execution, so we're running two tools where one suffices, and `git`'s answer about conflicts isn't necessarily the same as `jj`'s (different conflict markers, jj-specific divergence resolution).
- `jj rebase` on conflict leaves the workspace in a recognisable state — `jj log` will show conflict markers in the working copy, and `jj st` will surface them. Detection is reliable.
- The "dirty workspace after failed rebase" worry is solvable cheaply: the engine's rebase attempt runs in its own cube workspace (leased for the attempt), so failure just means we hand that workspace to the worker as-is, *with the conflict already loaded in*. That's strictly better than asking the worker to redo `jj rebase` from scratch.

#### Engine-direct attempt sequence

1. **Lease a cube workspace** for the dependent PR's product. Use `cube workspace lease --task "auto-rebase <dependent-PR-url>"`. Prefer the workspace that last held the dependent PR's branch (cube remembers preferences via `--prefer`); if unavailable, take any free one.
2. **Sync state**: `jj git fetch`, then `jj edit <dependent-bookmark>` (the bookmark name comes from `headRefName` on the dependent PR).
3. **Rebase**: `jj rebase -d main -b <dependent-bookmark>`. Capture exit, stderr, working-copy state.
4. **Detect conflict**: parse `jj st --no-graph` for `Conflict in` markers, or check `jj log -r 'conflicts()' --no-graph` for any conflicted commit in the rebased range.
5. **If clean** → `jj git push --bookmark <dependent-bookmark>` (force-with-lease equivalent — `jj` push semantics are bookmark-replace, which is the right thing here), then `gh pr edit <PR#> --base main --repo <owner/repo>`.
6. **If conflicted** → leave the workspace as-is, *do not* clean up the conflict, and escalate to a worker chore that takes over the same lease (see Q5).
7. **Release lease** only on success or on a non-conflict failure. On escalation, hand the lease over to the worker.

#### When *not* to try engine-direct

- The dependent PR's branch is on a fork (`headRepositoryOwner != owner`). The engine can't push to a fork. Skip auto-rebase entirely; emit a UI notification (Q7).
- The dependent PR has unresolved review comments that requested specific commits be kept (we can't see this from the API cleanly; assume not for v1, revisit if it bites).
- The dependent PR is itself a draft. Still rebase — drafts are common in stacks.

---

## Design Question 3 — Work-Item Model

### Options

- **(A) New `system` work-item kind.** Alongside `task`, `chore`, `project_task` in the `tasks.kind` enum. Hidden from the kanban by default; surfaces in a dedicated "system" lane or a settings filter. Provides observability, idempotency, retry semantics.
- **(B) `system: bool` flag on existing chores.** Reuses chore lifecycle and storage. Kanban filters `WHERE system = 0` in its primary view.
- **(C) No work-item record at all** for engine-direct attempts; only conflict-driven rebases get a normal chore.
- **(D) New first-class `rebase_attempt` table** that is *not* a work item — it's a side log, joined to the dependent PR and the merged PR. Only when escalation happens do we materialise a chore (which links back to the rebase_attempt by id).

### Discussion

(A) is the most general — it lets any future engine-initiated background work share the same infrastructure (auto-merge, auto-label, future stuff). But "system task" widens the work-item taxonomy and forces every kanban / `boss task list` consumer to learn a `system` filter. The user has signalled openness to this in the chore notes; the cost is one well-scoped enum addition vs. permanent contamination of every list view that forgets to filter.

(B) is the least new schema. But `system` as a column on every chore means every chore CRUD path needs to think about it, every JSON shape carries the bit, and the failure mode "forgot to filter" is silent (junk in lists). Worse: `tasks.kind` is already the discriminator; adding a parallel `system` flag duplicates the role.

(C) is the simplest v1, but trades observability for simplicity. "Why didn't B get rebased?" / "How many auto-rebases ran today?" become invisible. The only retry path is "wait for the next merge of A" which never comes. Cleanups (e.g., a stale rebase that pushed but didn't retarget) become impossible to find.

(D) — a separate `rebase_attempts` table — gives the observability of (A) without polluting `tasks.kind`. The escalation path materialises a normal chore *and* keeps the `rebase_attempt` row as the audit record. It's also a natural fit for the "Engine activity" surface that we'll likely want for any future automated background work (auto-label, auto-add-reviewers, etc.), without committing the whole system-task taxonomy now.

### Recommendation

**Pick (D) for v1, and revisit (A) once we have a second engine-driven background workflow.**

The argument for (D) over (A):

- The data we want about auto-rebase isn't task-shaped. Tasks have `status` (kanban lane), `pr_url` (the PR they produced), `assignee` (none for system work). Trying to reuse task fields for engine bookkeeping ends up with awkward overloading — what does `pr_url` mean for an auto-rebase? The merged PR? The rebased PR? Both?
- An attempt has its own lifecycle (`pending → running → succeeded | escalated | failed`) that doesn't map to kanban columns.
- When the rebase escalates, we *do* create a chore. That chore is a normal `chore` row — no `system` flag, surfaces in the Doing lane like any other worker work — with `description` like *"Resolve rebase conflicts on PR #243 after PR #238 merged"* and a foreign-key `rebase_attempt_id` linking back. So the user always sees worker-driven work in the kanban; only the in-engine attempts are hidden, and they're surfaced via the engine activity feed instead.

The argument for (D) over (A) loses if/when we get three of these. At that point the right move is to introduce `system` as a work-item kind and migrate `rebase_attempt` to be one row per `tasks.kind = 'system_rebase'`. v1 is small enough that the standalone table is the cleaner ship.

### Schema sketch

```sql
CREATE TABLE IF NOT EXISTS rebase_attempts (
    id                        TEXT PRIMARY KEY,         -- 'reba_<ulid>'
    product_id                TEXT NOT NULL,
    base_pr_url               TEXT NOT NULL,            -- the merged PR
    base_branch               TEXT NOT NULL,            -- e.g. 'feat-A'
    dependent_pr_url          TEXT NOT NULL,            -- the open PR being rebased
    dependent_pr_number       INTEGER NOT NULL,
    dependent_branch          TEXT NOT NULL,            -- e.g. 'feat-B'
    head_sha_before           TEXT,                     -- sha of dependent's tip pre-rebase
    head_sha_after            TEXT,                     -- sha post-rebase, or NULL on failure
    status                    TEXT NOT NULL,            -- 'pending'|'running'|'succeeded'|'escalated'|'succeeded_via_close'|'blocked_pending_decision'|'failed'|'skipped'
    skip_reason               TEXT,                     -- non-null when status='skipped'
    failure_reason            TEXT,                     -- non-null when status='failed'
    escalated_chore_id        TEXT,                     -- FK into tasks.id when status='escalated'
    cube_lease_id             TEXT,                     -- lease the engine held during the attempt
    cube_workspace_id         TEXT,
    created_at                TEXT NOT NULL,
    started_at                TEXT,
    finished_at               TEXT,
    UNIQUE (dependent_pr_url, base_pr_url)              -- idempotency, see Q6
);

CREATE INDEX rebase_attempts_status_idx ON rebase_attempts(status);
CREATE INDEX rebase_attempts_product_idx ON rebase_attempts(product_id);
```

Bump `metadata.schema_version`. The unique constraint on `(dependent_pr_url, base_pr_url)` is the load-bearing idempotency guarantee — see Q6.

---

## Design Question 4 — Failure Modes

### Engine-direct rebase fails mid-way

Five sub-cases, in increasing severity:

1. **`gh pr list` returns nothing** — no dependents to rebase. Not a failure; the merge poller logs "no stacked PRs found for `feat-A`" and we move on.
2. **Lease unavailable.** Cube pool is exhausted. Mark the attempt `pending` (not `failed`); the merge poller's next pass will retry. A `pending` attempt older than 30 minutes auto-promotes to a worker chore on the next sweep — at that point the engine has waited long enough that the human has probably noticed the lag, and a worker can pick it up out-of-band.
3. **`jj git fetch` or `jj rebase` errors with something other than a conflict.** Network blip, jj version mismatch, transient disk issue. Mark the attempt `failed`, record `failure_reason`, retry on the next merge poller pass (bounded to 3 retries, then auto-promote to worker chore).
4. **Push rejected.** Either the dependent's branch was force-pushed by a human between fetch and push (we should retry — likely the human did the rebase manually), or the engine's auth doesn't have push rights to that branch (rare; surface as `failed` with an explicit "push rejected — auth or concurrent push" reason; do NOT auto-promote, this needs a human to look).
5. **`gh pr edit --base main` fails.** Push succeeded, retarget failed. Bad state: dependent PR's branch contains the rebased commits but its base still points at the deleted `feat-A`. Retry the retarget call directly (no rebase needed; the push already happened). If retry fails, mark `failed` with a high-priority `failure_reason`, and surface a notification to the user — this is the worst residual state and worth alerting.

The general policy: **automatic retries for transient/contention errors; promotion to worker chore only after retries exhaust *and* the failure isn't auth-shaped**. Auth failures (push rejected, retarget rejected) should stop and ask, not silently spawn a worker that will hit the same wall.

### Downstream PR has CI red after rebase

We've pushed the rebased commits. CI re-runs. Three observations:

- Whether CI passes is independent of whether the rebase was mechanical. A clean text rebase can still break a build (e.g. A renamed a function, B called it).
- The auto-rebase doesn't claim to make B mergeable — it claims to make B *no longer based on a merged branch*. CI failure is the human's signal that something needs human attention.
- We should *not* spawn a worker just because CI went red. The user has not asked for "auto-fix-CI" and that's a separate, much larger feature.

So: **the engine attempts the rebase, force-pushes, retargets. CI re-running is GitHub's job. If CI fails, the PR card surfaces it the way any other red CI does today.** Don't auto-react.

### Merged PR's branch was deleted from the remote

GitHub's default for many orgs is to auto-delete the head branch after merge. The dependent PR's `baseRefName` *still resolves* to the (now deleted) ref name as a string — that's just text in the PR record. `gh pr edit --base main` works fine even when the original base is gone (in fact this is the most common case in practice). So branch deletion is not a blocker.

The only thing that *can* break is `jj git fetch` if it tried to fetch the deleted bookmark — but `jj` handles this fine (the bookmark just goes away in the local view). No special handling needed.

### Multiple dependents stacked off the same merged base (a tree, not a chain)

PR A merges; B, C, and D were all stacked on `feat-A`. Three independent rebase attempts. They have no commits in common (other than what was on `feat-A`, which is now in `main`), so they can run in parallel.

**Recommendation: fan out, but cap concurrency at 2 in v1.** Cube workspaces are a finite shared pool with the worker spawner; running 8 simultaneous engine-direct rebases would starve the workers. Cap at 2 concurrent attempts engine-wide; queue the rest. The merge poller's next pass picks them up.

### Chains: A → B → C, where B was already stacked on A

A merges. B's auto-rebase succeeds, B is now based on `main` (and so is its branch on the remote, and the PR's `baseRefName` is now `main`). C was based on `feat-B`; C's `baseRefName` is still `feat-B`. C is *not* affected by A's merge in the GitHub view — it's correctly based on B, which still hasn't merged.

So the chain doesn't trigger anything special: the auto-rebase only fires when *C's base PR (B)* merges. Which is correct.

---

## Design Question 5 — Worker Authority on Escalation

### The problem

A normal chore creation hands the worker a fresh cube workspace and tells it "do this work, then push to your own branch." For an escalated rebase chore, the worker has to push to *someone else's* branch (e.g. Riker opened PR #243 on bookmark `riker/feat-B`; the worker spawned for the auto-rebase chore is Worf). Two questions:

1. Does the worker have *push* rights to that branch? On the same `gh` token, yes — the token belongs to the user's GitHub account and has write access to all their branches. So in this single-user-but-many-personas setup, no auth mismatch in v1.
2. Should the engine pass any extra context to the worker so it knows it's working on someone else's branch? Yes — the chore description should explicitly call out *which PR*, *which branch*, and *that the cube workspace already contains the failed rebase state*.

### Recommendation

**No auth changes for v1.** The existing spawn flow injects no GitHub-user-specific secrets; it relies on the local `gh` config inherited from the user's environment. That config has push rights to all the user's branches. Cross-personas works.

**Hand off the cube lease.** The escalation path:

1. Engine has a cube workspace with the failed rebase loaded (conflicted commits visible in `jj st`).
2. Engine creates a chore with `description` like:

   > Rebase PR #243 (`riker/feat-B`) onto `main`. The base PR #238 (`feat-A`) merged at 2026-05-07T14:22:01Z. The cube workspace already has the failed `jj rebase -d main` loaded — `jj st` shows the conflicted files. Resolve, `jj squash` if appropriate, push with `jj git push --bookmark riker/feat-B`, and run `gh pr edit 243 --base main` once the push succeeds. PR will need its CI re-run (just push); do not change anything else.

3. Engine creates a `work_executions` row for the chore *with the existing cube_lease_id, cube_workspace_id, workspace_path pre-populated* — i.e. the spawn flow takes a "pre-leased" path that skips the lease step. The worker session inherits the workspace as-is.
4. The `rebase_attempts.escalated_chore_id` records the link.
5. The spawn flow proceeds normally from there: pane spawned, transcript starts, worker takes over.

The pre-leased spawn path doesn't exist today — every spawn currently starts with `cube workspace lease`. Adding "use this pre-existing lease" is a small extension to `spawn_flow.rs`; the chore description spells out the contract for the new path in Q5's follow-up chore.

### Cross-account / fork scenarios

If a future world has multiple GitHub accounts (e.g. a service account for the engine, a personal account for the user), or if the dependent PR comes from a fork:

- **Service account + personal user**: the engine's `gh` would push as the service account, leaving a different commit author than the user expects. v1 doesn't do this — same `gh` config throughout — but doc it as a future consideration.
- **Fork PR**: out of scope (Q2 already excludes them at engine-direct stage; same exclusion applies to escalation — we can't push to a fork's branch).

---

## Design Question 6 — Ordering and Idempotency

### The hazard

PR #238 merges. Engine starts an auto-rebase of #242 (which was based on #238). Mid-rebase, PR #244 also merges (#244 was *also* based on #238 — a tree, not a chain). The merge poller's next pass sees #244, finds the same dependent #242 in its `gh pr list --base feat-244` query? No — #244 has a different branch. But what if #242 was based on #244 instead, and we're racing two passes against the same dependent?

Or worse: PR A merges, B is rebased onto main, then a week later A is reverted (which on GitHub is itself a merge of a revert PR, not an unmerge). Does the merge poller see that as a new merge of `feat-A`? No, because the revert PR has its own branch.

### The actual idempotency invariant

A `rebase_attempt` is identified by `(dependent_pr_url, base_pr_url)`. The schema's `UNIQUE (dependent_pr_url, base_pr_url)` constraint means the engine cannot accidentally start two attempts for the same pair. If the merge poller fires twice for the same merged PR (which it can, in pathological retry scenarios), the second `INSERT` fails, and the existing attempt's status is consulted:

- `pending` / `running` → no-op, an attempt is already in flight.
- `succeeded` → no-op, already done.
- `escalated` → no-op, the worker chore is handling it.
- `failed` (transient retries exhausted) → don't retry without human intervention; surface in the failures view.
- `skipped` → no-op (e.g. fork, opted-out — see Q8).

### Chains under pressure

If A and B both merge before the engine has rebased C (which was on B, which was on A):

- After A merges: engine sees C is based on `feat-B`, NOT `feat-A`. So C is not in scope for A's auto-rebase pass — only B is.
- A's pass attempts to rebase B onto main. Depending on how fast B then merges:
  - If B merges *after* A's rebase of B succeeds: the rebased B is what merges. Then B's merge triggers C's rebase onto main. Correct chain.
  - If B's merge happens *before* the engine rebases B: A's rebase of B is racing the merge. The unique key on `(dependent='feat-B', base='feat-A')` means we record the attempt; the rebase may push B forward, *then* B is merged. CI green is the merge gate; the user merging B by hand sees a normal-looking PR. C's rebase fires from B's merge.
- If both A and B merge in the same merge-poller sweep: the sweep processes them in `updated_at ASC` order today (`work.rs:1613`). A is older, so A is processed first; B's chore probably also flips to `done` in the same sweep, and B's auto-rebase pass for C runs against a B-branch that's now mainline. No conflict.

So the order is enforced by `merge_poller`'s existing iteration order (oldest merge first), and the unique-key invariant on `rebase_attempts` prevents double-processing.

### Recommendation

**Don't model "stacks" as a graph in v1.** Each base-PR-merge → dependent-PRs-list → per-dependent rebase is a flat fan-out. Recursion is implicit: B's eventual merge fires C's rebase via the same mechanism. The `UNIQUE (dependent_pr_url, base_pr_url)` constraint in `rebase_attempts` is sufficient idempotency; ordering via `merge_poller`'s `updated_at ASC` sweep is sufficient ordering.

If a real-world tree fan-out turns out to require explicit batching (e.g. five PRs based on the same merged base, all needing rebases that touch the same files), the fix is a per-product mutex around the engine-direct rebase path, not a graph model.

---

## Design Question 7 — UI Surfacing

### What the user should see

- **Successful engine-direct rebase**: a passive, low-volume notification. The user didn't ask for this; surfacing it loudly (a toast, a modal) is annoying. A quiet entry in an "Engine activity" feed (new in v1, see below) plus a small badge on the dependent PR card saying *"auto-rebased after #238 merged"* with a click-through to the activity entry.
- **Failed engine-direct rebase that escalated to a worker**: visible as the worker chore now in the Doing lane (normal chore — appears as any other worker run). The chore's description carries the context. The activity feed records the escalation.
- **Failed engine-direct rebase that did NOT escalate (auth-shaped failure)**: this is the loudest case. A persistent banner on the dependent PR card: *"auto-rebase failed: push rejected, needs attention."* Click → activity entry with full failure_reason and a "retry" button.
- **Skipped (fork, opted-out)**: just the activity entry, no card-level surfacing. Skipping a fork PR is the expected behaviour.

### Engine activity feed

There is no engine activity feed today. v1 doesn't *need* a full feed — we can render the `rebase_attempts` table as a list view, ordered by `created_at DESC`, with a filter (`status`, `product_id`). CLI surface:

```
boss engine rebase-attempts list [--product <selector>] [--status succeeded|failed|escalated|...] [--limit 50]
boss engine rebase-attempts show <attempt-id>
boss engine rebase-attempts retry <attempt-id>      # only valid for status=failed
```

The macOS app gets a small "Engine" tab next to "Workers" / "Work" with the same list rendered as a table.

### PR card badge

When a Boss kanban card has an associated `pr_url` and that PR is the *dependent* of a successful rebase attempt within the last 24h, render a small "↶ rebased" chip in the card footer. Click → activity feed scrolled to the attempt. After 24h the chip ages out (the data is still available via the activity view, but the card stops calling attention to it).

### Notifications

No system / OS notifications in v1. The activity feed is enough. If the user wants notifications for the failure case, that's a follow-up wired through the existing `bossctl agents probe` notification surface.

---

## Design Question 8 — Opt-Out

### Granularity options

- **Global**: one switch, on or off for the whole engine.
- **Per-product**: each product has an `auto_rebase` boolean.
- **Per-PR**: a label like `boss/no-auto-rebase` on the open PR opts it out.

### Recommendation

**Per-product, defaulting to ON. Per-PR opt-out via label as a secondary lever.**

Reasoning:

- A global switch is too coarse — it forces "all repos behave the same," which doesn't match how some teams operate (a team may want auto-rebase on their feature repo and not on a shared library where stacked PRs are rare and reviews are sensitive).
- Per-product covers the common "this product's team prefers manual control" case without per-PR fiddling.
- Per-PR via label covers the rare "I have one weird PR that should not be touched" case without a code change. The label name `boss/no-auto-rebase` is the same shape as other GitHub-label-driven Boss conventions if we adopt any; for now it's just a string the engine looks for in the PR's labels list.

Storage: `products.auto_rebase_enabled BOOLEAN NOT NULL DEFAULT 1`. CLI:

```
boss product update <selector> --auto-rebase on|off
```

Per-PR label is read via `gh pr view --json labels` during the auto-rebase pass; the cost is negligible (we're already calling `gh pr view` in the merge poller).

---

## Design Question 9 — Conflict Recovery and Force-Push Side Effects

### The key fact

When the engine (or a worker) force-pushes a rebased branch, GitHub treats the new commits as a fresh head. Per GitHub's defaults, **all existing review approvals on the PR are dismissed** when a reviewable commit is pushed. The PR transitions back to "waiting for review" until someone re-approves.

This is a real cost. A reviewer who already +1'd PR B before A merged is now expected to re-approve. For a 2-commit mechanical rebase that didn't actually change B's content (just changed its parent), this is annoying friction.

### Options to mitigate

1. **Do nothing.** Accept that a force-push dismisses approvals; the human re-approves. This is the only behaviour that's safe across all org configurations.
2. **Detect "approvals will be dismissed" via the GitHub Branch Protection API and skip auto-rebase if so.** Too defensive — dismissal is the *default*, not an exceptional configuration. Skipping any time it would dismiss means skipping nearly always.
3. **Try to opt out of dismissal via the API.** GitHub does not expose a per-push override for this (the org-level setting `dismiss_stale_reviews` is the only knob; we can't temporarily disable it from a PR-side action).
4. **Comment on the PR after a successful auto-rebase**: *"This PR was auto-rebased onto `main` after #238 merged. Approvals were dismissed by GitHub's branch protection; please re-approve when you have a moment."* This converts the surprise into a notification.

### Recommendation

**(1) + (4).** Accept the dismissal; post a single comment on the PR explaining what happened and what's needed. The comment is informational, helps the approving reviewer see why their +1 vanished, and costs us nothing.

The comment template:

```
🤖 boss auto-rebased this PR onto `main` after #238 merged. The branch was force-pushed; per branch protection, prior approvals have been dismissed. Re-approve when ready. Original head sha: <head_sha_before>; new head: <head_sha_after>.
```

`<head_sha_before>` and `<head_sha_after>` are recorded in the `rebase_attempts` row, so users can verify the rebase didn't touch anything unexpected with `git log <before>..<after>` — which for a clean rebase will show only the moved commits.

### Other side effects

- **Required status checks re-run.** Expected; not a behaviour change worth surfacing.
- **PR comment thread tied to outdated commit shas.** GitHub handles this — comments on outdated commits are folded under "outdated" automatically. No mitigation needed.
- **CODEOWNERS re-evaluated.** The rebase doesn't change file ownership, so CODEOWNERS should land in the same place. No mitigation needed.

---

## Design Question 10 — PR-Detection Symbiosis

### What `PrDetector` and `merge_poller` emit today

- `merge_poller::sweep_one` calls `WorkDb::mark_chore_pr_merged` on a successful merge detection, then `publisher.publish_work_item_changed(product_id, work_item_id, "pr_merged")`.
- `completion.rs::CompletionHandler` calls into the same DB path during a worker's Stop event, with a similar publication.

There is no "merged-PR-event" callback abstraction. Both paths just mutate the DB and publish a generic `work_item_changed` topic event.

### Options

- **(i)** Add an explicit callback / observer trait `MergeObserver` that auto-rebase implements, registered with `merge_poller` at startup.
- **(ii)** Subscribe `auto_rebase` to the existing `work_item_changed` topic and filter by `reason == "pr_merged"`.
- **(iii)** Direct call from `merge_poller::sweep_one` into `auto_rebase::on_base_merged` — same module dependency the merge poller already has on `WorkDb` and `ExecutionPublisher`.

### Recommendation

**Pick (iii) for v1.** The auto-rebase subsystem is engine-internal; we don't need a generic observer abstraction yet, and pub/sub adds latency for no benefit (we *want* the auto-rebase scan to start as soon as the merge is detected, not after a topic round-trip).

`merge_poller::sweep_one` becomes:

```rust
async fn sweep_one(...) -> bool {
    // ... existing detection logic ...
    if !state.merged { return false; }
    let updated = match work_db.mark_chore_pr_merged(...) { ... };

    // NEW: kick auto-rebase. Fire-and-forget; auto_rebase has its own
    // error handling and persistence (rebase_attempts). The poller
    // never blocks on the rebase.
    let auto_rebase = auto_rebase.clone();
    let product_id = candidate.product_id.clone();
    let pr_url = candidate.pr_url.clone();
    tokio::spawn(async move {
        auto_rebase.on_base_merged(&product_id, &pr_url).await;
    });

    publisher.publish_work_item_changed(...).await;
    true
}
```

When (and if) we get a second consumer of merge events (e.g. auto-label, auto-archive), we can refactor to (i) or (ii). For one consumer, the direct call is simpler and faster.

### Should `PrDetector`'s on-Stop path also fire?

The on-Stop path detects merges that happen during a worker's run — rare, but possible (the worker pushed, the PR auto-merged immediately because all checks were already cached green from a prior CI run). If the merge is detected on Stop and `mark_chore_pr_merged` is called there, the same auto-rebase trigger should fire. Add the same direct call from `completion.rs`'s post-merge branch.

This is tiny (one extra call site) and avoids a window where a same-run merge skips auto-rebase until the next merge poller pass, which could be a minute away.

---

## Design Question 11 — Merge Conflict Handling

### Why this needs its own section

Q2 establishes the mechanical-vs-conflict split and Q5 covers the auth side of escalation, but neither answers the operational question: *what does the worker actually do with the conflict, and how does the engine set them up to succeed?* Conflicts are the case where the engine has done the cheap part and a human-in-the-loop (the worker) has to do the expensive part — so the loading dock matters. A worker that opens the chore and sees "rebase failed, fix it" is going to spend a turn re-running `jj rebase` to figure out what went wrong; a worker that opens the chore and sees a structured diagnosis can go straight to resolving.

### Conflict diagnosis the engine collects before escalating

Between detecting the conflict and creating the chore, `auto_rebase` runs a small diagnostic pass against the post-rebase workspace state. None of these calls mutate anything — they just gather context that's expensive for the worker to recompute:

1. **Conflicted-file list**. `jj st --no-graph` lists conflicted files; `jj log -r 'conflicts()' --no-graph -T '...'` lists the conflicted commits in the rebased range.
2. **Per-file conflict shape**. For each conflicted file, run `jj resolve --list <file>` to get the conflict marker view (jj's three-way conflict format: base + two sides). For text files under ~200 lines, capture the full conflict block; for larger files, capture the first 60 lines around the first conflict marker plus a count of total markers.
3. **Upstream change summary**. `gh pr view <base-PR#> --json files,additions,deletions` gives the merged PR's file-level footprint. The intersection of *that* file set with the dependent's conflicted-file set is exactly the *"what did the upstream change touch that we also touched"* answer.
4. **Dependent-side commits in the rebased range**. `jj log -r 'main..<dependent-bookmark>' --no-graph -T 'change_id ++ " " ++ description.first_line()'` — so the worker sees which of their own commits are in play.
5. **Build / test signal (optional, time-boxed)**. If the conflict is in a small set of files (≤ 5), kick a 30-second `bazel query 'deps(//path/...)'` to identify which targets the conflicted files belong to. Skip if it would take longer than the 30s budget.

All of this goes into the escalated chore's `description` field as a structured block, see below.

### Chore description template (escalated rebase)

```
## Auto-rebase escalated to manual conflict resolution

**Dependent PR**: #243 — `riker/feat-B`
**Base PR (merged)**: #238 — `feat-A` (merged 2026-05-07T14:22:01Z by user)
**Workspace**: pre-loaded with the failed `jj rebase -d main` state. `jj st` shows the conflict markers; do not re-run `jj rebase`.

### Conflicted files (3)

- `tools/boss/engine/src/work.rs` — both sides modified `fn list_chores_pending_merge_check` (base PR refactored signature; dependent PR added a new branch). 1 conflict marker.
- `tools/boss/engine/src/coordinator.rs` — base PR removed `CoordinatorContext::stale_lease_id`; dependent PR still references it. 2 conflict markers.
- `tools/boss/engine/Cargo.toml` — both sides bumped `serde` (different versions). 1 conflict marker.

### Dependent commits in flight (2)

- `wmnpqxyl` "engine: add new branch for stale-lease detection"
- `kvqrtsuv` "engine: wire stale-lease detection into coordinator"

### Suggested approach

1. Run `jj st` to confirm the workspace state.
2. For each conflicted file, run `jj resolve <file>`. The base PR's intent (see the diff at <merged-PR-url>) and the dependent PR's intent are both in the conflict block.
3. Once all files resolve, run `bazel test //tools/boss/...` to verify nothing broke. Iterate until green.
4. `jj git push --bookmark riker/feat-B`
5. `gh pr edit 243 --base main --repo <owner/repo>`
6. `gh pr comment 243 --body "Resolved rebase conflicts after #238 merged. ..."` — see the comment template in `tools/boss/docs/designs/auto-rebase-stacked-prs.md` Q11.

### Stop conditions (do NOT push if any of these apply)

- The merged PR (`#238`) appears to have done the same work this PR was attempting (semantic obsolescence). Close the dependent PR with a comment instead.
- Resolving the conflict requires a non-mechanical product decision (e.g. picking between two divergent API shapes). Stop, comment on the PR with the question, and ping the user. Set this chore's status to `blocked`.
- Tests fail in a way that suggests the dependent's design no longer makes sense post-merge (e.g. the abstraction the dependent extends has been removed). Same as above — stop, comment, block.
```

### Validation rule: tests-green before push

A rebase that compiles is the minimum bar; a rebase that doesn't break tests is the bar we want. The worker chore description must specify the project's test command (for the boss product, that's `bazel test //tools/boss/...`; for other products, the engine looks up `products.test_command` if present, or omits the validation step if not configured) and the worker is expected to run it before pushing. CI will re-run on push regardless, but a green local run prevents a "rebase, push, CI red, second push to fix" thrash.

If the engine can't determine a sensible test command for the product, the chore description omits step 3 and the worker just pushes; we accept that the post-rebase CI may be the first signal.

### Stop conditions in detail

Three patterns where the worker should *not* push a resolved rebase but instead stop and surface the situation:

1. **Semantic obsolescence.** The merged PR's diff and the dependent PR's diff overlap heavily and accomplish the same thing. The dependent is no longer needed. The worker closes the dependent PR with `gh pr close <PR#> --comment "Closing — the work in this PR was superseded by #<merged-PR>."`, marks the chore `done`, and the engine records the rebase attempt as `succeeded_via_close` (a new substatus we add to `rebase_attempts.status`). The user sees this in the activity feed and can object if the worker misjudged.

2. **Product decision required.** The conflict needs a human to choose between two valid resolutions (e.g. the merged PR named a function `foo_v2` and the dependent named the same function `foo_async`; both are reasonable). The worker comments on the dependent PR describing the decision, then sets the chore status to `blocked` (which surfaces it on the kanban Blocked lane) and stops. The engine records the rebase attempt as `blocked_pending_decision`.

3. **Architectural mismatch.** The merged PR removed the abstraction the dependent was extending. The dependent isn't obsolete — it might still be wanted — but it needs re-scoping or splitting, which is well beyond a rebase. Same handling as (2): comment, block, stop.

In all three cases, the dependent PR's branch is *not* pushed, the GitHub state is unchanged, and the user has a clear breadcrumb (kanban + activity feed entry) to take over.

### What about conflicts the worker resolves "incorrectly"?

A worker could in principle resolve a conflict in a way that looks clean (compiles, tests pass) but loses the dependent's actual intent — e.g. silently dropping the dependent's modification of a function in favor of the merged PR's version. v1 trusts the worker to flag this in the PR comment; the human reviewer of the dependent PR is the second line of defence. The PR comment template (next subsection) specifically asks the worker to enumerate the resolutions.

### PR comment template (post-resolution)

The worker's final step before marking the chore done is a comment on the dependent PR explaining what they resolved. This is *the* surface where the human reviewer sees what the auto/manual rebase actually did:

```
🤖 boss escalated this auto-rebase to manual conflict resolution after #238 merged.

Resolutions:
- `tools/boss/engine/src/work.rs`: kept the dependent's new branch and adapted it to the refactored `list_chores_pending_merge_check` signature.
- `tools/boss/engine/src/coordinator.rs`: removed the stale `CoordinatorContext::stale_lease_id` references; the dependent's stale-lease detection now reads from the new `lease_table` API introduced in #238.
- `tools/boss/engine/Cargo.toml`: kept the higher `serde` version from #238.

`bazel test //tools/boss/...` is green. Force-push retargeted base from `feat-A` to `main`. Per branch protection, prior approvals have been dismissed; please re-review.

Original head: a3f2c1e8; new head: 7d9e4b23.
```

### Risk on stop conditions: false-positive "obsolescence"

A worker that closes a dependent PR mistakenly classifying it as obsolete is the worst residual outcome — the dependent's work is lost (the branch may be auto-deleted by the org, the PR is marked closed without merge). Mitigation: the close path emits a more visible activity-feed entry than a normal succeeded rebase (red, with explicit "review needed" badge), and the engine waits 24h before allowing the cube workspace's branch to be cleaned up so the user can `jj git fetch && gh pr reopen <PR#> && jj git push --bookmark <branch>` to recover. Document this recovery path in the engine README.

### Recommendation summary

- Engine collects a structured conflict diagnosis before escalating (files, conflict shapes, upstream-change footprint, dependent commits).
- Chore description embeds the diagnosis verbatim plus the resolution playbook.
- Worker is required to run product-configured tests before pushing; if no test command is configured, omit the step.
- Worker has three explicit stop conditions (obsolescence, product decision, architectural mismatch) — surface each via PR comment + kanban `blocked` + activity-feed entry.
- Post-resolution PR comment template enumerates the per-file resolutions for the human reviewer.

---

## Sequence Diagrams

### Happy path: engine-direct rebase

```
┌──────────┐  ┌────────────┐  ┌─────────────┐  ┌──────┐  ┌────────────┐  ┌────────────┐
│ GitHub   │  │ merge_poll │  │ auto_rebase │  │ cube │  │  jj / gh   │  │ rebase_att │
│ (PR A    │  │            │  │             │  │      │  │  (shell)   │  │  (sqlite)  │
│  merged) │  │            │  │             │  │      │  │            │  │            │
└────┬─────┘  └─────┬──────┘  └──────┬──────┘  └──┬───┘  └─────┬──────┘  └─────┬──────┘
     │ (gh pr view) │                │            │            │                │
     │◄─────────────┤                │            │            │                │
     │ state=MERGED │                │            │            │                │
     │──────────────►                │            │            │                │
     │              │ on_base_merged │            │            │                │
     │              │ ──────────────►│            │            │                │
     │              │                │ gh pr list │            │                │
     │              │                │ --base feat-A           │                │
     │              │                │────────────┼───────────►│                │
     │              │                │            │  [PR B,C]  │                │
     │              │                │◄───────────┼────────────│                │
     │              │                │ INSERT (B, A) status=pending             │
     │              │                │────────────┼────────────┼───────────────►│
     │              │                │ INSERT (C, A) status=pending             │
     │              │                │────────────┼────────────┼───────────────►│
     │              │                │ lease workspace (B)     │                │
     │              │                │───────────►│            │                │
     │              │                │            │ ws_id      │                │
     │              │                │◄───────────│            │                │
     │              │                │ UPDATE B status=running, lease=...       │
     │              │                │────────────┼────────────┼───────────────►│
     │              │                │ jj git fetch && jj rebase -d main        │
     │              │                │────────────┼───────────►│                │
     │              │                │            │   OK,      │                │
     │              │                │            │ no conflict│                │
     │              │                │◄───────────┼────────────│                │
     │              │                │ jj git push --bookmark feat-B            │
     │              │                │────────────┼───────────►│                │
     │              │                │            │   OK       │                │
     │              │                │◄───────────┼────────────│                │
     │              │                │ gh pr edit B --base main                 │
     │              │                │────────────┼───────────►│                │
     │              │                │            │   OK       │                │
     │              │                │◄───────────┼────────────│                │
     │              │                │ gh pr comment B "auto-rebased…"          │
     │              │                │────────────┼───────────►│                │
     │              │                │ release lease           │                │
     │              │                │───────────►│            │                │
     │              │                │ UPDATE B status=succeeded, head_sha_after│
     │              │                │────────────┼────────────┼───────────────►│
     │              │                │ (loop for C: same shape)                 │
```

### Conflict escalation: engine attempts, escalates to worker

```
┌────────────┐  ┌─────────────┐  ┌──────┐  ┌──────────┐  ┌────────────┐  ┌────────────┐
│ merge_poll │  │ auto_rebase │  │ cube │  │ jj/gh    │  │ spawn_flow │  │ worker     │
│            │  │             │  │      │  │ (shell)  │  │            │  │ (Claude)   │
└─────┬──────┘  └──────┬──────┘  └──┬───┘  └────┬─────┘  └─────┬──────┘  └─────┬──────┘
      │                │            │           │              │               │
      │ on_base_merged │            │           │              │               │
      │───────────────►│            │           │              │               │
      │                │ list deps  │           │              │               │
      │                │───────────────────────►│              │               │
      │                │                        │ [PR B]       │               │
      │                │◄───────────────────────│              │               │
      │                │ INSERT (B, A) status=pending          │               │
      │                │ lease ws   │           │              │               │
      │                │───────────►│           │              │               │
      │                │ jj rebase  │           │              │               │
      │                │───────────────────────►│              │               │
      │                │                        │ Conflict!    │               │
      │                │◄───────────────────────│              │               │
      │                │ UPDATE B status=escalated             │               │
      │                │ create_chore("Resolve rebase…")       │               │
      │                │ + chore.execution(cube_lease_id=...)  │               │
      │                │ UPDATE B escalated_chore_id=chore_X   │               │
      │                │                        │              │               │
      │                │ pre-leased spawn(chore_X, ws=existing)│               │
      │                │──────────────────────────────────────►│               │
      │                │                        │              │ pane spawned  │
      │                │                        │              │──────────────►│
      │                │                        │              │               │ jj st (sees conflict)
      │                │                        │              │               │ resolve files
      │                │                        │              │               │ jj squash / jj describe
      │                │                        │              │               │ jj git push --bookmark feat-B
      │                │                        │              │               │ gh pr edit B --base main
      │                │                        │              │               │ gh pr comment "rebased manually"
      │                │                        │              │               │ STOP
      │                │ (chore Stop → completion path → in_review → done on PR merge later)
```

The escalated chore from this point looks like any other chore in the kanban: it appears in Doing, transitions to Review when the worker pushes, and to Done when its (no-op) PR detection sees the dependent PR is updated. Note: the chore does NOT open a new PR — it pushes to an existing branch — so the completion path's "find a PR for the worker's commits" call returns the *dependent* PR (B) as the associated PR, and that's correct.

---

## Schema and Wire Summary

### New table

```sql
CREATE TABLE rebase_attempts (
    id                        TEXT PRIMARY KEY,
    product_id                TEXT NOT NULL,
    base_pr_url               TEXT NOT NULL,
    base_branch               TEXT NOT NULL,
    dependent_pr_url          TEXT NOT NULL,
    dependent_pr_number       INTEGER NOT NULL,
    dependent_branch          TEXT NOT NULL,
    head_sha_before           TEXT,
    head_sha_after            TEXT,
    status                    TEXT NOT NULL,
    skip_reason               TEXT,
    failure_reason            TEXT,
    conflict_diagnosis        TEXT,                     -- JSON blob from Q11 (files + shapes)
    escalated_chore_id        TEXT,
    cube_lease_id             TEXT,
    cube_workspace_id         TEXT,
    created_at                TEXT NOT NULL,
    started_at                TEXT,
    finished_at               TEXT,
    UNIQUE (dependent_pr_url, base_pr_url)
);
CREATE INDEX rebase_attempts_status_idx  ON rebase_attempts(status);
CREATE INDEX rebase_attempts_product_idx ON rebase_attempts(product_id);
```

### Column adds

```sql
ALTER TABLE products ADD COLUMN auto_rebase_enabled INTEGER NOT NULL DEFAULT 1;
ALTER TABLE products ADD COLUMN test_command        TEXT;   -- e.g. 'bazel test //tools/boss/...'
```

`test_command` is consulted by the escalated-chore template (Q11) — when null, the worker's chore description omits the "run tests before push" step.

Bump `metadata.schema_version`.

### Protocol additions (`tools/boss/protocol/src/types.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseAttempt {
    pub id: String,
    pub product_id: String,
    pub base_pr_url: String,
    pub base_branch: String,
    pub dependent_pr_url: String,
    pub dependent_pr_number: u64,
    pub dependent_branch: String,
    pub head_sha_before: Option<String>,
    pub head_sha_after: Option<String>,
    pub status: String,                  // see RebaseAttemptStatus enum
    pub skip_reason: Option<String>,
    pub failure_reason: Option<String>,
    pub escalated_chore_id: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRebaseAttemptsInput {
    pub product: Option<String>,
    pub status: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryRebaseAttemptInput {
    pub attempt_id: String,
}
```

`Product` gains `pub auto_rebase_enabled: bool`.

### Topic / event

Reuse `work_item_changed` for the dependent PR's chore (it's already broadcast on every status flip). For attempt-level updates (e.g. the user wanting a live view of `pending → running → succeeded`), add a new topic `engine.rebase_attempts` that publishes a `RebaseAttempt` snapshot on every state transition. Subscribers do a full re-render; tiny payload, low rate.

### CLI verbs

```
boss engine rebase-attempts list  [--product <selector>] [--status <s>] [--limit N]
boss engine rebase-attempts show  <attempt-id>
boss engine rebase-attempts retry <attempt-id>          # only when status=failed
boss product update <selector>    --auto-rebase on|off
```

### Engine module split

- `engine/src/auto_rebase.rs` — new module. Public entry point: `AutoRebase::on_base_merged(product_id, base_pr_url)`. Internal helpers: `discover_dependents`, `attempt_rebase_one`, `escalate_to_worker`. Uses traits for `gh` and `jj` shell-out (`GhCli`, `JjCli`) so we can stub them in tests.
- `engine/src/merge_poller.rs` — call `auto_rebase.on_base_merged(...)` from `sweep_one` after `mark_chore_pr_merged`.
- `engine/src/completion.rs` — call the same hook from the on-Stop path after a `Merged` classification.
- `engine/src/spawn_flow.rs` — extend `WorkSpawner` to accept a pre-leased workspace (skip the `cube workspace lease` call).
- `engine/src/work.rs` — add `WorkDb::insert_rebase_attempt`, `update_rebase_attempt_status`, `list_rebase_attempts`, `find_rebase_attempt_by_pair`. Add `WorkDb::set_product_auto_rebase`.

### App / UI deltas

- New "Engine" tab (or add into existing Settings/Activity surface) listing `RebaseAttempt` rows with filters and per-row detail.
- PR-card chip: *"↶ rebased after #N"* on cards for dependent PRs with a successful attempt < 24h old.
- PR-card banner (red): *"auto-rebase failed: <reason>"* on cards for dependent PRs with a failed attempt that didn't escalate.
- Product settings: per-product toggle for `auto_rebase_enabled`.

---

## Risks

**R1 — `jj rebase` corner cases on bookmark-relative ranges.** `jj rebase -d main -b <bookmark>` rebases all commits reachable from the bookmark that aren't in `main`. If the dependent branch has commits from *both* the merged base and the dependent's own work intermixed (rare but possible after a sloppy local rebase), the range may include more or less than expected. Mitigation: the v1 implementation runs `jj log -r main..<bookmark>` first and logs the range it's about to operate on; the integration test covers the "branch contains a merge commit" edge case explicitly. If it gets weird, fall back to `--source` form.

**R2 — Push race with a human.** A user notices PR B is stranded and rebases it manually at the same moment the engine is doing the rebase. Both push. The engine's push is `--bookmark` (jj's bookmark-set semantics, equivalent to force-with-lease) — if the remote moved between fetch and push, the push fails. Mitigation: catch the rejected push, refresh, and retry once; on second rejection, mark `failed` with reason "concurrent push", do NOT escalate (the human is already on it).

**R3 — Approvals dismissed unexpectedly.** Q9's mitigation (post a comment) helps but doesn't undo the dismissal. If a team relies heavily on long-lived approvals (e.g. a release manager pre-approves many PRs in a batch), auto-rebase will dismiss those approvals and slow them down. Mitigation: per-product opt-out (Q8) is the escape hatch; document in the engine README that auto-rebase + strict review-dismissal = re-approval friction, and recommend opting out for repos where this matters.

**R4 — Cube workspace pool exhaustion.** Auto-rebase consumes cube workspaces; if 5 PRs merge in quick succession and 5 stacked dependents each kick off a rebase, we could starve the worker pool. Mitigation: cap concurrent engine-direct attempts at 2 (Q4). If the cap is hit, attempts queue as `pending` and the next merge poller pass picks them up.

**R5 — `gh pr view` rate-limit pressure.** Adding `gh pr list --base <branch>` to every detected merge increases the engine's GitHub API call volume. GitHub's REST limit is 5000/hour for an authenticated user; this should be well under it for any realistic workload. Mitigation: piggyback on the existing rate-limit handling in the merge poller (which retries on 403 with a backoff); if we hit ceilings, switch to GraphQL batching.

**R6 — Mixed-jj / mixed-git histories.** A PR that was rebased manually with `git rebase -i` (not `jj`) might leave commits with descriptions or signatures that `jj` doesn't expect. Auto-rebase shouldn't touch the *contents* of commits — `jj rebase -d main` is a parent-pointer change only — but signed commits will need re-signing, and that requires the signing key to be available in the cube workspace's environment. Mitigation: v1 documents that signed commits will lose their signatures on auto-rebase; if signing matters for a repo, opt out.

**R7 — Wrong-branch confusion.** A worker chore was created for branch `riker/feat-B`, but in the time between creation and worker pickup, the user (or another worker) merged B and created a new PR on the same branch. The escalated chore acts on a stale state. Mitigation: the escalated chore's first action should be `gh pr view <PR#> --json state,headRefOid` to verify the PR is still open and the branch's tip matches what the engine recorded; if not, exit with a "PR state changed since rebase attempt was created" message and mark the rebase attempt `failed` for human review.

---

## Follow-up Implementation Chores (to enqueue once approved)

Bite-sized; each fits one worker session.

1. **Schema + migration**: `rebase_attempts` table, `products.auto_rebase_enabled` column, schema_version bump. Migration is no-op for existing rows (default `auto_rebase_enabled = 1`). Acceptance: fresh init and migration from prior schema both yield the new schema; existing products report `auto_rebase_enabled = true`.

2. **Protocol types**: `RebaseAttempt`, `ListRebaseAttemptsInput`, `RetryRebaseAttemptInput`; `Product.auto_rebase_enabled`. Mirror in `app-macos/Sources/Models.swift`. Acceptance: serde/Codable round-trips green; existing wire tests still pass.

3. **Engine module: shell wrappers** (`GhCli`, `JjCli` traits + command implementations). Methods needed: `gh pr list --base`, `gh pr edit --base`, `gh pr comment`, `gh pr view --json labels`; `jj git fetch`, `jj edit <bookmark>`, `jj rebase -d main -b <bookmark>`, `jj st`, `jj git push --bookmark`. All shell-out via `tokio::process::Command` like `merge_poller`. Acceptance: trait-level stubs; integration tests inject fakes; no engine wiring yet.

4. **Engine module: `auto_rebase` core**. `AutoRebase::on_base_merged(product_id, base_pr_url)` — discovers dependents, inserts `rebase_attempts` rows, attempts rebase one-by-one (with the concurrency cap), escalates on conflict. Unit tests cover: no dependents found, one mechanical success, one conflict-escalation, fork PR skipped, opted-out product skipped, push race retry. Acceptance: tests green; no merge_poller integration yet.

4a. **Engine: conflict diagnosis collector** (Q11). After `jj rebase` reports a conflict, the engine runs the diagnostic pass: list conflicted files, capture per-file conflict shape (capped at 60 lines per file or 200 lines for small files), intersect with the merged PR's file footprint via `gh pr view --json files`, list dependent commits in the rebased range. Persist the structured result into `rebase_attempts.conflict_diagnosis` (JSON). Acceptance: integration test asserts a conflict produces a populated `conflict_diagnosis` with the expected files and shapes.

4b. **Engine: escalated-chore description template** (Q11). Render the chore description from the `conflict_diagnosis` blob plus the resolution playbook (including the test step from `products.test_command`, omitted if null) plus the explicit stop conditions. Unit-tested against fixture diagnoses to produce stable Markdown output. Acceptance: golden-output test for two diagnoses (small clean diff, large multi-file diff).

4c. **Engine: `succeeded_via_close` and `blocked_pending_decision` substatuses** (Q11). Worker chore completion handler maps the worker's terminal action (closed-PR / set-chore-blocked / pushed) onto one of the new `rebase_attempts.status` values via the chore's metadata. Acceptance: integration test simulates each terminal-action path and asserts the right status lands on the attempt row.

5. **Wire merge_poller hook**: extend `merge_poller::sweep_one` to call `auto_rebase.on_base_merged` after `mark_chore_pr_merged`. Wire `AutoRebase` instance through `app.rs` setup. Acceptance: integration test creates two chores B-on-A and C-on-A, marks A merged, asserts both rebase attempts run.

6. **Wire completion.rs hook**: same call site from the on-Stop merge path, for in-run merges. Acceptance: integration test simulates worker Stop with a Merged classification, asserts auto-rebase fires.

7. **Pre-leased spawn path**: extend `spawn_flow.rs` to accept `(cube_lease_id, cube_workspace_id, workspace_path)` from a caller (skip the lease call). The escalation path uses this to hand the worker the engine's existing workspace. Acceptance: a chore created with pre-set lease fields spawns without calling `cube workspace lease`; tests verify the lease passes through unchanged.

8. **CLI: `boss engine rebase-attempts list|show|retry`**. Reference doc updated. Acceptance: `--help` covers the verbs; CLI integration test covers list-after-attempt and a retry of a failed attempt.

9. **CLI: `boss product update --auto-rebase on|off`**. Acceptance: toggle persists; subsequent merges respect the setting.

10. **macOS: Engine activity tab**. Render `RebaseAttempt` list with filter, sortable by `created_at`; tap-through to detail view. Acceptance: snapshot tests; visual review on a fixture board with 3 attempts (succeeded / escalated / failed).

11. **macOS: PR card chip + banner**. Chip on dependent cards with a successful attempt < 24h; red banner on cards with a failed-and-not-escalated attempt. Acceptance: snapshot tests.

12. **macOS: per-product auto-rebase toggle in settings**. Acceptance: toggle persists, kanban / activity view re-renders without restart.

13. **(Optional follow-up) Cross-repo / cross-product stacks**: requires a new index of "PRs known to Boss across all products." Out of v1 hard scope; file as design-needed.

14. **(Optional follow-up) Promotion to `system` work-item kind**: when a second engine-driven background workflow lands (e.g. auto-label, auto-archive), revisit Q3 and migrate `rebase_attempts` to be one row per `tasks.kind = 'system_rebase'`. Out of v1 hard scope.

---

## Out of Scope

- Cross-product / cross-repo PR stacks.
- Auto-merging downstream PRs once rebased.
- Auto-fix CI failures introduced by a rebase.
- Detecting "stack relationships" Boss didn't see opened (e.g. a PR opened directly via `gh` outside Boss). Auto-rebase will still process them via the `gh pr list --base` query — *that is the intended behaviour* — but no special-case handling.
- Per-PR opt-out via anything other than a label (no comment-driven opt-outs, no per-attempt CLI vetoes; if the user wants to stop one attempt, they retry it as `failed` and we don't restart it).
- Heuristics to skip the force-push when the rebase is a no-op (always push; idempotency is at the `rebase_attempts` row, not the git push).
- Multi-account `gh` authentication. Single user / single token in v1.
- Engine-side detection of "this rebase will require rerunning a flaky test." Out of scope; CI is GitHub's job.
