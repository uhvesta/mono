# Boss CI: Buildkite Pipeline Mirroring Flunge

## Status (2026-05-14)

Two pieces of the original design have landed independently of the pipeline itself; the rest of this doc still describes work to do.

- **P1 (bazel engine-lib coverage) has landed.** `tools/boss/engine/BUILD.bazel:86` now declares `rust_test(name = "engine_lib_test", crate = ":engine_lib", ...)`, so `bazel test //tools/boss/engine/...` exercises the engine lib tests that the motivating incident exposed. The doc no longer needs a `cargo test` transitional step in the pipeline; bazel is the canonical rust signal from v1. See "Historical: cargo→bazel transition" below for context.
- **Kanban CI status indicator has shipped.** `PrCiIndicator` in `tools/boss/app-macos/Sources/ContentView.swift:2683` renders a small badge on Review-lane cards: yellow clock for `in_progress`, green checkmark for `success`, red X for `fail`, hidden for `unknown`. It is fed by the existing `ci_watch` probe path; no new schema column was needed. See "Boss UI integration — shipped" below.

The pipeline itself (the `.buildkite/` skeleton, the per-step shells, the branch-protection ramp) is still pending implementation. Everything below describes that remaining work.

## Motivating incident

On 2026-05-12, the `#[cfg(test)]` blocks in `tools/boss/engine/src/{completion,merge_poller}.rs` drifted out of sync with their prod signatures. `cargo test -p boss-engine --no-run` reported six compile errors on `main`. The drift sat undetected on `main` for roughly twenty-four hours. La Forge's investigation in closed PR #438 (chore `task_18af35a1e855d7f0_24`) tracked down the cause; the doc didn't land on main, but the findings stand.

Why nothing caught it: at the time, `bazel test //tools/boss/engine/...` resolved to only two integration test targets — there was no `rust_test(crate=":engine_lib")`, so bazel silently skipped 561 lib tests. Sibling chore `task_18af3caac58d9748_2c` ("P1") tracked that gap separately and has since landed (see Status above); the rule now lives at `tools/boss/engine/BUILD.bazel:86`. With no other gate, every dispatched worker is a bet that what landed on `main` since the last green check is still buildable.

This design proposes the structural fix: a buildkite pipeline for the mono repo, mirroring the shape of the existing flunge pipeline, gating merges on green.

## Goals

- A PR to mono cannot be merged while buildkite reports red. GitHub branch protection enforces it; no engine-side gating required.
- The pipeline runs on every PR open + push + branch update, plus pushes to `main`.
- The first PR that introduces a `bazel test` failure (or a `cargo check` compile failure) is blocked from merging.
- Reviewers see a clear pass/fail signal in the GitHub PR UI; the Boss UI surfaces the same signal next to in-review rows.
- Same shape as flunge's buildkite — same buildkite org, same secrets store, same merge-blocking semantics. Diverge only where the rust + bazel + jj surface forces it.

## Non-goals

- **Engine-side gating.** Today the engine does not call `gh pr merge` (verified: no occurrences in `tools/boss/engine`). Merges happen out-of-band — typically a human types `lgtm` and the worker or human runs `gh pr merge`. The CI gate is enforced by GitHub branch protection, not by the engine refusing to merge. If/when the engine ever auto-merges, it must honour the same status checks; that work is out of scope here.
- **Performance dashboards.** Flunge doesn't have one; nor does this.
- **macOS app code-signing CI.** Different surface (signing certs, notarisation). Follow-up project.
- **Engine test-suite perf work.** Tracked separately under P2 (`task_18af3cad1705c0a8_2d`).
- **Repobin dispatch-cache.** Already landed in PR #439.
- **Cross-repo CI orchestration.** This pipeline gates the mono repo only. Flunge has its own. The Boss UI can show CI status for both products independently.
- **Rewriting `ci_watch.rs`.** The engine already models buildkite as a CI provider (`merge-conflict-handling-in-review.md` §Q7, `tools/boss/engine/src/ci_watch.rs:1-60`). Nothing about that module needs to change for v1 — once real checks exist, the existing remediation pipeline picks them up automatically.

