# boss-engine

`boss-engine` is the long-running daemon at the heart of the Boss
automation system — the single process that owns the work database,
schedules and dispatches autonomous coding workers, watches their PRs and
CI to completion, and drives the macOS app that observes the whole
lifecycle. Every other Boss component (the `boss` CLI, `bossctl`, the
macOS app, and the `boss-event` hook shim) ultimately talks to this
daemon; the engine is where the durable state and all the policy live.

## Role as the daemon

The crate builds a single `boss-engine` binary that runs as a persistent
background process. It listens on two Unix sockets: a frontend RPC socket
the CLIs and the macOS app connect to, and an events socket the
`boss-event` shim uses to forward claude lifecycle hooks from running
workers back into the engine. On top of those, a tree of always-on
background loops (schedulers, sweeps, pollers) advance work without any
client request. The engine is intended to be the only writer of the work
database, so all coordination funnels through it rather than through
concurrent clients racing on shared state.

## Architecture

The engine is organised around a handful of cooperating subsystems that
share the work database as their common substrate.

**Work database.** A SQLite-backed store is the authoritative model of all
work — tasks, chores, projects and their executions, plus the side tables
that track PR bindings, attention items, CI remediations, conflict
resolutions, and automation run history. It serialises concurrent writes
(CLI mutations and engine loops alike) behind a busy-timeout and a set of
guard heuristics that damp runaway churn, and it exposes the typed
queries the rest of the engine builds on. Domain types and status
vocabularies come from `boss-protocol`.

**Scheduling and dispatch.** When a work item becomes ready, the
coordinator leases a throwaway git/jj workspace from `cube`, writes the
worker's per-run setup files into it, and spawns a pane-hosted claude
session through the macOS app. It tracks each worker against its slot and
lease, and — when a run ends — decides from the run's wait state whether
to release the workspace or retain it for a follow-up turn (waiting on a
human, a review, a merge, or an upstream dependency). Hook events arriving
on the events socket are correlated back to the owning run through a
process-ancestry registry.

**Completion, CI, and PR watching.** A run's Stop boundary drives the
in-band create-and-merge path, but most PRs reach their fate after the
worker has exited. A merge poller therefore sweeps open PRs to drive
`in_review → done`, detect new merge conflicts, and notice CI failures.
Conflict and CI handling each have their own detection/retire pipelines
that flip a parent item to the appropriate blocked state, record a
remediation attempt, and let a fresh worker take over — all idempotently,
so repeated probes converge rather than thrash. PR/comment text and merge
policy are shaped by the `boss-editorial`, `boss-pr-template`, and
`boss-conflict-diagnosis` helper crates; transcript reading and rendering
come from the `boss-transcript-*` crates.

**Automation.** A scheduler advances cron-style automations, collapsing
backlogs accumulated while the machine was asleep into a single
occurrence, respecting per-automation open-task caps, and recording every
decision in run history. Due occurrences are handed to a triage path that
spins up an automation worker. Feature gating for these and other paths
flows through `boss-feature-flags`.

**App RPC and live status.** The frontend socket speaks the
engine↔app/client protocol re-exported from `boss-protocol`: request
handlers for every work-management operation, and a publish/subscribe
event stream that pushes work, execution, and worker-state changes to
subscribers. A live-status subsystem summarises each active worker's
transcript on hook triggers so the app can show "what the worker is doing"
in near real time, alongside a structured trace stream the app's activity
log consumes.

**Resilience.** Because the daemon is meant to survive machine sleep,
restarts, and worker crashes, a layer of reconciliation and sweep loops
reclaims orphaned and stale workers, recovers from transient claude API
errors, unblocks dependency chains, backs up the database, and keeps an
audit trail of starts, shutdowns, and crashes. Background-loop errors are
logged but never propagated, so a transient failure can never take the
engine down.

## Where it sits

`boss-engine` depends on `boss-protocol` for its shared types and wire
protocol and on the focused engine-support crates (`boss-editorial`,
`boss-pr-template`, `boss-conflict-diagnosis`, `boss-ssh-transport`,
`boss-transcript-tail`, `boss-transcript-markdown`, `boss-feature-flags`)
for specific slices of behaviour. It is depended on by `bossctl` and the
`boss` CLI, which reach it as a library and over the RPC socket via
`boss-client`. The engine does not lease workspaces from a library — it
shells out to `cube` — and it spawns workers by asking the macOS app to
host their panes.
