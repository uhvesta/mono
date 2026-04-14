# Checkleft: Bazel-Packaged External Checks

## Overview

Checkleft already supports external checks in two forms:

- `source` mode, where Checkleft builds JavaScript or TypeScript source into a
  wasm artifact through its checked-in JS componentizer toolchain,
- `artifact` mode, where Checkleft loads a prebuilt wasm artifact described by
  a manifest and executes it directly.

The missing piece is first-class Bazel integration for repositories that want
to keep check implementations in-repo, build them with Bazel, and then hand the
resulting packages to Checkleft without manual manifest authoring or ad hoc
shell glue.

This design adds a Bazel-facing authoring and packaging flow for external
checks. The core idea is:

1. Checkleft ships a Bazel macro for JS/TS checks that compiles source into a
   wasm artifact and packages it,
2. Checkleft also ships a generic Bazel rule that wraps any existing wasm
   artifact into an external-check manifest,
3. an aggregate Bazel rule emits a generated-package index,
4. Checkleft consumes that index through its existing generated-package
   provider path.

This keeps compilation in Bazel, keeps runtime execution in Checkleft, and
avoids teaching the `checkleft` binary how to compile arbitrary languages at
runtime.

## Current State

Several parts of this design already exist in the framework:

- external package manifests already support `artifact` mode with
  `artifact_path`, `artifact_sha256`, and optional `[provenance]`,
- the wasm runtime already executes prebuilt artifact packages and verifies the
  artifact digest before execution,
- the generated external package provider already resolves
  `implementation = "generated:<id>"` through an index TOML configured by
  `CHECKLEFT_EXTERNAL_CHECK_INDEX`,
- external artifact execution already accepts either a core wasm module or a
  component-style artifact.

What does not exist yet:

- a Checkleft-owned Bazel macro that compiles JS/TS check source into wasm,
- a Bazel rule that writes artifact manifests,
- a Bazel rule that writes the generated-package index,
- a documented repo-level contract for passing Bazel-built packages into
  Checkleft,
- a first-class Bazel authoring story for repo-owned external checks.

## Goals

- Let a Bazel-based repository implement Checkleft checks as checked-in source
  code in that same repository.
- Let Bazel remain the source of truth for building those checks into wasm.
- Reuse Checkleft's existing external package manifest and generated-index
  model instead of inventing a second packaging format.
- Give repo authors a zero-custom-plumbing path for JS/TS checks under Bazel.
- Avoid requiring policy authors to hand-maintain `artifact_sha256` values.
- Support multiple external checks in one repo through a single generated index.
- Keep Bazel visibility narrow by default.
- Keep the runtime contract language-agnostic at the packaging layer.

## Non-Goals

- Replacing Checkleft's existing JS/TS `source` mode.
- Teaching Checkleft to invoke Bazel automatically on every run.
- Designing a new cross-repo download or remote package distribution protocol.
- Defining Bazel compilation rules for every supported language in this change.
- Expanding the wasm host API in this design. Package building and runtime host
  capabilities are separate concerns.

For languages beyond JS/TS, compilation should stay in repo-local Bazel
toolchains or macros. This design standardizes how Bazel-built wasm artifacts
become Checkleft external packages, and it gives JS/TS a first-class authoring
path because that toolchain already exists inside Checkleft today.

## Why Use Bazel-Produced Artifacts

For Bazel repositories, packaging external checks as prebuilt wasm artifacts is
the right split of responsibility:

- Bazel already knows how to compile checked-in code with repo-specific
  toolchains and dependencies.
- Checkleft already knows how to validate and execute wasm artifacts.
- The packaging boundary is small and stable: manifest plus digest plus index.

This is better than having Checkleft shell out to Bazel itself because:

- it avoids hidden build work during `checkleft run`,
- it keeps build graph ownership in Bazel,
- it makes CI wiring explicit,
- it avoids coupling Checkleft to Bazel target analysis semantics.

## Proposed User-Facing Model

### Check Configuration

Repository `CHECKS.toml` should continue to refer to external packages by
generated implementation ID:

