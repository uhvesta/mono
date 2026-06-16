# Why does the `checks` CI step (checkleft) take 12–29s with 0 findings?

**Status:** investigation / writeup (no code change)
**Date:** 2026-06-14
**Scope:** the `:white_check_mark: checks` Buildkite step in the `flunge/mono` pipeline, which runs `checkleft run`.

## TL;DR

The 12s-vs-29s gap the operator noticed is real, but it is the *small* end of a much wider distribution: the same step ran the same class of work in anywhere from **2s to 56s** across recent builds. The headline number ("11 checks, 0 findings") is misleading — 0 *findings* does not mean 0 *work*. Three of the checks (`lint/rust`, `format/rust`, `rust-test-rule-coverage`) do real work whenever the change touches Rust, and **`lint/rust` runs clippy through `bazel build` on the reverse-dependencies of the changed Rust files**. For build 3381/3383 the commit edited two files inside `boss-engine-core` (a large crate), so clippy had to compile that crate's rdeps.

That clippy-via-bazel compile is the dominant cost, and its duration is gated almost entirely by **how warm the bazel disk/action cache is on whichever agent the step happened to land on** — not by the number of checks, not by checkleft startup, and not by acquiring the checkleft binary. The variance is therefore explained by (1) the `bazel-any` queue scattering the step across heterogeneous Linux/macOS agents with independent caches, and (2) those caches being cold vs warm at the moment the step runs.

**Floor:** when everything is warm and the change is small, the 11 checks finish in **~2s** of checkleft time (~5–9s job wall including checkout + repobin). The 29s run is roughly **20–25s of avoidable cold clippy/bazel work**.

## What was measured

Two builds of the **same commit** `90e3b366` (11 checks, 0 findings in both):

