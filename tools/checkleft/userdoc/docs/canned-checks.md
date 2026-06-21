# Built-in checks

This page documents the built-in check implementations currently registered in the checks binary.

## `bazel-policies`

Purpose:

- Flags configured Bazel Starlark policy violations in changed `BUILD`,
  `BUILD.bazel`, `MODULE.bazel`, and supported `.bzl` files.

Config keys:

- `rules` (required array)
- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

Per-rule keys:

- `kind` (required)

Supported rule kinds:

- `forbidden_rule_call`
  - `symbols` (required array of callee names such as `genrule` or `native.genrule`)
  - `message` (optional string)
  - `severity` (optional `error|warning|info`)
  - `remediation` (optional string)
- `forbidden_package_default_visibility`
  - `values` (required array of forbidden `package(default_visibility=...)` strings)
  - `message` (optional string)
  - `severity` (optional `error|warning|info`)
  - `remediation` (optional string)

Example:

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

        - kind: forbidden_package_default_visibility
          values:
            - //visibility:public
```

Notes:

- `forbidden_rule_call` is AST-backed and matches direct call syntax, not macro expansion.
- `forbidden_package_default_visibility` only applies to build files.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `bazelrc-policies`

Purpose:

- Flags configured Bazel rc policy violations in changed `.bazelrc` files and
  their imported rc fragments.

Config keys:

- `rules` (required array)
- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

Per-rule keys:

- `kind` (required)

Supported rule kinds:

- `required_flag`
  - `commands` (required array of Bazel command scopes)
  - `flag` (required flag name without leading dashes)
  - `value` (optional exact required value)
  - `message` (optional string)
  - `severity` (optional `error|warning|info`)
  - `remediation` (optional string)
- `forbidden_flag`
  - `commands` (required array of Bazel command scopes)
  - `flag` (required flag name without leading dashes)
  - `message` (optional string)
  - `severity` (optional `error|warning|info`)
  - `remediation` (optional string)

Example:

```yaml
checks:
  - id: bazelrc-policies
    check: bazelrc-policies
    config:
      rules:
        - kind: required_flag
          commands: [build]
          flag: downloader_config
          value: /etc/bazel/downloader.cfg

        - kind: forbidden_flag
          commands: [build, test]
          flag: remote_download_all
```

Notes:

- This check parses rc declarations rather than computing full effective flag expansion.
- `common` and Bazel command inheritance are honored for matching unconditional rules.
- Config-scoped entries such as `build:ci` are parsed but ignored by the initial rule set.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `bazelversion-policies`

Purpose:

- Flags configured `.bazelversion` policy violations in changed repository-root `.bazelversion` files.

Config keys:

- `rules` (required array)
- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

Per-rule keys:

- `kind` (required)

Supported rule kinds:

- `allowed_version_patterns`
  - `patterns` (required array of allowed glob-style patterns)
  - `message` (optional string)
  - `severity` (optional `error|warning|info`)
  - `remediation` (optional string)

Example:

```yaml
checks:
  - id: bazelversion-policies
    check: bazelversion-policies
    config:
      rules:
        - kind: allowed_version_patterns
          patterns:
            - channel:live
            - channel:alpha
            - 8.*
```

Notes:

- The check reads the trimmed `.bazelversion` contents and matches them against glob-style patterns such as `channel:*` or `8.*`.
- Only changed repository-root `.bazelversion` files are evaluated.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `code-patterns`

Purpose:

- Flags configured language-aware code patterns in changed source files.

Config keys:

- `lang` (required, currently only `java`)
- `rules` (required array)
- `message` (optional default for rules)
- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

Per-rule keys:

- `nocall` (required string pattern)
- `message` (optional string)
- `severity` (optional `error|warning|info`)
- `remediation` (optional string)

Java `nocall` syntax:

- `<fully.qualified.Type>#<method>()`

Example:

```yaml
checks:
  - id: blocking-java-calls
    check: code-patterns
    config:
      lang: java
      message: Blocking wait without timeout.
      remediation: Use a timeout-bearing API or propagate the async result instead of blocking.
      rules:
        - nocall: java.util.concurrent.Future#get()
        - nocall: com.linkedin.parseq.Task#await()
        - nocall: com.linkedin.parseq.Task#get()
```

