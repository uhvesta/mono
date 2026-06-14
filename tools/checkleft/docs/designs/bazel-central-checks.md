# Checkleft: Central Dev-Infra Checks for Bazel Repos

> **Superseded (runtime unification).** References to the `exec-v1` runtime below
> predate the runtime unification: `exec-v1` has been folded into the
> **declarative** runtime (a custom binary is now a declarative invocation with
> the `passthrough` transform). The trust boundary this design relies on — that
> framework-invoked binaries are rejected from `external_checks_url` and allowed
> only from checked-in / file config — is unchanged; it now applies to the
> `declarative` runtime in place of `exec-v1`.

## Overview

This design adds a Bazel-first integration model for centrally owned Checkleft
policy in `ci-infra`.

The desired operator experience is:

1. `ci-infra` owns the central `CHECKS.yaml` and the implementations of
   shared custom checks.
2. each Bazel consumer repo pins one `ci-infra` version through bzlmod,
3. each consumer repo builds one aggregate Checkleft index that includes:
   - central checks exported from `ci-infra`,
   - repo-local checks owned by that repo,
4. Checkleft loads the central config first, then applies the repo's checked-in
   root and child `CHECKS.yaml` / `CHECKS.toml` files on top.

This keeps Bazel repos on one versioning mechanism and one execution path while
preserving the current Checkleft model of a single generated index input.

## Why This Design Exists

Checkleft already has several pieces that are close to this model:

- root-to-child config layering,
- remote root config loading through `external_checks_url`,
- generated external package resolution through one index,
- trusted `exec-v1` execution for repo-local Bazel-backed checks.

That is not yet enough for central Bazel-owned policy because the current
remote-config path is intentionally not allowed to introduce `exec-v1` checks.
That restriction is correct for downloaded config, but it blocks the desired
`ci-infra` model where config and trusted check implementations are both pinned
through Bazel.

This design introduces a Bazel-specific central-config path that matches the
existing trust model better than HTTP config loading does.

## Goals

- Let `ci-infra` own a checked-in central `CHECKS.yaml`.
- Let `ci-infra` own shared custom check implementations.
- Use the consumer repo's bzlmod pin to version both central config and central
  check implementations together.
- Keep one aggregate generated index per consumer repo.
- Support central custom checks implemented as trusted `exec-v1` Bazel
  executables.
- Keep the consumer-facing command as one stable Bazel wrapper target for local
  use and CI.
- Preserve checked-in per-repo `CHECKS.yaml` / `CHECKS.toml` for repo-specific
  additions and overrides.

## Non-Goals

- Solving Gradle or other non-Bazel repos in this design.
- Requiring multiple generated indexes in one Checkleft invocation.
- Moving central policy delivery onto HTTP for Bazel repos.
- Replacing `sandbox-v1` or the existing wasm package model.
- Providing hard anti-bypass override semantics in the first phase.

## Current Constraints

Several existing Checkleft behaviors shape this proposal:

- Generated external checks are resolved through one configured generated index.
- `exec-v1` is a trusted runtime intended for repo-local Bazel-backed checks.
- Checks loaded from `settings.external_checks_url` are currently rejected if
  they resolve to `exec-v1`.
- `external_checks_url` is an HTTP URL flow, not a bzlmod-pinned file flow.

Those constraints imply two things:

1. a Bazel consumer should continue to pass one aggregate generated index to
   Checkleft,
2. central config for Bazel repos should not be loaded through the HTTP
   `external_checks_url` path.

## Proposed Model

### Ownership

`ci-infra` owns:

- the central `CHECKS.yaml`,
- Bazel targets that package shared custom checks for Checkleft,
- optional helper macros that expose the set of central checks to consumers.

Each consumer repo owns:

- its checked-in root and child `CHECKS.yaml` / `CHECKS.toml`,
- any repo-specific custom checks,
- one aggregate `check_index(...)` target that includes both central and local
  checks,
- one canonical Bazel wrapper target used by developers and CI.

### Configuration Model

Add a new Bazel-oriented external root config input to Checkleft:

- `--external-checks-file <path>`

This file is loaded before the repo's own root config, using the same merge
order that external root config already uses today:

1. external central config,
2. local root config,
3. local child configs from root to leaf.

This keeps the central config behaving like a repo-wide parent or "superfolder"
policy file.

For Bazel consumers, the canonical wrapper target should pass the pinned
`ci-infra` config file through this CLI flag rather than asking repos to
hardcode Bazel output paths in checked-in config.

