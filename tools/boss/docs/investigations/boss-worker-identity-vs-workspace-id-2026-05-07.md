# Boss worker identity vs cube workspace id — 2026-05-07

Static audit triggered by user concern that boss may be conflating worker-pane identity with cube workspace ids — i.e. treating workspaces 1-8 as "owned" by specific worker agents (e.g. worker pane 1 always uses `mono-agent-001`). That would violate cube's contract: cube provides temporary leases, and any worker can lease any free workspace.

The audit's punchline: **no such conflation exists in the boss-side code.** The user-visible "agent" identity is the libghostty pane slot id (1..8) — which the macOS app allocates with first-free-slot semantics — surfaced through the shared crew roster (`Riker`, `Data`, …, `O'Brien`). Cube workspace ids flow through the execution row (`work_executions.cube_workspace_id`, `workspace_path`), but they never become a worker's display name, slot label, or persistent identity attribute.

The investigation thesis stated in the now-closed PR #284 — *"the user-visible 'agent NNN' identifier is the cube workspace id, allocated by `tools/cube/src/store.rs::Store::claim_workspace`"* — was incorrect. The sequential-looking allocation the user observed comes entirely from cube's `ORDER BY workspace_id LIMIT 1` in `claim_workspace`. The orthogonal cube-side fix (`ORDER BY RANDOM()`, separate task `task_18ad7b0231f542d0_19`) addresses that. Boss-side code needs no functional change.

## Scope and methodology

- Static read-through of every Rust source file under `tools/boss/engine/src/`, `tools/boss/bossctl/src/`, and `tools/boss/protocol/src/`, plus every Swift source under `tools/boss/app-macos/Sources/`.
- For each occurrence of `workspace_id`, `cube_workspace_id`, `agent_id`, `worker_id`, `slot_id`, the audit traced where the value comes from and where it ends up — specifically asking: "could this make a workspace id reach a user-visible identity surface?"
- Specific surfaces inspected for accidental `workspace → identity` leaks: `bossctl agents *` output, `LiveWorkerState`, `WorkRun.agent_id`, `WorkersDetailView`, `WorkerNames` (Rust + Swift halves), engine logs, the engine's frontend wire shape (`AgentInfo`, `WorkspacePoolEntry`).

## Identity flow as it actually exists

The chain that produces a worker's user-facing identity is:

1. **macOS app** allocates a free pane slot in `WorkersWorkspaceModel.spawnWorkerPane` (`tools/boss/app-macos/Sources/Ghostty/WorkersWorkspaceModel.swift:22`). Slots are `1…workerSlotCount` (currently 8); allocation is "first slot whose `session == nil`". This slot id is the only durable, user-facing identifier for a worker pane.
2. **macOS app** returns the slot id to the engine via `EngineToAppResponse::SpawnWorkerPane { Ok(SpawnWorkerPaneResult { slot_id, shell_pid }) }`.
3. **Engine** stamps the slot id onto `RunOutcome.slot_id` (`tools/boss/engine/src/runner.rs:440`).
4. **Coordinator** rewrites the run record's `agent_id` from the worker-pool placeholder (`worker-{pool_bucket}`) to `worker-{slot_id}` (`tools/boss/engine/src/coordinator.rs:927`). After this point every persisted reference to the pane uses the pane slot id, never the cube workspace id.
5. **Engine** publishes a `LiveWorkerState` whose `name` field is computed from `slot_id` via `worker_names::name_for_slot` (`tools/boss/protocol/src/live_worker_state.rs:163`).
6. **macOS app** renders the same name via the Swift mirror `WorkerNames.name(forSlot:)` (`tools/boss/app-macos/Sources/Ghostty/WorkerNames.swift:49`). Pane titlebar, idle character portrait, kanban Doing icon, tooltip — all sourced from `slot.slotId`, never from `cube_workspace_id`.
7. **bossctl** resolves `agents <ref>` arguments through `resolve_agent_ref` (`tools/boss/bossctl/src/main.rs:251`), which matches against `LiveWorkerState.run_id`, `slot_id`, and `name`. There is no path that resolves an argument by, or displays, the cube workspace id as identity.

The only field on the `LiveWorkerState` that mentions the workspace at all is by virtue of the run id (`execution_id`); the workspace is *not* serialized into the state at all.

## Where cube workspace id legitimately appears

For completeness, here are the load-bearing places `cube_workspace_id` (or the equivalent affinity hint) does appear in boss code, with a classification.

| Location | Field | Classification |
| --- | --- | --- |
| `tools/boss/engine/src/work.rs` (DB schema) | `work_executions.cube_workspace_id` | **Execution attribute, not identity.** Records which workspace cube leased for this execution. Cleared on release. Used by the engine to ferry the workspace path into the runner. |
| `tools/boss/engine/src/coordinator.rs:328` | `WorkerSlot.last_workspace_id` | **Affinity hint, not identity.** Stored on the in-memory worker pool slot purely so a follow-up dispatch with `preferred_workspace_id` set can pick the same worker. Never read except inside `claim_worker` / `claim_worker_force` matching. Never serialised, never logged as identity, never exposed to the UI. |
| `tools/boss/engine/src/work.rs` `request_execution`/`reschedule_execution` | `preferred_workspace_id` column on `work_executions` | **Resume hint.** Set when the user explicitly asks for a specific workspace (e.g. via `cube workspace lease --prefer`). Bypassed when unset. |
| `tools/boss/protocol/src/wire.rs:643` | `WorkspacePoolEntry.workspace_id` | **Cube pool view.** Surfaced by `bossctl workspaces` for the operator to inspect cube state. Never joined to any agent identity. |
| `tools/boss/engine/src/coordinator.rs:790`, `1024` | `lease.workspace_id` argument to `start_execution_run` and `release_worker` | **Run-level argument.** Persisted on the execution row (column above) and stashed on the worker pool slot as `last_workspace_id`. Both already classified above. |

There is no surface that ever does `format!("agent {}", workspace_id)`, `name_for_workspace(...)`, or any analogue. The closest visual collision is the workspace path, e.g. `~/Documents/dev/workspaces/mono-agent-007`, which contains the literal string `agent-007` because that's how the on-disk workspace directories are named — but that's a path, not an identity. The pane in slot 7 is named "Yar", regardless of which workspace cube hands out.

## Why the user's evidence pointed elsewhere

The PR #284 writeup quoted an engine log line where `worker_id` advanced (7, 4, 5, 1, 2, 3, …) while `cube_workspace_id` advanced strictly `001 → 002 → 003`. That observation is correct — the worker pool *does* claim slots randomly today (PR #219), and cube *did* claim workspaces sequentially until the cube-side fix landed. The two columns describing different layers of identity advancing on different cadences is exactly the expected behaviour of the existing code; nothing about it implies the boss layer is deriving worker identity from the workspace id.

What the user-visible "agent NNN" identifier *actually* is depends on the surface:

- **`bossctl agents list`** → `LiveWorkerState.slot_id` and `LiveWorkerState.name` (crew roster), keyed off pane slot.
- **macOS app pane header** → `WorkerNames.name(forSlot: slot.slotId)`.
- **`work_runs.agent_id`** (the persisted column) → `worker-{slot_id}` after spawn.
- **Engine log lines** that emit `cube_workspace_id=mono-agent-007` are intentionally labelling the *workspace*, not the worker; the field name carries that.

None of these read from the cube workspace id when computing a worker's identity.

## What this audit is *not* claiming

- **Not claiming there's no worker–workspace stickiness.** There is, by design: `WorkerSlot.last_workspace_id` plus `preferred_workspace_id` create resume-affinity for the same worker pane to land back in the same workspace it last touched, when the operator explicitly asks for it. That's an affinity feature, not an identity coupling.
- **Not claiming the cube-side issue is invalid.** Cube's `ORDER BY workspace_id LIMIT 1` does cause sequential allocation, which is what the user actually observed. The fix is the cube-side `ORDER BY RANDOM()` change tracked in `task_18ad7b0231f542d0_19`. Boss-side code needs no change to support that.

## Recommendation

No code fix on the boss side. The PR landing alongside this writeup adds one small regression test (`agent_id_independent_of_cube_workspace_id_even_when_scrambled`) that explicitly pins the invariant: even when cube hands out workspaces in `001 → 002 → 003` order while the worker pool selects pane slots in scrambled order, the resulting `WorkRun.agent_id` is `worker-{pane slot id}`, not `worker-{workspace number}`. Future code that tries to derive worker identity from cube workspace id will trip this assertion.

If the cube-side `ORDER BY RANDOM()` fix lands and the user still observes a sequential-looking pattern, the next thing to check is the macOS app's pane allocator (`WorkersWorkspaceModel.spawnWorkerPane`), which is itself "first free slot" — that is intentional (the operator wants the leftmost pane to fill first when the workspace is empty), but it does mean a single dispatch into an idle pool always lands in slot 1. That's a UI choice, not a workspace-identity bug.