```toml
[[checks]]
id = "frontend-no-legacy-api"
check = "frontend-no-legacy-api"
implementation = "generated:frontend-no-legacy-api"

[checks.config]
legacy_modules = ["api/v1", "api/legacy"]
```

This keeps repo policy config stable even if the Bazel output path changes.

### Bazel Rules

Add three Bazel-facing rules or macros under Checkleft-owned Starlark:

1. `js_check`
2. `check`
3. `check_index`

`js_check` compiles a JS/TS check into wasm and packages it. `check` packages
an existing wasm artifact. `check_index` aggregates packaged checks into a
generated-index file that Checkleft already knows how to consume.

### Operational Flow

The intended execution flow is:

1. Bazel builds the external-check index target.
2. Bazel outputs:
   - one manifest per packaged check,
   - one generated index TOML,
   - the wasm artifacts referenced by those manifests.
3. The caller passes the generated index path to Checkleft.
4. Checkleft resolves `generated:<id>` implementations through that index and
   executes the referenced wasm artifacts.

For the first implementation, passing the generated index path should reuse the
existing provider input:

- `CHECKLEFT_EXTERNAL_CHECK_INDEX=<path-to-index.toml>`

A wrapper script or CI step can hide that env var from users. A later follow-up
may add a first-class CLI flag, but that is not required for the Bazel package
model itself.

### Build Integration

Compilation should happen through normal Bazel target dependencies, not through
implicit logic inside the `checkleft` binary.

For JS/TS checks, Checkleft should provide the compile step as a Bazel macro.
Requiring each consuming repo to compose `rules_js`, `esbuild`, `jco`,
ComponentizeJS, and the Checkleft WIT contract is too much authoring overhead
for what should be a simple policy check.

That means the repo should define one aggregate Bazel target for external
checks, for example:

- `//checks:check_index`

Building that target must transitively:

- compile every repo-owned external check implementation into wasm,
- package each one into a Checkleft manifest,
- write the generated index consumed by Checkleft.

This gives the repo one stable build target that answers "are all custom
checks compiled and packaged for this revision?"

### Recommended Execution Model

The recommended operational model is two-layered:

1. Checkleft ships packaging primitives and runtime support.
2. Checkleft ships one Bazel-facing operational wrapper macro that builds the
   external-check index and then runs Checkleft with the right env vars.

The wrapper macro is the canonical command humans and CI should run.

This keeps build orchestration explicit while still giving users a one-command
entrypoint.

Because this repo is already on Bazel 8, that wrapper should be implemented as
a symbolic macro rather than asking each repo to hand-author an `sh_binary`.
A symbolic macro is the right fit here because it:

- hides the internal launcher target and its path assumptions,
- gives typed attributes and cleaner visibility behavior,
- avoids repeating boilerplate shell-wrapper logic in every consuming repo,
- matches Bazel's recommended macro style for Bazel 8+.

### Why Checkleft Should Own JS/TS Compilation

There are usable third-party building blocks for this space, but they do not
remove enough complexity for Checkleft consumers:

- the repo still has to choose and integrate the JS/TS Bazel stack,
- the repo still has to wire `esbuild` output into `jco componentize`,
- the repo still has to target Checkleft's WIT contract correctly,
- the repo still has to decide how that wasm target turns into a Checkleft
  package.

Checkleft already owns a checked-in JS componentizer toolchain for `source`
mode. Extending that ownership into Bazel gives users a much simpler and more
consistent story:

- `js_check` for JS/TS source authored in-repo,
- `check` for prebuilt wasm from any other language,
- `check_index` for aggregation.

## Proposed Bazel Rules

### `js_check`

This macro compiles a JS/TS check implementation into a wasm component and then
packages it as a Checkleft external check.

Suggested attributes:

- `id`: external package ID and default generated implementation ID.
- `entry`: JS/TS entrypoint for the check implementation.
- `srcs`: source files needed to build the check.
- `commands`: optional manifest command capability list.
- `implementation_name`: optional generated implementation ID override; defaults
  to `id`.

Suggested behavior:

1. bundle the entrypoint to one ESM artifact,
2. componentize that bundle using Checkleft's runtime WIT world,
3. delegate to `check` to write the manifest and expose package metadata.

