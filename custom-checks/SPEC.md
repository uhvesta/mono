# Custom checks: process protocol v1

Proposed extension API. Existing checkleft configuration and reporting still apply.

## One bundle per check

Bundle the CLI and its runtime dependencies with this metadata:

```text
check.toml
check       # fixed executable entry point
```

`check.toml`:

```toml
schema_version = 1
id = "proto/compatibility"
include = ["api/**/*.proto"]
always_read = ["proto/validator.toml"]
network = ["https://google.com/some/endpoint"]
```

Checkleft runs `check` directly. The bundle includes any launcher it needs.

- `schema_version`, `id`, and a nonempty `include` are required.
- `include` selects files using checkleft globs and exclusions; `**` matches all.
- `always_read` lists exact repository-relative files to copy into every run,
  such as configs or shared schemas. No globs or directories. Missing files fail
  preparation. These files come from the repository, not the bundle.
- `network` lists exact HTTPS URLs the process may request. Redirect targets must
  also be listed. Omitted or empty means no network.

`always_read` and `network` default to `[]`. The bundle is immutable and identified
by digest. Configuration is an ordinary `always_read` file.

## Host and process boundaries

Checkleft selects files, builds the input tree, runs the sandbox, reads results,
and applies fixes. The CLI only validates or proposes fixes.

The host restricts the process and its children to:

- Read-only bundle, inputs, and a defined platform runtime.
- Writes to fresh output and scratch directories only.
- Declared network URLs only; no other external IPC or inherited sockets.
- A fixed environment and execution limits; no host `PATH`, home, or credentials.

Checkleft will use macOS sandbox policies or Linux namespaces/Landlock. Enforce network rules at
the URL level, not just host/port; block bypasses, it only isolates which networks the client reaches out to. Fail
preparation if the required restrictions cannot be enforced.

All checks must be reproducible.

## One input layout

Every invocation receives:

```text
input/
  change.json
  diff.patch
  before/api/x.proto
  after/api/x.proto
  always_read/proto/validator.toml
output/                           # initially empty
```

Run once for the selected files; skip an empty selection. Match either path of a
rename or copy. `always_read` files cannot receive findings or fixes unless also selected.

`change.json` lists the selected entries from the same Git diff as `diff.patch`.
It is a JSON array with no version field:

```json
[
  { "status": "M", "before_path": "api/x.proto", "after_path": "api/x.proto" },
  { "status": "A", "before_path": null, "after_path": "api/new.proto" },
  { "status": "D", "before_path": "api/gone.proto", "after_path": null },
  { "status": "R", "before_path": "api/old.proto", "after_path": "api/renamed.proto" },
  { "status": "C", "before_path": "api/source.proto", "after_path": "api/copied.proto" }
]
```

`status` uses [Git's status letters](https://git-scm.com/docs/git-diff#_raw_output_format):
`A` added, `D` deleted, `M` modified, `R` renamed, `C` copied, `T` type changed,
without similarity scores. Include binary and mode-only changes too; `diff.patch`
contains the hunks and Git headers. Unmerged files fail preparation.

Paths are relative to `before/` and `after/`. Null marks an absent side: additions
only have after content; deletions only have before content. Both paths are required
for other statuses. Freeze complete file bytes, current `always_read` files, and
the matching before-to-after patch. Missing content or a requested baseline is an error.

Content-only checks read `after/`. A full-code scan supplies identical before/after
files with `status: null` and an empty patch. Bundle or `always_read` changes rerun all matching files
and invalidate cached results. Identify each result by `after_path`, or `before_path`
for deletions.

## Standard invocation, including argfiles

Every `check` accepts both forms:

```sh
check --checkleft-protocol-version 1 --checkleft-mode validate --checkleft-input input --checkleft-output output
check --checkleft-argfile invocation.args
```

`invocation.args` is UTF-8, one literal argument per line. No JSON or shell parsing;
spaces, quotes, and backslashes are literal. Accept LF or CRLF and an optional final
newline. Arguments cannot contain newlines.

```text
--checkleft-protocol-version
1
--checkleft-mode
validate
--checkleft-input
input
--checkleft-output
output
```

The four flags appear exactly once, directly or in the argfile. Modes are `validate`
and `fix`. Reject unknown flags/versions/modes, mixed forms, and nested argfiles.
`--checkleft-input` names the input directory. The host may always use an argfile.

Argument paths are relative to the run directory. File paths use `/` and cannot
be absolute or contain `..`. Reject symlink inputs and outputs; validate containment.

## Standard findings

Write findings to `output/validations/<file>.checkleft.validation`.
For a removed field in `api/x.proto`:

```json
{
  "schema_version": 1,
  "findings": [
    {
      "severity": "blocking",
      "message": "Field customer_id was removed.",
      "location": { "side": "before", "line": 12 }
    }
  ]
}
```

Require `severity` and `message`. The only severities are:

- `blocking`: fails validation.
- `shadow`: experimental finding collected as telemetry; never fails validation.

Optional fields:

- `location`: 1-based `line`, optional inclusive `end_line`, optional 1-based UTF-8
  byte `column` on the start line, and `side` (default `after`). Omit for a whole file.
- `fixable`: whether an autofix exists; defaults to `false`.

A report for `docs/a.txt` can use a column, line range, or whole-file location:

```json
{
  "schema_version": 1,
  "findings": [
    {
      "severity": "blocking",
      "message": "Remove trailing whitespace.",
      "location": { "line": 2, "column": 5 },
      "fixable": true
    },
    {
      "severity": "shadow",
      "message": "Review this paragraph.",
      "location": { "line": 3, "end_line": 4 }
    },
    { "severity": "shadow", "message": "Prefer a shorter file." }
  ]
}
```

Every completed run writes `output/result.json`:

```json
{
  "schema_version": 1,
  "validations": ["validations/docs/a.txt.checkleft.validation"],
  "findings": [],
  "fixes": []
}
```

Report and replacement paths are relative to `output/`.
`findings` here is for whole-change findings without locations, e.g.
`{"severity":"blocking","message":"A migration note is required."}`.
Arrays default to empty: `{"schema_version":1}` means a clean run. Files without
findings need no report. Reject missing/unlisted reports, missing `result.json`,
bad schemas or locations, duplicate files, and out-of-scope paths.

Exit `0` means completed, even with blocking findings. Nonzero, timeout, or crash means
execution failed. Checkleft fails validation if any finding is `blocking`; a run with
only `shadow` findings passes. Stdout/stderr are logs. Preserve
before-side findings through added-line filters; display a summary if needed.

## Optional autofix through the same entry point

Use the same arguments with mode `fix`. Write replacement bytes to
`output/fixes/docs/a.txt` and list them in `output/result.json`:

```json
{ "schema_version": 1, "fixes": [{ "path": "docs/a.txt", "replacement": "fixes/docs/a.txt" }] }
```

Both modes must work; a check without autofixes returns no replacements.
`validate` cannot emit fixes. Only replace selected existing after-side files;
no creates, deletes, or renames. Unlisted files stay unchanged; empty bytes empty a file.

Checkleft validates all fixes, rejects changed originals or unauthorized paths
before writing, applies replacements, then reruns validation. The CLI never writes
the checkout. Remaining findings still apply.

Proposed command: `checkleft fix --check text/style`.
