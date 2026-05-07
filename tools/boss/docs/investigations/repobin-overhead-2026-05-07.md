# Repobin per-call overhead — where the ~325 ms goes

**Date:** 2026-05-07
**Author:** investigation chore
**Sibling chore:** `boss task create-many` bulk verb (queued the same day)

## TL;DR

On a warm cache, every `boss` / `bossctl` invocation pays ~325 ms when run from a
non-workspace cwd (the case the user reported), and ~750 ms when run from inside
the mono workspace itself. Both numbers are dominated by **two `bazel` client →
server roundtrips per invocation** — `bazel build` then `bazel cquery` — which
repobin issues *unconditionally* on every dispatch, even when the target binary
on disk is already up to date.

Process startup, repo cache check, git, and subprocess fork+exec are
collectively under ~20 ms. There is no `git fetch` or network op in the warm
path — the cache TTL gate (`fetch_within_ttl`) elides remote git operations
correctly. Repobin's own work is cheap; the cost is bazel.

## Reproducer and baseline

User-reported numbers (from `~`, warm cache):

```
$ time (boss --json --no-input --no-autostart product list >/dev/null 2>&1)
0.01s user  0.01s system  5% cpu  0.346 total
$ time (boss --json --no-input --no-autostart product list >/dev/null 2>&1)
0.01s user  0.01s system  4% cpu  0.325 total
```

Reproduced on the same machine, same day, after warmup:

| cwd | run 1 | run 2 | run 3 | run 4 | run 5 |
|---|---|---|---|---|---|
| `~` (default-mode / cache repo) | 1.51 s* | 0.32 s | 0.32 s | — | — |
| `~/Documents/dev/workspaces/mono-agent-004` (workspace) | 1.13 s* | 0.70 s | 0.76 s | 0.80 s | 0.77 s |

\* first call after a long idle period — bazel JVM server cold-starts.

Two distinct steady-state floors emerge: **~325 ms** in default mode (cache
repo) and **~750 ms** in workspace mode. Same code path either way, the
difference is the size of the bazel server's loaded build graph.

## Phase-by-phase breakdown

Each component measured in isolation, warm, on the same machine.

### 1. Process startup + Rust runtime + repobin's own work

Run a repobin command that takes the same code path *up to* the dispatch
decision but does no bazel work (`repobin list`):

```
real 0.01  real 0.01  real 0.00  real 0.00  real 0.00
```

**~10 ms.** Mach-O load, dyld, Rust runtime init, argv parsing, REPOBIN.toml
read, and exit. Negligible.

### 2. The actual `boss` binary, invoked directly

Bypassing repobin entirely (running `bazel-bin/tools/boss/cli/boss` directly):

```
real 0.01  real 0.01  real 0.01  real 0.01  real 0.01
```

**~10 ms.** Boss CLI itself — clap parsing, config read, engine socket dial,
request, response, JSON output — completes in roughly 10 ms warm. The boss CLI
is *not* the slow thing.

### 3. `bazel build` (warm, nothing to do)

```bash
$ bazel build --color=no --curses=no --show_result=0 --noshow_progress \
    --ui_event_filters=-info //tools/boss/cli:boss
```

| cwd | warm runs (real seconds) |
|---|---|
| cache repo (`~/.cache/repobin/repos/mono-*/checkout`) | 0.18, 0.16, 0.17 |
| workspace | 0.42, 0.36, 0.40, 0.37, 0.40 |

**~170 ms (cache) / ~400 ms (workspace)** even when there is nothing to build.
This is the bazel client connecting to the running JVM bazel server, the server
walking the action cache to confirm the target is up to date, and returning.
Nothing builds; this is pure analysis overhead.

### 4. `bazel cquery` (warm)

```bash
$ bazel cquery --color=no --curses=no //tools/boss/cli:boss \
    --output=starlark '--starlark:expr=target.files_to_run.executable.path ...'
```

| cwd | warm runs (real seconds) |
|---|---|
| cache repo | 0.14, 0.13, 0.14 |
| workspace | 0.39, 0.36, 0.40, 0.37, 0.39 |

**~140 ms (cache) / ~380 ms (workspace).** Same pattern: cquery has to load and
analyse the build graph to resolve `files_to_run.executable.path`, even though
the answer is identical to the previous invocation a second ago.

### 5. Bare bazel client → server roundtrip floor

```
$ bazel info workspace
real 0.08  real 0.04  real 0.04
```

**~40–80 ms.** Establishes the absolute floor for any bazel call. Both `build`
and `cquery` are well above this floor because they involve graph analysis, not
just an info query.

### Summing it up

```
default mode (cache repo, cwd=~)
  process startup + dispatch          ~10 ms
  RepoCache.lock + ensure_up_to_date  <5 ms (TTL gate, no git)
  bazel build (warm)                 ~170 ms
  bazel cquery (warm)                ~140 ms
  exec + boss CLI                     ~10 ms
                                     ──────
                                     ~335 ms  → matches observed ~325 ms

workspace mode (cwd=mono workspace)
  process startup + dispatch          ~10 ms
  bazel build (warm)                 ~400 ms
  bazel cquery (warm)                ~380 ms
  exec + boss CLI                     ~10 ms
                                     ──────
                                     ~800 ms  → matches observed ~750 ms
```

## Specific questions the chore asked