`js_check` should be the main authoring path for repo-owned JS/TS checks. It
should not require the consuming repo to vendor its own `esbuild`, `jco`, or
ComponentizeJS setup just to author a check.

### `check`

This rule packages one Bazel-built wasm artifact as a Checkleft external-check
manifest.

Suggested attributes:

- `id`: external package ID and default generated implementation ID.
- `wasm`: label producing exactly one `.wasm` file.
- `commands`: optional manifest command capability list.
- `generator`: optional provenance generator string, default `bazel`.
- `implementation_name`: optional generated implementation ID override; defaults
  to `id`.

Suggested outputs:

- `<name>.check.toml`

Suggested manifest shape:

```toml
id = "frontend-no-legacy-api"
mode = "artifact"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "bazel-bin/checks/frontend_no_legacy_api/check.wasm"
artifact_sha256 = "<computed digest>"

[provenance]
generator = "bazel"
target = "//checks/frontend_no_legacy_api:check"

[capabilities]
commands = ["grep"]
```

Rule behavior:

1. require exactly one wasm output,
2. compute the canonical sha256 digest during the Bazel action,
3. write a manifest that uses a repo-root-relative `artifact_path`,
4. record Bazel provenance in the manifest,
5. expose provider metadata needed by the aggregate index rule.

The rule should not care whether the wasm artifact is a core module or a
component. Checkleft runtime already supports both forms.

### `check_index`

This rule aggregates many packaged checks into one generated-index TOML.

Suggested attributes:

- `checks`: list of packaged check targets produced by `js_check`, `check`, or
  future language-specific macros that delegate to `check`.

Suggested outputs:

- `<name>.index.toml`

Suggested index shape:

```toml
version = 1

[[packages]]
implementation = "generated:frontend-no-legacy-api"
manifest = "bazel-bin/checks/frontend_no_legacy_api/frontend_no_legacy_api.check.toml"

[[packages]]
implementation = "generated:workflow-shell-strict"
manifest = "bazel-bin/checks/workflow_shell_strict/workflow_shell_strict.check.toml"
```

Rule behavior:

1. read package metadata from dependencies rather than reparsing manifests,
2. fail if two dependencies claim the same generated implementation ID,
3. write manifest paths relative to the generated index file location.

The current generated-package provider resolves manifest paths relative to the
index directory. That means the index rule should usually place manifest paths
as sibling or descendant paths of the index output tree.

## Bundle Format

This design intentionally does not introduce a tarball or zip archive format.

The effective "bundle" is:

- the generated index TOML,
- the per-check manifest TOMLs,
- the referenced wasm artifacts.

Reasons:

- Checkleft already consumes this shape today.
- The provider model is file-based, not archive-based.
- Bazel already tracks these outputs individually.
- Digest verification already happens at the artifact file boundary.

If a repository wants a single distributable archive later, that can be layered
on top of this file set without changing Checkleft's package model.

## Path Semantics

The design should distinguish three different path kinds:

1. the generated index path passed into `CHECKLEFT_EXTERNAL_CHECK_INDEX`,
2. manifest paths written inside that generated index,
3. `artifact_path` written inside each external-check manifest.

Recommended semantics:

- the generated index path passed to Checkleft may be repo-root-relative or
  absolute; prefer repo-root-relative in docs and wrappers,
- manifest paths inside the generated index should be relative to the index
  file directory,
- `artifact_path` inside each manifest should be repo-root-relative.

That means Bazel outputs should be referenced through stable workspace-visible
paths such as `bazel-bin/...`, but not all path fields resolve relative to the
same base.

This keeps the manifests:

- relocatable within a given workspace,
- readable by Checkleft's existing file and generated-index providers,
- suitable for both local runs and CI runs from repo root.

Absolute paths should be avoided because:

- they make build outputs machine-specific,
- they complicate caching and reproducibility,
- the existing manifest validators already prefer safe relative paths.

## Example Repository Usage

The repository author experience should look roughly like this:

```starlark
load("//tools/checkleft/bazel:defs.bzl", "check_index", "js_check")

js_check(
    name = "frontend_no_legacy_api",
    id = "frontend-no-legacy-api",
    entry = "check.ts",
    srcs = ["check.ts"],
)

check_index(
    name = "check_index",
    checks = [":frontend_no_legacy_api"],
)
```

