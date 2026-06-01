# checkleft

Status: experimental / under active development. Not yet recommended for
general use. The CLI behavior, built-in checks, and library API may change
without notice.

`checkleft` is a repository convention checker. It runs built-in and external
checks against the files in a source tree and reports findings as human-readable
output or JSON.

## Install

```bash
cargo install checkleft
```

## Usage

Run from the root of a Git or Jujutsu repository:

```bash
checkleft run
checkleft run --verbose
checkleft run --all
checkleft run --external-checks-file /path/to/shared/CHECKS.yaml
checkleft run --external-checks-url https://example.com/CHECKS.yaml
checkleft list
```

### Change detection is automatic

`checkleft run` (no flags) detects which files changed on its own. It
classifies the environment — PR build, merge-queue build, push-to-main, or
local branch — and computes the correct base commit without any help from the
caller. **No SHA or base-ref plumbing is needed in the CI step or repo
integration.**

For a repo using Buildkite or a similar CI system the entire CI step is:

```bash
bin/checkleft run
```

The flags below exist as **escape hatches** for unusual situations:

| Flag | When to use |
|------|------------|
| `--base-ref=<sha>` | Override the auto-detected base (e.g. a custom merge strategy that produces a non-standard HEAD layout). |
| `--all` | Scan the entire repository regardless of what changed. Manual use only — catching and fixing pre-existing violations that per-diff runs would miss. Never run `--all` automatically in CI. |
| `--default-branch=<name>` | Tell checkleft the default branch name when it differs from `main` (e.g. `master`, `trunk`). |

`checkleft` looks for `CHECKS.yaml` or `CHECKS.toml` files from the repository
root down to the file being evaluated.

The root config can also set `settings.external_checks_url` to merge an
externally hosted root config before applying local root and child overrides.
The CLI flag `--external-checks-url` provides the same behavior for repos that
do not yet have a root config file.

## Minimal config

```yaml
checks:
  - id: typo
    check: typo
```

## Stale-exclusion auditing

Exclusions in `CHECKS.toml` (e.g. a check's `exclude_structs` / `exclude_files`
list) are easy to add and easy to forget. Once the reason an exclusion existed
goes away — the excluded struct gains a builder, a referenced file is deleted —
the entry becomes dead weight that quietly weakens coverage.

`checkleft` audits for this. When a file an exclusion depends on changes in the
diff, the owning check re-evaluates that exclusion as if it were not present; if
the rule now passes without it, `checkleft` reports a finding **on the
`CHECKS.toml` entry itself** telling you the exclusion can be removed. The audit
is diff-gated (it only re-evaluates exclusions whose dependencies changed) and
fails safe (an exclusion whose dependency can't be pinned to concrete files is
never flagged).

Severity is configurable, globally via `[settings]` and per-check via
`[checks.policy]`. The default is `warning`; set `error` to fail CI on dead
exclusions, or `off` to disable the audit:

```toml
[settings]
# Global default for every check in this subtree (off | warning | error).
stale_exclusion_severity = "error"

[[checks]]
id = "rust-giant-structs-use-builder"

[checks.policy]
# Per-check override of the global default.
stale_exclusion_severity = "warning"
```

## Notes

- `checkleft` shells out to `git` or `jj` to discover repository state.
- Some built-in checks are specific to Bazel- or monorepo-style workflows.
- JS/TS external checks in `source` mode cache built wasm artifacts under a
  repo-scoped path in `${XDG_CACHE_HOME:-$HOME/.cache}/checkleft/`, and share
  the JS toolchain install across repos when the pinned toolchain inputs match.
  Those entries are derived-only and can be deleted to force a rebuild.

## Examples

- Repo-local `exec-v1` example with typed JavaScript API usage:
  [`examples/typed_js_local_check/README.md`](examples/typed_js_local_check/README.md)
