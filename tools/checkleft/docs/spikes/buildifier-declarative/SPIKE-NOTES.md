# Spike: buildifier as a fully declarative (zero-code) external check

**Status:** sanctioned throwaway spike. Prototype quality. Sibling to T1397's wasm spike (#1376), a *separate* PR. The built-in `BuildifierCheck` is left in place for direct comparison.

## What this proves

buildifier can run as a **fully declarative** external check: no wasm module, no shipped binary, **zero check-authored code**. The entire check is a package manifest. The checkleft *framework* — not a sandboxed guest — owns binary resolution and invocation. This validates the **declarative tier** the design discussion concluded is the right shape for tool-wrapper checks, and confirms T1397's conclusion that buildifier is the wrong fit for wasm (a command-capable wasm check is effectively unsandboxed; the framework should own invocation).

Three external-check tiers now exist in the model:

| tier | runtime tag | who runs the work | this spike |
| --- | --- | --- | --- |
| declarative | `declarative-v1` | framework resolves + runs declared binaries | **built here** |
| wasm/artifact | `sandbox-v1` | sandboxed wasm guest (pure computation) | unchanged |
| exec | `exec-v1` | ship + run a binary | unchanged |

A declarative check decomposes into: **select files → resolve declared binaries → run declared invocations → apply declared transforms → emit Findings.** buildifier needs all of it except real computation, so it collapses to pure declaration.

## The manifest schema I settled on

The definition lives in a **package manifest, not in CHECKS.yaml**. It reuses the existing `load_external_check_package_manifest` infrastructure: a new `mode = "declarative"` alongside `artifact`/`exec`, validated into `ExternalCheckPackageImplementation::Declarative(...)`. Declarative manifests are **YAML** (distinct from the TOML used by `artifact`/`exec` manifests) — the schema is richer (invocations/transforms) and reads more naturally as YAML, which also matches the design discussion's field-for-field sketch and `CHECKS.yaml` itself. The committed definition lives at **`tools/checkleft/checks/buildifier/check.yaml`**; the parity tests source from it directly via `include_str!` so the test and the shipped file cannot drift.

```yaml
id: buildifier-declarative
mode: declarative
runtime: declarative-v1
api_version: v1
applies_to:
  - "**/BUILD"
  - "**/BUILD.bazel"
  - "**/*.bzl"
  - "**/*.star"
  - "**/WORKSPACE"
  - "**/WORKSPACE.bazel"
  - "**/MODULE.bazel"

needs:
  buildifier:
    default:
      bazel: "@buildifier_prebuilt//:buildifier"

invocations:
  - id: format
    run: buildifier
    mode: batch
    args: ["--mode=check", "--format=json", "{{files}}"]
    exit:
      "0": findings
      default: error
    transform:
      kind: json
      select: ".files[] | select(.formatted == false)"
      finding:
        path: "{{item.filename}}"
        message: "file needs buildifier formatting"
        severity: warning
        remediations:
          - "Run `buildifier {{item.filename}}` to auto-format."

  - id: lint
    run: buildifier
    mode: per_file
    args: ["--mode=check", "--lint=warn", "--format=json", "{{file}}"]
    exit:
      "0": findings
      default: error
    transform:
      kind: json
      select: ".files[].warnings[]"
      finding:
        path: "{{input.file}}"
        line: "{{item.start.line}}"
        column: "{{item.start.column}}"
        message: "{{item.category}}: {{item.message}}"
        severity: warning
        remediations:
          - "Run `buildifier --lint=fix {{input.file}}` to auto-fix, or resolve manually."
```

