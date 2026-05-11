# Post-crash recovery: orphaned executions

When the Boss macOS app is force-quit (or crashes) while a worker is
mid-task, the libghostty pane that hosted the worker dies along with
the app. On the next relaunch the engine restores the `work_executions`
row from sqlite but has no live worker to reattach it to — the row is
**orphaned**.

This doc describes how the engine detects orphans, how to recover from
them manually, and what state survives the cycle.

## What "orphaned" means

`orphaned` is a terminal status on the `work_executions` table,
alongside `completed` / `failed` / `cancelled` / `abandoned`. It
specifically denotes: *a worker was spawned for this execution, then
the engine lost the ability to verify it was still alive (typically
because the libghostty pane died across an engine restart).*

Compared to the other terminal statuses:

- `completed` / `failed`: the worker finished its turn and the
  completion handler stamped the verdict.
- `cancelled`: a human invoked `bossctl work cancel <execution>` or
  `bossctl agents stop <agent>`.
- `abandoned`: the row never produced a `work_runs` entry (the engine
  crashed mid-dispatch, before the worker spawned).
- `orphaned`: the row had a live worker once; the engine no longer
  has signal that it does.

**Critical difference from `cancelled`:** the cube workspace lease on
an orphaned row is intentionally **not** released. The workspace
filesystem typically still holds in-flight commits the next worker
should resume against. The engine clears `cube_lease_id` /
`cube_workspace_id` / `workspace_path` only when the human explicitly
cancels, or when cube's lease TTL elapses and cube force-releases it.

## How orphans get detected

### Automatic detection at engine startup

`tools/boss/engine/src/app.rs::serve` runs a probe at startup over
every non-terminal `work_executions` row that carries a recorded
`cube_lease_id`. The probe (`crate::run_reconcile::probe_in_flight_runs`)
asks cube whether the lease is still bound to the same workspace and
not yet expired; the verdict is one of:

- `Live` — cube confirms the lease. The engine leaves the row alone.
- `Dead` — cube says the workspace is free, the lease id has changed,
  or the lease has logically expired (TTL passed). The engine marks
  the execution `orphaned` immediately and inherits the workspace_id
  into the next ready row's `preferred_workspace_id` so the redispatch
  resumes against the same branch.
- `Unknown` — the probe couldn't decide (cube call failed, workspace
  not in the snapshot, sparse persisted state). The row is left
  alone; a loud `tracing::warn!` is emitted so the operator can
  resolve manually.

After the reaper passes, `reconcile_active_dispatch` runs as before
and creates fresh `ready` rows for work items whose Doing-column
status no longer matches a live execution.

### Manual escape hatch: `bossctl agents reap <run-id>`

The automatic probe is bounded by cube's lease TTL (30 minutes by
default — see `tools/cube/src/app.rs::DEFAULT_LEASE_TTL_SECS`). If the
app crash was recent, cube still reports the lease as `leased` and the
probe verdict is `Live`, even though the worker pane is gone.

For that gap, the coordinator (Boss-only) can reap manually:

```
bossctl agents reap exec_18ad6336fedcb190_12
```

This bypasses the probe and transitions the execution straight to
`orphaned`. The same workspace-preservation rules apply: cube columns
are left intact so a re-dispatch can pick the same branch back up.

`agents reap` requires `RpcTier::BossOnly`. Worker panes cannot
invoke it (they shouldn't be reaping each other).

## What happens after reap

Once the predecessor is `orphaned`:

1. The work item's kanban status is unchanged. If it was `active`
   (Doing), it stays there — the dispatcher will pick it back up.
2. `bossctl work start <work-item>` or the auto-dispatcher creates a
   new `work_executions` row in `ready`. The new row's
   `preferred_workspace_id` defaults to the orphan's
   `cube_workspace_id`, so cube will re-lease the same workspace
   when one is free.
3. The fresh worker spawns into that workspace. Inside the lease,
   `jj git fetch && jj edit <bookmark>` brings it back to the branch
   the orphan was working on; from there it can push and open / update
   the PR as if no crash had happened.

## Recovery cheat-sheet

```
# Inspect: is the execution still considered live?
bossctl agents list                 # in-memory live workers (empty on relaunch)
boss chore show <work-item-id>      # kanban + latest execution status

# Force the orphan reap if the engine missed it:
bossctl agents reap <run-id>

# Re-dispatch a fresh worker:
bossctl work start <work-item-id>   # picks up the orphan's workspace_id

# If the workspace itself is stuck (rare — cube usually self-heals
# via TTL after 30 min), see tools/cube/docs/remaining-work.md for
# `cube workspace force-release`.
```

## Why we don't release the lease

The default would be to release the cube lease whenever an execution
goes terminal — that's what `cancel_execution` does. The reaper path
deliberately doesn't, because:

- The workspace's filesystem usually has uncommitted work, partial
  branches, or open PRs the next worker should pick up. Releasing the
  lease lets cube hand the workspace to someone else (or auto-clean
  it), which makes the in-flight state harder to recover.
- Cube's lease TTL provides a safety net: orphaned leases that no
  worker is heartbeating expire on their own within 30 minutes.
- A human who *does* want a clean slate can invoke `bossctl agents
  stop <run-id>` first, then `agents reap` — the stop path is the
  documented way to release the lease deliberately.
