# Checkleft: Bazel Policy Checks

## Overview

This design adds first-class Bazel policy enforcement to `checkleft`.

The policies discussed here fall into two distinct syntax families:

- Bazel Starlark files such as `BUILD`, `BUILD.bazel`, `MODULE.bazel`, and
  selected `.bzl` files,
- Bazel rc files such as `.bazelrc` and imported rc fragments.

Although both families are "Bazel configuration", they should not be modeled
as one generic parser or one generic check. Starlark and rc files have
different syntax, different semantics, and different kinds of policy rules.

The proposed design therefore adds two built-in checks:

- `bazel-policies` for AST-based checks over Bazel Starlark files,
- `bazelrc-policies` for parsed checks over rc files.

Both checks remain change-scoped. They should scale primarily with the size of
the change rather than the size of the repository.

## Motivation

Several repository policies come up repeatedly in Bazel code review:

- forbidden rule declarations, such as "no `genrule`",
- Bazel visibility conventions,
- required rc flags, such as "`--downloader_config` must be set to X",
- forbidden rc flags,
- dependency restrictions such as "packages under X must not depend on Y".

Some of these are good fits for Bazel's own enforcement mechanisms, especially
visibility. Others are not. `checkleft` is a good home for the remainder when
the policy needs to run in local edit loops, in presubmit, and only on changed
files.

## Existing Options

There are useful existing Bazel tools, but none is a complete fit for the
problem here:

- `buildifier` / `buildtools` provide parsing and linting for Bazel Starlark
  files, including `MODULE.bazel`, but they do not provide a repo-specific
  policy engine for arbitrary custom rules and do not cover `.bazelrc`.
- Bazel visibility and load visibility are the strongest solution for certain
  dependency restrictions and should still be preferred where they can express
  the policy.
- Bazel-driven lint orchestration can run tools efficiently, but it does not
  replace a repository policy model.

The gap is a change-scoped, repo-owned policy engine for Bazel source files and
rc files.

## Goals

- Add AST-based Bazel Starlark policy checks to `checkleft`.
- Add parsed rc-file policy checks to `checkleft`.
- Keep evaluation proportional to the changed files plus any directly imported
  rc-file closure needed to interpret them.
- Use a typed rule vocabulary rather than a raw query DSL.
- Support multiple rules per configured check instance.
- Preserve room for future Bazel-specific policy families without forcing all
  Bazel configuration into one abstraction.

## Non-Goals

- Building a fully generic Starlark query engine.
- Replacing Bazel visibility for policies that Bazel can already enforce
  precisely.
- Modeling the entire analyzed Bazel graph in `checkleft`.
- Expanding macros or evaluating arbitrary Starlark.
- Treating `.bazelrc` files as Starlark-like syntax.

## Why Built-In Checks

These checks should be built into `checkleft`, not introduced first as
repo-local external checks.

That choice is justified by the same reasons called out in the existing
`code-patterns` design:

- the checks need direct access to `checkleft`'s change-scoped file model,
- Starlark parsing logic is likely to be reused across multiple Bazel checks,
- the policies are structural and syntax-aware rather than just command
  wrappers,
- the checks should run with the same local/CI ergonomics as the rest of the
  built-in catalog.

Repo-local external checks remain a useful extension point for repo-specific
policies that do not belong in the shared built-in set.

## High-Level Recommendation

Implement two new built-in checks:

1. `bazel-policies`
2. `bazelrc-policies`

Do not fold `.bazelrc` into the same check as Starlark files.

Do not expose a generic tree-sitter query interface in configuration. Instead,
define a small typed rule language and grow it deliberately.

## `bazel-policies`

### Scope

`bazel-policies` operates on changed Bazel Starlark files:

- `BUILD`
- `BUILD.bazel`
- `MODULE.bazel`
- selected `.bzl` files for rule kinds that explicitly support them

It parses each changed file once, then evaluates configured rules against the
syntax tree.

