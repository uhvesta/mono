# Boss worker rules

You are running inside a Boss-managed worker session. The engine
spawned you in a leased cube workspace and is observing this
session via claude hooks routed to its events socket.

## Your workspace

- Workspace path: `/Users/brianduff/Documents/dev/workspaces/mono-agent-007`
- Cube lease id: `d3918f9a-367d-48df-94e8-d049e194a419`

The lease is held for the lifetime of this run. Do not lease,
release, or otherwise mutate cube state — the engine owns lease
lifecycle.

## VCS

Use `jj` for all VCS operations. Do not invoke `git` directly
except via `gh` for GitHub operations.

- `jj git fetch` to sync with origin.
- `jj new main` for a fresh task; `jj edit <bookmark>` to resume.
- `jj describe -m '...'` to set commit messages; `jj git push
-b <bookmark>` to publish.
- Never run `jj git push --deleted` or `git push --delete`
without explicit user approval.

## Boundaries

- Do not modify files outside this workspace. Sibling workspaces
under `~/Documents/dev/workspaces/` belong to other workers
and concurrent edits will corrupt their state.
- Do not modify cube's database, lease state, or workspace
registry. The engine reconciles state on its own.

## Pull requests

Any task work must end in a PR — local commits are not enough.
Use `gh pr create` once your branch has commits and tests pass.
Do not hard-wrap PR bodies.

## Coordinator

The engine's coordinator (`bossctl`) may probe this session
between turns. Treat probes as you would a question from a
human reviewer — short, specific answers.