## Alternatives considered

### Alternative A — GitHub Actions instead of Buildkite

GitHub Actions is the obvious "free" option: no infra to stand up, no agent fleet to manage, native PR integration. Rejected:

- Flunge runs on buildkite. Mandate is *mirror flunge*, not *re-pick the CI vendor*.
- The mono build is rust + bazel + node + (eventually) Xcode. Bazel cache hit rate matters — GitHub-hosted runners are ephemeral, so a remote cache is mandatory just to be tolerable; we'd have to stand one up anyway. Self-hosted GH runners would re-introduce the agent-fleet question without buying anything over buildkite.
- We already have buildkite secrets, an org, an agent fleet, and the engine knows how to parse `buildkite.com` job URLs (`ci_watch.rs`, `merge-conflict-handling-in-review.md:998-1007`). Reusing that surface is materially cheaper than building a parallel actions parser.

### Alternative B — A dedicated mono agent fleet from day one

Stand up a separate buildkite agent fleet, tagged `mono-only`, provisioned with the rust toolchain, bazel, and node from the start. No interaction with flunge agents. Rejected for v1:

- Doubles the operational surface. Two agent pools, two sets of dashboards, two secret stores, two cost lines.
- The toolchain bootstrap is the only meaningful difference from flunge's agents. We can install rust + bazel + pnpm into the existing flunge agents (idempotent install scripts at job start) and let the queue tag route mono jobs separately. The agents themselves don't care.
- If the install-on-boot cost becomes painful (multi-minute toolchain pull every job), the fix is a baked AMI / docker image, not a separate fleet. That's a v2 optimisation, not v1 architecture.

We can split fleets later if real evidence shows mono jobs starving flunge jobs (or vice versa). Until then, one fleet, two queue tags.

### Alternative C — `cargo` only, drop `bazel` from CI

Drop bazel from CI entirely. Run only `cargo check` / `cargo test` / `pnpm test`. Rejected:

- The motivating incident was caught by `cargo test`, not bazel — so cargo-only CI would have caught it. But the pattern we're guarding against is broader. The repobin dispatch-cache regression that PR #439 fixed wasn't visible to cargo; it was a bazel state bug. We want bazel signals in CI on top of, not instead of, the rust-level signal.
- `bazel build //...` and `bazel test //...` are cheap once the cache is warm. They expose dependency-graph rot (visibility violations, missing srcs, dropped deps) that cargo cannot.
- With P1 landed, `bazel test //...` now exercises the engine lib tests too, so the historical concern that bazel-test was "a green box that doesn't catch anything" no longer applies. Bazel is the canonical rust signal in v1; `cargo check` stays as a cheap, fast-failing compile guard that's still useful when the bazel target graph itself is broken.

## Chosen approach

### Repo layout

Mirror flunge's `.buildkite/` directory structure verbatim:

```
.buildkite/
  pipeline.yml              # buildkite reads this; defines steps + queue tags
  steps/
    bootstrap.sh            # ensure rust toolchain + bazel + pnpm present; cache restore
    bazel-build.sh          # bazel build //...
    bazel-test.sh           # bazel test //... (covers engine lib tests via P1)
    checks.sh               # checkleft / CHECKS.yaml runner (no-generated-artifacts, etc.)
  README.md                 # what each step does, how to debug a red build locally
```

`.buildkite/pipeline.yml` is the source of truth wired into the buildkite project. Each `steps/*.sh` is a small shell script; logic stays in the repo so it's reviewed and versioned, not in the buildkite UI. The pipeline.yml just declares steps, plugins, queue tags, and dependencies — no inline shell. This matches what flunge does and is also what the existing engine doc (`merge-conflict-handling-in-review.md:998`) assumes when it shells out to `bk job log <id>`.

