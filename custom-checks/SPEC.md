# Custom checks: process protocol v1

Status: proposed extension API. Existing checkleft configuration and reporting
remain supported. MUST and MAY describe required and optional behavior.

## One bundle per check

Authors distribute the CLI, metadata, runtime dependencies, and supporting files
together. Every bundle has this layout:

```text
check.yaml
check                           # fixed executable entry point
runtime/                        # packaged interpreter/libraries, if needed
constant_inputs/
  policy.json                   # check-specific configuration is an ordinary file
  shared.proto                  # other explicitly packaged validation inputs
```

`check.yaml` contains exactly these required fields:

```yaml
schema_version: 1
id: proto/compatibility
include: ["api/**/*.proto"]
```

Checkleft executes `check` directly. The bundle supplies any launcher needed for
its implementation language. Metadata does not select an executable, subcommand,
build system, or runtime mode. Packaging and installation belong to the host.

`constant_inputs/` is always present, possibly empty. Its packaged regular files
are the explicit supporting input set: no repository globs, directory discovery,
or separate configuration channel. Bundle contents are immutable and identified
by digest, including executable, dependencies, metadata, and constant inputs.

## Host and process boundaries

Checkleft owns all planning: file selection, diff generation, snapshotting, input
materialization, sandbox setup, execution, output validation, and fix application.
The process implements only `validate` and `fix` using supplied files.

The host MUST enforce these restrictions for the process and its descendants:

- Read-only access to the bundle and frozen inputs, plus the host's explicitly
  defined platform runtime. No ambient checkout, home directory, credentials,
  inherited sockets, or executables found through the host's `PATH`.
- Writes only to a fresh output directory and private scratch space.
- No network access or IPC access to services outside the sandbox.
- An explicit environment and host-enforced execution limits.