### Central Config Restrictions

The central config file loaded through `--external-checks-file` should support:

- built-in checks,
- `generated:<id>` external implementations.

It should not support:

- repo-relative file implementation manifests.

This keeps the integration narrow and matches the aggregate-index design. The
central config declares which shared checks should exist, while the consumer
repo's aggregate index decides which generated implementations are actually
present for that invocation.

### Aggregate Index Model

Each Bazel consumer repo builds one aggregate `check_index(...)` target that
includes:

- `CheckInfo` providers exported from `ci-infra`,
- `CheckInfo` providers for repo-local checks.

The aggregate index is the only generated index passed to Checkleft.

This preserves the current Checkleft model:

- one index,
- one resolution path for `generated:<id>`,
- one wrapper target that builds and runs everything needed for the repo.

The aggregate-index model is preferred over a standalone `ci-infra` index
passed directly to Checkleft because it composes naturally with repo-specific
checks without teaching Checkleft about multiple indexes.

### Bazel Packaging Model

`ci-infra` should export packaged checks as normal Checkleft Bazel targets, not
just as one opaque prebuilt index.

That means `ci-infra` should expose targets that provide `CheckInfo`, so the
consumer repo can aggregate them alongside local checks through its own
`check_index(...)`.

Recommended shape:

- one target per central check package,
- optionally one macro or alias target group that expands to the standard
  central set.

This keeps the integration flexible:

- consumer repos can include the full central set,
- consumer repos can add local checks in the same aggregate index,
- future phased rollouts can include subsets without changing Checkleft's core
  package model.

`ci-infra` should also export a convenience `check_index(...)` target that
contains the standard central set.

That index is not the final index passed to Checkleft in consumer repos.
Instead, it serves two purposes:

- it gives `ci-infra` one canonical target that proves the central set builds
  and packages correctly,
- it provides a concrete example and reusable grouping for consumers that want
  to mirror the standard central set in their own aggregate index.

The final active index for a consumer repo should still be consumer-owned so it
can compose:

- the standard central set,
- optional repo-specific central subsets,
- repo-local checks.

## Trust Model

This design keeps `exec-v1` in the trusted Bazel path, not the downloaded HTTP
path.

The trust boundary is:

- the consumer repo chooses to depend on a pinned `ci-infra` version,
- Bazel builds the central and local check executables into local output trees,
- Checkleft executes those local built launchers through the aggregate index.

This is different from allowing arbitrary remote config to point at trusted
repo-local executables. The central config file is not fetched over HTTP at
runtime; it is supplied by the pinned Bazel dependency and wrapper target.

## Required Checkleft Changes

### 1. Add external root config file support

Add a filesystem-based external root config input for Bazel consumers:

- CLI flag: `--external-checks-file <path>`

Recommended behavior:

- the path may be absolute or repo-root-relative,
- the file may be `CHECKS.yaml` or `CHECKS.toml`,
- it is loaded before local root config,
- config diagnostics should report the external file path as the source.

This should be separate from `external_checks_url`, not a replacement for it.

### 2. Add config origin distinction for external file input

Checkleft currently distinguishes local config from HTTP-loaded external config.
It should distinguish three origins:

- local config,
- external HTTP config,
- external file config.

That distinction matters because the allowed implementation model differs by
origin.

### 3. Allow `exec-v1` for external file config

Checks loaded from the new external file origin should be allowed to resolve to
`exec-v1` when they use `implementation = "generated:<id>"`.

Checks loaded from external file origin should reject file-based implementation
references.

Checks loaded from external HTTP config should keep the current `exec-v1`
restriction.

This yields a simple trust rule:

- HTTP external config remains sandbox-oriented,
- Bazel-supplied external file config can participate in the trusted generated
  index flow.

### 4. Extend the Bazel wrapper macro

The Bazel wrapper that runs Checkleft should grow one new attribute:

- `external_checks_file`

The wrapper should:

1. include the central config file in runfiles,
2. continue to set `CHECKLEFT_EXTERNAL_CHECK_INDEX` to the aggregate index,
3. invoke `checkleft run --external-checks-file <resolved-path>`.

This keeps the consumer command stable while hiding path setup from users.

## Configuration Semantics

### Merge Order

For a changed file, the effective config should be resolved in this order:

1. external central config file from `ci-infra`,
2. consumer repo root config,
3. consumer repo child configs from root to leaf.

Checks still merge by `id`, consistent with current Checkleft behavior.

### Override Behavior

In the first phase, local config should continue to be allowed to override or
disable central checks by `id`, because that matches existing merge semantics.