### Pipeline shape

Three logical phases, parallelised where they don't depend on each other:

```
bootstrap (queue=linux-amd64) ─┬──► bazel-build ──┐
                               ├──► checks       ──┼──► (wait) ──► bazel-test ──► green
```

- `bootstrap` is a single step that primes the agent: installs / pins the rust toolchain (read `rust-toolchain.toml`), pins bazelisk, installs pnpm, and restores the bazel disk cache. Subsequent steps inherit a warm checkout.
- Cheap static checks (`bazel build`, `checks.sh`) run in parallel. Any one failing is enough to redden the build; reviewers don't need to wait for the heavy steps to see compile failures.
- Test steps (`bazel test`) run after the static checks pass. `bazel test //...` is the canonical rust test step — with P1 landed it covers the engine lib tests as well as the integration test targets.
- Every step is its own buildkite step (not a sub-target of one umbrella step). When one is red, the buildkite UI shows exactly which, and the engine's `ci_watch` (which already parses required-check names) can route by failing-step name in the future.

### Required checks for branch protection

GitHub branch protection on `main` requires:

- `buildkite/mono/bootstrap`
- `buildkite/mono/bazel-build`
- `buildkite/mono/bazel-test`
- `buildkite/mono/checks`

The pipeline shape focuses on bazel as the canonical build and test system. Cargo-check and pnpm-based steps have been dropped in favor of bazel's dependency-graph checking and unified rust+node test coverage.

The promotion sequence is the only thing in this design that touches GitHub branch protection settings directly; everything else is repo-local.

### Agent topology

Single buildkite agent fleet, shared with other projects, queue tag:

- `queue=linux-amd64` — default Linux queue for all projects including mono.

Agents run a `bootstrap.sh` step that ensures the required toolchain is present:

- `rustup toolchain install` per `rust-toolchain.toml` if not present.
- `bazelisk` available on `$PATH`.
- `pnpm` available on `$PATH`.
- `jj` *not* installed on agents. Buildkite checks out via git natively; the jj-in-workspace concern is a worker-side issue, not a CI concern (see Risk R3).

If contention shows up, we can tag and isolate mono jobs separately. Until then, shared queue.

### Bazel cache

Two-tier:

1. **Per-agent disk cache.** Each agent runs with `--disk_cache=/var/cache/bazel-mono` set via `.bazelrc.ci`. Persists across jobs on the same agent. Warm-cache rebuilds are seconds.
2. **Remote cache.** A read-write remote cache (BuildBuddy or self-hosted bazel-remote) shared across agents. A cold agent (or a new agent type) reads remote first, falls back to local build, and writes back.

v1 ships **disk cache only**. The remote cache is a fast-follow, not a v1 requirement. Reasoning: a single-fleet setup with sticky agents (buildkite has reasonable agent affinity) gets most of the disk-cache benefit without standing up a separate remote-cache service. We'll know remote is needed when (a) `bazel-test` cold-cache exceeds ~10 minutes, or (b) we split fleets and inter-fleet sharing matters.

Cache keying:
- The disk cache key includes `bazel info release` and a hash of `MODULE.bazel.lock` (matches flunge's approach and what the recent PR #439 dispatch-cache fix established for repobin).
- `cargo` shares the agent-level `~/.cargo/registry` cache; that's it for cargo. Target dirs are not cached across jobs (they're cheap relative to bazel and the cache-poisoning risk is higher).

### Historical: cargo→bazel transition

The original v1 plan ran `cargo test` and `bazel test` side-by-side until P1 closed the engine-lib coverage gap, and included `cargo check` as a cheap compile guard. **Both are no longer needed.** P1 has landed: `tools/boss/engine/BUILD.bazel:86` declares `rust_test(name = "engine_lib_test", crate = ":engine_lib", ...)`, and `bazel test //tools/boss/engine/...` exercises the lib tests that the May-12 drift exposed. Bazel's dependency-graph checking (`bazel build`) catches what cargo-check would catch, making the extra compile guard redundant.

