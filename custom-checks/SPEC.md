# Custom checks: process protocol v1

Status: proposed extension API; these examples are not implemented configuration.
Existing checkleft checks, configuration inheritance, and reporting remain supported.
MUST and MAY describe required and optional behavior.

## Boundaries

| Owner             | Responsibility                                                                                                                                                                |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Author            | Declare file selection, a CLI, explicit supporting files, optional network requests, and supported modes; implement validation and optional fixes in any language.            |
| Checkleft host    | Select files, freeze inputs, resolve the complete tool runtime, perform declared requests, enforce isolation, validate outputs, apply policy, and apply user-requested fixes. |
| Validator process | Read supplied inputs; write structured outputs. No repository discovery, ambient filesystem access, credentials, network, or direct checkout writes.                          |

An invocation MUST have immutable inputs, an explicit environment, private scratch
space, and a fresh writable output directory. The host MUST enforce filesystem and
network isolation; a temporary working directory alone is insufficient. Undeclared
environment, time, and randomness MUST NOT influence results. Tool identity includes
its interpreter, libraries, and data dependencies; a host `PATH` name is insufficient.

## Declaration

This example declares every author-facing feature. `tool.bazel` resolves a packaged
executable and its runtime dependencies; `prefix` is an optional fixed subcommand.
The host constructs all remaining arguments. Check-specific options belong in `config`.

```yaml
schema_version: 1
id: proto/compatibility
include: ["api/**/*.proto"]
input_mode: transition
tool:
  bazel: //tools/checks/proto:validator
  prefix: [checkleft]
capabilities: [validate, plan, fix]
constants: [checks/proto/policy.json]
config: { allow_field_removal: false }
network_requests:
  - id: registry
    operation: schema_registry.lookup
    required: true
```

`validate` is mandatory; `plan` and `fix` are optional. `include` uses checkleft's
framework globs and exclusions, not check-specific selection logic. Match either
path of a rename and retain the complete change record. The host invokes once per
selected configuration group, without arbitrary batching; no matches means no run.
`prefix`, `constants`, and `network_requests` default to `[]`; `config` defaults to
`{}`. Other declaration fields are required; `include` MUST be nonempty.

`constants` MUST enumerate concrete repository-relative regular files, never globs
or directories. Their frozen current bytes are available on every invocation.
Missing files fail preparation. Other source dependencies must also be explicitly
enumerated; the validator cannot discover and read undeclared imports. Tool,
configuration, constant, and network-response changes invalidate cached results.
Changes to tool/configuration/constants require revalidation of affected matching
subjects, including unchanged files; the host supplies identical before/after
versions with `kind: unchanged` for unchanged subjects in transition mode.

## Inputs and invocation

Each invocation has this layout; source paths retain their repository structure:

```text
input/change.json
input/diff.patch
input/before/api/x.proto
input/after/api/x.proto
input/constants/checks/proto/policy.json
input/metadata.json
tool/...
output/result.json
output/validations/api/x.proto.checkleft.validation
output/fixes/api/x.proto
```

The host freezes the patch and both snapshots consistently before execution.
The patch represents before-to-after changes for the selected subjects, without
user-specific diff drivers. Complete file bytes are authoritative, including binary
files. Missing required content is an error, not an empty file or a clean result.

```json
{
  "schema_version": 1,
  "check_id": "proto/compatibility",
  "input_mode": "transition",
  "config": { "allow_field_removal": false },
  "files": [
    { "kind": "modified", "before_path": "api/x.proto", "after_path": "api/x.proto" },
    { "kind": "added", "before_path": null, "after_path": "api/new.proto" },
    { "kind": "deleted", "before_path": "api/gone.proto", "after_path": null },
    { "kind": "renamed", "before_path": "api/old.proto", "after_path": "api/renamed.proto" }
  ]
}
```

Paths in `files` are relative to their respective `before/` and `after/` roots.
Only existing sides are materialized. A subject's report path is `after_path`, or
`before_path` for a deletion. Snapshot checks instead receive `input_mode: snapshot`,
`kind: present`, null `before_path`, and populated `after_path`; `before/` is empty
and `diff.patch` is absent. Transition checks MUST fail if a baseline is unavailable.

The resolved CLI MUST support both equivalent forms below, after its fixed prefix:

```sh
proto-validator checkleft --checkleft-protocol-version 1 --checkleft-mode validate --checkleft-input input/change.json --checkleft-output output
proto-validator checkleft --checkleft-argfile invocation.json
```

`invocation.json` is a UTF-8 JSON argument array, with no shell expansion:

```json
[
  "--checkleft-protocol-version",
  "1",
  "--checkleft-mode",
  "validate",
  "--checkleft-input",
  "input/change.json",
  "--checkleft-output",
  "output"
]
```

All four direct flags are required exactly once. Modes are `plan`, `validate`, and
`fix`; unsupported modes/versions/flags are errors. Argfile mode replaces all direct
flags; mixed forms and nested argfiles are forbidden. Argfile support is mandatory,
and the host MAY always use it. File lists and configuration live in `change.json`.
Argument paths are relative to the invocation root; input siblings are relative to
`change.json`'s directory. Protocol paths use `/`, MUST be relative, and MUST NOT
contain `..` or escape through symlinks. V1 rejects symlink subjects and outputs.

## Declared network inputs: paired planning example