This means the first phase does not provide strong "cannot be turned off"
enforcement inside Checkleft itself.

That limitation should be explicit in the design. Operational enforcement in
phase one comes from:

- the canonical wrapper target used in CI,
- code review on changes to local `CHECKS` files,
- ownership of the central config and central check implementations in
  `ci-infra`.

Hard non-overridable central checks can be explored in a follow-up if needed.

## Versioning

This design intentionally keeps Bazel repos on one pin:

- the bzlmod version of `ci-infra`.

That one pin versions:

- the central `CHECKS.yaml`,
- the central custom check implementations,
- any Bazel macros or target wiring exported by `ci-infra`.

This is better than an HTTP URL-based central config for Bazel repos because it
avoids a second version source that can drift from the checked-in Bazel pin.

## Example Consumer Shape

In `ci-infra/checkleft/BUILD.bazel`:

```starlark
local_check(
    name = "no_genrules",
    id = "no-genrules",
    binary = ":no_genrules_bin",
)

check_index(
    name = "central_checks",
    checks = [
        ":no_genrules",
    ],
)
```

In `ci-infra/checkleft/defs.bzl`:

```starlark
def central_check_targets():
    return [
        "@ci_infra//checkleft:no_genrules",
    ]
```

In a consumer repo:

```starlark
load("@ci_infra//checkleft:defs.bzl", "central_check_targets")

check_index(
    name = "all_checks",
    checks = central_check_targets() + [
        "//tools/checks:repo_specific_policy",
    ],
)

checkleft(
    name = "check",
    check_index = ":all_checks",
    external_checks_file = "@ci_infra//checkleft:CHECKS.yaml",
)
```

In the central config file owned by `ci-infra`:

```yaml
checks:
  - id: no-genrules
    implementation: generated:no-genrules
```

In the consumer repo's checked-in root config:

```yaml
checks:
  - id: oversized-files
    check: file/size
    config:
      max_lines: 600
```

The central config applies first. The consumer repo can then add or override
checks using its own checked-in config files.

This example shows the intended composition pattern:

- `ci-infra` exports a canonical `:central_checks` index for its own build and
  packaging boundary,
- `ci-infra` also exports a helper such as `central_check_targets()` so
  consumers can reuse the standard central set without listing each check
  target manually,
- the consumer repo still owns the final aggregate `check_index(...)` target
  that Checkleft runs against.

## Migration Plan

### Phase 1

1. Add `--external-checks-file` support to Checkleft config resolution.
2. Add a new config origin for external file input.
3. Permit `generated:<id>` plus `exec-v1` for external file origin.
4. Reject file-based implementations from external file origin.
5. Extend the Bazel wrapper macro to pass both:
   - the aggregate generated index,
   - the external central config file.

### Phase 2

1. Package one central `ci-infra` custom check through Bazel.
2. Add one aggregate consumer index that includes:
   - that central check,
   - one repo-local check.
3. Move one central policy entry into the `ci-infra` config file.
4. Switch one Bazel repo's CI and local docs to the wrapper target.

### Phase 3

1. Expand the central check set.
2. Standardize the wrapper macro across Bazel repos.
3. Revisit stronger enforcement or override restrictions if required.

## Alternatives Considered

### Use `external_checks_url` for Bazel repos

Rejected for Bazel v1.

Problems:

- introduces a second version pin separate from bzlmod,
- keeps central config on the path that intentionally rejects `exec-v1`,
- makes Bazel consumers depend on HTTP delivery for something already pinned in
  the build graph.

### Pass the `ci-infra` index directly to Checkleft

Rejected.

Problems:

- does not compose with repo-local generated checks cleanly,
- pushes multiple-index composition pressure into Checkleft,
- weakens the consumer repo's clear ownership of its final active check set.

### Keep central config local to each repo

Rejected as the primary design.

Problems:

- duplicates policy config across repos,
- weakens central review and rollout control,
- makes central and local policy harder to distinguish.

## Open Questions

- Should the Bazel wrapper accept exactly one `external_checks_file`, or a list
  for future layering?
- Should `ci-infra` export only individual `CheckInfo` targets, or also a
  convenience macro that returns the standard central set for aggregation?
- Do we want a follow-up settings key for checked-in local discovery of an
  external file path, or is wrapper-only configuration sufficient for Bazel
  repos?
- When stronger enforcement is needed, should it be:
  - config-level non-overridable checks,
  - CI policy outside Checkleft,
  - or both?