> Is repobin running `git fetch` / `bazel` / any subprocess on warm cache? If
> so, when and why?

**`bazel`:** Yes, *twice*, on every single invocation, unconditionally. See
`tools/repobin/src/dispatch.rs:62-65`:

```rust
fn plan_from_target<B: BazelAdapter>(
    bazel: &B, repo_root: &Path, tool_name: &str, target: &str, ...
) -> Result<DispatchPlan, RepobinError> {
    bazel.build(repo_root, target)?;
    let executable_path = bazel.resolve_executable(repo_root, target)?;
    ...
```

There is no on-disk freshness check, no mtime gate, no skip-if-built logic.
Every dispatch goes through both `bazel build` and `bazel cquery`.

**`git`:** No, not in the warm path. `RepoCacheLock::ensure_up_to_date`
(`tools/repobin/src/cache.rs:111-140`) gates remote git work behind a TTL stamp
(`fetch_within_ttl`, default 300 s, `REPOBIN_DEFAULTS_TTL_SECS`). Within the
TTL the function returns `Cached { refreshed: false }` after a single
`metadata()` call on `fetch_stamp` — no `git rev-parse`, no `git ls-remote`, no
`git fetch`. On TTL expiry it does run `git rev-parse HEAD` and `git ls-remote
origin HEAD` to compare, but that's not the warm-cache case.

In workspace mode (cwd inside the mono repo, `REPOBIN.toml` found locally), the
cache code path is not taken at all — `prepare_dispatch` succeeds without
falling through to `prepare_default_plan`, so there is no git subprocess and no
cache lock.

**Other subprocesses:** Just the final `exec` of the resolved binary
(`Command::exec` in `app.rs:412-416`) — that's the boss/cube/etc. binary
itself, not overhead.

## Architecture observation

The two bazel calls serve different purposes and neither is obviously
elide-able without redesign:

- `bazel build` exists to make sure the target binary is up to date on disk
  before we exec it. Removing this would mean stale binaries when the user
  edits sources without manually running bazel.
- `bazel cquery` exists to resolve the absolute output path
  (`bazel-bin/.../boss`). The path is stable in practice, but bazel does not
  guarantee that — it can change with `--config`, platforms, or transitions.

Both calls re-do graph analysis from scratch every invocation. There is no
in-memory daemon on the repobin side and no on-disk cache of "for target T at
configuration C, the executable is at P, validated as fresh at time T0".

The bazel JVM server *does* cache the analysis state, which is why the warm
numbers are 170–400 ms and not several seconds. But there is no way to skip
the client→server roundtrip entirely without bypassing bazel.

## Suggested follow-up chores (not in scope here)

Ranked by expected impact on the 325 ms floor:

1. **`repobin` resolved-path + freshness cache** *(est. saves 280–310 ms,
   reduces warm overhead to ~15–40 ms)*

   On dispatch, look up `(repo_root, target)` in a small on-disk cache that
   stores the resolved `bazel-bin/...` path plus a "validated at" timestamp
   and a content hash of nearby `BUILD.bazel` / `MODULE.bazel` / source mtime
   bound. If the cache entry is recent (configurable TTL, e.g. 60 s) and the
   resolved binary still exists with mtime ≥ the cache entry's "validated at",
   skip both `bazel build` and `bazel cquery` and exec the binary directly.

   On TTL expiry or any mtime/hash mismatch, fall back to the current path.
   Failure mode is at worst "we exec'd a binary that bazel might have decided
   needed rebuilding 90 seconds ago" — same risk as a developer who hasn't run
   bazel since editing.

2. **Skip `bazel build` if `bazel-bin/<runfiles_path>` exists and is newer than
   any tracked source under the target's package** *(est. saves 170–400 ms,
   keeps `cquery`)*

   Cheaper variant of (1): only optimise the build step. Still pays the cquery
   cost.

3. **`boss task create-many` bulk verb** *(already queued as a sibling chore)*

   Doesn't reduce per-call overhead but collapses N calls into one, which is
   the actual fix for the coordinator/script use cases. Should land regardless
   of (1) and (2).

4. **`REPOBIN_TRACE=1` instrumentation flag** *(diagnostic, not a fix)*

   Add tracing-spans-to-stderr behind an env var so future regressions are
   easy to attribute. Low risk, low cost. Would make this investigation
   reproducible without external timing tools.

Recommend (1) as the headline fix — it eliminates ~95 % of repobin's overhead
and turns `boss` into a near-free wrapper. (3) is the right fix for the
specific N-call-stack problem the user hit. (2) and (4) are optional.

## Validation notes

- All measurements were taken on the same machine on 2026-05-07 with bazel
  warm (server already running). Cold-start runs (1.1–1.5 s first call after
  idle) are excluded from the per-phase tables but visible in the reproducer
  table above.
- `dtruss -f -t execve` requires disabling SIP and was not used; the timing
  decomposition is built from `/usr/bin/time -p` on each isolated phase, plus
  reading the dispatch source. The numbers reconcile to within ~10 ms of the
  observed end-to-end totals, which gives high confidence that no significant
  phase was missed.
- `REPOBIN_VERBOSE=1` confirms the default-mode path runs from
  `~/.cache/repobin/repos/mono-*/checkout` and prints
  `repobin: building //tools/boss/cli:boss...` on every invocation —
  consistent with the unconditional `bazel.build` call in dispatch.
