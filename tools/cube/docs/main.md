# Cube: Design for Parallel Agent Workspaces

## Overview

Cube is a small, opinionated CLI for agents that need to do coding work in
parallel on the same repository without paying the cost of cloning that repo
for every task.

The design assumes a pool of reusable local workspaces per repository. Each
workspace is an independently editable checkout with its own working copy,
build outputs, and source-control state. Agents lease a workspace, create or
update a change stack inside it, and ask Cube to keep the corresponding pull
requests in sync.

Cube is not a full wrapper over `jj` or `git`. It exposes a narrow set of
task-oriented operations:

- lease and release reusable workspaces safely,
- create and mutate stacked changes,
- rebase a stack or subtree onto updated `main`,
- sync commits to GitHub PRs,
- merge stacked PRs without exposing forge-specific branch lifecycle hazards,
- report enough underlying state that an agent can fall back to `jj`, `git`,
  `gh`, `bazel`, or repo-local tools directly when needed.

The initial design is `jj`-first. `git` remains an implementation detail for
remote synchronization and GitHub PR integration.

## Goals

- Allow many agents to work concurrently on the same repo using reusable
  workspaces rather than fresh clones.
- Keep workspace acquisition safe under contention via explicit leases and
  locking.
- Model stacked commits as a first-class concept, including forked stacks.
- Make the common agent operations simple and deterministic.
- Preserve compatibility with GitHub PR workflows.
- Hide forge-specific stacked-PR edge cases such as base-branch retargeting
  during merges.
- Reuse workspaces long enough to benefit incremental build systems such as
  Bazel.

## Non-Goals

- Exposing the full `jj` command surface.
- Hiding the underlying repository state completely.
- Supporting every forge or CI system in v1.
- Solving distributed execution across multiple hosts in v1.
- Guaranteeing conflict-free rebases or merges without agent intervention.

## Assumptions

- A single machine hosts a pool of reusable workspaces for a repo.
- Agents can invoke local CLIs and edit files in the leased workspace.
- GitHub is the initial PR backend.
- `jj` is installed and available. `git` and `gh` are also available.
- The repo has one primary integration branch, referred to as `main`.

## User Model

Cube has three core objects:

### Repo Pool

A repo pool defines:

- the canonical repo identity,
- the path containing reusable workspaces,
- the default integration branch,
- the PR backend configuration,
- optional repo-specific hooks such as fetch, build, test, and workspace setup
  commands.

Example:

```text
repo: mono
origin: git@github.com:spinyfin/mono.git
main_branch: main
workspace_root: ~/.local/share/cube/workspaces
workspace_prefix: mono-agent-
pr_provider: github
setup_steps:
  - id: secrets
    command: ./tools/dev/decode-secrets.sh
  - id: deps
    command: pnpm install --frozen-lockfile
```

When Cube materializes repo pools by convention, it should use Cube-managed
roots rather than an existing human-managed workspace pool. A reasonable
default is:

```text
repos: ~/.local/share/cube/repos
workspaces: ~/.local/share/cube/workspaces
```

### Workspace Lease

A workspace lease is a time-bounded claim on one reusable workspace. It records:

- workspace id and absolute path,
- holder identity (agent id, pid, hostname, optional task id),
- lease start and expiry,
- task summary,
- current repo head at lease time,
- optional heartbeat metadata.

A leased workspace is exclusive. One agent holds it until it releases the
lease, the lease expires, or an operator force-breaks it.

### Change Node

A change node corresponds to one logical commit in a stack. Cube stores:

- a stable local change id,
- underlying `jj` change id and commit id,
- parent change ids,
- branch/bookmark name for Git export,
- preferred review base change and current remote base branch,
- PR number/url if one exists,
- title/body metadata,
- sync status,
- whether the exported branch is pinned because another open PR still targets
  it as a base,
- whether the node is the root of a task, a child, or one branch of a fork.