The implementation now wires `bazel test //...` and `bazel build //...` directly as the canonical rust signal steps, with no transitional `cargo-check` or `cargo-test` steps needed.

Verification for bazel-test:
- Spot-check that the bazel-test wall-clock on a warm cache is within the budget assumed by the sharding decision below.

### Sharding

No sharding in v1. The full mono test suite (rust + node) is currently well under ten minutes on a warm cache. Sharding adds buildkite-step orchestration complexity (test discovery, distribution, retry semantics) for an improvement we don't need yet.

Re-evaluate when wall-clock for the slowest required step exceeds 15 minutes on a warm cache. At that point the natural unit is bazel test target groups (one shard per top-level package), not file-level test sharding — bazel already shards across cores within a target.

### Boss UI integration — shipped

This piece has landed ahead of the pipeline itself. `PrCiIndicator` at `tools/boss/app-macos/Sources/ContentView.swift:2683` renders a small `caption2`-weight icon next to the PR link on Review-lane cards:

| Engine-reported state | Icon (SF Symbol)         | Colour | Tooltip                                                |
|-----------------------|--------------------------|--------|--------------------------------------------------------|
| `in_progress`         | `clock.fill`             | yellow | "Required CI checks in progress"                       |
| `success`             | `checkmark.circle.fill`  | green  | "All required CI checks passed"                        |
| `fail`                | `xmark.circle.fill`      | red    | "Required CI check(s) failed: <names from rollup>"     |
| `unknown` / other     | (hidden)                 | —      | hidden, so "no signal yet" doesn't look like all-green |

State is fed by the engine's existing `ci_watch` probe (`tools/boss/engine/src/ci_watch.rs`), which already polls `gh pr view --json statusCheckRollup` for `in_review` rows. The failure detail payload is the same `failed_checks` JSON shape `ci_watch` uses internally — `PrCiIndicator` parses it for the tooltip so reviewers can see which check name(s) reddened the build without leaving the kanban.

No new schema column was needed (the original design speculated a `pr_ci_status` text field on `tasks`; in practice the indicator reads from the same `ci_watch`-populated path as the rest of the remediation pipeline, which kept the change additive). Open polish items, if any, belong on a follow-up chore; nothing about this section blocks the pipeline rollout.

## Risks / open questions

### R1 — `auto_pr_maintenance_disabled` interaction

The engine's `ci_watch.rs` (and `conflict_watch.rs`) already gate on a per-product `auto_pr_maintenance_disabled` flag. Today nothing red-buttons engine remediation if a flaky CI check fires repeatedly. Once a real pipeline exists, a flake storm could trigger a cascade of CI-remediation workers.

Mitigation:
- The pipeline ramp above keeps flaky steps advisory until they're stable. `ci_watch` only acts on *required* failures (`merge-conflict-handling-in-review.md` §Q4-Q5).
- The per-PR opt-out label is already wired (`ci_watch.rs:42`). A human can quiet a remediation loop on a single PR without disabling the product-wide flag.
- If a product-wide flake storm hits, the user disables the flag, restores green, re-enables. Same playbook as the conflict-watch one.

This is *not* a v1 blocker — `ci_watch` is already defensive — but the implementation task that promotes a step to required must check `ci_watch`'s remediation-budget config and confirm the per-PR cap is sane (default is 3 attempts).

### R2 — Engine auto-merge path (project description claim)

The parent project description says "Today the engine auto-merges on PR resolved; the CI gate must come BEFORE that path so red PRs are not auto-merged."

Verified by grep — the engine does *not* call `gh pr merge` anywhere in `tools/boss/engine`. The `merge_poller` *detects* merges that have already happened; it does not perform them. So there is no engine-side path to gate; branch protection on the GitHub side is the only enforcement layer needed for v1.