### Rule Model

`bazel-policies` should follow existing `checkleft` patterns and accept a
`rules` array in its config.

The rule model should be typed. That means the config identifies the semantic
kind of policy being enforced, and the implementation owns the AST traversal.
The config should not directly describe grammar node kinds or raw tree-sitter
queries.

Suggested initial rule kinds:

- `forbidden_rule_call`
- `forbidden_package_default_visibility`

Suggested future rule kinds:

- `forbidden_load`
- `forbidden_module_call`
- `forbidden_direct_dep`

### `forbidden_rule_call`

This rule flags direct calls to forbidden Bazel rule symbols in build-like
files.

Example YAML:

```yaml
checks:
  - id: bazel-policies
    check: bazel-policies
    config:
      rules:
        - kind: forbidden_rule_call
          symbols:
            - genrule
            - native.genrule
          message: Do not add genrules.
          remediation: Use a dedicated rule or a checked-in macro with narrower semantics.
          severity: error
```

Semantics:

- parse changed `BUILD` / `BUILD.bazel` files with `tree-sitter-starlark`,
- visit `call` nodes,
- normalize the callee shape,
- emit a finding when the callee matches one of the configured symbols.

This is the right first implementation for the "no genrules" policy because it
is:

- AST-based,
- cheap,
- change-scoped,
- easy to explain to authors.

The rule intentionally enforces "no direct `genrule(...)` declaration in the
changed file", not "the final analyzed build graph contains no genrule
anywhere". Macro expansion is out of scope for this check.

### `forbidden_package_default_visibility`

This rule generalizes the current `repo_visibility` built-in behavior.

Example YAML:

```yaml
checks:
  - id: bazel-policies
    check: bazel-policies
    config:
      rules:
        - kind: forbidden_package_default_visibility
          values:
            - //visibility:public
          message: package default_visibility must not be //visibility:public
          remediation: Remove the package default visibility or narrow visibility on individual targets.
          severity: error
```

This can either:

- replace `repo_visibility` outright, or
- ship alongside `repo_visibility` first and later deprecate the narrow check.

The second path is lower risk.

### Direct Dependency Policies

Policies like "X cannot depend on Y" need special care.

There are two different kinds of enforcement:

1. Bazel-native enforcement
2. Source-level review enforcement

For Bazel-native enforcement, repositories should prefer Bazel visibility or
load visibility when those mechanisms can express the intended boundary. Bazel
is the strongest source of truth for those rules.

For source-level review enforcement, `bazel-policies` may later add a rule kind
such as `forbidden_direct_dep`, but it should be explicitly limited to direct,
syntactic label edges visible in changed source files. It should not claim to
model:

- macro-generated dependencies,
- computed labels,
- post-expansion graph semantics,
- full-module resolution behavior.

That makes `forbidden_direct_dep` a useful review-time policy, but not a
replacement for Bazel visibility.

## `bazelrc-policies`

### Scope

`bazelrc-policies` operates on changed `.bazelrc` files and any imported
fragments needed to interpret them.

Unlike `bazel-policies`, this check should not be AST-backed. Rc files are not
Starlark. They need their own parser and rule model.

### Why A Separate Check

Combining rc-file rules with Starlark rules would create a poor abstraction:

- different parser,
- different file discovery,
- different evaluation model,
- different rule semantics.

Multiple Bazel-related checks are acceptable. Existing `checkleft` practice
already supports multiple rules within a check and multiple related checks
within one policy domain.

### Parser Model

The check should parse rc entries into a normalized representation:

- source file path,
- line number,
- entry kind such as `import`, `try-import`, or option stanza,
- command scope such as `common`, `build`, `test`, `startup`,
- optional config selector such as `build:ci`,
- flag name,
- flag value when present.

The parser should understand:

- comments and blank lines,
- `import`,
- `try-import`,
- command-scoped option lines,
- config-scoped option lines.

