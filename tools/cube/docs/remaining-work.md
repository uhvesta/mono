# Cube — Remaining Work

This doc tracks the gap between cube as designed
([main.md](./main.md)) and cube as implemented today, organised by what
Boss V2 specifically needs vs the broader cube roadmap.

It is the actionable companion to the design doc: items here are
candidates for work, not aspirations.

## Status today

What works (current `main`):

- `cube repo add` / `list` / `info`
- `cube workspace lease` (single-pool, no auto-create, no setup engine,
  no `flock`)
- `cube workspace release` (resets via `jj git fetch && jj new main`)
- `cube workspace status` (delegates to `jj status`)
- `cube change create` and `cube change info` (records local
  change-graph metadata against a leased workspace; `change checkout`
  remains unimplemented)
- SQLite-backed `repos`, `workspaces`, and `changes` metadata
  (`store.rs`)
- Both `cargo build -p cube` and `bazel build //tools/cube` build
  cleanly

What's stubbed or missing — see the sections below.

The full audit lives in
[boss `v2-design-risks.md` R4](../../boss/docs/designs/v2-design-risks.md).

## V2 prerequisites

Items that must land before Boss V2 takes a hard dependency on cube.
Priority order; (1) blocks the others least and is the smallest fix.

- [ ] **(1) Add a `--database` / explicit-data-dir CLI flag.** Today,
      out-of-tree callers must set `CUBE_DATA_DIR` (`paths.rs:6`).
      The internal plumbing accepts an optional database path
      (`app.rs:114`), but no CLI flag exposes it (only `--json` is
      defined on `Cli` in `cli.rs`). Boss V2 needs a flag for clean
      per-product / per-test paths.

- [ ] **(2) Add `flock` around `claim_workspace`.** Concurrency
      currently relies solely on SQLite atomicity (`store.rs:240`).
      A repo-pool-level `flock` is documented in the design but not
      implemented.

- [ ] **(3) Implement `workspace setup`.** Today returns
      "No setup steps are configured for {workspace_id}."
      unconditionally (`app.rs:447`). The setup engine, fingerprinting,
      and `on-create` / `on-fingerprint-change` / `always` policies are
      described in
      [main.md §Setup and Provisioning](./main.md#setup-and-provisioning)
      but unimplemented.

- [ ] **(4) Auto-create workspaces from `--source` on pool
      exhaustion.** `repo add --source` accepts a seed path
      (`cli.rs:65`) but `lease` never reads it. Currently a full pool
      blocks new leases with exit code 4.

- [ ] **(5) Add lease-lifecycle commands required by Boss V2's
      integration sketch:**
      - `cube workspace heartbeat --lease <id>` — Boss-engine pings
        to refresh lease TTL
      - `cube workspace release --reason crash --keep-dirty` — release
        flag for crash recovery so cube records dirty state but frees
        the slot
      - `cube workspace force-release --lease <id>` — operator-grade
        release that bypasses ownership checks for orphan reclamation

When all five land, R4's "cube prerequisites" close.

### Already landed

- ✓ Fix the `head_commit` template parsing bug — `current_workspace_commit`
  now uses `--no-graph -r @` (`app.rs:659`); covered by tests in
  `app.rs` (e.g. line 1075).

## Beyond V2 scope

The remaining stacked-change and PR features described in the design
doc are unbuilt and not required for Boss V2 (which drives `jj` / `gh`
/ `git` directly inside leased workspaces). They are still cube's
broader roadmap.

- [ ] **`change checkout`** (`app.rs:542`, `NotImplemented`).
      `change create` and `change info` are already implemented
      (`app.rs:474`, `app.rs:545`); only `checkout` remains to round
      out the local change-graph commands.
- [ ] **`stack rebase`** (`app.rs:559`, `NotImplemented`). Subtree
      and linear rebase with descendant rewrite tracking.
- [ ] **`pr sync`** (`app.rs:566`, `NotImplemented`). Export changes
      to deterministic Git branches, push, create / update PRs,
      manage base-branch retargeting.
- [ ] **`pr merge`** (`app.rs:566`, `NotImplemented`). Stacked merge
      with branch pinning, descendant retargeting, and reopen-on-orphan
      recovery — the core value-add over hand-rolled `gh pr merge`.
- [ ] **`graph`** (`app.rs:573`, `NotImplemented`). Local change
      graph view.
- [ ] **`doctor`** (`app.rs:579`, `NotImplemented`). Diagnostic
      command for stale leases, metadata drift, deleted base branches,
      and rebase conflicts.

Schema work this implies (`repos`, `workspaces`, and `changes` exist
today in `store.rs:501-545`; `prs` is still absent):

- [ ] `prs` table for PR ↔ change mapping with branch pinning state
- [ ] migration story when this schema addition lands

## Known quirks

Smaller items that don't block but should be tracked.

- [ ] `cube workspace release` does not clean up abandoned `jj`
      changes a worker may have created. Working copy is clean for
      the next lease (because of `jj new main`), but commit history
      accretes. Optional cleanup hook on release should prune
      orphaned non-`main`-descendant changes.
- [ ] No structured logging / event emission. The integration sketch
      in R4 contemplates a "workspace `released`" notification on a
      subscription channel; today, callers must poll
      `cube workspace list --json`.
- [ ] No lease TTL enforcement. Design references a 30-min default;
      actual implementation has no expiry sweep.

## Cross-references

- Design: [tools/cube/docs/main.md](./main.md)
- Boss V2 dependency: [tools/boss/docs/designs/v2-design-risks.md](../../boss/docs/designs/v2-design-risks.md) — R4
- Boss V2 plan: [tools/boss/docs/plans/active/swiftui-boss-v2.md](../../boss/docs/plans/active/swiftui-boss-v2.md)