If/when the engine grows an auto-merge capability, that design must check the buildkite status before merging. Recommend the implementation task adds a defensive grep-guard test ("no `gh pr merge` calls in engine source") to lock the assumption.

### R3 — jj-vs-git on the CI worker

Buildkite checks out the repo via git, into a path it controls. There is no jj on the agent. The pipeline scripts (`steps/*.sh`) speak git only; they never invoke `jj`. The repo's `jj` workspace surface only matters when Boss workers run inside leased cube workspaces — *that's worker plumbing, unrelated to CI*.

One gotcha: if any of the test code or check scripts assumes a `.jj/` directory exists (e.g., a checkleft check that reads `jj log`), it will fail in CI. None of the current checks do this (verified against `CHECKS.yaml`), but the implementation task should add a grep-guard or accept a `--no-jj` flag where relevant.

### R4 — Secrets and auth

Flunge's buildkite has access to a shared secrets store (BUILDKITE_AGENT_TOKEN, ssh deploy keys, ghcr creds). Mono's pipeline will need:

- Repo read access (already covered if we share the flunge agent fleet's GitHub token).
- Buildkite API token if any step needs to call back into buildkite (probably not in v1).
- No write access to anything outside the build directory.

This is a checklist item for the "mirror pipeline shell" implementation task, not a design choice. Listed here so it doesn't get forgotten.

### R5 — How does the `checks.sh` step interact with CHECKS.yaml?

The repo already runs `checkleft` (or equivalent) against `CHECKS.yaml`. In CI the same runner must execute; the implementation task confirms the exact invocation (`bazel run //tools/checkleft -- --against=origin/main`? `pnpm checks`? `cargo run -p checkleft`?). Recommend reading what the worker entrypoint does today (the `.claude/CLAUDE.md` hook chain) and mirroring it in the CI script.

### Q1 — Do we want the pipeline triggered on PR draft → ready transitions, or only on push?

Buildkite's default is "on push to a tracked branch". GitHub-mode triggers can also fire on PR label changes. v1 recommendation: push only. If a human flips draft → ready without pushing, they can re-trigger manually with `bk build create`. Re-evaluate if this turns into a recurring source of stale-CI confusion.

### Q2 — Required-check naming convention

The branch protection rules reference exact check names. Once those names are wired into branch protection, renaming a step in `pipeline.yml` silently breaks the gate (the rule waits forever for `buildkite/mono/cargo-check` to report, but the renamed step now emits `buildkite/mono/cargo-verify`). v1 should:

- Pick names once and treat them as a public contract.
- Add a `.buildkite/REQUIRED_CHECKS.md` listing the canonical names so a PR that renames a step is forced to update branch protection in lockstep.

Open question for the human reviewer: do you want the name format `buildkite/mono/<step>` (matches flunge?) or `mono / <step>` (matches typical GitHub Actions naming)? Buildkite emits the former by default; flagging in case there's a house style.

---

## Follow-up implementation tasks (informational — not part of this PR)

The next project decomposition will likely produce, roughly in this order:

1. Audit flunge's buildkite pipeline; produce a short reference doc.
2. Land `.buildkite/` skeleton in mono (empty steps, "hello world" green).
3. Wire the static checks (`cargo check`, `bazel build`, `pnpm typecheck`, `checks.sh`).
4. Wire the test steps (`bazel test`, `pnpm test`).
5. Promote checks to required (per ramp above); turn on branch protection.
6. ~~Surface CI status in Boss UI (kanban badge on in-review cards).~~ **Done** — see "Boss UI integration — shipped" above.
7. ~~Post-P1 cleanup: drop `cargo-test`; expand bazel coverage signal.~~ **Done in this refresh** — P1 has landed, so the pipeline ships without a `cargo-test` step in the first place; no follow-up trim required.
8. Bazel remote cache (fast-follow once contention warrants).

Each lands as its own PR. Each remaining item (#1–#5, #8) is gated on the previous within that chain.
