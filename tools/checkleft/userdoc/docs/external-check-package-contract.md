# External Check Package Contract (Phase 0 Freeze)

This page freezes the initial external-check contracts for the sandboxed work.

Scope for this phase:

1. Manifest schema (`component` and `declarative` modes).
2. Implementation reference syntax (`generated:` and file path refs).
3. Capability policy contract shape.
4. Host API operation names and semantics.

> **Runtime tiers.** There are two external-check runtimes: `component`
> (WebAssembly Component Model, runtime tag `component-v1`) and `declarative`
> (framework-owned invocation of declared binaries + declarative transforms,
> runtime tag `declarative-v1`). The former `exec` mode / `exec-v1` runtime has
> been **folded into the declarative runtime**: a custom binary that emits a
> checkleft findings document is now expressed as a declarative invocation with
> the `passthrough` transform. The legacy `wasm` / `sandbox-v1` tier has been
> removed.

Non-goals for this phase:

1. Runtime execution wiring.
2. Bazel rule implementation.
3. Full migration of existing built-in checks.

## Implementation Reference Contract

The `CHECKS` file uses `implementation` to point to external check packages.

Supported reference forms:

1. File reference (relative path):
   - Example: `checks/workflow-shell-strict/check.toml`
2. Generated reference:
   - Prefix: `generated:`
   - Example: `generated:domain-typo-check`

Validation requirements:

1. Empty references are invalid.
2. File references must be safe relative paths (no absolute paths, no `..` traversal).
3. `generated:` references must include a non-empty ID suffix.

## Provider Contract (Phase 1)

`checkleft` resolves external packages through provider implementations:

1. File provider (always enabled): resolves file references from repo root.
2. Generated-index provider (optional): resolves `generated:` references from an index TOML.

Generated provider configuration:

1. Set `CHECKLEFT_EXTERNAL_CHECK_INDEX` to an index TOML path (relative to repo root or absolute).

Generated index shape:

```toml
[[packages]]
implementation = "generated:domain-typo-check"
manifest = "./domain_typo.check.toml"
```

Validation/diagnostics requirements:

1. Missing package for an `implementation` reference is an error.
2. Malformed package manifests are errors.
3. If multiple providers resolve the same implementation, resolution fails with a provider conflict error.

## Manifest Schema Contract

All external check package manifests are TOML.

Required common fields:

1. `id`
2. `runtime`
3. `api_version` (currently must be `v1`)
4. `mode` (`component` or `declarative`)

### `component` mode fields

`runtime = "component-v1"`. Required:

1. `artifact_path` (safe relative path to the `.wasm` artifact)
2. `artifact_sha256` (64-character lowercase hex SHA-256 digest)

Optional:

1. `[limits]`
   - `timeout_ms`
   - `max_memory_mb`
2. `checks` (list of check IDs exported by this component; used for defense-in-depth validation)
3. `[provenance]`
   - `generator`
   - `target`

Not allowed in `component` mode:

1. `executable_path`, `args`
2. `applies_to`, `needs`, `invocations` (declarative-only)

### `declarative` mode fields

`runtime = "declarative-v1"`. The framework selects files, resolves declared
binaries, runs declared invocations, and applies declared transforms. Required:

1. `applies_to` — non-empty list of file globs the check applies to.
2. `needs` — at least one declared binary, each with a `default` binding
   (`{ bazel = "<label>" }` or `{ path = "<path-or-name>" }`).
3. `invocations` — at least one invocation, each with `id`, `run` (a declared
   binary), `mode` (`batch` | `per_file`), templated `args`, an `exit` map
   (codes → `ok` | `findings` | `error`, plus a required `default`), and a
   `transform`.

Transform strategies:

- `json` — a `select` (jq subset) locates issue rows and a `finding` map projects
  each into a finding.
- `passthrough` — the binary already emits a checkleft findings document
  (`{"findings":[…]}`) on stdout; it is returned unchanged. This is how the former
  `exec` tier is expressed. `passthrough` must not set `select` or `finding`.

Not allowed in `declarative` mode:

1. `artifact_path`, `artifact_sha256`
2. `executable_path`, `args` (top-level), `[provenance]`

## Host API Contract Surface (Names Frozen)

External checks use host calls with these operation names:

1. `changeset.list_changed_files()`
2. `tree.read_file(path)`
3. `tree.exists(path)`
4. `tree.list_dir(path)`
5. `tree.glob(pattern)`
6. `changeset.bypass_reason(name)`

Semantics:

1. Source-tree paths are repository-relative and validated.
2. Host operations are deterministic for a fixed input.
3. No ambient network or filesystem access is implied by the API.

## Code Stubs

Phase 0 schema stubs and validators live in:

1. `tools/checkleft/src/external/mod.rs`

These stubs define the frozen contract types and parsing/validation behavior without integrating runtime execution yet.