For one-off manual usage, a local wrapper or CI step can run:

```bash
bazel build //checks:check_index
export CHECKLEFT_EXTERNAL_CHECK_INDEX=bazel-bin/checks/check_index.index.toml
bazel run //tools/checkleft:checkleft -- run
```

The wrapper can also set:

- `CHECKLEFT_EXTERNAL_PROVIDER_MODE=generated-only`

when the repo wants to require Bazel-generated packages and avoid fallback to
repo-root file manifests.

### Recommended Repo-Local Wrapper

For regular use, Checkleft should provide a symbolic macro named `checkleft`
that instantiates the operational wrapper target.

That macro should depend on both:

- the Checkleft binary target,
- the aggregate external-check index target.

At the callsite, a repo should be able to write:

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft")

checkleft(
    name = "run_checkleft",
    check_index = ":check_index",
)
```

The symbolic macro can expand internally to a private launcher target, likely a
small `sh_binary`, that does:

```bash
#!/usr/bin/env bash
set -euo pipefail

export CHECKLEFT_EXTERNAL_PROVIDER_MODE=generated-only
export CHECKLEFT_EXTERNAL_CHECK_INDEX="bazel-bin/checks/check_index.index.toml"

exec bazel-bin/tools/checkleft/checkleft run "$@"
```

The exact binary path details can vary by repo and Bazel conventions. The key
point is that the macro hides those details while ensuring target dependencies
force external-check compilation before Checkleft runs.

With that wrapper in place, CI and developers get a single stable command:

```bash
bazel run //checks:run_checkleft -- --base-ref origin/main
```

That is the recommended answer to "what do I actually run?"

### Concrete TypeScript Example

For TypeScript checks, the user-facing target should be `js_check`, not a
repo-local macro. Internally, that macro should reuse the same core flow the
existing Checkleft JS path already uses:

- `esbuild` to bundle TS/JS entrypoints to ESM,
- `jco componentize` to turn that bundle into a wasm component,
- `tools/checks_js_componentizer/wit/check-runtime.wit` as the runtime contract.

So the Checkleft-owned Bazel macro should do the same thing under Bazel.

At a high level, the BUILD file for one check would look like:

```starlark
load("//tools/checkleft/bazel:defs.bzl", "check_index", "js_check")

package(default_visibility = ["//visibility:private"])

js_check(
    name = "frontend_no_legacy_api",
    id = "frontend-no-legacy-api",
    entry = "check.ts",
    srcs = ["check.ts"],
)

check_index(
    name = "check_index",
    checks = [":frontend_no_legacy_api"],
)
```

Internally, `js_check` could expand to a private wasm-producing helper plus the
public `check` rule. The private helper could look roughly like:

```starlark
load("@npm//:defs.bzl", "npm_link_all_packages")
load("@aspect_rules_js//js:defs.bzl", "js_run_binary")

# Exact generated repo names and macro symbols depend on npm_translate_lock
# output. These labels are illustrative.
load("@npm__esbuild__0.25.0//:package_json.bzl", esbuild_bin = "bin")
load("@npm__at_bytecodealliance_jco__1.15.0//:package_json.bzl", jco_bin = "bin")

def _js_check_wasm(name, entry, srcs):
    bundle_name = name + "_bundle"

    npm_link_all_packages(name = name + "_node_modules")

    js_run_binary(
        name = bundle_name,
        tool = esbuild_bin.esbuild_binary,
        srcs = srcs,
        args = [
            "$(execpath %s)" % entry,
            "--bundle",
            "--platform=neutral",
            "--format=esm",
            "--log-level=warning",
            "--outfile=$(execpath :%s.bundle.mjs)" % name,
        ],
        outs = [name + ".bundle.mjs"],
        data = [name + "_node_modules"],
    )

    js_run_binary(
        name = name,
        tool = jco_bin.jco_binary,
        srcs = [
            ":" + bundle_name,
            "//tools/checks_js_componentizer/wit:check-runtime.wit",
        ],
        args = [
            "componentize",
            "$(execpath :%s.bundle.mjs)" % name,
            "--wit",
            "$(execpath //tools/checks_js_componentizer/wit:check-runtime.wit)",
            "--world-name",
            "check-runtime",
            "--disable",
            "all",
            "--out",
            "$(execpath :%s.wasm)" % name,
        ],
        outs = [name + ".wasm"],
        data = [name + "_node_modules"],
    )