The implementation should follow imports only as needed to interpret the
changed file's closure. It should not recursively scan unrelated rc files in
the repository.

### Initial Rule Kinds

Suggested initial rule kinds:

- `required_flag`
- `forbidden_flag`

Example YAML:

```yaml
checks:
  - id: bazelrc-policies
    check: bazelrc-policies
    config:
      rules:
        - kind: required_flag
          commands:
            - build
          flag: downloader_config
          value: /etc/bazel/downloader.cfg
          message: build must set --downloader_config to the approved config.
          remediation: Update .bazelrc so build declares the approved downloader config.
          severity: error

        - kind: forbidden_flag
          commands:
            - common
            - build
            - test
          flag: remote_download_all
          message: Do not enable remote_download_all in repository bazelrc files.
          remediation: Remove the flag or switch to the approved remote download mode.
          severity: error
```

### Initial Semantics

The first implementation should use declaration-oriented semantics rather than
full effective-option evaluation.

That means:

- `required_flag` answers whether an applicable stanza explicitly declares the
  required flag and, when configured, the required value,
- `forbidden_flag` answers whether an applicable stanza explicitly declares the
  forbidden flag.

This is a good v1 tradeoff because it is:

- simpler to explain,
- deterministic,
- cheap,
- robust enough for common repository policy cases.

### Future Effective Semantics

The parser and internal data model should leave room for a later effective
evaluation mode that accounts for:

- `common` inheritance into command-specific scopes,
- config-specific sections such as `build:ci`,
- import order,
- last-one-wins behavior for scalar flags,
- repeatable flags.

That future expansion should be additive. It should not force a redesign of the
v1 configuration shape.

## Configuration Style

The design should follow established `checkleft` patterns:

- one configured check instance may contain multiple `rules`,
- top-level defaults for severity/message/remediation may be added if they
  materially simplify common configs,
- rule-specific overrides remain supported.

The config should stay semantic. For example, `forbidden_rule_call` should
infer that it applies to build-like Starlark files. The design should avoid
introducing a generic `file_kinds` knob unless a concrete future use case
requires it.

## Scaling Model

These checks should preserve `checkleft`'s normal scaling model.

`bazel-policies` should:

- examine changed Bazel Starlark files only,
- parse each file once,
- evaluate all configured rules against that parse result.

`bazelrc-policies` should:

- examine changed rc files only,
- load only imported rc fragments necessary to interpret those changed files,
- evaluate configured rules against the parsed entry model.

Neither check should require whole-repository analysis in v1.

## Alternatives Considered

### One Generic "Bazel Check"

Rejected.

This would force Starlark files and rc files into one abstraction even though
they have different syntax and policy semantics.

### One Generic Starlark Query DSL

Rejected.

That would shift too much parser detail into user config, make policies harder
to understand, and make later evolution harder. A typed rule model is a better
fit for `checkleft`.

### External Checks First

Rejected for the common policy families described here.

Repo-local external checks remain useful for custom org- or repo-specific
policies, but the baseline Bazel policy families belong in the built-in set.

## Phasing

Recommended implementation order:

1. add shared Bazel Starlark parsing helpers extracted from `repo_visibility`,
2. add `bazel-policies` with `forbidden_rule_call`,
3. optionally add `forbidden_package_default_visibility` and later migrate
   `repo_visibility`,
4. add `bazelrc-policies` with `required_flag` and `forbidden_flag`,
5. evaluate whether a limited `forbidden_direct_dep` rule is still needed once
   visibility-based solutions are documented and available.

## Open Questions

- Should `repo_visibility` remain as a permanent narrow built-in alias for the
  corresponding `bazel-policies` rule, or should it eventually be removed?
- Should `bazelrc-policies` v1 support config-scoped rules like `build:ci`
  immediately, or defer them until a real use case appears?
- Is there a common direct-dependency policy that cannot be expressed with
  Bazel visibility and is important enough to justify a `forbidden_direct_dep`
  rule in the initial rollout?