The change graph is a DAG, not only a linear stack. That allows one parent to
have multiple independent child changes.

## Why `jj`

`jj` is the right primary backend because it already gives Cube most of the
change-graph semantics that agents need:

- immutable commit history with mutable working-copy state,
- explicit change identities,
- natural support for stacked work,
- easier rebasing of a subtree of changes,
- better ergonomics for rewriting local history before export to GitHub.

Cube should treat `git` as the transport layer to the remote forge and `gh` as
the PR control plane. If a future repo cannot use `jj`, Cube could grow a
reduced-capability `git` backend, but that should not drive the initial shape
of the abstraction.

## Core Operations

### 1. Workspace Leasing

```text
cube workspace lease mono --task "write cube design doc"
cube workspace lease mono --task "resume parser work" --prefer mono-agent-007
```

Behavior:

- Acquire a repo-level lock.
- If `--prefer <id>` is provided and that workspace is free in the repo, claim
  it; otherwise fall back to the first free workspace. The flag is best-effort
  and silently falls back when the preferred workspace is leased or unknown,
  so callers should compare the returned workspace id against their preference
  to detect a fallback.
- Otherwise find a free, healthy workspace from the pool, or create one. The
  pool is an optimisation (reuse a known-good checkout), not a hard cap: a
  lease for a registered, reachable repo always succeeds by growing the pool
  on demand. The only hard stop is a pool in which every free workspace has a
  dirty working copy — those may hold unpushed work, so the operator must
  reclaim them (`cube workspace force-release --reason crash`) rather than have
  cube silently discard the changes.
- Self-heal degraded pool entries on lease. A free workspace whose directory
  has neither `.jj/` nor `.git/` (a "broken-empty" husk, e.g. from a clone
  interrupted before this guarantee landed) holds no recoverable work, so cube
  deletes the husk, forgets its registry row, and provisions a fresh workspace
  instead of surfacing the husk as a lease failure.
- Mark the workspace leased with holder metadata.
- Run the repo-specific reset sequence before returning it.
- If the workspace is newly created, or its recorded setup state is stale, run
  repo-specific setup steps before returning it.
- Return machine-readable output including workspace path and lease id.

Recommended reset sequence for `jj` repos:

1. `jj git fetch`
2. `jj new main`
3. Optional cleanup hooks for untracked/generated files if configured.

Recommended setup model:

1. Declare setup steps in repo configuration.
2. Give each step a stable id plus an invalidation key.
3. Record the last successful setup fingerprint per workspace.
4. Re-run only the steps whose invalidation key changed, or all steps for a
   newly created workspace.

Examples of setup steps:

- decoding local secrets,
- `pnpm install`,
- generating code or language toolchains,
- authenticating to internal package registries,
- writing repo-local config files that should not be committed.

Lease release:

```text
cube workspace release --lease <lease-id>
```

Behavior:

- Optionally snapshot current state for debugging.
- Clear the lease and task metadata.
- Leave build outputs intact for reuse.
- Optionally run a lightweight reset hook so the next agent starts from a known
  clean working copy.

### 2. Change Creation

```text
cube change create --workspace <path> --title "Add parser"
cube change create --workspace <path> --parent <change-id> --title "Wire CLI"
```

Behavior:

- Create a new `jj` change on the selected parent.
- Record metadata locally.
- Return the new change id plus underlying `jj` identifiers.

Forked children are created by naming the same parent twice:

```text
cube change create --parent parser-root --title "Rust parser"
cube change create --parent parser-root --title "Python parser"
```

### 3. Change Navigation

```text
cube change checkout --change <change-id>
cube change info --change <change-id>
cube graph --workspace <path>
```

Agents need a cheap way to:

- move the working copy to a specific change,
- inspect parent/child relationships,
- find which PR belongs to which node,
- discover whether a node is dirty, conflicted, or behind `main`.

### 4. Rebase and Restack

