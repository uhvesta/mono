# Motivation

The checks framework exists to keep repo conventions consistent without relying on reviewers to catch everything manually.

The core idea is presubmit-style automation: run policy checks before merge so developers get fast, actionable feedback while changes are still fresh.

It is intentionally designed for both human developers and coding agents. For both, checks tighten feedback loops; for agents in particular, checks act as guardrails that enforce strong repository conventions automatically.

## Problems it solves

- Repeated review comments for the same policy violations.
- Drift between local habits and CI expectations.
- Slow feedback when policy violations are only detected after push.
- Difficulty scaling conventions across large, multi-language codebases.

## Design goals

- Fast local feedback for changed files.
- Fast feedback loops for both humans and agents during iterative development.
- Opinionated guardrails that enforce repository conventions in agent-generated changes.
- Declarative configuration with hierarchical `CHECKS.yaml` / `CHECKS.toml` files.
- Reusable built-in checks plus check-specific configuration.
- Human-readable output for developers and JSON output for automation.
- Consistent behavior in local dev, PR tooling, and CI.

## Why this approach

### Why not just write tests for this?

Tests validate product behavior. Many repository conventions are not product behavior:

- file layout and path policy
- workflow script hygiene
- docs update requirements tied to code changes
- comment or metadata formatting rules

Those are better enforced as policy checks that run on changed files, independent of runtime test suites.

### Why not just put all logic in GitHub Actions YAML?

Workflow YAML is good for orchestration, but not for complex policy logic:

- harder to structure, test, and reuse
- weaker ergonomics for parsing files and producing precise findings
- poor portability to local developer workflows

Keeping policy logic in checks code provides one implementation that runs both locally and in CI.

### Why not build a separate custom tool for every rule?

Many small tools increase maintenance and reduce consistency:

- duplicated change-detection and output formats
- inconsistent severity handling and remediation messages
- fragmented developer experience

A shared checks framework centralizes execution, configuration, and reporting while still allowing distinct check implementations.

It also provides a single, opinionated policy surface for both people and agents, which helps keep autonomous edits aligned with repository standards.

### Why not just encode conventions in prompts or `AGENTS.md`?

Prompts and `AGENTS.md` are a good place to start. They communicate expectations quickly and are often the fastest way to introduce new conventions.

But prompt guidance is not deterministic enforcement:

- instructions can be interpreted differently
- behavior can vary across runs and models
- violations may slip through without explicit checks

For conventions that must be enforced every time, convert them into repeatable checks.

A practical pattern is:

1. Introduce the convention in prompts/`AGENTS.md`.
2. Observe whether it is important and stable enough to enforce.
3. Promote it into a checks rule for deterministic, auditable enforcement.

## What you get in practice

- A single command (`./tools/checks run`) for local validation.
- The same checks enforced before PR creation via `./tools/create-pr`.
- CI integration with machine-readable output.
- A policy model that can be scoped by directory using child `CHECKS.yaml` / `CHECKS.toml` files.
