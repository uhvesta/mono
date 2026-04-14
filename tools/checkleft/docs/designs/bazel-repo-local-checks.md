# Checkleft: Bazel-Backed Repo-Local Checks

## Overview

This design adds a low-friction execution mode for repository-owned custom
checks in Bazel repos.

Instead of compiling checks to wasm, the repo defines ordinary Bazel executable
targets and `checkleft` executes those built launchers directly. The check
process receives a JSON request on stdin, runs in the repository root, and
writes a JSON findings payload on stdout.

This is intentionally a different tradeoff from `sandbox-v1` wasm checks:

- wasm is the right fit for portable, language-neutral, sandbox-oriented
  checks,
- repo-local execution is the right fit for trusted, checked-in policies where
  low authoring friction matters more than artifact portability.

For same-repo custom checks, this is a much simpler authoring story than
componentizing every language into wasm.

## Why This Exists

The current external-check design is centered on wasm:

- `source` mode currently builds JavaScript and TypeScript into wasm,
- `artifact` mode currently executes wasm artifacts,
- the long-term sandbox contract assumes deterministic host APIs and deny-by-
  default capabilities.

That is a good long-term design for portable external checks, but it creates
real complexity for repo-local policies:

- every language needs a wasm or component toolchain story,
- Bazel integration becomes a packaging problem on top of a componentization
  problem,
- debugging author-written checks becomes harder,
- the current runtime is already paying toolchain complexity before getting the
  full benefit of the richer host API contract.

For trusted code checked into the same repo, Bazel-backed local execution is a
better first step.

## Current State

Several relevant pieces already exist:

- `checkleft` already resolves external checks through file and generated-index
  providers,
- `checkleft` already passes a JSON runtime input shape of `changeset`,
  `config`, and `capabilities` to external checks,
- the generated provider already supports a repo-local index configured by
  `CHECKLEFT_EXTERNAL_CHECK_INDEX`,
- Bazel wrapper entrypoints are already the expected shape for CI in this repo.

What does not exist yet:

- a trusted repo-local execution runtime distinct from `sandbox-v1`,
- a manifest mode for repo-local executables,
- Bazel rules that wrap executable targets into `checkleft` manifests and
  generated indexes,
- policy around where repo-local execution is allowed to be configured from.

## Goals

- Make repo-owned custom checks easy to author in Bazel repos.
- Avoid requiring wasm or component tooling for same-repo policy checks.
- Reuse the existing external package resolution flow where practical.
- Avoid nested Bazel invocations from inside `checkleft`.
- Support any Bazel-produced executable target that can be launched from
  `bazel-bin`.
- Keep CI and local usage to one stable wrapper command.
- Provide small helper libraries for the stdin/stdout JSON protocol in common
  implementation languages.

## Non-Goals

- Replacing `sandbox-v1` or the wasm runtime.
- Running untrusted third-party code.
- Providing cross-repo portable artifacts.
- Enforcing fine-grained command, filesystem, or network sandboxing.
- Teaching `checkleft` how to compile arbitrary languages itself.

## Trust Model

This mode is explicitly trusted and repo-local.

A repo-local check executable:

- runs with the same user or CI identity as `checkleft`,
- can read or write files that process identity can access,
- can spawn arbitrary subprocesses,
- can use the network if the environment allows it.

That means this mode is suitable for:

- code checked into the same repository,
- policies owned by the same trust boundary as the repo itself.

It is not suitable for:

- downloaded third-party checks,
- org-wide remote check packages that should remain sandboxed,
- scenarios where capability enforcement is a hard requirement.

Because capability enforcement is not real in this mode, repo-local manifests
should reject `[capabilities]` rather than pretending to honor them.

## Proposed Runtime Model

Add a new external runtime:

- `runtime = "exec-v1"`

This runtime executes a repo-local executable using a simple stdio JSON
protocol.

### Execution Contract

`checkleft` should:

1. resolve the manifest to an executable path,
2. launch the executable with `cwd` set to the repository root,
3. write one JSON request to stdin,
4. read one JSON response from stdout,
5. treat stderr as diagnostic output only,
6. treat a non-zero exit code as a check execution error, not a finding.

The request payload should include the information repo-local checks actually
need:

```json
{
  "changeset": {
    "changed_files": ["frontend/src/app.tsx"]
  },
  "config": {
    "legacy_modules": ["api/v1", "api/legacy"]
  }
}
```

The response payload should match the existing external output shape:

```json
{
  "findings": [
    {
      "path": "frontend/src/app.tsx",
      "message": "Import from api/v1 is not allowed."
    }
  ]
}
```

Recommended process setup:

- `cwd = <repo root>`
- set `CHECKLEFT_REPO_ROOT` to the absolute repo root
- set `CHECKLEFT_CHECK_ID` to the configured external package ID