```text
cube stack rebase --root <change-id> --onto main
cube stack rebase --subtree <change-id> --onto <new-parent-change-id>
```

Behavior:

- Rebase either one linear stack or an entire subtree.
- Preserve fork structure where possible.
- Update local change metadata after rewrite.
- Report conflicts at the change-node granularity.

Cube does not need to invent new semantics here. It can delegate to `jj`
operations that already understand rewriting descendant commits.

### 5. PR Sync

```text
cube pr sync --root <change-id>
cube pr sync --change <change-id>
```

Behavior:

- Export each change node to a deterministic Git branch/bookmark name.
- Push the branch to origin.
- Create the PR if one does not exist.
- Update an existing PR if the branch already maps to one.
- Recompute parent/child PR relationships for stacks.
- Refresh existing PR state from GitHub before changing bases or deleting
  branches.

For a linear stack, Cube should prefer the previous exported change as the
review base. For a fork, each child PR should prefer its direct parent change.
This keeps the review graph close to the local change graph, but the local
graph remains the source of truth. GitHub base branches are transport state
that Cube may retarget as ancestors merge.

That distinction matters for stacked merges. If PR `#35` merges and its branch
is deleted while descendant PR `#37` still targets that branch, GitHub may
auto-close `#37` even though the descendant change is still valid. Cube should
prevent that class of failure by enforcing these invariants:

- never delete an exported branch while an open descendant PR still targets it
  as a base,
- retarget descendants onto the nearest surviving ancestor branch or `main`
  before branch cleanup,
- keep branch-retention state in local metadata so recovery does not depend on
  branch names alone,
- treat reopened or retargeted PRs as a normal sync outcome, not a manual
  repair path.

Cube should emit structured output such as:

```json
{
  "root_change": "chg_123",
  "synced": [
    {
      "change": "chg_123",
      "branch": "cube/mono/chg_123-add-parser",
      "pr": 101
    }
  ]
}
```

### 6. PR Merge

```text
cube pr merge --change <change-id>
cube pr merge --stack <root-change-id>
```

Behavior:

- Merge one PR or a whole ready sub-stack in dependency order.
- After each merge, refresh GitHub state before touching descendant PRs.
- Retarget open descendants off the merged branch before deleting or unpinning
  that branch.
- Re-export or reopen descendants automatically if an external branch deletion
  closed them unexpectedly.
- Rebase or restack remaining local descendants when the merge advances
  `main`.

This command exists to hide GitHub's stacked-PR lifecycle quirks from agents.
The safe merge sequence should be owned by Cube rather than hand-written into
agent prompts.

### 7. Status and Recovery

```text
cube workspace status --workspace <path>
cube doctor --workspace <path>
```

Agents need a recovery path for common failures:

- stale lease,
- workspace drift from manual commands,
- missing PR metadata,
- exported branch diverged from local change,
- rebase conflicts,
- failed push or CI sync.

`cube doctor` should identify the mismatch and suggest the next safe operation
rather than trying to auto-heal every case.

## CLI Shape

The CLI should stay small and noun-oriented:

```text
cube repo ...
cube workspace ...
cube change ...
cube stack ...
cube pr ...
cube graph ...
cube doctor ...
```

Every command should support:

- `--json` for structured agent consumption,
- stable exit codes,
- concise human-readable defaults,
- enough raw ids in output that an agent can drop down to `jj`, `git`, or `gh`.

## Metadata Storage

Cube needs small, durable metadata outside the repo working tree so that it
survives resets and does not pollute commits.

Suggested layout:

```text
~/.local/share/cube/
  repos/<repo-id>/config.toml
  repos/<repo-id>/workspaces/<workspace-id>.toml
  repos/<repo-id>/changes/<change-id>.toml
  locks/<repo-id>.lock
```

Workspace metadata tracks leases, health, and setup state. Change metadata
tracks the mapping between logical change ids, `jj` ids, branches, and PRs.

