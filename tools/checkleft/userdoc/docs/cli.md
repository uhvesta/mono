# Running checks

Use the repo wrapper:

```bash
./tools/checks <subcommand> [flags]
```

The wrapper builds and runs the checks binary (preferring Bazel, falling back to Cargo).

## Subcommands

## `run`

Run configured checks for a computed change set.

```bash
./tools/checks run [--all] [--base-ref <ref>] [--format <human|json>] [--show-progress[=<bool>]] [--external-checks-url <url>]
```

Flags:

- `--all`: run checks for all tracked files.
- `--base-ref <ref>`: run checks against changes since `<ref>`.
- `--format human|json`: output format (`human` default).
- `--show-progress[=<bool>]`: show the interactive progress UI (see below). Auto-detected by default.
- `--external-checks-url <url>`: fetch an external root `CHECKS.yaml` or `CHECKS.toml` and merge it before local config resolution.

Behavior:

- Exit code is `1` if any check reports an `error` finding.
- `warning` and `info` findings do not fail the command.

### Interactive progress UI (`--show-progress`)

When `run` is attended at an interactive, color-capable terminal it shows a live
progress display: failures and warnings stream into a scrolling log area while a
pinned status block at the bottom shows one line per check —

```text
⠹ typo: checking 12 files [1s]          (in progress; spinner + elapsed)
✔ workflow/shell-strict: 12 files passed [0ms]   (done, clean)
✖ file/size: 2 files failed [123ms]     (done, with findings)
```

A check that completes faster than a short debounce never flashes a spinner — it
settles straight into its result line.

This is presentation-only: it never changes which checks run, the findings, or
the exit code. Detection follows checkleft's existing color handling:

- **On** by default only when both stdout and stderr are interactive terminals,
  color is enabled, and the run is not in CI. Honors `NO_COLOR` / `CLICOLOR`.
- **Off** by default for pipes, redirects, CI, `--format=json`, and `NO_COLOR`.
- `--show-progress=false` forces it off; the resulting output is byte-identical
  to the non-interactive path. `--show-progress` (or `--show-progress=true`)
  forces it on.

## `list`

List check IDs configured for the computed change set.

```bash
./tools/checks list [--all] [--base-ref <ref>] [--external-checks-url <url>]
```

If no checks apply, output is:

```text
No configured checks found.
```

## Common workflows

Run on current local diff:

```bash
./tools/checks run
```

Run all checks locally before large refactors:

```bash
./tools/checks run --all
```

Use base ref in CI-like flows:

```bash
./tools/checks run --base-ref main --format=json
```

## PR workflow integration

Use `./tools/create-pr` for `gh pr create` and `gh pr edit` paths.

- It runs `./tools/checks run` first.
- Emergency local bypass for this pre-PR gate:

```bash
FLUNGE_SKIP_CHECKS=1 ./tools/create-pr create ...
```

`FLUNGE_SKIP_CHECKS=1` skips the preflight check run in `tools/create-pr`; it does not disable CI checks.
