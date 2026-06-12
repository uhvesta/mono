# checkleft

Status: experimental / under active development. Not yet recommended for
general use. The CLI behavior, built-in checks, and library API may change
without notice.

`checkleft` is a repository convention checker. It exists to enforce a
repository's house rules — naming conventions, forbidden imports, file-size
limits, doc-link integrity, and similar policies — at change-review time. It
runs a configured set of checks against the files in a source tree and reports
findings as human-readable output or JSON, so the same rules can guard a CI step
and a local pre-push run. It ships as both a standalone CLI (`checkleft`) and a
library, and is a standalone developer tool independent of the Boss automation
system.

## Architecture

`checkleft` is organized around a small set of cooperating abstractions:

- **Change detection.** Rather than scanning the whole tree, a run normally
  evaluates only what changed. The change-detection layer classifies the
  environment (PR build, merge-queue build, push-to-main, or a local branch),
  resolves the default/integration branch, computes the correct base commit, and
  produces a `ChangePlan` (a scoped diff, an "all files" plan, or an empty plan).
  It shells out to `git`/`jj` to inspect repository state and fetch history when
  a shallow clone lacks the needed commits.

- **Configuration.** `CHECKS.yaml` / `CHECKS.toml` files are discovered from the
  repository root down to each evaluated file; a config resolver merges them so
  child directories can extend or override the root. A root config (or a CLI
  flag) may also pull in an externally hosted config before local overrides
  apply.

- **Checks.** Every check implements a small `Check` / `ConfiguredCheck` trait
  pair and is keyed by id in a registry. **Built-in checks** are compiled in
  (the Rust/Bazel/workflow/docs conventions). **External checks** are resolved
  from packages — referenced by file path or as generated/exec implementations —
  letting a repo (or a shared, remotely hosted config) define checks that aren't
  baked into the binary. External checks run either as WebAssembly Component
  Model artifacts (component mode, via `wasmtime`) or as declarative invocations
  (declarative mode, where the framework owns binary execution and transform).

- **Runner and output.** The runner takes the change plan, the resolved config,
  and the registry, schedules each applicable check, applies per-check policy
  (severity overrides, bypasses), and collects `Finding`s into a `CheckResult`.
  Findings carry a location, message, remediation, and optional suggested fix,
  and serialize to the same JSON shape whether produced by a built-in or
  external check.

The crate is consumed both as the `checkleft` binary and as a library; external
check packages are an extension point rather than a compile-time dependency.

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

### Run before every push

`checkleft install` drops a git `pre-push` hook that runs `checkleft run`
against the outgoing changes before each push, so a convention violation is
caught locally instead of on CI:

```bash
checkleft install      # install the pre-push hook (idempotent)
checkleft uninstall    # remove it (alias: checkleft install --remove)
```

The hook is recognised by a marker line, so re-running `install` is a no-op,
and `install` / `uninstall` never touch a `pre-push` hook you wrote yourself.
When a check fires, fix the finding or add a `BYPASS_<CHECK>=<reason>` directive
to the commit message or PR description (see the bypass docs).

> **jujutsu note.** `jj git push` is a native implementation that does **not**
> run git hooks, so this hook does not fire for jj-driven pushes. In a
> jj-based workflow, run `checkleft run` before pushing (or wire it into your
> push tooling) rather than relying on the git hook.

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

Exclusions in a `CHECKS` file (e.g. a check's `exclude_structs` / `exclude_files`
list) are easy to add and easy to forget. Once the reason an exclusion existed
goes away — the excluded struct gains a builder, a referenced file is deleted —
the entry becomes dead weight that quietly weakens coverage.

`checkleft` audits for this. When a file an exclusion depends on changes in the
diff, the owning check re-evaluates that exclusion as if it were not present; if
the rule now passes without it, `checkleft` reports a finding **on the
`CHECKS` file entry itself** telling you the exclusion can be removed. The audit
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
id = "rust/giant-structs"

[checks.policy]
# Per-check override of the global default.
stale_exclusion_severity = "warning"
```

## Notes

- `checkleft` shells out to `git` or `jj` to discover repository state.
- Some built-in checks are specific to Bazel- or monorepo-style workflows.
