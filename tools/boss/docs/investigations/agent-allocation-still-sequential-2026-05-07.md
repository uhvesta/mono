# Agent allocation still sequential after PR #219 — 2026-05-07

User report: a recent PR was supposed to randomize which agent (worker pane / workspace) is used when dispatching new tasks, but in practice early-numbered workspaces (`mono-agent-001`, `mono-agent-002`, …) are still picked first. Investigation, root cause, and recommended fix below. **No code change in this branch — diagnosis only.**

## TL;DR

PR #219 (`feat(boss): randomize free pane selection on auto-dispatch`) randomizes the **worker pane** index that boss-engine assigns to a dispatch (`worker-1`..`worker-N`). The user-visible "agent number" — `mono-agent-NNN` — is the **cube workspace id**, allocated by `tools/cube/src/store.rs::Store::claim_workspace`, which still does `ORDER BY workspace_id LIMIT 1`. So the layer the PR fixed is genuinely random, but the layer the user is observing was never touched. The fix is in cube, not boss-engine.

## 1. The PR

- **PR:** [spinyfin/mono#219](https://github.com/spinyfin/mono/pull/219) — `feat(boss): randomize free pane selection on auto-dispatch`, merged 2026-05-07T20:16:52Z as `bef799eae394` (currently on `main`).
- **Files changed:** `Cargo.lock`, `Cargo.toml`, `MODULE.bazel.lock`, `tools/boss/engine/Cargo.toml`, `tools/boss/engine/src/coordinator.rs` (+61 / -34). **`tools/cube/**` is not in the diff.**
- **What it actually does:** in `tools/boss/engine/src/coordinator.rs` it replaces the LRU `last_used_seq` fallback inside `WorkerPool::claim_worker` (and `claim_worker_force`) with a uniform random pick over the currently free worker slots, using a per-pool `fastrand::Rng` advanced under the existing pool mutex. Affinity (preferred-workspace stickiness) still wins when present; randomization only kicks in for the fallback path. The unused `last_used_seq` / `next_seq` LRU bookkeeping is removed.
- **PR body's framing of the bug:** *"the coordinator's auto-dispatcher used to fall back to the lowest-numbered free worker slot whenever no workspace-affinity match was available — concretely, an LRU pick with `last_used_seq` tied at 0 across never-used slots, so slot 1 always won. Cosmetic effect: Riker did the bulk of the work and the rest of the crew was idle."* That is a true and now-fixed bug — but **note that "Riker" is a worker-pane name, not a workspace id**.

## 2. The two layers — and where the PR landed

A boss-engine dispatch threads through two distinct allocators in series:

1. **Worker pane** — `WorkerPool::claim_worker(execution_id, preferred_workspace_id)` in `tools/boss/engine/src/coordinator.rs:363`. The pool is a `Vec<WorkerSlot>` of fixed slots named `worker-1`…`worker-N` that map to UI panes the macOS app renders. **This is the layer PR #219 randomized**, and the change is correct: with no affinity preference the dispatcher now does

   ```rust
   let free: Vec<usize> = inner.workers.iter().enumerate()
       .filter(|(_, w)| w.execution_id.is_none())
       .map(|(idx, _)| idx)
       .collect();
   let chosen_idx = *inner.rng.choice(&free)?;
   ```

   Same pattern in `claim_worker_force` (`coordinator.rs:402`). RNG is `fastrand::Rng::new()` (entropy-seeded, not deterministic) constructed in `WorkerPool::new` and reused under the pool mutex — there is no obvious way for this to degenerate into a deterministic order.

2. **Cube workspace** — `Store::claim_workspace` in `tools/cube/src/store.rs:281`. After the worker pane is claimed, `Coordinator::schedule_execution` (`coordinator.rs:730`) calls `cube_client.lease_workspace(repo_id, task, preferred_workspace_id)`, which shells out to `cube workspace lease` and ultimately runs:

   ```sql
   SELECT workspace_id, workspace_path
   FROM workspaces
   WHERE repo = ?1 AND state = ?2
   ORDER BY workspace_id
   LIMIT 1
   ```

   …in **both** the no-prefer branch (`store.rs:330`) and the prefer-fallback branch (`store.rs:314`). `workspace_id` is a string like `mono-agent-001`, so `ORDER BY workspace_id` is a strict alphabetic sort — the lowest-numbered free workspace always wins. **This is the user-visible "earlier-numbered agents are picked first" behaviour. PR #219 did not touch this file.**

The dispatcher's own log fields make the distinction explicit: every "started execution run" event records both `worker_id="worker-N"` (pane) and `cube_workspace_id="mono-agent-NNN"` (workspace), and they are independently chosen.

## 3. Hypothesis and confirmation from `/tmp/boss-engine.log`

Hypothesis (e) from the prompt — *the user is observing a code path that was never randomized* — is correct: the user is observing cube workspace allocation, not boss-engine pane allocation.

The engine log confirms this directly. A representative slice of consecutive dispatches with no `preferred_workspace_id` set (so each one falls through to the random/LRU branch on both allocators):

| timestamp (UTC) | worker_id | cube_workspace_id |
| --- | --- | --- |
| 01:04:27 | worker-7 | mono-agent-001 |
| 01:05:02 | worker-1 | mono-agent-001 *(001 had been released)* |
| 01:05:42 | worker-4 | mono-agent-002 |
| 01:22:43 | worker-8 | mono-agent-003 |
| 01:24:28 | worker-3 | mono-agent-004 |
| 01:24:41 | worker-6 | mono-agent-005 |
| 01:25:10 | worker-6 | mono-agent-006 |
| 01:25:15 | worker-2 | mono-agent-007 |
| 01:25:17 | worker-5 | mono-agent-008 |
| 01:26:29 | worker-4 | mono-agent-009 |
| 01:26:31 | worker-5 | mono-agent-010 |
| 03:47:06 | worker-7 | mono-agent-001 |
| 03:47:28 | worker-4 | mono-agent-002 |
| 03:47:49 | worker-5 | mono-agent-003 |
| 03:48:05 | worker-1 | mono-agent-004 |
| 03:48:25 | worker-2 | mono-agent-005 |
| 03:49:07 | worker-3 | mono-agent-006 |

Worker IDs in the left column are not in any monotonic order — that is PR #219 working as advertised. Workspace IDs in the right column are strictly monotonically increasing (modulo "the prior workspace was released and reclaimed before the next one") — that is the cube allocator's `ORDER BY workspace_id`, exactly as the SQL says. Across a long enough window where some workspaces are concurrently leased and others are free, the next dispatch deterministically lands on the lowest-numbered *free* row every time.

The other hypotheses are ruled out:

- **(a) RNG seeded deterministically.** No — `fastrand::Rng::new()` seeds from system entropy. Worker-id column above shows actual spread.
- **(b) Random pick over a list of one.** No — log shows multi-element pools (e.g. fresh boot at 01:04 with all 8 worker slots free) still randomizing the worker but still landing on `mono-agent-001` for cube.
- **(c) Downstream re-sort.** No — the dispatcher just passes `lease.workspace_path` straight through; the order is set inside `Store::claim_workspace`'s SQL.
- **(d) Feature-flag gating.** No — there is no flag in the PR.
- **(f) PR was reverted.** No — `bef799eae394` is on the current trunk and `coordinator.rs` matches the merged form.

## 4. Where the fix should land

`tools/cube/src/store.rs::Store::claim_workspace` has two `ORDER BY workspace_id LIMIT 1` SELECTs that need to become a uniform random pick over free rows. SQLite supports `ORDER BY RANDOM() LIMIT 1`, so the minimal change is one-line per branch:

```sql
-- prefer-fallback branch (store.rs:308-321)
SELECT workspace_id, workspace_path
FROM workspaces
WHERE repo = ?1 AND state = ?2
ORDER BY RANDOM()
LIMIT 1

-- no-prefer branch (store.rs:323-337)
SELECT workspace_id, workspace_path
FROM workspaces
WHERE repo = ?1 AND state = ?2
ORDER BY RANDOM()
LIMIT 1
```

Notes for whoever lands the fix:

- `claim_workspace` is wrapped in a per-repo `RepoLock` plus a SQLite transaction, so concurrent dispatches don't race for the same row even with random ordering. Existing `transaction.rollback()` / `LIMIT 1` semantics still apply.
- Anything that currently relies on lowest-id-first being deterministic in tests will break. There appear to be no tests in `tools/cube/src/` that assert on which workspace id is returned by an unbiased `claim_workspace` call (`grep -n "claim_workspace\|workspace_id" tools/cube/src/store.rs` shows tests asserting state transitions, not allocation order); a quick `cargo test -p cube` and `bazel test //tools/cube/...` is enough to confirm.
- Worth verifying that the boss-engine integration test `worker_pool_random_fallback_spreads_across_free_slots` (`tools/boss/engine/src/coordinator.rs:1953`) does not also implicitly depend on cube allocation order — at a glance it doesn't, because it talks to a fake cube, but flag this for the implementer.
- Optional polish: factor the SELECT into a single helper since the two branches differ only in the `WHERE workspace_id = ?` clause for the prefer hit — currently fine as is, just observation.

The boss-engine side does not need any further change. The `last_workspace_id` affinity branch in `WorkerPool::claim_worker` (`coordinator.rs:370-379`) is correct as written — when affinity matches it short-circuits, and when it doesn't the random fallback is genuinely random. The complaint is about workspace allocation only.

## 5. Recommended follow-up

Open a single chore against `tools/cube/src/store.rs::claim_workspace`: replace `ORDER BY workspace_id` with `ORDER BY RANDOM()` in both branches, run `cargo test -p cube` and `bazel test //tools/cube/...`, and verify with a fresh boot that consecutive dispatches no longer climb `mono-agent-001` → `…002` → `…003` deterministically. Reference this investigation and PR #219 in the chore description so the reviewer has the layer-confusion context.