Notes:

- The initial implementation is Java-only and matches zero-argument instance method calls.
- Java matching is AST-backed with best-effort local type resolution, not raw text matching.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `format/prettier`

Purpose:

- Flags changed web/config files that are not [Prettier](https://prettier.io)-formatted (`*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`, `*.json`, `*.css`, `*.scss`, `*.less`, `*.html`, `*.vue`, `*.md`, `*.markdown`, `*.yaml`, `*.yml`, `*.graphql`, `*.gql`).

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `prettier --list-different --ignore-unknown <file>` per changed file and emits one finding per file that needs reformatting.
- Prettier discovers the **repo's own** configuration (`.prettierrc` / `prettier.config.*`) and `.prettierignore` relative to the repo root — checkleft imposes no formatting options of its own.

Tool provisioning and version pinning:

- By default Prettier is provisioned via `npx --yes prettier@<version>`, pinned to **3.8.4**. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: format/prettier
    policy:
      severity: error
    config:
      needs:
        prettier:
          npm:
            version: "3.9.0"
```

- When `npx` is not on `PATH`, the check falls back to a `prettier` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.prettier.bazel: "<label>"`, or at an explicit path with `needs.prettier.path: "<path>"` — no change to the bundled definition required.

Notes:

- Each finding's remediation is ``Run `prettier --write <file>` to auto-format.``
- A file Prettier has no parser for is skipped (`--ignore-unknown`) rather than reported as an error.
- Findings take the configured policy severity, which defaults to `error` when unset (like the other format checks). Set `[checks.policy].severity: warning` for a non-blocking instance.
- See [needs version pinning](external-check-package-contract.md#declarative-mode-fields) for the full `needs` binding schema.

## `md/link-integrity`

Purpose:

- Validates internal markdown links in changed `*.md` files anywhere in the repo.

Config keys:

- None.

Notes:

- External URLs (`http`, `https`, `mailto`, `tel`) and same-page anchors are ignored.
- Image links (`![alt](target)`) are skipped.
- Severity is `warning`.
- Runs as an embedded WASM check with `whole_repo` access so it can verify targets anywhere in the repo.

## `file/size`

Purpose:

- Flags files exceeding a max line count. Only triggers when the file grew in the current change — pre-existing oversized files that did not grow are not flagged.

Config keys:

- `max_lines` (optional integer, default `500`)
- `exclude_files` (optional array of glob strings; `exclude_globs` is a supported alias)

Notes:

- Findings default to `warning`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.
- There is only one bundled check for file size. The `check: file/size` field in CHECKS config lets you create a named instance (e.g. `id: my-size-limit`) of the same underlying implementation — this is the aliasing feature, not a separate check.

## `forbidden-imports-deps`

Purpose:

- Flags line-level matches for forbidden import/dependency regex patterns.

Config keys:

- `rules` (required array)

Per-rule keys:

- `pattern` (required regex string)
- `message` (required string)
- `include_globs` (optional array of globs)
- `exclude_globs` (optional array of globs)
- `severity` (optional `error|warning|info`)
- `remediation` (optional string)

Top-level defaults:

- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

## `forbidden-paths`

Purpose:

- Flags changed file paths matching rule-scoped forbidden globs.

Config keys:

- `rules` (required array)
- `exclude_globs` (optional array of glob strings)
- `severity` (optional `error|warning|info`, default `error`)
- `remediation` (optional string)

Per-rule keys:

- `remediation` (required string)
- `when` (required array of `added|modified|deleted|renamed`)
- `patterns` (required array of glob strings)

Notes:

- Rules match repository-relative paths, so filename policies can use patterns like `**/*.swp` or `**/package-lock.json`.
- Findings default to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.

## `file/ifchange`

Purpose:

- Enforces `LINT.IfChange` / `LINT.ThenChange` contracts so linked files or linked blocks change together.
- Requires companion changes when a glob-matched surface changes (policy-declared couplings).

**1. In-source markers** (code-declared):

```text
LINT.IfChange
LINT.IfChange(label)
LINT.ThenChange(path)
LINT.ThenChange(path:label)
LINT.ThenChange(fileA, fileB)
LINT.ThenChange(fileA:region, fileB)
```

- Directives should live on their own lines inside normal source comments
  (`//`, `#`, `--`, `;`, `/* */`, `* `, `<!-- -->`).
- `ThenChange(path)` requires any change to the linked file.
- `ThenChange(path:label)` requires a touched `LINT.IfChange(label)` block in the
  linked file.
- `ThenChange(fileA, fileB)` — comma-separated list of targets; every listed target
  must be changed. Targets can be file paths or `path:label` block references and can
  be mixed freely (e.g. `ThenChange(schema.rs, docs/api.md:endpoints)`).
- When a multi-target `ThenChange` is violated, a separate finding is emitted for each
  unmet target, naming only that specific target, so each finding is independently
  actionable.
- Enforced even when a marked region is deleted or its markers are removed (via
  base-revision content).

**2. Config globs** (policy-declared):

```yaml
- id: api-surface-docs           # local policy label (drives findings/bypass/severity)
  check: file/ifchange
  config:
    trigger_globs: ["backend/blob/src/v3/**"]
    required_globs: ["docs/backend.md", "docs/product-specs/**"]
    message: "Potential backend API surface change without docs update."
    remediation: "Update docs/backend.md or a relevant product spec in this PR."
```

If any changed (non-deleted) file matches `trigger_globs` but no changed file matches
`required_globs`, every trigger file is flagged. For multiple couplings in one instance,
use a `couplings` array, each entry carrying its own
`trigger_globs` / `required_globs` / `message` / `remediation`.

Config keys:

- `trigger_globs` (optional, array of glob strings) — flat single-coupling trigger set.
- `required_globs` (optional, array of glob strings) — required when `trigger_globs` is set.
- `message` (optional string)
- `remediation` (optional string)
- `couplings` (optional array of `{ trigger_globs, required_globs, message?, remediation? }`)

Notes:

- Severity defaults to `error`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass` (see
  [Bypass mechanism](bypass.md)).

## `rust-test-rule-coverage`

Purpose:

- Requires new Rust test files to be in packages with a Bazel `rust_test(...)` rule.

Config keys:

- None.

Severity:

- `error` by default; can be overridden per instance with `[checks.policy].severity`.

## `todo-expiry`

Purpose:

- Requires `TODO`/`FIXME` comments to include owner and date metadata.

Config keys:

- `required_pattern` (optional regex string)
- `severity` (optional `error|warning|info`, default `warning`)
- `remediation` (optional string)

Default accepted format:

```text
TODO(@owner,YYYY-MM-DD): ...
FIXME(@owner,YYYY-MM-DD): ...
```

## `typo`

Purpose:

- Flags configured terminology typos in changed files.

Config keys:

- `rules` (required array)

Per-rule keys:

- `typo` (required string)
- `canonical` (required string)
- `kind` (optional `word|substring`, default `word`)
- `guidance` (optional string)

Severity:

- `error`.

## `workflow-action-version`

Purpose:

- Enforces configured `uses:` action version pins in GitHub workflow files.

Config keys:

- `rules` (required array of `{ action, version }`)
- `severity` (optional `error|warning|info`, default `error`)
- `remediation` (optional string)

## `workflow-run-patterns`

Purpose:

- Flags GitHub workflow `run:` scripts that match configured regex rule patterns.

Config keys:

- `rules` (required array)

Per-rule keys:

- `pattern` (required regex string)
- `message` (required string)
- `must_include` (optional array of string tokens)
- `severity` (optional `error|warning|info`)
- `remediation` (optional string)

Top-level defaults:

- `severity` (optional default for rules, default `error`)
- `remediation` (optional default for rules)

## `workflow-shell-strict`

Purpose:

- Requires multi-line workflow `run:` scripts to begin with `set -euo pipefail`.

Config keys:

- None.

Severity:

- `error`.