macOS uses sandbox policies to restrict filesystem, process, and network access;
these are access restrictions, not Linux-style filesystem/network namespaces.
Linux may combine namespaces and Landlock to enforce the same boundary. The host
MUST verify the required isolation is available and fail preparation otherwise.
Landlock's available restrictions depend on the kernel ABI; it is not an assumed
complete replacement for namespace isolation. See the [kernel documentation](https://docs.kernel.org/userspace-api/landlock.html).

The sandbox enforces access; the host enforces argument/output schemas. Authors
MUST produce the same results for the same inputs and declared runtime, without
depending on ambient time or randomness.

## One input layout

Every invocation receives:

```text
input/
  change.json
  diff.patch
  before/api/x.proto
  after/api/x.proto
  constant_inputs/              # read-only materialization of the bundled files
output/                        # initially empty
```

`include` uses existing checkleft glob/exclusion semantics. The host selects whole
change records, matching either path of a rename, and invokes the check once for
the selected set. Empty selection means no run. Supporting files are readable
dependencies, not additional subjects on which findings or fixes may be emitted.

`change.json` identifies the selected files:

```json
{
  "schema_version": 1,
  "files": [
    { "before_path": "api/x.proto", "after_path": "api/x.proto" },
    { "before_path": null, "after_path": "api/new.proto" },
    { "before_path": "api/gone.proto", "after_path": null },
    { "before_path": "api/old.proto", "after_path": "api/renamed.proto" }
  ]
}
```

Paths refer to the respective `before/` and `after/` roots. Null means that side
does not exist; both sides MUST NOT be null. Renames retain their original paths.
Files contain complete bytes, including binary contents. The host freezes both
sides and a consistent before-to-after patch, without user-specific diff drivers.
Missing expected content or an unavailable requested baseline is an error.

Content-only validators simply read `after/`; there is no input mode. For an
explicit full-code scan, the host supplies identical before/after files and an
empty patch. Bundle changes invalidate cached results and require revalidation of
all matching subjects. Output paths identify a subject by its `after_path`, or
`before_path` for a deletion.

## Standard invocation, including argfiles

Every bundled `check` MUST accept these two equivalent forms:

```sh
bundle/check --checkleft-protocol-version 1 --checkleft-mode validate --checkleft-input input --checkleft-output output
bundle/check --checkleft-argfile invocation.json
```

The argfile is a UTF-8 JSON argument array, with no shell expansion:

```json
[
  "--checkleft-protocol-version",
  "1",
  "--checkleft-mode",
  "validate",
  "--checkleft-input",
  "input",
  "--checkleft-output",
  "output"
]
```

All four direct flags are required exactly once. The only modes are `validate`
and `fix`. Argfile support is mandatory; it replaces all direct flags and cannot
nest. Unknown flags, versions, or modes are errors. The host MAY always use an
argfile. `--checkleft-input` names the directory with the fixed layout above.

Argument paths are relative to the invocation root. Protocol paths use `/`, MUST
be relative, and MUST NOT contain `..`. V1 rejects symlink subjects, supporting
files, and outputs. The host validates containment before reading or applying them.

## Standard findings

Write one JSON report per subject with findings to
`output/validations/<subject>.checkleft.validation`. For example, removing a field
from `api/x.proto` produces `output/validations/api/x.proto.checkleft.validation`:

```json
{
  "schema_version": 1,
  "findings": [
    {
      "severity": "error",
      "message": "Field customer_id was removed.",
      "location": { "side": "before", "line": 12 }
    }
  ]
}
```

Each finding requires `severity` (`error`, `warning`, `info`) and `message`.
Optional `location` requires a 1-based `line`; `end_line` is an optional inclusive
range end. Optional `column` is a 1-based UTF-8 byte position on the start line.
`side` defaults to `after`. Omit `location` for a whole-file finding.
Optional `fixable` defaults to `false` and indicates an available automatic fix.

A text check reading only `after/docs/a.txt` can emit all three location forms:

```json
{
  "schema_version": 1,
  "findings": [
    {
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

Every completed invocation MUST write `output/result.json`, listing its reports:

```json
{
  "schema_version": 1,
  "validations": ["validations/docs/a.txt.checkleft.validation"],
  "findings": [],
  "fixes": []
}
```

`findings` here uses the same schema without locations for whole-change findings,
e.g. `{"severity":"error","message":"A migration note is required."}`.
The three arrays default to empty; `{"schema_version":1}` is a completed clean run.
Sparse per-file reports are allowed. Missing completion output, missing or unlisted
reports, malformed documents, unknown fields, duplicate subjects, invalid locations,
and out-of-scope paths are protocol errors. Stdout/stderr are logs only.

Exit `0` means execution completed, including when findings contain errors.
Nonzero exit, timeout, or crash means execution failed. Checkleft decides pass/fail
from findings and policy. Before-side findings MUST NOT be discarded by filters
that only consider added after-side lines; use a summary when inline display is
unavailable. Each invocation has its own output root.

## Optional autofix through the same entry point

For the text example, the host invokes the same standard arguments with mode `fix`.
The process writes complete replacement bytes to `output/fixes/docs/a.txt` and emits:

```json
{ "schema_version": 1, "fixes": [{ "path": "docs/a.txt", "replacement": "fixes/docs/a.txt" }] }
```

Both modes MUST be understood. Implementing fixes is optional: a check with no
available fixes returns no replacements; the host reruns validation and retains
remaining findings. There is no capability declaration or additional discovery call.
`validate` MUST NOT emit fixes.

Only selected existing after-side files may be replaced. V1 has no create/delete/
rename fix operations. An absent replacement leaves a file unchanged; an empty
replacement empties it. The host validates the entire proposal, rejects stale
originals or unauthorized paths before writing, applies replacements, and reruns
validation against the updated inputs. Processes never modify the checkout.

Proposed user entry point: `checkleft fix --check text/style`. The host finds the
installed bundle and handles the complete invocation.

## Deferred work

Network request declarations and brokered responses need a separate concrete
design; they are not part of v1. Validators remain offline. Any future network
data must be fetched by checkleft and frozen before validator execution.

A future ReAPI backend executes the same bundled CLI, input tree, arguments, and
explicit environment/platform, returning `output/`. Packaging transport and remote
execution do not change the process contract; checkout mutation remains host-owned.
