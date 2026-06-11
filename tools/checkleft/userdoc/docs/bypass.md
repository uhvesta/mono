# Bypassing checks

This page describes bypass support for checks that opt into bypass.

## Scope

- Bypass is opt-in per check.
- By default, checks do not allow bypass.
- In the current default config, bypass is enabled for:
  - `file-size`
  - `api-breaking-surface`
  - `no-usfa-typo` (directive name: `BYPASS_NO_USFA_TYPO`)

```toml
[[checks]]
id = "api-breaking-surface"

[checks.policy]
allow_bypass = true

[[checks]]
id = "file-size"

[checks.policy]
allow_bypass = true

[[checks]]
id = "no-usfa-typo"
check = "typo"

[checks.policy]
allow_bypass = true
bypass_name = "BYPASS_NO_USFA_TYPO"
```

## Directive format

Use a single-line directive in commit or PR description:

```text
BYPASS_<CHECK_NAME>=<specific legitimate reason>
```

For `api-breaking-surface`:

```text
BYPASS_API_BREAKING_SURFACE=No public API behavior changed; docs update would be misleading.
```

For `file-size`:

```text
BYPASS_FILE_SIZE=Generated file mirrors upstream source and cannot be split safely.
```

For `no-usfa-typo`:

```text
BYPASS_NO_USFA_TYPO=Legacy upstream terminology is intentionally retained in this change.
```

## Where directives are read from

Checks parse directives from:

- current commit description
- PR description

If both contain the same bypass name, PR description wins.

## Behavior when bypass applies

When a check has `[checks.policy] allow_bypass = true` and a matching directive with non-empty reason exists:

- the normal failure is bypassed
- the check emits a `warning` finding recording bypass use and reason

This keeps bypass use visible in output and CI logs.

## Behavior when bypass is enabled but not used

If policy fails and bypass is enabled but no directive exists:

- normal policy failure is emitted
- remediation text includes bypass instructions and warns against convenience bypasses

## Legacy config compatibility

During migration, some checks still honor `allow_bypass` / `bypass_name` under `[checks.config]`. Prefer `[checks.policy]` for all new or updated configuration.

## CI/environment context

The checks CLI resolves PR description context using a layered fallback:

1. **`CHECKS_PR_DESCRIPTION`** — explicit description text (highest precedence, no network call).
2. **`CHECKS_CHANGE_ID` / `CHECKS_PR_NUMBER`** — explicit PR number.
3. **CI-native env** — resolved automatically, no harness wiring needed:
   - Buildkite: `BUILDKITE_PULL_REQUEST` (used when not `"false"`; present on PR builds).
   - GitHub Actions: `GITHUB_REF` parsed as `refs/pull/{N}/merge` (present on `pull_request` events).
4. **Branch→PR lookup** — when no PR number is available, checkleft detects the current branch (from `BUILDKITE_BRANCH`, `GITHUB_HEAD_REF`, `refs/heads/{branch}` in `GITHUB_REF`, or VCS) and queries the GitHub API for an open PR on that branch. This is what enables bypass directives in the PR description on push-triggered builds (this repo's normal CI flow) without any CI script changes.

All network-based resolution (levels 3–4) is best-effort: if no GitHub token is available or no open PR is found, checkleft falls back to commit-description directives only — no error is raised.

GitHub auth is read from `CHECKS_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` (checked in that order). `CHECKS_REPOSITORY` overrides the repository slug if needed; otherwise it is inferred from the git remote.

## Policy guidance

Bypasses are for rare, legitimate exceptions with concrete rationale.

Do not use bypasses to skip required work for convenience.
