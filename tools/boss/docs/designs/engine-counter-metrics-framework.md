# Boss: Engine Counter Metrics Framework

## Motivating questions

Two recent designs landed observability infrastructure for the engine and
immediately raised the same follow-up question: *we now know an event
happened, but how often does it happen, and is the cold path worth keeping?*

- PR #465 (`feat(engine): primary-path PR URL capture from worker hook
  stream`) shipped the primary path for PR-binding. The reconstruction path
  (`detect_pr` → `jj_candidate_commit_shas` → `gh api commits/{sha}/pulls`)
  is preserved as a cold-path fallback for engine-restart recovery. **Open
  question:** does the cold path actually fire in production? If we
  instrument both paths with counters
  (`pr_url_capture.primary_path.hit`,
  `pr_url_capture.reconstruction_path.hit`,
  `pr_url_capture.reconstruction_path.failed`) and observe primary >> 0,
  reconstruction == 0 for weeks, we can simplify.
- T387's reconciliation chore observed a ~3-hour stale block in the
  dependency-unblock sweeper. A `longest_stale_seconds` gauge per sweep
  pass would have told us before the chore caught it.

In both cases, the question is **rate over time**, not *did this specific
event happen* — the latter is already covered by
[`engine-dispatch-instrumentation`](engine-dispatch-instrumentation.md).
Today the engine grows a bespoke `XxxStats` struct + bossctl verb every
time someone wants to count something:

- `live_status_loop::DispatcherStats` — ~10 atomic counters, surfaced via
  `bossctl live-status debug`.
- `merge_poller::SweepOutcome` — per-bucket counts logged at info level,
  not queryable.
- A long tail of `tracing::info!("released worker pane …")` log lines that
  are de facto counters but aren't queryable.

Each addition is a one-line counter at the call site plus 30+ lines of
struct + getter + RPC plumbing. That ratio is the problem this framework
fixes.

## Goals

- Declaring a new counter is a one- or two-line change at the call site,
  with no bespoke RPC, struct, or bossctl verb.
- Counters are queryable by an operator without tailing logs or restarting
  the engine.
- Counter values survive engine restart so questions like "did the
  reconstruction path fire over the last two weeks?" are answerable.
- The framework subsumes the existing ad-hoc counter clusters
  (`DispatcherStats`, `SweepOutcome`) on a migration path, so the engine
  trends toward one counter mechanism rather than three.
- Bounded memory and disk cost — counters are cheap; the framework must
  not become an attractive nuisance for high-cardinality data.

## Non-goals

- **A generic time-series database.** No queries over arbitrary time
  ranges, no rate-over-time aggregation inside the engine, no alerting.
  If we ever need that, we integrate with Prometheus / OpenTelemetry
  rather than reinventing it.
- **Histograms in v1.** Latency questions (T387's sweep cadence) are
  approximated with a `longest_stale_seconds` gauge — a single value
  updated on each sweep — instead of a per-sample histogram. Histograms
  add real memory and surface complexity; defer until a counter and a
  gauge cannot answer the question.
- **Tag / dimension support.** `pr_url_capture.primary_path.hit` is a
  separate counter from `pr_url_capture.reconstruction_path.hit`; we do
  not support `pr_url_capture.path_kind{kind="primary"}`. Prom-style
  tags add memory cost and an unbounded-cardinality footgun; if a future
  use case needs them, that's a follow-up design.
- **Worker-side metrics.** Workers are Claude sessions that don't have
  access to engine state. Their telemetry is the existing hook event
  stream and the dispatch event stream from
  [`engine-dispatch-instrumentation`](engine-dispatch-instrumentation.md).
  This framework is engine-side only.
- **Boss app (macOS) metrics.** The app's frame rate, view render times,
  etc. are a different concern.
- **Replacing
  [`engine-dispatch-instrumentation`](engine-dispatch-instrumentation.md).**
  That design is about *discrete events* attributed to a specific
  execution — "did stage 5 succeed for `exec_…`?". This design is about
  *aggregate rates* — "how many stage 5 successes since last restart?".
  Both are needed; they don't overlap.

## Alternatives considered

### Alternative A — Stay with bespoke `XxxStats` structs

Keep growing one struct per subsystem (`DispatcherStats`,
`SweepOutcome`, …), each with hand-written getters and a bespoke
surfacing path.

**Why not:** the motivating cost. Every new counter is ~30 lines of
plumbing for a one-line conceptual change. The surfacing path is
inconsistent — some go through `bossctl live-status debug`, some are
info-level log lines you can't query, some live in test outcomes that
production never sees. There is no shared persistence, so the
"reconstruction path over two weeks" question cannot be answered at all.
This is the status quo we are explicitly trying to leave.

### Alternative B — Embed Prometheus client library + expose `/metrics`

Pull in `prometheus` (or `metrics` + `metrics-exporter-prometheus`) and
expose a scrape endpoint. Operators run a local Prometheus or just
`curl` the endpoint.

**Why not:** wrong shape for Boss's deployment. Boss is single-host,
runs on a developer laptop, and has no "always-on" scrape infrastructure.
Adding a Prometheus dependency means either (a) the operator runs
Prometheus locally just to query "did the cold path fire" — too much
weight for the question — or (b) we ship without a scraper and the
counters are reset on every engine restart, which defeats the
multi-week-observation use case. The dependency surface and the
in-memory-only default both fail the goals above. If we ever need true
TSDB semantics we should bring in Prometheus *as an integration*
(scraping an export endpoint we expose alongside our own surface), not
as the primary storage.

### Alternative C — `tracing` + log scraping

Emit each counter increment as a structured `tracing::info!` line with a
known target (`metrics.pr_url_capture.primary_path.hit`). An operator
greps / `jq`s `/tmp/boss-engine.log` to get totals.

**Why not:** O(N events) on disk for the *count* of something is wildly
asymmetric — a counter increment is 16 bytes in memory and ~200 bytes
as a tracing line. Worse, log rotation deletes the counter history; the
"two weeks of data" question becomes "two weeks of unrotated logs",
which is fragile. Tracing is the right tool for *what happened with
context*; it is the wrong tool for *how many times*.

### Alternative D — In-memory only, reset on restart (matches DispatcherStats)

Counters live in `Arc<AtomicU64>`s, never persist, reset on engine
restart. Surface via `bossctl metrics`.

**Why not:** the motivating use case explicitly wants days-to-weeks of
data to decide whether to keep the PR-reconstruction cold path. Engine
restarts happen — on every `bossctl` upgrade, on every deliberate
restart for testing, on every host reboot. An in-memory-only design
gives us the API ergonomics win but loses the question we are
designing this for.

### Chosen approach — In-memory counters, periodic flush to `state.db`

A hybrid. Counters live in `Arc<AtomicU64>`s for the hot path (no lock,
no disk on increment) and are flushed to a new `metrics` table in the
existing `state.db` every 30 seconds and on graceful shutdown. On engine
start the framework reads the table back into the in-memory counters so
the values are continuous across restarts.

This gives us:
- the API ergonomics of Alternative D (one-line increment, no lock),
- the persistence of Alternative B (multi-week answers possible) without
  adding a Prometheus dependency,
- bounded disk cost: one row per counter, ~50 counters expected → KB,
  not MB.

The rest of this design covers the chosen approach in detail.

## Chosen approach

### Counter declaration

A counter is declared once at module load via a `register_counter!` macro
that takes a stable string name and a one-line description, and returns
a typed handle the call site uses to increment.

```rust
// tools/boss/engine/src/metrics/registry.rs
register_counter!(
    PR_URL_CAPTURE_PRIMARY_HIT,
    "pr_url_capture.primary_path.hit",
    "On-stop hook found a staged PR URL and skipped the detector."
);
```

At the call site:

```rust
// tools/boss/engine/src/on_stop.rs
PR_URL_CAPTURE_PRIMARY_HIT.inc();
```

That's the one-line cost. No struct, no getter, no RPC field, no
bossctl verb addition — the registry is the source of truth and every
surface enumerates from it.

The macro expands to a `LazyLock<Counter>` that registers itself with
the global registry on first use. Names are validated at registration
time: lowercase ASCII letters, digits, dots, underscores; dot-separated
namespaces by convention (`pr_url_capture.primary_path.hit`). Duplicate
names panic at registration — surfaced at engine startup, not at
runtime when the increment finally fires.

### Gauges (the "last observed value" shape)

A second primitive, `register_gauge!`, for the T387-class question:
"what was the longest stale dependency block observed in the most
recent sweep?". A gauge is a single atomic `i64` overwritten by the
producer, read by the surface. No history, no aggregation — the
producer is responsible for choosing what value to publish.

```rust
register_gauge!(
    DEPENDENCY_UNBLOCK_LONGEST_STALE_SECONDS,
    "dependency_unblock.longest_stale_seconds",
    "Longest observed (now - last_check) over blocked rows in the last sweep."
);

