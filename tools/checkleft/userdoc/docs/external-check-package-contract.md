# External Check Package Contract (Phase 0 Freeze)

This page freezes the initial external-check contracts for the sandboxed polyglot work.

Scope for this phase:

1. Manifest schema (`source`, `artifact`, and `exec` modes).
2. Implementation reference syntax (`generated:` and file path refs).
3. Capability policy contract shape.
4. Host API operation names and semantics.

Non-goals for this phase:

1. Runtime execution wiring.
2. Bazel rule implementation.
3. Full migration of existing built-in checks.

## Implementation Reference Contract

`CHECKS.toml` uses `implementation = ...` to point to external check packages.

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
4. `mode` (`source`, `artifact`, or `exec`)
5. Optional `[capabilities]` table for sandboxed modes only

### `source` mode fields

Required:

1. `language`
2. `entry` (safe relative path)
3. `build_adapter`

Optional:

1. `sources` (array of safe relative paths)

Not allowed in `source` mode:

1. `artifact_path`
2. `artifact_sha256`
3. `provenance`

### `artifact` mode fields

Required:

1. `artifact_path` (safe relative path)
2. `artifact_sha256`

Optional:

1. `[provenance]`
   - `generator`
   - `target`

Not allowed in `artifact` mode:

1. `language`
2. `entry`
3. `build_adapter`
4. `sources`

### `exec` mode fields

Required:

1. `runtime = "exec-v1"`
2. `executable_path` (safe relative path)

Optional:

1. `args`
2. `[provenance]`
   - `generator`
   - `target`

Not allowed in `exec` mode:

1. `[capabilities]`
2. `language`
3. `entry`
4. `build_adapter`
5. `sources`
6. `artifact_path`
7. `artifact_sha256`

## Source Build Cache

For JavaScript and TypeScript `source` mode packages using the
`javascript-component` adapter, `checkleft` keeps derived state in a per-user,
cache root:

1. `${XDG_CACHE_HOME:-$HOME/.cache}/checkleft/repos/<repo>-<repo-hash>/source-mode/artifacts/<build-hash>/check.wasm`
2. `${XDG_CACHE_HOME:-$HOME/.cache}/checkleft/toolchains/js-componentizer/toolchains/<toolchain-hash>/`

Built artifacts remain repo-scoped so unrelated repos do not share those
entries. The JS toolchain install is shared across repos when the checked-in
toolchain inputs match. The cache contents are disposable and may be removed to
force a fresh toolchain install and rebuild. Manifest-declared `artifact_path`
values remain repository-relative; the per-user cache is only used for
internally generated source-mode artifacts.

## Capability Contract

Capabilities are deny-by-default.

Current contract:

```toml
[capabilities]
commands = ["grep", "sed"]
```

Validation requirements:

1. Commands must be bare command names (not paths).
2. Commands must not contain whitespace.
3. Duplicate command names are invalid.

Runtime policy (enforced later in execution wiring):

1. Effective allowed commands = global checkleft command ceiling ∩ manifest `commands`.
2. Shell entrypoints remain hard-blocked.

`exec-v1` packages do not support capabilities. They run as trusted repo-local
executables and must omit the `[capabilities]` table entirely.

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
