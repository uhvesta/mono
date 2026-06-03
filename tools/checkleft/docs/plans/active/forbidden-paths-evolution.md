# Plan: Evolve `forbidden-paths`

## Goal

Extend the existing `forbidden-paths` check so it can express reasoned,
change-kind-aware path policy using grouped gitignore-style patterns.

## Done Looks Like

1. `forbidden-paths` supports a new `rules[]` config model.
2. Each rule contains:
   - `reason`,
   - `when`,
   - `patterns`.
3. Rules can target `added`, `modified`, `deleted`, and `renamed` changes.
4. One rule can contain multiple path patterns.
5. Findings include the configured reason and the matched pattern.
6. Repo config instances are updated to the new shape in the implementation
   change.
7. User docs explain the new config shape.

## Recommended Implementation Order

### Phase 1: Extend Config Parsing In Place

1. Update
   `tools/checkleft/src/checks/forbidden_paths.rs`
   to parse a new `rules` array.
2. Define per-rule config for:
   - `reason`,
   - `when`,
   - `patterns`.
3. Keep supporting top-level fields that still make sense with the new shape:
   - `exclude_globs`,
   - `severity`,
   - `remediation`.
4. Remove the old flat `patterns` parsing path.
5. Add config-parse tests for:
   - valid rule-based config,
   - invalid empty reason,
   - invalid empty `when`,
   - invalid empty `patterns`,
   - invalid glob syntax.

### Phase 2: Add Change-Kind-Aware Evaluation

6. Map configured `when` values onto `ChangeKind`.
7. Apply rules only to matching change kinds:
   - `added`,
   - `modified`,
   - `deleted`,
   - `renamed`.
8. For `renamed`, evaluate both the old path and new path where available.
9. Continue honoring `exclude_globs`.
10. Emit one finding per matched rule per file, including:
    - path,
    - configured reason,
    - matched pattern,
    - remediation text if configured.
11. Add tests for:
    - add-only rule,
    - modify-only rule,
    - delete-only rule,
    - rename-only rule,
    - multiple patterns under one rule,
    - multiple rules matching one file,
    - exclusion behavior with rule-based config.

### Phase 3: Update Repo Usage And Docs

12. Update any existing repo `forbidden-paths` configs to the new `rules[]`
    shape in the same implementation change.
13. Update
    `tools/checkleft/userdoc/docs/canned-checks.md`
    with the new `rules[]` model.
14. Update
    `tools/checkleft/userdoc/docs/checks-config.md`
    with an example showing add/modify/delete rule scoping.

## Testing Strategy

Use two layers of coverage:

1. Unit tests in
   `tools/checkleft/src/checks/forbidden_paths.rs`
   for config parsing and rule evaluation.
2. Runner-level integration tests for:
   - YAML config resolution,
   - policy severity override,
   - bypass handling.

Important edge cases:

- delete rules now firing where the current implementation skipped deletions,
- rename rules matching only the old path,
- rename rules matching only the new path,
- one file matching two separate reasons,
- filename-style policies expressed as `**/name` patterns.

## Risks

- Rename handling can be subtle if policies care about both origin and
  destination paths.
- Expanding `forbidden-paths` without keeping the docs crisp could make the
  check feel more complicated than it is.

## Suggested PR Shape

One PR is sufficient:

1. extend `forbidden-paths` config and matching,
2. update in-repo config instances,
3. update user docs and examples.