```

Then `js_check(...)` would call:

1. `_js_check_wasm(...)` to produce the `.wasm`,
2. `check(...)` to emit the manifest and package metadata.

The internal wasm step is still doing two explicit phases:

1. bundle `check.ts` to one ESM file,
2. componentize that bundle to `check.wasm`.

That is probably the right first implementation because:

- it matches the existing Checkleft JS componentizer behavior,
- this repo already has `aspect_rules_js` and pnpm integration,
- it avoids adding `rules_ts` just to build one bundled check artifact.

The important design point is that this complexity should be hidden inside
Checkleft's Bazel support, not pushed onto each consuming repo.

If the repo later wants full type-checking as part of the check build, that can
be added separately as:

- a sibling `tsc --noEmit` target, or
- a future `rules_ts`-based compilation layer.

The important point is that the wasm-producing target does not need to wait for
that larger TypeScript build story to exist.

## Implementation Sketch

### Bazel Side

Add Starlark under a Checkleft-owned Bazel package, for example:

```text
tools/checkleft/bazel/
  defs.bzl
```

Suggested implementation direction:

- implement `js_check` as a macro that expands to a private wasm-producing
  target and a public `check` target,
- implement `checkleft` as a symbolic macro that expands to a private launcher
  target and exports only the runnable wrapper target,
- define a provider carrying:
  - generated implementation ID,
  - manifest output file,
  - Bazel label string,
- have `check` emit the manifest through a small action,
- have `check_index` aggregate provider data into one TOML.

The JS/TS macro should use Checkleft-owned, pinned toolchain inputs rather than
depending on the consuming repo's application `package.json`. The
manifest-writing action can be a simple hermetic script or small helper binary.
The important contract is that the emitted TOML matches Checkleft's existing
`artifact` manifest schema exactly.

### Checkleft Side

The initial framework change should stay small:

- document the Bazel package flow,
- ship a first-class `js_check` macro for JS/TS checks,
- continue using the existing generated-index provider,
- continue using the existing wasm executor,
- document the expected wrapper-based orchestration model for CI and local use,
- optionally add a user-facing CLI flag later as a convenience alias for the
  existing env var.

No new manifest schema is required.

## Testing Expectations

Expected coverage for the Bazel packaging work:

- manifest generation writes valid `artifact` mode TOML,
- digest generation is canonical and stable,
- generated index generation rejects duplicate implementation IDs,
- generated index manifest paths resolve relative to the index directory,
- manifest `artifact_path` values are repo-root-relative,
- a small end-to-end test proves:
  - Bazel emits manifest and index,
  - Checkleft resolves `generated:<id>`,
  - Checkleft executes the packaged wasm artifact successfully.

## Migration Path

Repositories should be able to adopt this incrementally:

1. keep existing built-in checks,
2. move one repo-specific JS/TS policy into `js_check`,
3. aggregate it into the generated index,
4. point `CHECKS.toml` at `generated:<id>`,
5. add a `checkleft(...)` wrapper target that depends on the generated index,
6. switch local usage and CI to run that wrapper target.

This makes repo-specific checks portable within the repo without forcing all
repos to adopt the same language or compilation model.

## Open Questions

- Should Checkleft eventually add `--external-check-index <path>` as a public
  CLI flag, or is the existing env var plus wrappers sufficient?
- Should `js_check` also be named or aliased as `ts_check`, or is one JS/TS
  macro enough?
- Should the aggregate rule emit a single index file only, or also a directory
  artifact that groups index and manifests together for easier handoff?
- Do we want a future mode where Checkleft asks Bazel to build a named index
  target automatically, or should build orchestration remain outside Checkleft
  permanently?
- When the external runtime gains a richer host API, do we want language
  toolchain macros to target core wasm, components, or both?
