# Boss/mono Buildkite CI

This directory contains the Buildkite CI pipeline for the mono repo. It mirrors the shape of the [flunge pipeline](../tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md) but adapts for mono's rust + bazel + node surface.

The full design is at [`tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md`](../tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md).

## Directory layout

```
.buildkite/
  pipeline.yml          # Buildkite reads this; declares steps, queue tags, depends_on only
  steps/
    bootstrap.sh        # Prime the agent: rust toolchain, bazelisk, pnpm, cache restore
    cargo-check.sh      # cargo check --workspace (cheap compile guard)
    bazel-build.sh      # bazel build //... (dependency-graph compile guard)
    bazel-test.sh       # bazel test //... (canonical rust + integration tests)
    pnpm-typecheck.sh   # pnpm -r typecheck
    pnpm-test.sh        # pnpm -r test
    checks.sh           # CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.)
  README.md             # this file
```

## Pipeline shape

```
                      ┌──► cargo-check    ──┐
                      ├──► bazel-build    ──┤
bootstrap (queue=mono)┼──► pnpm-typecheck ──┼──► (wait) ──► bazel-test ──┐
                      ├──► checks         ──┘               pnpm-test  ──┴──► green
```

- `bootstrap` runs first; all other steps depend on it.
- `cargo-check`, `bazel-build`, `pnpm-typecheck`, and `checks` run in parallel after bootstrap.
- `bazel-test` and `pnpm-test` run only after all static checks pass (the `wait` step).
- `pnpm-test` ships as advisory (run-but-not-required) in v1 until its flake rate is stable.

## Step details

### `bootstrap.sh`

Ensures the agent has the required toolchain:
- Rust: installs / pins via `rust-toolchain.toml` using `rustup`.
- Bazel: `bazelisk` should be on `$PATH`; version is read from `.bazelversion`.
- pnpm: installs if not present, pins to the version in `package.json#packageManager`.
- Restores the agent-local bazel disk cache (`/var/cache/bazel-mono`, configured in `.bazelrc.ci`).

### `cargo-check.sh`

Runs `cargo check --workspace`. This is a cheap, fast-failing compile guard — useful when the bazel target graph itself is broken (e.g., a missing `srcs` entry that hides a file from bazel but not from cargo). Does not run tests.

### `bazel-build.sh`

Runs `bazel build //...`. Catches build-graph rot (visibility violations, missing deps, broken generated files) that cargo cannot see.

### `bazel-test.sh`

Runs `bazel test //...`. This is the canonical rust test step. With P1 landed (`tools/boss/engine/BUILD.bazel:86` — `rust_test(name = "engine_lib_test", crate = ":engine_lib")`), this covers the engine lib tests that the 2026-05-12 drift incident exposed, in addition to the integration test targets.

### `pnpm-typecheck.sh`

Runs `pnpm -r typecheck` across all TypeScript workspaces.

### `pnpm-test.sh`

Runs `pnpm -r test` across all JavaScript/TypeScript workspaces.

### `checks.sh`

Runs the `CHECKS.yaml` checks via `checkleft` (or the equivalent runner). Scoped to changed paths on PR builds. Does not invoke `jj`; base-ref detection uses git.

## Agents and queue

All steps run on `queue=mono`. Agents are shared with the flunge fleet (`queue=flunge`) but tagged separately. The `bootstrap.sh` step handles mono-specific toolchain setup that flunge agents don't need by default.

## Debugging a red build locally

Each `steps/*.sh` script can be run directly from the repo root (no buildkite-specific env required for the placeholder steps). Once real checks are wired:

```sh
# Run a specific step locally
bash .buildkite/steps/cargo-check.sh

# Reproduce bazel step with CI config
bazel test //... --config=ci
```

For bazel steps, the CI config is in `.bazelrc.ci` (added when real bazel steps are wired in #3).

## Required checks (branch protection)

v1 ships as advisory — no required-status checks yet. Branch protection is added in task #5 per the ramp in the design doc:

1. Land skeleton as advisory (this PR).
2. Promote `bootstrap`, `cargo-check`, `bazel-build`, `checks` to required.
3. After two weeks, promote `bazel-test` and `pnpm-typecheck`.
4. Promote `pnpm-test` once flake rate is < 1%.

The check names buildkite will report (once the pipeline is wired to the buildkite project) are `buildkite/mono/<step-key>`, e.g. `buildkite/mono/cargo-check`. Treat these as a public contract — renaming a step key in `pipeline.yml` requires updating branch protection in lockstep.

## Status

Task #3 (static checks) is complete. `cargo-check.sh`, `bazel-build.sh`, `pnpm-typecheck.sh`, and `checks.sh` now run real invocations. `bazel-build.sh` uses `--config=ci` which sets `--disk_cache=/var/cache/bazel-mono` (defined in `.bazelrc`). `bazel-test.sh` and `pnpm-test.sh` remain placeholders; they are wired in task #4 (test steps).