Those env vars are convenience only. The primary contract remains stdin/stdout
JSON.

### Error Handling

Execution should fail when:

- the executable cannot be launched,
- the executable exits non-zero,
- stdout is not valid JSON,
- stdout JSON does not match the findings schema.

Implementation should bound resource usage with:

- the same wall-clock timeout budget used for built-in checks,
- stdout and stderr capture limits.

## Manifest Model

This design keeps the external package manifest and provider model, but adds a
new execution mode for repo-local executables.

Suggested manifest shape:

```toml
id = "frontend-no-legacy-api"
runtime = "exec-v1"
api_version = "v1"
mode = "exec"
executable_path = "bazel-bin/checks/frontend_no_legacy_api/frontend_no_legacy_api"

[provenance]
generator = "bazel"
target = "//checks/frontend_no_legacy_api:frontend_no_legacy_api_bin"
```

Required fields:

- `id`
- `runtime = "exec-v1"`
- `api_version = "v1"`
- `mode = "exec"`
- `executable_path` (safe relative path)

Optional fields:

- `args` (static argv entries appended after the executable path)
- `[provenance]`
  - `generator`
  - `target`

Not allowed in repo-local exec mode:

- `[capabilities]`
- `artifact_path`
- `artifact_sha256`
- `language`
- `entry`
- `build_adapter`
- `sources`

This keeps the mode honest: it is an executable contract, not a sandboxed
artifact contract.

## Why Not Invoke `bazel run` Per Check

The most obvious design is to put a Bazel target in the manifest and have
`checkleft` call `bazel run` for each check.

That is not the right model.

Problems with nested `bazel run`:

- `checkleft` itself is likely to be launched from a Bazel wrapper target,
- nested Bazel invocations are slow and operationally brittle,
- repeated analysis and execution per check adds unnecessary latency,
- stdio handling gets messier because Bazel's own output sits in the middle,
- it becomes harder to reason about what exactly was built versus what was run.

The better design is:

1. Bazel builds all repo-local check executables first,
2. Bazel emits manifests and a generated index,
3. `checkleft` executes the built launchers directly from `bazel-bin`.

This keeps Bazel responsible for building and `checkleft` responsible for
running checks.

## Bazel Packaging Rules

This design needs two Bazel-facing rules or macros:

1. `local_check`
2. `check_index`

`local_check` wraps one Bazel executable target as a repo-local external check.
`check_index` aggregates many packaged checks into one generated index.

### `local_check`

Suggested attributes:

- `id`: optional external package ID; defaults to `name`
- `binary`: executable Bazel target to run
- `args`: optional static argv suffix
- `implementation_name`: optional generated implementation ID override

Suggested outputs:

- `<name>.check.toml`

Suggested behavior:

1. require an executable dependency,
2. write a repo-local exec manifest with `runtime = "exec-v1"`,
3. expose provider metadata for aggregation,
4. ensure the wrapped binary is a dependency of any consumer target.

The generated `executable_path` should point at the built launcher under
`bazel-bin/...`, not at a source file.

### `check_index`

Suggested attributes:

- `checks`: list of packaged check targets produced by `local_check`

Suggested outputs:

- `<name>.index.toml`

Suggested index shape:

```toml
version = 1

[[packages]]
implementation = "generated:frontend-no-legacy-api"
manifest = "bazel-bin/checks/frontend_no_legacy_api/frontend_no_legacy_api.check.toml"
```

Behavior should match the existing generated provider expectations:

- fail on duplicate implementation IDs,
- write manifest paths relative to the index location,
- keep the index file repo-local and deterministic.

## Example Repository Usage

The main authoring goal is that repo owners can use normal Bazel language
rules.

For example, a Python-based check:

```starlark
load("//tools/checkleft/bazel:defs.bzl", "check_index", "local_check")

package(default_visibility = ["//visibility:private"])

py_binary(
    name = "frontend_no_legacy_api_bin",
    srcs = ["check.py"],
    main = "check.py",
)

local_check(
    name = "frontend_no_legacy_api",
    binary = ":frontend_no_legacy_api_bin",
)

check_index(
    name = "check_index",
    checks = [":frontend_no_legacy_api"],
)
```

When the Bazel target name is not the desired check ID, repos can still set
`id = ...` explicitly.

The check program itself can stay simple:

