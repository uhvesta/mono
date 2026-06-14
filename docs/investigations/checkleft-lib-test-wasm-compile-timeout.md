# Why `checkleft_lib_test` approaches the 60 s small-test timeout — measured

**Status:** investigation (measurements complete, no code changed)
**Date:** 2026-06-14
**Target:** `//tools/checkleft:checkleft_lib_test` (`rust_test`, `size = "small"` → hard 60 s Bazel timeout)
**Context:** the target timed out on the wasm-consolidation work (PR #1502 / T1695, buildkite 3252), was clawed back to ~48 s by R1 (T1703), and ~48 s for a "small" unit-test target is suspect.

## TL;DR — root cause

The slow cost is **Component-Model (Cranelift) compilation of the bundled `rust/giant-structs` wasm component**, and it is slow for one dominant reason:

> **The Bazel test binary is built in `fastbuild` (unoptimized / debug) mode, so the `wasmtime`/`cranelift` code *embedded in the test* runs ~16× slower than in a release build. One cold compile of the syn-heavy `rust/giant-structs` component takes ≈ **10 s locally** (≈ **12.2 s** in a clean debug harness) versus **0.72 s** in a release build, against a **13–21 ms** AOT cache hit.**

Each of the 7 wasm-exercising tests does **exactly one** compile. The on-disk `.cwasm` AOT cache *does* dedupe compiles **within a single run** (when the heavy tests serialize on it, only the first compiles), but:

1. The cache lives under the platform cache dir (`$HOME/Library/Caches/checkleft/cwasm`). Under the Bazel sandbox `$HOME` is **fresh and discarded every run**, so the cache delivers **zero cross-run benefit** — the ~10 s compile is paid on every invocation.
2. Under test **parallelism**, several heavy tests start before the first writes its `.cwasm`, so they **all cold-miss and compile simultaneously**, oversubscribing the cores with parallel-Cranelift and ballooning wall time (measured 13 s serial → 32 s at 7-way concurrency on this host). This is the latent flakiness that pushed CI toward the 60 s ceiling.

`syn` is a real contributor (it accounts for ~80 % of the giant-structs component's compile cost), but it is *not* the whole story — the WASI-p2 + Component-Model + SDK baseline alone compiles in 2.5 s debug, and the debug build multiplier is the larger lever.

**The fix is to stop compiling the component at test runtime** (ship a build-time-precompiled `.cwasm` fixture, or build the test's wasmtime with optimization), **not** to bump the target to `size = "medium"` (that hides the cost — see [Rejected](#explicitly-rejected-fix-sizemedium)).

---

## Measurement environment

- Host: Apple Silicon (`hw.ncpu = 12`), macOS (Darwin 25.3.0).
- Toolchain: rustc 1.95.0; `wasmtime` pinned at **42.0.2** (matches `tools/checkleft/BUILD.bazel` `CHECKLEFT_WASMTIME_VERSION` and `Cargo.toml`).
- Two measurement vehicles:
  1. **The real Bazel test binary** `bazel-bin/tools/checkleft/checkleft_lib_test` (fastbuild/debug) — authoritative for what CI runs. Compile dedup was driven via `CHECKLEFT_CWASM_CACHE_DIR` to a fresh (cold) or pre-warmed dir.
  2. **A standalone harness** in `/tmp` (`wasmtime = "42.0.2"`, same engine config: `wasm_component_model(true)` + `epoch_interruption(true)`) built once `--release` and once `dev`/debug, run against the real `.wasm` artifacts. Used to isolate the cold-vs-warm ratio and the debug-vs-release multiplier. (This harness is throwaway and is **not** part of this change.)

All `.wasm` artifacts were produced by the in-repo `rust_wasm_component` Bazel rule.

---

## 1. Cold (JIT) vs warm (AOT `.cwasm`) compile — quantified

Engine config mirrors production (`runtime.rs::build_wasmtime_engine`): component-model + epoch-interruption.

### `rust/giant-structs` (the component the tests actually compile)

| build | cold `Component::new` (JIT) | warm `Component::deserialize` (AOT hit) | speedup |
|---|---:|---:|---:|
| **release** (opt-level 3) | **720 ms** | **13 ms** | **≈ 55×** |
| **debug** (opt-level 0) | **12 200 ms** | **21 ms** | **≈ 574×** |

`Engine::precompile_component` (the AOT *write* path) costs essentially the same as `Component::new` (~1.0×) — i.e. precompiling is one full Cranelift compile.

The **debug build inflates the cold compile ~16.9×** (720 ms → 12.2 s) while leaving the AOT-hit path cheap (13 → 21 ms). This is the single most important number in the investigation: the test target is debug, so it pays the 12 s figure, not the 0.72 s one.

### Baseline vs syn attribution (release & debug)

| component | deps beyond SDK | `.wasm` size | cold release | cold debug |
|---|---|---:|---:|---:|
| `trivial-check` (SDK baseline) | none | 2.80 MiB | 155 ms | 2 548 ms |
| `rust/giant-structs` | **`syn`** + serde | 5.05 MiB | 720 ms | ~12 200 ms |
| `file/size` | `globset` + serde | 6.12 MiB | 1 310 ms | (n/m) |

So for `rust/giant-structs`:
- **syn + serde add ~2.25 MiB of code** on top of the 2.80 MiB baseline, but **add ~565 ms (release) / ~9.7 s (debug) of compile** — i.e. **syn's bytes are ~4× more compile-expensive per byte** than the baseline (heavy generic monomorphization). **≈ 80 % of the giant-structs compile cost is syn.**
- The baseline (WASI-p2 + Component-Model glue + guest SDK + wit-bindgen) is itself non-trivial: 2.5 s in debug.

---

## 2. Component byte sizes and `.cwasm` sizes

| artifact | `.wasm` (source) | `.cwasm` (AOT serialized) |
|---|---:|---:|
| `trivial-check` (baseline) | 2 938 762 B (2.80 MiB) | 1 412 104 B (1.35 MiB) |
| `rust/giant-structs` | 5 291 269 B (5.05 MiB) | 6 843 840 B (6.53 MiB) |
| `file/size` | 6 416 913 B (6.12 MiB) | 12 292 352 B (11.72 MiB) |

Both bundled components are 5–6 MiB. Note `file/size` (no `syn`, uses `globset`) is *larger* and compiles *slower* than `giant-structs` — confirming the component baseline, not `syn` alone, sets the floor. Only `giant-structs` is compiled by `checkleft_lib_test` (see §3); `file/size` has its own native test (`file_size_check_test`) that does not go through the wasm host.

---

## 3. How many distinct compiles happen in one `checkleft_lib_test` run, and where

**7 tests** construct a `DefaultExternalCheckExecutor` and resolve/execute the `rust/giant-structs` component:

| location | test | executor ctor |
|---|---|---|
| `src/external/runtime/tests.rs:721` | `bundled_giant_structs_check_finds_violation_in_rs_file` | `::new` (platform cache dir) |
| `src/external/runtime/tests.rs:785` | `bundled_giant_structs_check_skips_files_not_in_changeset` | `::new` |
| `src/external/runtime/tests.rs:843` | `bundled_giant_structs_check_handles_large_rs_file` | `::new` |
| `src/runner/tests.rs:736` (`run_builder_audit`) | `stale_exclusion_surfaced_on_checks_toml_when_struct_gains_builder` | `::new` |
| `src/runner/tests.rs:736` | `load_bearing_exclusion_is_not_flagged` | `::new` |
| `src/runner/tests.rs:736` | `stale_exclusion_severity_setting_upgrades_to_error` | `::new` |
| `src/runner/tests.rs:736` | `stale_exclusion_severity_off_disables_audit` | `::new` |

Two further tests (`component_v1_non_component_bytes_give_compile_error` at `runtime/tests.rs:40`, `component_v1_digest_mismatch_is_rejected` at `:87`) use `new_with_cache(temp, temp/cache)` with an **isolated per-test temp cache**, but they pass *invalid/mismatched* bytes — the first fails fast in `precompile` on tiny wasm, the second rejects on digest before compiling. Neither pays the heavy compile.

**Each of the 7 heavy tests performs exactly one cold compile of `giant-structs`.** Direct evidence (real Bazel test binary, debug; `--exact` single tests):

| | cold (fresh cache) | warm (pre-warmed cache) |
|---|---:|---:|
| `bundled_giant_structs_check_finds…` | 11.2 s | 0.38 s |
| `bundled_giant_structs_check_handles…` | 9.8 s | 0.44 s |
| `bundled_giant_structs_check_skips…` | 9.8 s | 0.38 s |
| `stale_exclusion_surfaced…` | 10.4 s | 0.90 s |
| `load_bearing_exclusion…` | 10.4 s | 0.91 s |
| `stale_exclusion_severity_setting…` | 10.4 s | 0.90 s |
| `stale_exclusion_severity_off…` | 11.0 s | 0.38 s |

cold − warm ≈ 9.5–10.5 s per test = **one** debug-mode compile (≈ 10 s; the standalone debug harness measured 12.2 s — same order, the difference is parallel-compilation and bazel opt nuances). The three exclusion-audit tests are warm-slower (~0.9 s vs ~0.4 s) because the audit path (`declared_exclusions_for_component` / `evaluate_exclusion_for_component`) loads the component **again** (extra deserialize + instantiate); `severity_off` skips the audit and stays at 0.38 s.

---

## 4. Is the AOT `.cwasm` cache effective in the Bazel sandbox? — definitively

**Within a single run: yes (when tests serialize). Across runs: no.**

The 7 heavy tests built with `::new` all resolve the cache directory via `cwasm_cache::default_cache_dir()` → `$HOME/Library/Caches/checkleft/cwasm` (no `CHECKLEFT_CWASM_CACHE_DIR` override is set in the test target's `env`). Behaviour, measured on the real binary against **one shared** cache dir:

| scenario (7 heavy tests) | threads | wall | compiles |
|---|---:|---:|---:|
| WARM cache, serial | 1 | 3.97 s | 0 |
| **COLD cache, serial** | 1 | **13.41 s** | **1** (first test only; rest hit) |
| COLD cache, parallel (12) | 12 | 17.91 s | several (concurrent misses) |
| **COLD cache, parallel (7)** | 7 | **32.37 s** | up to 7 (concurrent misses) |
| WARM cache, parallel (12) | 12 | 2.30 s | 0 |

- **Within-run dedup works:** cold-serial = 13.4 s = one ~10 s compile + seven ~0.5 s warm runs. So the on-disk cache *does* let 7 independent executors share one compile — **but only because serial execution lets the first finish writing before the others look.** This also proves the sandbox `$HOME` is writable (if the cache failed to open, `component_cache` would be `None` and every test would JIT — the full suite would be ~70 s, not ~13 s).
- **Concurrency defeats it:** at 7-way concurrency the same cold cache yields 32 s — multiple tests miss simultaneously and each runs a full parallel-Cranelift compile, thrashing the 12 cores. This is the timeout-risk mechanism.
- **No cross-run benefit:** every Bazel test runs in a fresh sandbox with a fresh `$HOME`, so the `.cwasm` written in run *N* is gone in run *N+1*. Two consecutive `--cache_test_results=no` runs measured **13.7 s** and **11.8 s** — cold every time. The AOT cache, designed for the long-lived *product* binary, is **structurally unable to help the sandboxed test.**

**Full-suite confirmation:** the entire `checkleft_lib_test` (453 tests) runs in **11.8–13.7 s locally**, ≈ the cold-serial-7 figure — i.e. in the real run the heavy tests effectively serialize on the cache and **~1 cold debug compile (~10 s) dominates the whole target.** On the slower/loaded CI host that one debug compile (± a couple of concurrent ones under unlucky scheduling) is what produces ~48 s and the 60 s-timeout near-misses.

---

## 5. What R1 ("share the executor") actually bought

R1 (T1703) reduced the target from a 60 s timeout to ~48 s. The mechanism, consistent with the measurements: by reducing the number of executors that **cold-miss concurrently**, it cut the number of *simultaneous* debug-mode compiles. The data bracket this directly — going from 7 concurrent cold compiles to a serialized one drops wall time **32 s → 13 s** on this host (≈ proportional to CI's 60 s → 48 s).

Caveat on current `main`: the heavy tests are **not** sharing an executor *object* today — `run_builder_audit` (`runner/tests.rs:736`) and the three `runtime/tests.rs` tests each call `DefaultExternalCheckExecutor::new` independently. What they share is the **on-disk platform `.cwasm` cache**. The remaining exposure is exactly the concurrency case in §4: any scheduling that starts ≥2 heavy tests before the first writes its `.cwasm` re-introduces redundant simultaneous compiles. So R1 reduced — but did not remove — the cost; the floor is still **one full debug-mode compile per run (~10 s local / much more on CI)**.

---

## 6. Did "consolidation" raise per-compile cost?

The task's hypothesis was that #1502 merged all checks into one syn-carrying component, making each compile heavier. **On current `main` there is no single consolidated component** — there are two independent bundled components (`giant-structs` with `syn`, `file/size` with `globset`), each embedded via its own micro-library (`bundled.rs`). I therefore could not measure a "consolidated vs per-check" delta directly.

What the data *does* say about the cost shape:
- A check component's compile cost is **baseline (≈ 2.8 MiB / 2.5 s debug) + its parser deps.** For `giant-structs`, `syn` roughly **5×'s** the compile (2.5 s → 12.2 s debug). Any component that links `syn` inherits that.
- So *if* checks were consolidated into one `syn`-bearing component, the single compile would carry syn once (≈ today's `giant-structs` cost) rather than per-check — consolidation would not multiply the syn cost, but it does guarantee every run that touches the bundle pays the full syn compile. The dominant, measurable problem remains the **debug build + runtime compilation**, independent of consolidation.

---

## Root-cause statement

`checkleft_lib_test` approaches the 60 s small-test ceiling because it **JIT-compiles the `rust/giant-structs` Component-Model wasm artifact at test runtime, inside a test binary built unoptimized (`fastbuild`/debug)**. That one compile costs ≈ **10 s locally and substantially more on CI** (debug-mode Cranelift is ~16× slower than release; `syn` is ~80 % of the component's compile). The `.cwasm` AOT cache — built for the long-lived release product — **cannot help across sandboxed test runs** (fresh `$HOME` every run) and only de-dupes *within* a run when the heavy tests happen to serialize; under parallel cold-misses the compiles stack up and the wall time blows out toward the timeout.

---

## Recommended fix (root cause, not stopgap)

Goal: **no test compiles the component at runtime.** Directions, best first — to be evaluated, not pre-committed:

1. **Precompile the component to a `.cwasm` at build time and feed it to the test as a data fixture (strongest).** Add a Bazel rule that runs `Engine::precompile_component` (host tool, same wasmtime 42.0.2 + the exact `build_wasmtime_engine` config) over each bundled `.wasm`, producing a `.cwasm` whose key matches `cwasm_cache.rs`'s `(artifact_sha256, wasmtime_version, engine_config, target)`. Tests then `deserialize` only (~20 ms debug). This removes the compile from *every* run, cross-run, and is robust to parallelism. Cost/risk: the `.cwasm` is wasmtime-version + engine-config + host-target specific, so the rule must stay locked to those axes (the cache key already encodes them, and `CHECKLEFT_WASMTIME_VERSION` is already an `IFCHANGE` pin) — and host-target specificity means it is a host-build artifact, not a portable checked-in blob.

2. **Build the test's `wasmtime`/`cranelift` with optimization.** A debug→release Cranelift swing is the 16× lever (12.2 s → 0.72 s). Options: run this test under `-c opt`, or set a per-package `opt-level` override for the wasmtime/cranelift crates in the test configuration. This *removes* the slowness (it is real compute, not a masked limit), but per-dependency opt under `rules_rust` is awkward and `-c opt` changes the whole target's debuggability. Combine with (3) to also kill the concurrency blow-up.

3. **Share one pre-warmed, read-only AOT cache (or a single shared executor) across all heavy tests, and/or serialize the first compile.** Eliminates the concurrent-cold-miss multiplication (the 13 s→32 s effect) and the residual flakiness R1 only partially addressed. Does not by itself remove the one cold compile — pair with (1) or (2).

4. **Reduce the component's intrinsic compile cost — keep `syn` out of the wasm component.** ~80 % of the giant-structs compile is `syn`. Parsing structs host-side, or using a lighter parser in the guest, would cut the component compile substantially. Largest change; orthogonal to the debug-build multiplier.

**Recommended combination:** (1) build-time `.cwasm` fixture as the primary fix (removes runtime compilation entirely and is parallelism- and cross-run-safe); fall back to (2) if a build-time precompile rule is judged too heavy. (4) is a good longer-term reduction but not required to clear the timeout.

### Explicitly rejected fix: `size = "medium"`

Bumping `checkleft_lib_test` to `size = "medium"` (300 s timeout) is **rejected as the fix**: it hides the ~10 s-and-growing runtime compile behind a looser limit instead of removing it, leaves every developer/CI run paying the cost, and re-opens the door to the concurrency blow-up silently. It is acceptable only as a *temporary stopgap* if the real fix is deferred — and even then the real fix above should be tracked.

---

## Follow-up code work (out of scope here — file separately)

- Implement the build-time `.cwasm` precompile Bazel rule + test fixture wiring (direction 1), or the optimized-wasmtime test build (direction 2).
- Add a shared/prewarmed AOT cache (or shared executor) for the 7 heavy tests to remove concurrent-cold-miss redundancy (direction 3).
- Evaluate moving `syn`-based parsing out of the `rust/giant-structs` wasm guest (direction 4).
