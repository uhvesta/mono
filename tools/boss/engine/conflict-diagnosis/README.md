# boss-conflict-diagnosis

Pre-spawn merge-conflict analysis for Boss's conflict-resolution flow.
When a PR can no longer rebase cleanly onto its base, the engine uses
this crate to probe *what* would conflict before it spawns a worker to
fix it. The result is a structured, JSON-serialisable diagnosis the
worker prompt embeds so the resolution session starts with concrete
context instead of discovering the conflicts from scratch.

## How it fits

The crate exposes one async entry point, `collect`, which the engine
calls with a workspace path plus the base and head shas it has already
resolved. `collect` shells out to `git merge-tree` to compute the
would-be merge without touching working state, then parses the output
into a `ConflictDiagnosis` — the base/head shas, a per-file list of
`ConflictedFile` entries (path and a coarse conflict `shape`), and an
optional `error`. The diagnosis is deliberately *pure-ish*: it reads
from git but mutates nothing.

Two design constraints shape the implementation. First, probes must
never block a spawn: a failed probe (git missing, unresolvable refs,
unexpected exit code) returns a populated diagnosis with empty `files`
and a set `error` rather than an `Err`, so the worker can still take
over from a fresh rebase. The `Err` path is reserved for genuine caller
misuse. Second, Boss runs inside jj-only cube workspaces that have no
top-level `.git`, so the crate resolves the real git store via
`.jj/repo/store/git_target` and passes it as `GIT_DIR`, falling back to
a colocated `.git` for dev/test fixtures.

It also handles git-version drift: the modern `git merge-tree
--write-tree` form (git ≥ 2.38, signals conflicts via exit code) and
the legacy three-argument form (older git, signals conflicts via markers
in stdout) are parsed by separate paths, selected from the running
binary's version and defaulting to the safer legacy form when the probe
fails.

## Boundaries

This is a focused leaf crate with no internal Boss dependencies; it
depends only on `serde` (for the wire shape) and `tokio` (for spawning
git). `boss-engine` is the sole consumer — it persists the JSON in the
`conflict_resolutions.conflict_diagnosis` column and renders the markdown
surface when composing the worker's execution prompt. The schema mirrors
what the auto-rebase flow records so a future unified attempts view can
render both kinds with one template; bump `schema_version` when the shape
changes so consumers can refuse unknown versions.
