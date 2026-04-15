# Checkleft: High-Level Design

## Overview

Checkleft is a code-review and repository-policy framework. It runs a set of
checks against a source tree, reports violations in a consistent format, and
supports both built-in checks and externally packaged checks.

Today the framework lives in `flunge` under `cli/checkleft`. The intent of
moving it into `mono` is to make `checkleft` the shared, canonical home for
policy automation that can be reused by more than one repo or tool.

For the first migration step, duplication is acceptable. We do not need to
remove `checkleft` from `flunge` yet, and we do not need to switch `flunge` to
consume the `mono` copy in this change.

## Repository Shape

The proposed `mono` home for the framework is:

```text
tools/checkleft/
  BUILD.bazel
  docs/
    designs/
    plans/
  ...
```

The long-term implementation should preserve the same broad responsibilities as
the current `flunge` package:

- core check execution and result reporting,
- configuration loading,
- source-tree and VCS helpers,
- built-in checks,
- external check runtime and package contract,
- user-facing docs for authors and operators.

## Migration Goals

1. Establish `mono` as the canonical source location for `checkleft`.
2. Preserve source history from `flunge` where practical during the initial
   import.
3. Preserve behavioral parity during the initial duplication.
4. Integrate with `mono`'s Rust workspace and Bazel setup without widening
   visibility unnecessarily.
5. Keep follow-on adoption work separate from the initial code move.

## History Preservation

The preferred migration path is not a plain file copy. Instead, the first code
move should preserve commit history for `cli/checkleft` by importing the
package as filtered history from `flunge` into `mono/tools/checkleft`.

A practical approach is:

1. create a history stream in `flunge` that contains only `cli/checkleft`,
2. rewrite that stream so the files live under `tools/checkleft`,
3. merge that stream into `mono`,
4. do the `mono`-specific Cargo, Bazel, and docs fixes in follow-up commits on
   top.

This keeps the original development history available for blame and archaeology
while still allowing the `mono` version to diverge afterward.

## Explicitly Out Of Scope

- Removing `checkleft` from `flunge`.
- Updating `flunge` to consume `mono/tools/checkleft`.
- Redesigning the framework API during the initial move.
- Broadening the check catalog beyond what already exists in `flunge`.

## Related Designs

- [`ifchange-thenchange`](ifchange-thenchange.md)
- [`forbidden-paths-evolution`](forbidden-paths-evolution.md)
- [`code-patterns`](code-patterns.md)
- [`bazel-external-checks`](bazel-external-checks.md)
- [`bazel-repo-local-checks`](bazel-repo-local-checks.md)
- [`bazel-policy-checks`](bazel-policy-checks.md)
- [`bazel-central-checks`](bazel-central-checks.md)