The local metadata is a cache of durable intent, not the source of truth for
repo contents. Cube must be able to reconcile metadata against the actual `jj`
state inside a workspace.

## Locking Model

Workspace reuse is only safe if lease acquisition is atomic.

Required locks:

- repo-pool lock: protects allocation, creation, and release of workspaces,
- workspace lock: protects reset and destructive maintenance operations within
  one workspace,
- optional change-sync lock: prevents two agents from syncing the same stack to
  GitHub at the same time.

Implementation options:

- `flock` on lockfiles for local-only operation,
- SQLite with transactional row locking if Cube needs richer queries,
- both: SQLite for metadata plus `flock` to guard process-level critical
  sections.

For v1, SQLite plus `flock` is the safest local design:

- SQLite handles indexed lookup and stale lease inspection.
- `flock` gives simple mutual exclusion around lease acquisition and reset.

## Workspace Lifecycle

### Acquire

1. Lock repo pool.
2. Inspect known workspaces.
3. Reclaim any expired lease only if policy allows it.
4. Select the best candidate.
5. Reset it to a clean `main`-based working copy.
6. Run any required setup/provisioning steps.
7. Record lease metadata plus setup fingerprint.
8. Return path and lease id.

### Select Best Candidate

Selection should prefer:

1. already-existing free workspaces,
2. workspaces recently used for the same repo branch,
3. workspaces whose build outputs are likely still warm,
4. creating a new workspace only after the pool is exhausted.

Cube does not need perfect cache prediction in v1. "Prefer a free existing
workspace over creating a new one" is already a meaningful win.

### Create

Creating a workspace must be atomic with respect to the pool: a directory that
matches the workspace prefix must never be observable while it is only
partially populated. Cube clones into a hidden staging directory
(`.incoming-<workspace-id>`, which the pool scan ignores because it does not
match the prefix) and only publishes it under its final name via an atomic
rename once the clone and bookmark setup have completed. A clone interrupted
mid-flight therefore leaves only a staging directory, never a "broken-empty"
husk that a later lease would have to step around. The repo-pool lock is held
across the whole operation, so the staging name cannot collide with a
concurrent create for the same repo, and any leftover staging directory from a
previously interrupted run is cleared before the next clone.

### Setup and Provisioning

Some repos need one-time or occasionally refreshed setup after a workspace is
created or reset. Cube should make this explicit rather than forcing every
agent to remember repo-specific bootstrap commands.

Requirements:

- setup steps are defined per repo, in order,
- each step has a stable id and optional timeout,
- each step declares when it should run,
- setup status is recorded per workspace,
- setup can be retried independently of lease acquisition.

Useful run policies:

- `on-create`: only for brand new workspaces,
- `on-first-lease-after-reset`: after Cube performs a deep reset or repair,
- `on-fingerprint-change`: when tracked inputs changed,
- `always`: for cheap checks that must run every time.

Tracked inputs for a setup fingerprint could include:

- lockfiles such as `pnpm-lock.yaml`,
- setup script contents,
- a manual repo-defined setup version string,
- selected environment or secret version markers.

Example:

```text
cube workspace setup --workspace /ws/mono-agent-007
```

This command should:

- evaluate which steps are stale,
- execute only the required steps,
- stream logs for agent consumption,
- record success or failure per step,
- leave a clear diagnostic trail when setup fails.

`cube workspace lease` should call the same setup engine automatically when the
workspace needs provisioning. A separate `workspace setup` command exists so an
agent or operator can repair a workspace without releasing and reacquiring it.

### Release

1. Optionally summarize dirty state or unpushed changes.
2. Reset to a reusable baseline.
3. Mark the lease released.
4. Keep incremental build outputs.

Release should not discard successful setup state unless the reset invalidates
it. For example, a `pnpm install` cache can remain valid across many leases,
while decoded secrets may need a shorter lifetime or explicit rotation policy.

