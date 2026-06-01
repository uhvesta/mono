# cube

`cube` is a standalone CLI that manages a pool of reusable agent
workspaces — pre-cloned `jj`/git checkouts of one or more repos — and
hands them out under short-lived leases. It owns the lifecycle of those
checkouts: provisioning them, leasing exactly one to a caller for the
duration of a single task, resetting them back to a clean state on
release, and reclaiming abandoned leases. Boss is its primary consumer
(it leases a throwaway workspace per worker session), but `cube` knows
nothing about Boss and is usable on its own.

## Architecture

A central SQLite registry is the source of truth for the whole pool.
It records the repos `cube` knows about, the workspaces that exist for
each repo and their lease state, locally-tracked stacked changes, and
per-workspace setup bookkeeping. The database lives under the platform
data directory (`~/.local/share/cube/state.db` by default, overridable
via `CUBE_DATA_DIR`/`XDG_DATA_HOME`) and is created and migrated lazily
on first use. Because several workers may hit the same pool at once,
mutating operations take a per-repo advisory file lock so leasing and
releasing are serialized; an append-only, retention-bounded audit log
records each significant lease/release event for forensics.

The command surface is organised into a handful of nouns. `repo`
registers and materializes repo pools: `repo ensure` resolves a bare
`<reponame>` through a chain — an already-registered slug, then the
user's configured `repo-resolvers` (from `~/.config/cube/cube.toml`),
then a GitHub `<org>/<name>` fallback — so `cube` stays ignorant of any
particular hosting setup. `workspace` is the heart of the tool: `lease`
selects a free workspace (or the `--prefer`'d one), health-checks it,
resets it with `jj git fetch && jj new main`, and marks it leased with
a TTL; `heartbeat` extends that TTL so long-running holders aren't
swept; `release` frees the slot and resets the working copy; and
`force-release`/`remove`/`gc` exist for recovery, registry cleanup, and
reclaiming merged bookmarks. Recovery flags (`--allow-dirty`,
`--keep-dirty`, `--reason`) let a stranded dirty workspace be reclaimed
or quarantined instead of being silently wiped. `pr ensure` pushes the
current `jj` bookmark and creates-or-reuses a GitHub PR idempotently via
`gh`. The `change`, `stack`, and `pr sync`/`pr merge` verbs are
scaffolding for stacked-change management and are only partially
implemented today.

All external work — every `jj` and `gh` invocation — flows through a
single `CommandRunner` abstraction. The production implementation shells
out (suppressing colour when stdout isn't a TTY); tests substitute a
fake runner, which is why the bulk of the lease/reset/recovery logic is
exercised without touching a real repo. Workspace provisioning can also
run a declarative per-workspace setup file (`.cube/setup.yaml`) whose
steps are gated by run policies and input fingerprints recorded in the
registry, so idempotent first-time setup isn't repeated needlessly.

Every command supports a `--json` mode, making `cube` scriptable: Boss
parses the JSON lease result (workspace path, lease id) to place a
worker, then drives `heartbeat` and `release` over the lease's lifetime.
`cube` has no internal crate dependencies and is not depended on as a
library — it is consumed purely as a binary at the process boundary.