CHECKS.yaml stays thin — enable by id + repo-specific overrides only (this spike does **not** enable it in the repo's real CHECKS.yaml; doing so would break dogfooding CI). A commented example sits in `tools/checkleft/CHECKS.yaml`:

```yaml
checks:
  - id: buildifier
    check: buildifier-declarative
    implementation: tools/checkleft/checks/buildifier/check.yaml
    config:                             # optional repo-specific override
      needs:
        buildifier:
          path: /usr/local/bin/buildifier   # force the portable resolver
```

Code: `src/external/declarative/` (`mod.rs` schema + validation, `selector.rs`, `template.rs`, `transform.rs`, `resolve.rs`, `executor.rs`), plus the `mode`/runtime/implementation wiring in `src/external/mod.rs` (including `parse_declarative_check_manifest` — a YAML-based parser separate from the existing TOML parser), the executor arm in `src/external/runtime.rs`, and the URL-origin guard in `src/runner.rs`.

## How invocation + exit-semantics + the projection DSL are modeled

**Invocation orchestration** (`executor.rs`). An ordered list of self-contained invocation specs. Each carries: which declared binary (`run`), a `mode` (`batch` | `per_file`), templated `args`, exit semantics, and a transform. Findings from all invocations concatenate in order.

- `batch` mode: the standalone `{{files}}` arg expands to the N matched files; one process run.
- `per_file` mode: `{{file}}` is substituted per file; one process run per file.
- File selection is a globset over the changeset's non-deleted changed files (`applies_to`).

**Exit semantics** are explicit and load-bearing: a map of exit code → outcome (`ok` | `findings` | `error`) plus a **required** `default`. `ok` short-circuits to no findings; `findings` runs the transform over stdout; `error` aborts the whole check with an error (surfaced by the runner as a check error). The required `default` is what guarantees a crashing tool can never masquerade as "clean".

> **Empirical correction to the design sketch.** The sketch assumed `exit = { "0" = ok, "4" = findings }` (format) and `"5" = findings` (lint). **That is wrong for `--format=json`.** buildifier 7.3.1 with `--format=json` **always exits 0** — formatting/lint state is reported *in the JSON body* (`formatted`, `warnings[]`), not via the exit code. The built-in `BuildifierCheck` already relies on this: it ignores the exit code entirely and reads the JSON. So the correct, load-bearing semantics are `{ "0" = "findings", default = "error" }`: exit 0 ⇒ run the transform (which naturally yields zero findings for clean output); any nonzero ⇒ the tool crashed ⇒ check error. The `ExitOutcome::Ok` variant is kept in the model for tools whose exit code *does* signal cleanliness, but buildifier doesn't use it. This is exactly why exit semantics must be declared per-invocation rather than hard-coded.

**The projection DSL** (`transform.rs` + `selector.rs` + `template.rs`). Each invocation's `(stdout, exit_code, file-it-ran-on)` projects to `Vec<Finding>` via the `json` strategy:

- `select`: a jq-subset that locates the issue rows. Implemented: `.field`, `[]` (iterate array), and `select(.path == literal)` (literal = bool/int/null/quoted-string), joined by `|`. Enough for buildifier's `.files[] | select(.formatted == false)` and `.files[].warnings[]`.
- `finding`: a field map projecting each row → a `Finding`. Each field is a template of literal text + refs; `severity` is a literal. Three ref kinds:
  - **item refs** `{{item.start.line}}` — navigate into the selected JSON row.
  - **invocation-context refs** `{{input.file}}`, `{{exit_code}}` — *not* from stdout.
  - literal text anywhere around refs.
- `line`/`column` are optional, so **findings may be line-less** — the format pass emits exactly that (the file isn't clean, but there is no single offending line). The `Finding`/`Location` model already allows `line: None`.

## Where the `json` DSL strained

1. **The path is not always in the row (invocation context is mandatory, not optional).** buildifier's lint `--format=json` output puts warnings under `.files[].warnings[]`, but **the warning objects carry no `filename`** (confirmed against real 7.3.1 output). Once `.files[].warnings[]` flattens away the parent file, the row has no path. A one-dimensional flatten-to-leaf selector simply cannot recover it. Two ways out:
   - **per-file mode + `{{input.file}}`** (what I did): run lint once per file, so the path comes from invocation context, not stdout. This also mirrors the built-in, which runs per-file and uses the path it ran on. This is precisely the subtlety the task flagged: *the transform input includes invocation context, not just stdout.*
   - jq variable binding (`.files[] as $f | $f.warnings[]` with `path: {{$f.filename}}`) — real jq solves it, but that's beyond a minimal subset and is a natural place to stop and reach for a richer transform.

   The format pass keeps the file objects (`.files[]`), which *do* carry `filename`, so it stays in **batch mode** with `{{item.filename}}`. The spike manifest deliberately uses both modes to exercise both context-ref kinds.

2. **There is no `Finding.rule`/`category` slot.** The design sketch's `finding.rule: "{{item.category}}"` has nowhere to land — the `Finding` model is `{severity, message, location, remediations, suggested_fix}`. The built-in folds the category into the message (`"{category}: {message}"`), so the declarative manifest does the same via `message = "{{item.category}}: {{item.message}}"`. Real adoption would either extend `Finding` with a `rule` field or keep the fold convention.

3. **`select` literal/operator coverage is intentionally tiny.** Only `==` against bool/int/null/string. No `!=`, `<`, `and/or`, arithmetic, or object construction. That covers buildifier; anything past it is the seam below.

### The seam to the wasm pure-function transform tier

`Transform` is an enum (`transform.rs`); `regex` and `sarif` are reserved variants that the manifest validator explicitly rejects today ("reserved for a future spike"). All three strategies share one signature — `apply(stdout, exit_code, input_file) -> Vec<Finding>` — so they slot in without touching the executor:

- **`regex`**: line-oriented tools (no JSON). Parse stdout lines, capture groups → finding fields. Same context refs apply.
- **`sarif`**: map SARIF `runs[].results[]` (a fixed, richer shape) to findings.
- **computed transforms**: anything needing real logic — cross-referencing rows, deriving severities from thresholds, jq variable binding, de-duplication — is where a *declarative* projection stops being expressible. That is the boundary where the **wasm pure-function transform** earns its place: a sandboxed `(stdout, context) -> Vec<Finding>` pure function, with the framework still owning the (unsandboxed) command invocation. So the capability split the design discussion proposed holds up: **framework owns invocation; declarative DSL handles the easy projections; wasm handles computed projections.**

## Binary resolution (framework-owned) + bazel-vs-standalone conditionality

Resolution is owned by the framework (`resolve.rs`), not by any guest. A declared `needs` entry resolves via one of two resolvers, with an optional CHECKS-config override:

- **`path`** — a direct path or PATH name, used as-is. The **portable fallback**: standalone checkleft (no Bazel workspace) always has this.
- **`bazel`** — a Bazel label, built (`bazel build`) then resolved to its executable (`bazel cquery --output=starlark`). **Environment-conditional**: it requires a Bazel workspace, so it works in-repo but not in standalone checkleft. It **reuses the exact resolver the built-in buildifier check uses** (`checks::buildifier::resolve_bazel_target_executable`, promoted to `pub(crate)`), which is concrete evidence the framework can own what the built-in hand-rolled. Default binding for buildifier = `@buildifier_prebuilt//:buildifier`, matching the built-in.

The override (`config.needs.<name>.{path|bazel}`) is how a repo without bazel, or a CI image with a vendored buildifier, points at a concrete binary while the manifest's definition stays put — CHECKS.yaml carries only the override.

Sandboxing the invocation is **out of scope** (deferred by design). The framework runs the resolved binary directly at the repo root, exactly as the built-in and exec tiers do.

## Honest parity result

**Yes — buildifier ran end-to-end through the declarative pipeline**, and parity with the built-in holds at two levels.

1. **Transform-level parity (deterministic, runs in `bazel test`).** Real captured buildifier 7.3.1 `--format=json` output (a format-unformatted file, a clean file, and the spike fixture's 3 lint warnings) is fed through *both* the built-in parsers (`parse_format_output` / `parse_lint_output`) and the declarative `json` transform using the manifest's `select` + `finding` map. The resulting `Vec<Finding>` are asserted **equal** — same severity, message, location (path/line/column), and remediations. See `format_transform_matches_builtin_*` and `lint_transform_matches_builtin_*` in `src/external/declarative/tests.rs`. The tests source the manifest directly from `tools/checkleft/checks/buildifier/check.yaml` via `include_str!` so the test and the shipped definition cannot drift.

2. **Real end-to-end run (gated, run manually this session).** Two tests behind `CHECKLEFT_SPIKE_E2E=1`:
   - `e2e_bazel_resolver_resolves_buildifier` — the framework's bazel resolver resolves `@buildifier_prebuilt//:buildifier` to an existing executable.
   - `e2e_declarative_runs_buildifier_end_to_end` — the full pipeline (file selection → binary resolution → batch + per-file invocations → exit semantics → JSON transform) runs the **real bazel-resolved buildifier** over the fixture, producing 3 lint findings (format clean), matching the built-in's parse of the same output.

   Both pass (`2 passed` under `CHECKLEFT_SPIKE_E2E=1 cargo test ... e2e`). They are gated because the hermetic `bazel test` sandbox has no buildifier; this is requiring an external tool, not skipping a failing assertion.

   The one gap worth naming: the deterministic CI parity is at the transform level (CI can't reach buildifier); the *binary execution* parity is demonstrated by the gated tests, run by hand. A future productionization would wire buildifier as a test `data` dep to make the e2e hermetic.

### Notable fixture finding

`tests/fixtures/buildifier/malformed.bzl.fixture` is **format-clean** under buildifier 7.3.1 (its comment claims formatting issues; the tool disagrees). Its real lint warnings are `module-docstring` + `unused-variable` (line 11) and `no-effect` (line 12) — not the `function-docstring` its comment predicts. The parity tests use the *actual* captured output, not the fixture's comments.

## Recommendation

For tool-wrapper checks (buildifier is the forcing example), the **declarative tier is the right default**: the entire check is data, the framework owns resolution + invocation, and the only real code is the shared DSL — which a tool like buildifier exercises end-to-end with zero check-authored logic. Reach for wasm only when the projection needs computation the declarative DSL can't express (the seam above). Reach for exec only when you must ship a bespoke binary.
