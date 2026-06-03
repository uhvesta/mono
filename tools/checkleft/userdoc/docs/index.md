# Checks framework

The Checks Framework turns repository conventions into automated checks that run before changes are submitted for review or merge.

In other words, it is a pre-submit system: it checks your change early, not just after it lands in CI.

It runs targeted checks on changed files locally and in CI, so policy violations are caught early and consistently. The result is fewer repeated review comments and clearer enforcement of conventions.

## Why it exists

- Shift common policy feedback earlier in the dev cycle.
- Keep local behavior aligned with CI enforcement.
- Scale conventions across large, multi-language codebases.
- Make policy changes explicit, versioned, and reviewable.

## Who this is for

- Engineers running checks locally before pushing or creating a PR.
- Engineers authoring or updating `CHECKS.yaml` / `CHECKS.toml` policy.
- Engineers implementing new built-in checks in Rust.
- Coding agents that make repository changes under policy guardrails.
- Engineers configuring and maintaining agent workflows that should follow repository conventions.

## Quick start

1. Run checks for your current change:

```bash
./tools/checks run
```

2. See which checks apply to your current change:

```bash
./tools/checks list
```

3. Run checks against all tracked files:

```bash
./tools/checks run --all
```

4. Emit JSON output for tooling/CI:

```bash
./tools/checks run --format=json
```
