# Concepts

This page describes the execution model for checks at a high level: what a check is, what it receives, what it should do, and what it returns.

## What a check is

A check is a policy evaluator. It inspects a change and reports findings when repository conventions are violated.

Checks are designed to be:

- fast enough to run in local edit loops
- deterministic and repeatable
- useful to both humans and coding agents

## What information a check gets

Each check receives three inputs:

1. `ChangeSet`: the changed files and change metadata.
2. `SourceTree`: read-only access to repository files.
3. `config`: the check-specific config from the `CHECKS` file.

At a high level, this means checks run from the facts of the change plus repository content, not from external services.

## What a check is expected to do

Checks should:

- evaluate conventions against the change
- produce actionable findings with precise paths (and lines when possible)
- stay deterministic for the same inputs
- stay quick enough for local and CI feedback loops

Checks should generally avoid:

- network calls
- dependence on wall-clock time
- hidden global state
- expensive full-repository scans when change-scoped logic is sufficient

## What a check is expected to return

A check returns one `CheckResult` containing:

- `check_id`
- `findings[]`

Each finding can include:

- `severity`: `error`, `warning`, or `info`
- `message`
- `location` (`path`, optional `line`, optional `column`)
- `remediation` (optional guidance)
- `suggested_fix` (optional machine-applicable edits)

In CLI behavior, `error` findings fail the run; `warning` and `info` findings do not.

## Hermetic execution model

The framework is intentionally designed for hermetic-style checks: evaluate against provided inputs and avoid external dependencies.

In practice:

- checks operate on the provided `ChangeSet` and `SourceTree`
- file access is constrained to repository-relative paths through `SourceTree`
- the same inputs should produce the same findings

This is what makes checks repeatable, quick, and suitable for automation.

## Change-scoped by design

Checks are designed to operate on changes, not entire repositories.

By default, the runner computes the current changed files (or changes since a base ref), resolves configured checks for those files, and executes checks on that scoped set. This keeps runtime proportional to change size and scales to large repositories.

`--all` exists for full sweeps, but the default workflow is change-scoped presubmit feedback.