// later, at end of sweep:
DEPENDENCY_UNBLOCK_LONGEST_STALE_SECONDS.set(longest);
```

Gauges are the minimum-viable answer to "I want a value over time"
without committing to histograms. The `metrics` table stores the most
recent gauge value plus the timestamp it was published; that's enough
for an operator to ask "did this ever exceed 3600?" with a manual
inspection script.

### Aggregation: monotonic counters only (plus gauges)

Counters are monotonic. There is no `dec()`, no `add(-1)`. A counter
that needs to go down is two counters (a successes counter and a
failures counter — the consumer subtracts) or a gauge.

This is a deliberate constraint to keep the framework's mental model
small and to prevent abuse (a non-monotonic "counter" is a gauge with
extra steps). Histograms and summaries are explicitly out of scope.

### Reset semantics

Counters are **truly monotonic across the framework's lifetime** — they
do not reset on engine startup, because the values are persisted to
`state.db` and rehydrated on start. The only ways a counter goes back
to zero are:
1. An operator runs `bossctl metrics reset <name>` (or `--all`), which
   writes 0 to the `state.db` row and clears the in-memory atomic.
2. The `state.db` is deleted (full Boss state reset).

This matches the "did the reconstruction path fire in the last two
weeks?" use case directly: the value is a running total since the last
explicit reset.

Gauges are *overwritten* on each producer publication; there is no
"reset" for a gauge — the producer is the source of truth.

### Persistence: `state.db` table

```sql
CREATE TABLE IF NOT EXISTS metrics_counter (
  name           TEXT PRIMARY KEY,
  value          INTEGER NOT NULL,
  updated_at_ms  INTEGER NOT NULL,
  description    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS metrics_gauge (
  name             TEXT PRIMARY KEY,
  value            INTEGER NOT NULL,
  observed_at_ms   INTEGER NOT NULL,
  description      TEXT NOT NULL
);
```

The framework owns a periodic flush task on the engine runtime that, every
30 seconds, snapshots every registered counter / gauge and `UPSERT`s into
the appropriate table in a single transaction. Cost is one transaction
per 30s flush, with ~50 rows touched — negligible against the engine's
existing write traffic.

On engine startup, the framework reads every row and seeds the
in-memory atomic for the matching registered counter. Rows in the table
without a matching registered counter are *kept*, not deleted — they
represent counters from a previous engine version (e.g. an A/B
experiment that was removed) and the operator may still want to query
them via `bossctl metrics show`. They are surfaced as "stale: not
registered by current engine" in the listing.

On graceful shutdown, one final flush runs before the SQLite handle
closes. On crash, up to 30s of counter increments are lost — acceptable
for monotonic counts, since the next flush after restart resumes from
the last persisted value and the framework user can read the cost as
"counts are accurate to within ~30s of crash time".

### Surface: `bossctl metrics` verbs

Three new verbs, all read-only against `state.db` (no engine RPC needed
for read paths — operator can read counters even if the engine is
wedged):

```
bossctl metrics list [--prefix <pattern>] [--json]
bossctl metrics show <name> [--json]
bossctl metrics reset <name | --all>     # the one write path; goes through engine RPC
```

- `list` enumerates registered counters and gauges with their current
  value and last-update time. Default rendering is one line per metric:
  ```
  pr_url_capture.primary_path.hit            42  (3m ago)   counter
  pr_url_capture.reconstruction_path.hit      0  (never)    counter
  dependency_unblock.longest_stale_seconds 10921 (1m ago)   gauge
  ```
  `--prefix pr_url_capture` filters; `--json` emits the structured
  form for tooling.
- `show <name>` prints a single metric with its description, current
  value, and metadata. Useful for the operator who has the name from a
  design doc and wants the value.
- `reset` is the one write path. It runs through engine RPC because the
  in-memory atomic must be cleared in lockstep with the `state.db`
  row; doing it via direct sqlite write would leave the atomic stale
  until the next flush, which is confusing.

This intentionally does *not* extend `bossctl live-status debug`. The
`live-status` verb is a snapshot of *live worker state* — counters are
engine-global and unrelated. Mixing them would re-introduce the
"DispatcherStats is buried inside the live-status debug payload" ugliness
this design is trying to leave behind.

### Subscription topic (deferred)

A `metrics.updates` topic broadcasting `(name, value, observed_at_ms)`
on each flush would let the Boss app surface counter values live in a
debug pane. Not part of v1 — the file-and-CLI surface is the foundation.
Add the topic when there is a concrete UI consumer.

### Migration: `DispatcherStats` and `SweepOutcome`

Both are migrated in phase 2 (see below). Migration replaces:

- ~10 hand-written getter / `AtomicU64` pairs in `DispatcherStats` with
  ~10 `register_counter!` declarations; the `inc_*` methods become
  one-line wrappers that call into the framework, then are deleted
  in a follow-up once call sites move over.
- `DispatcherStatsReport` (the wire type currently emitted by
  `bossctl live-status debug`) stays in place but is populated by
  reading from the framework registry rather than the struct. We
  keep the wire type as a backwards-compat alias for two releases,
  then delete it.
- `SweepOutcome` is harder: it is currently a *per-sweep return value*
  used by callers and tests, not a global counter cluster. The
  migration plan is to *keep* the struct as a return value (callers and
  tests still need per-sweep counts) but to also `inc()` the matching
  registered counters from within `run_one_pass`. The framework becomes
  the global rate surface; `SweepOutcome` remains the per-call
  granularity surface.

The migration commits to a direction: **the framework is additive over
existing counter clusters**, not a forced rewrite. New counters use
the framework. Existing clusters migrate when a touch is needed
anyway.

### What ships against the framework after it lands

Five concrete counters / gauges, each filed as a sibling chore once this
design is approved:

1. **`pr_url_capture.{primary_path.hit, reconstruction_path.hit,
   reconstruction_path.failed}`** — the motivating triplet. Three
   `inc()` call sites in `on_stop.rs` and `recheck_for_pr`.
2. **`dependency_unblock.longest_stale_seconds`** (gauge) — set once at
   the end of every dependency-unblock sweep.
3. **`cube_workspace_lease.{success, failure, attempts}`** — three
   counters around the cube lease boundary in
   `ExecutionCoordinator::schedule_execution`.
4. **`hook_events.received_total`** — replaces
   `DispatcherStats::hook_events_total`. (Plus the rest of
   `DispatcherStats` as part of the migration.)
5. **`merge_poller.{merged, conflict_flagged, conflict_cleared,
   pr_recheck_recovered, conflict_redispatched,
   pr_recheck_unresolved}`** — counters that mirror `SweepOutcome` and
   make sweep totals queryable across restarts.

## Implementation phases

Vertical slices, each independently mergeable. Each phase is a sibling
chore filed against this project after the design is approved.

### Phase 1 — Registry, primitives, persistence

- `metrics/registry.rs` with `register_counter!` / `register_gauge!`
  macros, `Counter` / `Gauge` handles, and a global `Registry`.
- New `metrics_counter` and `metrics_gauge` tables in `state.db` plus
  the migration to add them.
- Periodic flush task on engine startup; final flush on shutdown.
- Seed-from-db on startup so values are continuous across restarts.
- Unit tests: register, increment, snapshot, flush+restart round trip,
  duplicate-name panic.

### Phase 2 — `bossctl metrics list / show / reset`

- Three new bossctl verbs.
- `list` and `show` read `state.db` directly; `reset` goes through
  engine RPC (new control verb: `MetricsReset { name: Option<String> }`).
- Pretty rendering for the human surface; `--json` for tooling.

### Phase 3 — Wire the motivating counters

- The `pr_url_capture.*` triplet at the three call sites identified in
  PR #465.
- `dependency_unblock.longest_stale_seconds` in the sweep.
- `cube_workspace_lease.*` in `schedule_execution`.

This phase is the smoke test: counters that answer real operational
questions, going through every surface (registration, increment,
flush, list, show, reset).

### Phase 4 — Migrate `DispatcherStats`

- `register_counter!` for each existing field.
- `inc_*` methods become one-line wrappers calling into the framework.
- `DispatcherStatsReport` is populated from the registry; the wire
  type stays as a compat shim.
- After one release, delete the shim and the wrapper methods.

### Phase 5 — Mirror `SweepOutcome` into counters

- `merge_poller::run_one_pass` calls `MERGE_POLLER_MERGED.inc_by(n)`
  etc. at the end of each pass.
- The `SweepOutcome` struct stays — callers and tests still need
  per-call counts. The framework adds the global view.

### Phase 6 — Subscription topic (deferred)

- `metrics.updates` topic on the existing broker, fired on each flush.
- Boss app debug pane consumes it. Filed separately when a UI consumer
  is ready.

## Risks / open questions

1. **`bossctl metrics list` reads `state.db` directly.** The framework
   flushes every 30s, so a freshly-incremented counter is up to 30s
   stale on the read side. For "did the cold path fire over two weeks"
   this is irrelevant; for "did my last test increment work?" it's
   surprising. The mitigation is `bossctl metrics show --live` which
   *does* go through engine RPC and reads the in-memory atomic. Open
   question: do we ship `--live` in v1, or wait until someone hits the
   30s-staleness surprise? Recommendation: ship `--live` in phase 2,
   it's cheap.

2. **Registration timing.** `LazyLock`-based registration means a
   counter doesn't appear in `bossctl metrics list` until its module
   has been loaded by the engine. Cold-path counters in modules that
   are only loaded on demand (`recheck_for_pr` for example) may not
   show up immediately. Mitigation: a `metrics::init_all()` function
   called from `app.rs::start()` that explicitly references every
   counter handle, forcing registration at boot. Verbose but
   predictable. Recommendation: take it; the cost is one import line
   per module.

3. **Stale rows in `state.db`.** A counter that gets renamed leaves
   its old row behind under the old name. We choose to *keep* these
   (annotated "not registered by current engine") so historical
   answers stay queryable, but the table grows over time. At ~50
   counters expected, this isn't a real problem; we should not need
   compaction. Revisit if the count grows past ~500.

4. **Counter naming conventions.** This design uses dot-separated
   namespaces (`pr_url_capture.primary_path.hit`) by analogy with
   Prometheus / OpenTelemetry conventions, but we don't enforce a
   schema. Two engineers could pick `merge_poller.merged` vs
   `merge.poller.merged` and both pass validation. Mitigation: a
   one-page conventions section in this design plus a lint script
   that diff-checks registered names against a regex. Recommendation:
   document the convention here, do *not* lint until we see drift in
   practice.

5. **What about gauges that need a "last reset" semantic?** A gauge
   like `longest_stale_seconds` is only meaningful for the most
   recent sweep — if no sweep has run in 10 minutes, the value is
   stale. The framework records `observed_at_ms` per gauge, so an
   operator can see freshness, but there's no automatic decay.
   Acceptable for v1; the operator reads the timestamp.

6. **Concurrency cost of `register_counter!` panic on duplicate.** A
   panic at startup is fine; a panic at runtime (lazy registration on
   first increment of a duplicate name) would be terrible. The
   mitigation in (2) — explicit `init_all()` at boot — ensures any
   duplicate trips during engine startup before traffic, not in the
   middle of a busy sweep. This is load-bearing; the design should
   not relax (2) without revisiting (6).

7. **Test ergonomics.** Unit tests need a way to assert on counter
   values without leaking state across tests. The plan: counters are
   global by default (which is fine for the engine's single-instance
   shape) but tests construct a *test-local* `Registry` via a
   `with_test_registry` helper that swaps the global pointer for the
   duration of the test. Cost is one extra Mutex around the global
   handle; benefit is fully-isolated tests. Open question: is the
   global handle worth the test scaffolding, or should we plumb a
   `&Registry` through the engine like other singletons? Engine
   already plumbs `Arc<DispatcherStats>` through `App`; consistency
   argues for plumbing `Arc<Registry>` too and skipping the global
   altogether. **Recommendation: plumb `Arc<Registry>`, no global.**
   The macro then expands to a `RegistryHandle` resolved against the
   `App`'s registry at call time. Slightly more code, no test
   scaffolding hack, matches existing engine conventions.

## Related designs

- [`engine-dispatch-instrumentation`](engine-dispatch-instrumentation.md)
  — discrete per-execution events. This framework owns aggregate
  rates; that design owns event timelines. They are deliberately
  separate streams.
- [`worker-live-status`](worker-live-status.md) — per-worker live
  state. Counters are engine-global and unrelated; the design
  explicitly does not extend `bossctl live-status debug`.
- [`work-execution`](work-execution.md) — execution lifecycle.
  Several counters in phase 3 (`cube_workspace_lease.*`,
  `pr_url_capture.*`) are about the lifecycle's edges.