| | Build 3381 | Build 3383 |
|---|---|---|
| Buildkite build | [3381](https://buildkite.com/flunge/mono/builds/3381) (merge-queue, PR #1524) | [3383](https://buildkite.com/flunge/mono/builds/3383) (main, post-merge) |
| `checks` job wall | **32.3s** (04:15:04.359 → 04:15:36.654) | **18.3s** (04:17:28.403 → 04:17:46.748) |
| checkleft self-report | `11 checks ran in 29s` | `11 checks ran in 12s` |
| Agent the step ran on | `empiricist-2` (**Linux**, `/var/lib/...`, `/mnt/ssd/bazel`) | `anaplian-1` (**macOS arm64**, `/opt/homebrew/...`, `/Volumes/ssd/bazel`) |
| repobin install bazel | `Elapsed time: 0.736s` (warm) | `Elapsed time: 2.692s` + *"discarding analysis cache (this can be expensive)"* |

The first thing the grounding got slightly wrong: these two runs were **not** the same agent warm-vs-cold. They ran on **two different machines of two different OSes**. But that turns out to be a secondary factor — see the variance section.

### The wider distribution (the real story)

Pulling the `checks` job duration for the last ~30 builds and grouping by agent shows the cost is **bimodal even on a single machine**:

| Agent | OS | Observed `checks` job wall times |
|---|---|---|
| `empiricist-2` | Linux | **2s, 4s, 6s, 7s, 9s** … and **29s, 32s, 33s, 40s** |
| `empiricist-1` | Linux | 4s, 5s, 7s, 7s, 12s, 15s, 17s, 24s |
| `anaplian-1` | macOS | **12s, 18s, 18s** … and **22s, 56s, 61s** |
| `anaplian-2` | macOS | 22s, 23s, 50s |
| `diziet-1` | Linux | 37s, 85s |

The same Linux box (`empiricist-2`) did this work in **2s** (build 3387) and **29s** (build 3381). The same macOS box (`anaplian-1`) did it in **12s** (build 3365) and **56s** (build 3366). A 14–28× swing on identical hardware rules out "the slow agent is just slower" and points squarely at **per-agent cache state and per-commit workload**.

## Where the time actually goes

### Phase-by-phase breakdown of the `checks` job

The job script is tiny ([`.buildkite/steps/checks.sh`](../../.buildkite/steps/checks.sh)):

```sh
source .../ci-env.sh        # builds + installs repobin tools into bin/
echo "--- [checks] running checks"
bin/checkleft run
```

So the job wall decomposes into:

| Phase | What runs | Cost (3381, slow) | Cost (3383, fast) | Notes |
|---|---|---|---|---|
| **1. Git prep** | `git clean -ffxdq` ×2, `git fetch`, `git checkout -f` | ~1–2s | ~1s | Buildkite agent bootstrap, before the script. Small. |
| **2. repobin install** | `bazel build //tools/repobin` + install symlinks (`bin/checkleft -> repobin`, etc.) | ~0.7s (warm) | ~2.7s (analysis cache discarded) | This is the only *visible* bazel invocation in the log. |
| **3. checkleft dispatch** | `bin/checkleft` (a repobin symlink) resolves + execs the real checkleft binary | ~0–3s, **silent** | ~0–3s, **silent** | repobin suppresses bazel progress/info; only prints if the build takes >3s. A dispatch-cache hit spawns **no** bazel at all. |
| **4. `checkleft run`** | the 11 checks (`run_changeset`) | **29s** | **12s** | This is the self-reported number, and it is the bottleneck. |

The `29s` / `12s` is checkleft's own timer around `runner.run_changeset(...)` (`tools/checkleft/src/main.rs:379-381`, printed at `main.rs:797-802`). It measures only the checks, **not** git prep, repobin install, or dispatch. So ~29s of the 32.3s job wall is the checks themselves.

### Inside `checkleft run`: what the 11 checks do

The checks run **in parallel**, not serially — `runner.run_changeset` spawns every scheduled check into a `tokio::JoinSet` with no concurrency cap (`tools/checkleft/src/runner.rs:127-266`), WASM checks via `spawn_blocking`. So "per-check process startup × 11" is **not** a serial bottleneck; the fixed overhead overlaps.

What each check actually does (the cost centers that matter for this commit, which changed two `.rs` files in `boss-engine-core`):

| Check | Implementation | Shells out to | Real work for this commit? |
|---|---|---|---|
| `lint/rust` | declarative | **`bazel query` + `bazel build --aspects=…rust_clippy_aspect`**, then reads `.clippy.out` | **YES — clippy-compiles rdeps of the 2 changed crates. Dominant cost.** |
| `format/rust` | declarative | `rustfmt` (hermetic via rules_rust, may fetch toolchain) | Yes, but cheap (2 files) once the toolchain is present |
| `rust-test-rule-coverage` | built-in Rust | none (scans BUILD files on disk) | Yes, cheap |
| `file/size` + bundled rust checks | WASM component | none (in-process wasmtime) | First-run `.cwasm` JIT compile, then cached |
| `format/bazel`, `lint/bazel` | declarative | `buildifier` | No-op (no BUILD/`.bazel` files changed) |
| `md/link-integrity` (formerly `docs-link-integrity`), `no-generated-artifacts` (forbidden-paths), `todo-expiry`, `repo-visibility` | built-in Rust / WASM | none | Cheap, pure file/string work |

> Aside on "11": `CHECKS.yaml` enables 10 checks explicitly; the 11th reported is a bundled rust check (`file/size` ships in a multiplexed WASM component alongside a `rust/*` check that auto-applies to changed `.rs` files). The exact identity doesn't affect the timing story.

**The single dominant cost is `lint/rust` → `bazel build` of the clippy aspect.** Clippy-compiling `boss-engine-core` (and whatever depends on it) is expensive when the artifact isn't in the bazel disk cache, and near-instant when it is. Everything else in the run is small and parallelized. Upstream change detection is two `git diff` calls (`--name-status` then `--patch`) against a base ref (merge-base for PRs, `HEAD^1` for merge-queue/main) — negligible.

## Why 29s vs 12s (and really 2s–56s)

Three compounding factors, in order of impact:

### 1. Per-agent bazel cache warmth — the dominant driver

`lint/rust`'s clippy build, `format/rust`'s rustfmt toolchain, and the WASM `.cwasm` compile are all **disk-cache-gated**. A warm agent gets disk-cache hits and the run collapses to ~2s; a cold agent (freshly garbage-collected cache, first build of the day, or a crate whose exact clippy inputs aren't cached yet) pays the full compile. This is why `empiricist-2` ranges 2s↔29s and `anaplian-1` ranges 12s↔56s. The disk caches *are* persistent (`/mnt/ssd/bazel`, `/Volumes/ssd/bazel`), but they're per-agent and GC'd (`--experimental_disk_cache_gc_max_age`), so an agent that hasn't recently clippy-built the touched crate is cold for it.

A specific, observed amplifier: build 3383's log shows `WARNING: Build options …capture_clippy_output and …clippy_error_format have changed, discarding analysis cache (this can be expensive)`. The bazel server is **shared across the `bazel-build`, `checks`, and `bazel-test` steps** on the `bazel-any` queue. `lint/rust` enables the clippy aspect; the other steps don't. When they interleave on one agent, the differing build options can throw away bazel's analysis cache right before (or during) the checks step, forcing a re-analysis of thousands of targets.

### 2. Heterogeneous queue — secondary

The `checks` step requests `queue: ${BUILDKITE_ANY_QUEUE:-bazel-any}`. That queue is a **mixed pool of Linux (`empiricist-*`, `diziet-*`) and macOS-arm64 (`anaplian-*`, `skaffen-*`) agents**. The Buildkite scheduler scatters the three parallel pre-`wait` jobs (`bazel-build`, `mac-app-build`, `checks`) across whatever is free, so `checks` lands on a different machine almost every build, each with its own independent cache. In 3381 it landed on Linux `empiricist-2` (clippy cold → 29s) while `bazel-build` ran on macOS `anaplian-2`; in 3383 it landed on macOS `anaplian-1` (warmer → 12s). Different CPU and a different cache each time.

> Note: in 3381 `checks` ran *alone* on `empiricist-2` (bazel-build was on another box), so the 29s was not CPU contention with a concurrent bazel-build — it was a genuinely cold clippy compile on that agent.

### 3. What the commit changed — sets the ceiling

Cost scales with how much Rust the change touches (the clippy rdep set). A docs-only or trivial change → `lint/rust` has nothing to compile → ~2s (e.g. build 3387). A change inside a big, widely-depended-on crate like `boss-engine-core` (this commit) → clippy must rebuild that crate's rdeps → tens of seconds when not cached. The same-commit 3381/3383 pair holds this constant, which is why their gap is "only" 29↔12 rather than 2↔56.

## What was ruled out

- **checkleft binary acquisition per run** — *not* the problem. checkleft is **not** cargo-installed or built-from-source each run. `bin/checkleft` is a repobin symlink; repobin keeps a persistent dispatch cache at `~/.cache/repobin` (outside the workspace, so `git clean` doesn't wipe it) and short-circuits to the prebuilt bazel artifact when source mtimes match, spawning **no bazel at all** on a hit. The visible repobin bazel step is only 0.2–2.7s. This is already-solved prebuilt-binary territory; it is ~1–3s of fixed cost at most.
- **Per-check startup × 11 dominating** — *not* the problem. Checks run concurrently in a `tokio::JoinSet` with no cap. The 11-way fan-out overlaps; it does not serialize.
- **WASM compile every run** — mostly *not* the problem. Bundled checks ship as one multiplexed component and AOT `.cwasm` artifacts are cached at `~/.cache/checkleft/cwasm` (keyed by artifact hash + wasmtime version + target). Only the first run after eviction pays JIT.
- **Repo-walk / file enumeration over the monorepo** — *not* significant. Change detection is two `git diff` calls against a base ref, scoped to the changeset; checkleft does not walk the whole tree for this run.

## The floor for 11 no-op checks

From build 3387 (warm `empiricist-2`, small change): **`checks ran in 2s`**, total job wall ~5s. So the irreducible floor for this workload is roughly:

- ~1–2s git prep (Buildkite agent, hard to avoid)
- ~0.2–1s repobin install (warm bazel)
- ~0–1s checkleft dispatch + startup + change detection
- ~2s checks themselves when clippy is a cache hit

≈ **5–6s job wall is the practical floor.** Everything above that on the 29s run — roughly **20–25s** — is cold clippy/bazel/toolchain work that a warm, homogeneous, isolated cache would avoid.

## Recommendations (ranked)

1. **Isolate the clippy build options so they stop discarding the shared analysis cache.** Highest-leverage, observed directly in the logs (`discarding analysis cache (this can be expensive)`). Either set `capture_clippy_output` / `clippy_error_format` consistently for *all* steps in `.bazelrc`, or run `lint/rust`'s clippy in a dedicated `--output_base` so the `checks` step and the `bazel-build`/`bazel-test` steps don't thrash each other's bazel server on a shared agent. Low effort, attacks the variance amplifier.

2. **Give `checks` a homogeneous (and ideally dedicated) queue.** Pin the step to a single-OS pool (or its own small pool) instead of the mixed `bazel-any` queue. This removes the cross-OS scatter, makes cache warmth predictable, and stops `checks` from competing for the same bazel server as `bazel-build`/`bazel-test`. Cheapest win for *variance* specifically.

3. **Share clippy/rustfmt/cwasm artifacts across agents via a remote bazel cache.** The disk cache is per-agent today, so a cold agent re-compiles clippy from scratch even though another agent already built the identical artifact minutes earlier. A read-through remote cache (or a warm-up step that populates clippy aspect outputs in `bazel-build`, which already runs in parallel) directly attacks the dominant cost — the cold clippy compile — rather than just its variance.

4. **Add per-check timing to checkleft's output.** Today the only signal is the aggregate `ran in Xs`; there is no attribution. Emitting per-check durations (always, or behind a flag/env) would make regressions diagnosable and confirm `lint/rust` is the cost center on every run, not just by inference. Low effort, high diagnostic value.

5. **Reduce the clippy blast radius of `boss-engine-core`.** A two-line edit forcing a clippy recompile of a large crate's rdep set is the structural reason this commit is expensive. Splitting `boss-engine-core` into smaller crates (it is already on the file-size exclusion list for `completion.rs`, `coordinator.rs`, `merge_poller.rs`, `runner.rs`) would shrink the recompile set for both `checks` and the main `bazel-test`. Larger effort, broad benefit; track separately.

6. **(Low priority) keep the repobin dispatch cache effective.** It already works (0.2–2.7s), but ensuring the `git checkout` mtime behavior doesn't routinely invalidate the dispatch cache's source-mtime witnesses would keep step 3 a guaranteed no-bazel hit. Minor.

The first two are cheap and remove most of the *variance*; the third removes most of the *floor-to-ceiling gap* (the cold clippy compile) and is the real fix for "29s".

## Open questions / follow-ups for the operator

- Should `checks` keep sharing the `bazel-any` queue (and its bazel server) with `bazel-build`/`bazel-test`, or move to a dedicated pool? This is a fleet/capacity decision.
- Is a remote bazel cache already available in this fleet that `checks` could read from, or would that need standing up?
- Confirm the identity of the 11th reported check (bundled `rust/*` companion to `file/size`) if an exact per-check accounting is wanted — it does not change the conclusion.

*These recommendations are analysis only; per the task scope no code or pipeline files were changed. The clippy-option-isolation (#1), queue-pinning (#2), remote-cache (#3), and per-check-timing (#4) items are concrete enough to file as separate work.*
