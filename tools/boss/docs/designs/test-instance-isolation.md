# Test instance isolation

Run a second Boss engine + macOS app alongside the production install on the same machine, completely isolated except for the shared cube workspace pool. Nothing the test instance does should leak into production state (DB rows, audit/dispatch logs, control/events sockets, window state, worker hook routing).

This document is **investigation-only**. It surfaces the single-tenant assumptions in today's codebase, proposes one isolation strategy, sketches a phased rollout, and lists the open questions a human should answer before any code lands. A follow-up project will hold the implementation tasks.

## 1. Inventory of single-tenant state

The good news: most paths are already env-overridable. The audit was for `main` at the workspace HEAD.

| # | Surface | Default | Override (today) | Owner | Citation |
|---|---|---|---|---|---|
| 1 | State DB (sqlite) | `$HOME/Library/Application Support/Boss/state.db` | `BOSS_DB_PATH` | engine | `tools/boss/engine/src/config.rs:142-148` |
| 2 | Frontend (control) socket | `/tmp/boss-engine.sock` | `BOSS_SOCKET_PATH` (env) + `--socket-path` (CLI) | engine + every client | `tools/boss/engine/src/app.rs:37`; `tools/boss/client/src/lib.rs:23,69` |
| 3 | Engine pid file | `/tmp/boss-engine.pid` | `BOSS_ENGINE_PID_PATH` | engine + app supervisor | `tools/boss/engine/src/app.rs:38,1501`; `tools/boss/client/src/lib.rs:24,72`; `tools/boss/app-macos/Sources/EngineProcessController.swift:14-15` |
| 4 | Events socket (worker hooks) | `$HOME/Library/Application Support/Boss/events.sock` | `BOSS_EVENTS_SOCKET` | engine binds; shim + spawn_flow propagate | `tools/boss/engine/src/app.rs:1512-1520`; `tools/boss/engine/src/runner.rs:123-129`; `tools/boss/engine/src/spawn_flow.rs:323-329`; `tools/boss/event-shim/src/main.rs:53` |
| 5 | Engine stderr/file log | `/tmp/boss-engine.log` | `BOSS_ENGINE_LOG_PATH` | engine | `tools/boss/engine/src/main.rs:14,52-58` |
| 6 | Engine audit log | `$HOME/Library/Application Support/Boss/engine-audit.log` | `BOSS_ENGINE_AUDIT_PATH` | engine | `tools/boss/engine/src/audit.rs:58,81-90` |
| 7 | Dispatch-event JSONL root | `dirname(BOSS_DB_PATH)` → falls back to `$HOME/Library/Application Support/Boss` | implicit (follows `BOSS_DB_PATH`) | engine writes; bossctl + macOS app read | `tools/boss/engine/src/app.rs:517-534`; `tools/boss/engine/src/dispatch_reader.rs:37-40` |
| 8 | macOS dispatch-event reader root | `~/Library/Application Support/Boss` (hard-coded) | **none** | macOS app | `tools/boss/app-macos/Sources/DispatchEventsViewer.swift:97-122` |
| 9 | macOS `UserDefaults` (window state, filters, panel collapse, viewer visibility/frame) | shared `UserDefaults.standard` keyed by bundle id `dev.spinyfin.bossmacapp` | **none** | macOS app | `tools/boss/app-macos/Sources/ChatViewModel.swift:242-252`; `tools/boss/app-macos/Sources/DispatchEventsViewer.swift:826-891`; bundle id at `tools/boss/app-macos/BUILD.bazel:27` |
| 10 | Engine spawn command (used by app autostart) | `bazel run //tools/boss/engine:engine -- --socket-path <s>` | `BOSS_ENGINE_CMD` (macOS app); `BOSS_ENGINE_CMD` / `BOSS_ENGINE_BIN` (client lib) | macOS app + boss CLI client | `tools/boss/app-macos/Sources/EngineProcessController.swift:41-44`; `tools/boss/client/src/lib.rs:279-376` |
| 11 | App ↔ engine trust handshake (which app pid the engine accepts `RegisterAppSession` from) | `BOSS_APP_PID` injected by the macOS app into the engine env | env (already per-spawn) | macOS app sets; engine reads | `tools/boss/app-macos/Sources/EngineProcessController.swift:77-79`; `tools/boss/engine/src/app.rs:1862-1881` |
| 12 | Cube CLI binding (how the engine reaches cube) | `cube` (PATH-resolved) | `BOSS_CUBE_CMD` | engine | `tools/boss/engine/src/config.rs:8,60-66` |
| 13 | Per-worker hook env injected at spawn | `BOSS_EVENTS_SOCKET=<engine-socket>` + `BOSS_LEASE_ID` + `BOSS_RUN_ID` + `BOSS_WORKSPACE` | (engine-controlled, not env-driven) | engine spawn_flow + worker_setup template | `tools/boss/engine/src/spawn_flow.rs:182-225`; `tools/boss/engine/src/worker_setup.rs:154-177` |
| 14 | Worker `settings.json` hook command (literal env values baked into the worker's `.claude/settings.json`) | same four vars as #13 | (engine-controlled, baked per spawn) | engine worker_setup | `tools/boss/engine/src/worker_setup.rs:170-187` |

### What's already routable
Rows 1–7 plus 10–12 each respond to an env var. A test instance whose env is fully populated (every variable above pointing at a private state root) gets a clean engine: its own DB, its own sockets, its own logs.

Row 13/14 are derived state — they're populated from whatever the engine resolves at spawn time, so a test engine launched with a private `BOSS_EVENTS_SOCKET` will (correctly) bake that socket path into every worker's `settings.json`. Workers spawned by the test engine talk to the test engine, not production.

### What needs work
- **Row 8 — macOS dispatch reader hard-codes the state root.** `DispatchEventsPaths.stateRoot()` reads `~/Library/Application Support/Boss` directly with no env override. A test app instance would still tail the production `current.jsonl`. Small fix: thread the state root through, matching the engine's `BOSS_DB_PATH`-derived resolution. Confidence: easy.
- **Row 9 — `UserDefaults.standard` is per-bundle, not per-process.** Two simultaneous app processes with bundle id `dev.spinyfin.bossmacapp` share the same `UserDefaults` plist (`~/Library/Preferences/dev.spinyfin.bossmacapp.plist`). Window frames, viewer visibility, "include chores," panel widths — all cross-contaminate. Fix options below.
- **Cube identity.** Cube has no concept of "owner instance"; lease records carry a `task` string (free-form) and a `workspace_id`. Two engines leasing from the same pool look identical to cube. Concretely safe — workspaces aren't aliased — but lease accounting is shared (see open questions §4).

## 2. Proposed strategy: `BOSS_PROFILE` env var + a state-root resolver

Today's overrides are a soup of unrelated env vars (`BOSS_DB_PATH`, `BOSS_EVENTS_SOCKET`, `BOSS_ENGINE_PID_PATH`, …). Each must be set in lockstep to relocate one instance. That's brittle and easy to half-configure.

**Proposal:** introduce a single profile knob, `BOSS_PROFILE`, that resolves to a state-root directory and a socket-name suffix.

- `BOSS_PROFILE` unset → resolves to `default` → behaves exactly as today (`/tmp/boss-engine.{sock,pid}` + `~/Library/Application Support/Boss/`). Zero-risk for production.
- `BOSS_PROFILE=test` → resolves to:
  - state root: `~/Library/Application Support/Boss-test/`
  - control socket: `/tmp/boss-engine.test.sock`
  - pid file: `/tmp/boss-engine.test.pid`
  - events socket: `<state-root>/events.sock`
  - audit / dispatch / engine log: under the state root or `/tmp/boss-engine.test.log`

Implementation shape:
1. A single helper (e.g. `boss_paths::resolve(profile: &str) -> BossPaths`) returns the four-or-five paths every binary cares about. Engine, bossctl, boss CLI, macOS app, and the shim all call this — no more scattered `default_*_path()` helpers.
2. Each existing env override (`BOSS_DB_PATH`, `BOSS_SOCKET_PATH`, …) wins over the profile-derived default. Profile is just convenience.
3. The macOS app exposes a profile selector (env var first; later, a Settings toggle). When profile=test, it picks a different `UserDefaults` suite — see §3, Phase 4.

### Why `BOSS_PROFILE` over the alternatives

- **`--state-root` flag.** Already exists on bossctl's dispatch-tail verbs. Doesn't solve sockets/pid/logs. Would force every callsite (engine, app, shim, CLI) to grow a new flag. Loses for breadth.
- **Per-tool env vars.** What we have today. Still need a way to set them in lockstep — basically reinvent the profile in the caller's shell.
- **Config file (`~/.config/boss/profiles.toml`).** Heavier; introduces a new file format the engine has to parse before it can even bind a socket. Could land later as sugar on top.

A profile knob is the smallest change that addresses every surface in the inventory consistently.

### Why not a `--profile test` CLI flag instead of env

The shim and worker hook commands are spawned by the engine and don't have a CLI surface — they must read env. Picking env keeps one source of truth.

### Naming
Two short names worth considering:
- **`BOSS_PROFILE`** + `boss-test`/`boss-prod` for the resolved profile name. Pros: terse, room to add more profiles (`ci`, `demo`, `staging`) later. Cons: "profile" is overloaded in the macOS world (preferences, network profiles).
- **`BOSS_INSTANCE`** + `default`/`test`. Pros: matches the framing "second instance of Boss." Cons: longer, less idiomatic.

Recommendation: `BOSS_PROFILE`. Aligns with the existing comment in `client/src/lib.rs:65` ("Build a discovery profile from process env").

## 3. Phased rollout

Each phase is independently shippable and reversible.

### Phase 1: Centralise path resolution (no behaviour change)

- Introduce a `boss_paths` crate (or module on existing `boss_client`) with `BossPaths { state_root, control_socket, pid_file, events_socket, audit_log, engine_log }`.
- Replace the in-tree `default_*_path()` helpers with calls into the new module. Production paths stay identical.
- Add unit tests pinning the existing defaults so a regression is loud.
- No new env vars yet. **This phase is pure refactor.**

Touches: `engine/src/{app.rs,audit.rs,config.rs,dispatch_reader.rs,main.rs,runner.rs,spawn_flow.rs}`, `client/src/lib.rs`, `bossctl/src/main.rs`.

### Phase 2: Wire `BOSS_PROFILE` into the resolver

- `BossPaths::resolve(profile)` chooses defaults; per-tool env overrides still win.
- Profile is read once in each binary's `main`: engine, bossctl, boss CLI, event-shim (if needed), macOS app.
- Engine logs the resolved profile + every chosen path on startup so a misconfigured launch is obvious.
- Add a `bossctl env` (or `boss env`) verb that prints the resolved paths and which env vars supplied them — debugging aid.

Touches: same files as Phase 1, plus the binary entry points.

### Phase 3: Fix the macOS dispatch reader and adjacent surfaces

- `DispatchEventsPaths.stateRoot()` reads `BOSS_PROFILE` (or a direct `BOSS_STATE_ROOT`) and matches whatever the engine wrote.
- The dispatch viewer now opens a separate window per profile (or just respects whatever the parent app's profile is).

Touches: `tools/boss/app-macos/Sources/DispatchEventsViewer.swift`, related dispatch-events code paths.

### Phase 4: Per-profile macOS `UserDefaults` suite + a launcher

- Build a profile-aware defaults helper: `Defaults.shared(profile:)` returns either `UserDefaults.standard` (default) or `UserDefaults(suiteName: "dev.spinyfin.bossmacapp.test")` (test).
- Replace every `UserDefaults.standard` access in the Swift sources with the helper.
- Add a launcher (shell script or bazel target) that sets `BOSS_PROFILE=test`, exports a parallel `BOSS_APP_PID`, and runs the same `Boss.app` binary. Single binary, two preference suites, two engines, no second build.

Touches: `tools/boss/app-macos/Sources/{ChatViewModel,DispatchEventsViewer,DesignsView}.swift` and any other `UserDefaults` callsites we surface.

### Phase 5 (optional): Visual differentiation + safety rails

- Window title prefix when `BOSS_PROFILE != default` ("Boss [test]").
- Optional dock-icon badge.
- Optional safety rail: a `BOSS_PR_MODE={open,dry-run,fork}` env that the test engine threads into the worker prompt — see open question on PR safety.

## 4. Open questions

These are knobs the human should pin down before implementation starts, in rough priority order.

### Q1. PR / GitHub safety — biggest unknown
The test engine still spawns real `claude` against the real mono repo. Workers will follow CLAUDE.md and try `gh pr create`. **Stakes:** if the test engine runs five workers in parallel, those five PRs land on the real repo and notify reviewers as if they were production work.

Three plausible answers:
- **(a) Manual discipline only.** Test runs are opt-in, scoped to throwaway prompts, and the human babysits. Cheapest; risks an absent-minded `gh pr create`.
- **(b) Dry-run / fork mode.** Engine sets an env that the worker prompt acknowledges ("you are a test instance; do not push, do not open PRs"). Soft enforcement only — workers can ignore.
- **(c) Hard separation via a fork repo.** Test instances are pinned to a separate GitHub remote / fork; production stays on the main repo. Strongest isolation; biggest project (workspace ↔ remote indirection is new ground).

If (c), it cascades into: does cube need per-repo lease pools too? Today repos are identified by origin URL (`tools/boss/engine/src/coordinator.rs:55`); leasing from "the same workspace pool" presupposes the same origin. Probably (c) means each profile gets its own cube repo + workspace pool, which contradicts the user's "cube shared OK" framing.

**My read:** pick (b) for v1 — engine sets `BOSS_PR_MODE=dry-run` for test workers and the worker prompt is amended to say "don't push, don't open PRs." Honest soft rail. Re-evaluate (c) after a few sessions show whether soft is enough.

### Q2. Cube lease accounting
Cube has no concept of "owner engine"; both engines lease from the same pool. **Stakes:** if production needs 8 workers and the test instance has 4 leased, production starves on `cube workspace lease` calls until the test instance releases.

Options:
- Cap the test instance worker pool at, say, 2 (`BOSS_WORKER_POOL_SIZE=2`).
- Add an `--owner-tag` to cube leases so a human can see who holds what (cube change, not Boss change). Doesn't solve starvation but improves diagnostics.
- Tagged pools per-profile (test profile sees only workspaces in pool `test`). Cube refactor; out of scope unless the contention is real.

Decision: probably accept contention + cap test pool size. Bias toward the simplest answer until it bites.

### Q3. Crash recovery cross-talk
PR #316 added orphan-execution reaping at engine startup (`tools/boss/engine/src/app.rs:1712-1751`, `tools/boss/engine/src/run_reconcile.rs`). The reconcile probe reads `cube workspace list` (all workspaces, both instances) and only marks executions whose `lease_id` no longer matches.

**Reasoning:** the engine only iterates its own state DB's in-flight executions and matches by `lease_id`. Leases are UUIDs cube hands out; the test engine's lease IDs and production engine's lease IDs will never collide.

**Conclusion:** no cross-contamination, **but** there's a subtler risk. If the test engine crashes and cube auto-releases its leases via TTL while the test engine is down, those workspaces re-enter the pool and production may lease them next. The next test-engine startup then sees `lease_id` mismatches for its old rows → marks them orphaned → completion handler runs against work items production never knew about. That's fine in principle (each engine only sees its own DB), but the workspace it tries to clean might already hold production's commits. Concrete risk: the orphan-reap path "preserves the workspace for re-lease" rather than wiping it, so this should be safe by construction — but worth testing.

### Q4. Anthropic API spend
Each test worker burns Anthropic credits. **Stakes:** cheap to forget the test engine is running with workers spinning on a probe loop. A bad day could be tens of dollars without anyone noticing.

Mitigations to consider:
- Test profile gates workers behind an explicit `BOSS_TEST_WORKERS_ENABLED=1`.
- Default test-pool size = 0; user must opt in per session.
- Test runs print a one-line "spend so far" indicator on the engine console.

I'd ship Phase 4 with `BOSS_TEST_WORKERS_ENABLED` gating worker spawn entirely — test instance can do everything *except* spawn paying workers unless explicitly unlocked. Cheap belt.

### Q5. Boss-event shim — runfiles vs profile
The shim resolves its path through `BOSS_EVENT_BIN` or bazel runfiles relative to the engine binary (`tools/boss/engine/src/runner.rs:131-192`). If both production and test engines run from the same `bazel-bin`, they share the shim binary — that's fine; the shim is stateless and reads env per invocation. **Confirmed not a contamination vector.**

### Q6. Where do the test app's `cube` calls run from?
`cube` is invoked with `current_dir(&self.cfg.work.cwd)` (`tools/boss/engine/src/coordinator.rs:93`). The engine's cwd is set from `BUILD_WORKSPACE_DIRECTORY` or process cwd. Two engines running from the same mono checkout will call cube identically; cube doesn't care which engine asked. **No problem expected.**

## 5. Hardest problems

In rough order of "I don't have an obvious answer":

1. **PR safety (Q1).** The "right" answer depends on how the user wants to use the test instance. If it's purely for testing engine-side behaviour (dispatch, completion handling, UI), workers can be dry-run and we're done. If it's for shake-out testing of worker prompts / agentic loops, workers need to actually push and the answer probably is "fork repo." Needs human input.
2. **macOS `UserDefaults` migration.** Mechanically straightforward — every `UserDefaults.standard` becomes `defaults` injected via a helper — but it's a wide diff that touches a lot of files. Easy to miss a callsite and have one preference silently bleed across profiles. Mitigation: lint rule (grep) that fails CI if `UserDefaults.standard` reappears outside the helper.
3. **Profile awareness in the dispatch viewer window state.** If the user is running both profiles simultaneously, do they want one dispatch viewer per engine? Two viewers on screen at once is the obvious answer; that means viewer visibility key (`Self.visibilityDefaultsKey`) must be per-profile so opening test's viewer doesn't toggle production's UI. Phase 4 handles this once `UserDefaults` is profile-aware.
4. **Diagnosing "wrong instance" mistakes.** A bossctl invocation with `BOSS_PROFILE` unset will hit production even if the user thought they were debugging test. The `bossctl env` verb proposed in Phase 2 is a partial mitigation. Stronger: refuse to start the engine without an explicit profile (production = `BOSS_PROFILE=prod`). That's a bigger breaking change and probably not worth it.
5. **Cube workspace contention under load (Q2).** Unknown until both instances are running concurrently. Won't surface until it does. Plan to live with it; cap test pool size as a precaution.

## 6. Out of scope (for the design phase)

- Actually implementing any of the phases.
- Refactoring the existing path-discovery code beyond the centralisation described in Phase 1.
- Building the test-instance launcher script — that's a Phase 4 deliverable, not a design choice.
- Changing production behaviour. Phase 1 is a pure refactor; Phase 2 onwards is additive (profile=default == today).
- A standalone test repo / fork. Decision deferred until Q1 is answered.