## PR Model

Each change node owns exactly one exported branch and at most one open PR.

Recommended naming:

```text
cube/<repo>/<change-id>-<slug>
```

PR synchronization rules:

- If no PR exists, create one.
- If the change rewrites, force-push the exported branch.
- If the parent change changes, update the PR base branch when the forge allows
  it.
- If a node is closed or merged, mark its local metadata accordingly.
- If the stack forks, each child PR prefers its direct parent change as the
  review base.
- An exported branch stays pinned while any open descendant PR still targets it
  as its current remote base.
- Branch deletion happens only after descendants are retargeted to a surviving
  base branch.

This model allows a stack to be reviewed incrementally while preserving the
graph structure that agents used locally.

## Agent Workflow Example

```text
cube workspace lease mono --task "implement parser"
cube change create --workspace /ws/mono-agent-007 --title "Add parser model"
cube change create --parent chg_a --title "Add parser CLI"
cube change create --parent chg_a --title "Add parser tests"
...
cube pr sync --root chg_a
```

At any point the agent can still run raw commands in the leased workspace:

- `jj log`
- `jj diff`
- `bazel test ...`
- `gh pr view ...`

Cube is there to remove repetitive coordination work, not to prevent direct
inspection.

## Failure Modes

### Stale Lease

An agent crashes and leaves a workspace leased. Cube should expose:

- lease age,
- last heartbeat,
- owning pid/host if known,
- a `workspace force-release` command for operators.

### Metadata Drift

An agent uses raw `jj` commands that rewrite history behind Cube's back. Cube
should attempt reconciliation by matching stored `jj` change ids and descendant
relationships. If reconciliation fails, it should mark the change as needing
manual repair instead of guessing.

### PR Drift

A PR is retargeted or closed manually on GitHub. The next `cube pr sync` should
refresh remote state and either adopt the change or surface a repair action.

### Deleted Base Branch

An ancestor PR merges, its branch is deleted, and a descendant PR still targets
that deleted branch as its base. GitHub may auto-close the descendant PR even
though the local `jj` change graph is still healthy.

Cube should treat this as a first-class recovery case:

- detect descendants whose current base branch is about to disappear,
- retarget them before branch deletion whenever Cube controls the merge flow,
- if an external actor already deleted the branch, recreate the exported branch
  or reopen the descendant PR as needed,
- restack local descendants onto updated `main` or their new surviving parent
  after the remote repair.

## Implementation Sketch

Suggested phases:

1. Workspace pool and lease management.
2. Setup/provisioning engine and per-workspace setup fingerprints.
3. Local change metadata on top of `jj`.
4. PR sync to GitHub for linear stacks.
5. Safe stacked-PR merge and retarget orchestration.
6. Forked change graphs and subtree rebases.
7. Recovery commands and higher-quality status reporting.

Likely implementation language:

- Rust if Cube is intended to be a durable repo tool with structured CLI and
  local metadata,
- shell scripts are fine for exploration but will become brittle once locking,
  metadata, and PR reconciliation exist.

## Decisions Needed

The following points are still unclear enough that they should be decided
before implementation starts:

1. Should Cube own workspace creation directly, or should it integrate with an
   external workspace registry/index when one already exists?
2. Is GitHub the only PR backend worth designing for in v1, or do we want the
   storage model to anticipate multiple forges immediately?
3. Should lease expiry be time-based only, or should agents be required to
   heartbeat while they hold a workspace?
4. Do we want the change metadata to live purely in Cube's local database, or
   should some mapping also be encoded into `jj` bookmarks/commit metadata for
   easier recovery?
5. For stacked PRs, how much review-base policy should be configurable beyond
   Cube's safety invariant that descendants must be retargeted before their
   current base branch is deleted?
6. Which setup-step inputs should Cube fingerprint automatically, and which
   should the repo configuration declare explicitly?