The host owns a registry of read-only operations defining endpoint, parameter
schema/constraints, authentication, and response format. A check declares operation
IDs, never arbitrary URLs or credentials. Transport details are outside this API.
Declarations with `params` are fetched directly. A declaration without `params`
requires `plan`, which reads the same frozen inputs with empty network metadata.

For the declaration above, running the standard invocation with mode `plan` writes:

```json
{
  "schema_version": 1,
  "requests": [{ "id": "registry", "params": { "schema": "example.Message" } }]
}
```

This is `output/result.json`. Each ID MUST match a declaration and occur at most
once. A planner cannot override an operation or its constraints. Omitted dynamic
requests are skipped. There are no recursive planning/fetch rounds in v1.
Alternatively, declaring `params: {schema: example.Message}` needs no planner.

The host executes requests before validation/fixing and supplies `input/metadata.json`:

```json
{
  "schema_version": 1,
  "responses": { "registry": { "status": "ok", "value": { "required_fields": ["customer_id"] } } }
}
```

An optional failed request becomes `{"status":"error","message":"unavailable"}`;
an omitted dynamic request becomes `{"status":"skipped"}`. A required requested
operation failing aborts preparation. No requests means an empty `responses` object.
Fetch freshness is host policy; consumed response bytes are frozen and included in
execution identity. Replays reuse those bytes. Credentials never enter the bundle.

## Findings: paired transition and snapshot examples

The validator writes one JSON document per subject with findings, under
`output/validations/<subject>.checkleft.validation`. For example, removing
`customer_id` from `api/x.proto` produces:

```json
{
  "schema_version": 1,
  "findings": [
    {
      "code": "field-removed",
      "severity": "error",
      "message": "Field customer_id was removed.",
      "location": { "side": "before", "line": 12 },
      "remediations": ["Restore the required field."],
      "fixable": false
    }
  ]
}
```

`severity` (`error`, `warning`, `info`) and `message` are required. `code` is an
optional stable rule identifier; `remediations` defaults to `[]`, and `fixable` to
`false`. `fixable: true` requires the declared `fix` capability.
Optional `location` has a required 1-based `line`, optional inclusive
`end_line` (default: `line`), optional 1-based UTF-8 byte `column` on the start line,
and `side` (default: `after`). Coordinates MUST refer to the identified snapshot.
Omit `location` for a whole-file finding. Before-side findings MUST survive policy
filtering based solely on added after-side lines; unavailable inline presentation
falls back to a file/check summary.

A snapshot-only text validator uses the same contract:

```yaml
schema_version: 1
id: text/style
include: ["docs/**/*.txt"]
input_mode: snapshot
tool: { bazel: "//tools/checks/text:validator" }
capabilities: [validate, fix]
```

Its input contains `{"kind":"present","before_path":null,"after_path":"docs/a.txt"}`.
For trailing whitespace and optional guidance, it emits:

```json
{
  "schema_version": 1,
  "findings": [
    {
      "code": "trailing-space",
      "severity": "warning",
      "message": "Remove trailing whitespace.",
      "location": { "line": 2, "column": 5 },
      "fixable": true
    },
    {
      "severity": "info",
      "message": "Review this paragraph.",
      "location": { "line": 3, "end_line": 4 }
    },
    { "severity": "info", "message": "Prefer a shorter file." }
  ]
}
```

This report is `output/validations/docs/a.txt.checkleft.validation`. Sparse reports
are allowed. Every successfully completed invocation MUST write `output/result.json`:

```json
{
  "schema_version": 1,
  "validations": ["validations/docs/a.txt.checkleft.validation"],
  "findings": [],
  "fixes": []
}
```

Here `findings` holds locationless findings about the whole invocation, using the
same schema, e.g. `{"severity":"error","message":"Changes require a migration note."}`.
A clean run writes `{"schema_version":1,"validations":[]}`.
Absent arrays default to empty. Plan mode may emit only `requests`; validate mode
may emit `validations` and `findings`; fix mode may additionally emit `fixes`.
Unknown fields are protocol errors in v1. Unlisted reports, missing listed files,
out-of-scope subjects, malformed JSON, and missing completion output are errors.

Exit `0` means execution completed, even with error findings; nonzero, timeout, or
crash means execution failure. The host decides pass/fail from findings and policy.
Stdout/stderr are logs, never findings. Each invocation gets its own output root.

## Optional fixes: paired proposal and application

For the text example, mode `fix` writes complete replacement bytes to
`output/fixes/docs/a.txt`, then reports:

```json
{ "schema_version": 1, "fixes": [{ "path": "docs/a.txt", "replacement": "fixes/docs/a.txt" }] }
```

Only selected existing after-side files may be replaced; v1 has no create, delete,
or rename fix operations. Unlisted files remain unchanged; an empty replacement
means an empty file. Validators MUST NOT write snapshots or the checkout. The host
validates the entire proposal and rejects stale originals or unauthorized paths
before applying replacements, then reruns validation with the same network inputs.
Fix mode may leave unfixable findings; it MUST NOT claim those were resolved.

Proposed user entry point: `checkleft fix --check text/style`. The registered check
ID retains configuration, selection, tool identity, and declared dependencies.

## Execution backend boundary

Local and eventual ReAPI backends consume the same frozen input/tool tree, argument
vector, explicit environment/platform, and declared `output/` tree. ReAPI transport
is deferred; it does not change the validator protocol. Network fetching and
checkout mutation remain host operations outside the remotely executable action.