```python
import json
import pathlib
import sys


def main() -> int:
    request = json.load(sys.stdin)
    root = pathlib.Path.cwd()
    findings = []

    for path in request.get("changeset", {}).get("changed_files", []):
        if not path.endswith(".tsx"):
            continue
        contents = (root / path).read_text()
        if "api/v1" in contents:
            findings.append(
                {
                    "path": path,
                    "message": "Import from api/v1 is not allowed.",
                }
            )

    json.dump({"findings": findings}, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

The same packaging flow would also work for:

- `rust_binary`
- `go_binary`
- `sh_binary`
- JavaScript launchers
- any other Bazel executable that can run correctly from `bazel-bin`

## CI and Local Usage

The recommended operational model should be a Checkleft-owned symbolic macro
named `checkleft`, not a hand-written repo-local `sh_binary`.

Because this repo is already on Bazel 8, a symbolic macro is the right tool:

- it hides the launcher implementation details,
- it gives typed attributes and better visibility semantics,
- it removes repetitive wrapper boilerplate from consuming repos,
- it matches Bazel's recommended extension style for Bazel 8+.

At the callsite, the repo should write:

```starlark
load("//tools/checkleft/bazel:defs.bzl", "checkleft")

checkleft(
    name = "run_checkleft",
    checks = [":frontend_no_legacy_api"],
)
```

For repos that want the intermediate target to stay explicit, `checkleft(...)`
should also accept:

```starlark
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

The canonical command for humans and CI becomes:

```bash
bazel run //checks:run_checkleft -- --base-ref origin/main
```

That one command:

- builds the repo-local check executables,
- builds the generated index,
- runs `checkleft`,
- lets `checkleft` execute the built binaries directly.

## Interaction with Remote Config

Repo-local execution should be allowed only for repo-owned code.

That means:

- checked-in file manifests under repo root are allowed,
- Bazel-generated manifests and indexes under repo root are allowed,
- remote HTTP-loaded manifests should not be allowed to declare
  `runtime = "exec-v1"`.

This prevents remote config from introducing arbitrary local process execution.

A practical rule is:

- if a check definition originates from `settings.external_checks_url`, resolving
  to a repo-local exec package is an error.

The trusted local-exec runtime should require explicit repo ownership.

## Relationship to Wasm

This design is a complement to, not a replacement for, `sandbox-v1`.

Use repo-local execution when:

- the check is owned by the same repo,
- low authoring friction matters most,
- Bazel already builds the language well,
- trust boundaries are not the primary concern.

Use wasm when:

- the check should be portable across repos,
- sandboxing matters,
- the check may eventually be distributed outside the repo,
- a language-neutral runtime contract is important.

The likely long-term model is:

- repo-local execution for easy same-repo custom checks,
- wasm for hardened or shareable external checks.

## Implementation Sketch

### Checkleft Side

Framework changes should be:

- add `exec-v1` runtime validation,
- add `mode = "exec"` manifest validation,
- reject `[capabilities]` for repo-local exec packages,
- add a repo-local executor that spawns the executable directly,
- set `cwd` to repo root and pass stdin JSON,
- capture stdout and stderr with limits,
- apply the same timeout budget used for built-in checks,
- disallow repo-local exec packages coming from remote external config.

The current JSON input and output structs can be reused as the wire format.
Checkleft should also provide small helper libraries for common implementation
languages so authors do not have to hand-roll stdin/stdout protocol handling.
The first cut can be intentionally small, for example:

- Python helper for reading request JSON and writing findings JSON,
- JavaScript/TypeScript helper for the same,
- Rust crate with request/response types and a simple main-loop adapter.

### Bazel Side

Add Starlark under:

```text
tools/checkleft/bazel/
  defs.bzl
```

Suggested implementation direction:

- define a provider carrying implementation ID, manifest output, and wrapped
  executable metadata,
- implement `local_check` as a manifest-writing wrapper around an executable
  target,
- implement `check_index` as an aggregate index writer,
- implement `checkleft` as a symbolic macro that expands to a private launcher
  target and exports the runnable wrapper target,
- keep visibility narrow and avoid requiring public Bazel packages.

The key contract is that the built launcher referenced by
`executable_path = "bazel-bin/..."` is directly runnable by `checkleft`.

## Testing Expectations

Expected coverage:

- manifest parsing accepts valid `exec-v1` packages,
- manifest parsing rejects `capabilities` for repo-local exec mode,
- generated index aggregation rejects duplicate implementation IDs,
- repo-local execution succeeds for a simple launcher target,
- invalid stdout JSON is surfaced as an execution error,
- non-zero exit status is surfaced with captured stderr,
- remote external config cannot introduce repo-local exec packages,
- an end-to-end Bazel test proves the wrapper command runs a packaged local
  check successfully.

## Migration Path

Repositories should be able to adopt this incrementally:

1. keep existing built-in checks,
2. add one repo-local custom check as a normal Bazel executable,
3. wrap it with `local_check`,
4. aggregate it with `check_index`,
5. point `CHECKS.toml` at `generated:<id>`,
6. switch local usage and CI to a `checkleft(...)` wrapper target.

This gives repos a low-friction custom-check path without forcing a wasm
toolchain decision up front.

## Open Questions

- Should repo-local execution be allowed only from generated indexes, or also
  from checked-in file manifests?
- Should `local_check` accept only executable targets, or also Bazel test
  targets with a small adapter?
