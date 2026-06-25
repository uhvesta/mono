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

## `format/biome`

Purpose:

- Flags changed files that are not [Biome](https://biomejs.dev)-formatted. [Biome](https://biomejs.dev) is a single fast Rust tool covering both formatting and linting — a quick alternative to Prettier. The formatter applies to the file types Biome formats: `*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`, `*.json`, `*.jsonc`, `*.css`, `*.graphql`, `*.gql`. (Biome 2.5.0 does not format HTML or Markdown, so those are not included.)

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `biome format --files-ignore-unknown=true --reporter=json <files>` over the changed files in one batch invocation (Biome is fast and accepts the whole set at once) and emits one finding per file that needs reformatting. The invocation runs in format-CHECK mode — it never writes files.
- Biome discovers the **repo's own** `biome.json` / `biome.jsonc` relative to the repo root (invocations run with cwd = repo root). With no config, Biome uses its built-in defaults; checkleft imposes no formatting options of its own.
- `--files-ignore-unknown=true` skips any file Biome has no formatter for instead of erroring (the analogue of Prettier's `--ignore-unknown`).

Tool provisioning and version pinning:

- By default Biome is provisioned via `npx --yes @biomejs/biome@<version>`, pinned to **2.5.0**. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain. `format/biome` and `lint/biome` share the same pinned `@biomejs/biome` binding.
- Pinning also stabilises the JSON reporter: Biome warns that the json reporter is unstable **across** releases, which the version pin neutralises for the pinned 2.5.0 shape.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: format/biome
    policy:
      severity: error
    config:
      needs:
        biome:
          npm:
            version: "2.6.0"
```

- When `npx` is not on `PATH`, the check falls back to a `biome` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.biome.bazel: "<label>"`, or at an explicit path with `needs.biome.path: "<path>"` — no change to the bundled definition required.

Notes:

- Each finding's remediation is ``Run `<biome invocation> format --write <file>` to auto-format`` (the invocation prefix tracks whatever `needs` binding resolved, including any per-repo version override).
- Findings take the configured policy severity, which defaults to `error` when unset (like the other format checks). Set `[checks.policy].severity: warning` for a non-blocking instance.
- See [needs version pinning](external-check-package-contract.md#declarative-mode-fields) for the full `needs` binding schema.

## `format/oxc`

Purpose:

- Flags changed files that are not [oxfmt](https://oxc.rs)-formatted. oxfmt is the formatter from the [Oxc](https://oxc.rs) toolchain (the same project as oxlint) — a fast Rust formatter. The check applies to the file types oxfmt formats reliably at the pinned version: `*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`, `*.json`, `*.jsonc`, `*.json5`, `*.css`, `*.scss`, `*.less`, `*.html`, `*.vue`, `*.md`, `*.markdown`, `*.mdx`, `*.yaml`, `*.yml`, `*.toml`, `*.graphql`, `*.gql`.

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `oxfmt --list-different <file>` per changed file (CHECK mode — it never writes the file) and emits one finding per file that needs reformatting.
- oxfmt discovers the **repo's own** configuration (`.oxfmtrc.json` / `.oxfmtrc.jsonc`, etc.) and `.gitignore` / `.prettierignore` relative to the repo root (invocations run with cwd = repo root). With no config, oxfmt uses its built-in defaults; checkleft imposes no formatting options of its own.

Language scope — verified, not advertised:

- oxfmt is early-stage (pre-1.0). Each language above was verified against the pinned release by formatting representative and complex samples (markdown with tables/frontmatter/code-fences, YAML with anchors/merge-keys/multiline, HTML with embedded `<script>`, Vue single-file components) and confirming the output is correct and idempotent (a second format pass reports clean). Languages oxfmt does **not** yet handle are deliberately excluded so the check never claims coverage it doesn't have: notably `.svelte` and `.astro` (the pinned oxfmt does not recognise them).
- Because oxfmt is pre-1.0 and ships frequently, the version pin is load-bearing: re-verify the formatted-language set when bumping it.

Tool provisioning and version pinning:

- By default oxfmt is provisioned via `npx --yes oxfmt@<version>`, pinned to **0.55.0** (the latest stable at authoring time; an operator-confirmed pin is pending). oxfmt is distributed as the standalone `oxfmt` npm package with an `oxfmt` binary. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: format/oxc
    policy:
      severity: error
    config:
      needs:
        oxfmt:
          npm:
            version: "0.56.0"
```

- When `npx` is not on `PATH`, the check falls back to an `oxfmt` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.oxfmt.bazel: "<label>"`, or at an explicit path with `needs.oxfmt.path: "<path>"` — no change to the bundled definition required.

Notes:

- Each finding's remediation is ``Run `<oxfmt invocation> --write <file>` to auto-format`` (the invocation prefix tracks whatever `needs` binding resolved, including any per-repo version override).
- A file oxfmt cannot parse exits with an operational error (exit 2) and is reported as a per-file **error** finding rather than masquerading as clean; other files in the changeset are unaffected.
- Findings take the configured policy severity, which defaults to `error` when unset (like the other format checks). Set `[checks.policy].severity: warning` for a non-blocking instance.
- See [needs version pinning](external-check-package-contract.md#declarative-mode-fields) for the full `needs` binding schema.

## `lint/js`

Purpose:

- Flags ESLint violations in changed JS/TS source files (`*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`).

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `eslint --no-config-lookup --config <config_file> --format json` over the changed files in one batch invocation and converts the JSON output to findings.
- ESLint severity is preserved: severity 2 (error) maps to a checkleft `error` finding; severity 1 (warning) maps to a `warning` finding.
- `--no-config-lookup` prevents ESLint from discovering any config file from the filesystem — only the path you specify via `config_file` is applied.

Required config — `config_file`:

- `config_file` **must** be set in the check's `config:` block. There is no default — omitting it produces a clear runtime error rather than accidentally linting with an auto-discovered or default config.

Minimal CHECKS config:

```yaml
checks:
  - id: lint/js
    config:
      config_file: "eslint.config.js"
```

Tool provisioning and version pinning:

- By default ESLint is provisioned via `npx --yes eslint@<version>`, pinned to **10.5.0**. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: lint/js
    config:
      config_file: "eslint.config.js"
      needs:
        eslint:
          npm:
            version: "10.6.0"
```

- When `npx` is not on `PATH`, the check falls back to an `eslint` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.eslint.bazel: "<label>"`, or at an explicit path with `needs.eslint.path: "<path>"` — no change to the bundled definition required.

Notes:

- ESLint is invoked in batch mode (once per changed-file set, not once per file), which is more efficient for large changesets.
- Each finding includes the rule ID (e.g. `no-unused-vars: 'x' is defined but never used.`). Parse errors without a rule ID appear as the raw ESLint message.
- ESLint's per-finding severity is preserved end-to-end: severity 2 (error) produces a checkleft `error` finding; severity 1 (warning) produces a `warning` finding. No policy configuration is needed to get this distinction.
- To make all findings non-blocking (warnings), set `policy.severity: warning`. This overrides every finding's severity regardless of what ESLint reported. To keep errors blocking while making warnings non-blocking, configure two instances — one with no severity override (errors block, warnings are advisory) and one with `severity: warning` using a separate ESLint config that only enables warning-level rules:

```yaml
checks:
  - id: lint/js-errors
    check: lint/js
    config:
      config_file: "eslint.config.js"
  - id: lint/js-warnings
    check: lint/js
    config:
      config_file: "eslint.config.warnings.js"
    policy:
      severity: warning
```

- See [needs version pinning](external-check-package-contract.md#declarative-mode-fields) for the full `needs` binding schema.

## `lint/biome`

Purpose:

- Flags [Biome](https://biomejs.dev) lint violations in changed JS/TS source files (`*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`). Biome is a single fast Rust tool covering both linting and formatting — a quick alternative to ESLint.

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `biome lint --files-ignore-unknown=true --reporter=json <files>` over the changed files in one batch invocation and converts the JSON diagnostics to findings (file path, 1-based line/column, rule category, severity, and message). The invocation runs in lint-CHECK mode — it never writes fixes.
- Biome's per-diagnostic severity is preserved: `error`/`fatal` map to a checkleft `error` finding, `warning` to `warning`, and `information`/`hint` to `info`. Each finding's message is prefixed with the rule category (e.g. `lint/suspicious/noDoubleEquals: Using == may be unsafe ...`).

Config — none required:

- **Unlike `lint/js`, `lint/biome` requires no config key.** Biome is zero-config by design: it ships a built-in `recommended` rule set and auto-discovers the repo's own `biome.json` / `biome.jsonc` (invocations run with cwd = repo root) to refine it. With no `biome.json` present, it lints with Biome's recommended defaults; with one present, it is picked up automatically. (ESLint 9+ has no built-in defaults and so forces `lint/js` to make `config_file` mandatory — Biome does not.)

Tool provisioning and version pinning:

- By default Biome is provisioned via `npx --yes @biomejs/biome@<version>`, pinned to **2.5.0**. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain. `lint/biome` and `format/biome` share the same pinned `@biomejs/biome` binding.
- Pinning also stabilises the JSON reporter: Biome warns that the json reporter is unstable **across** releases, which the version pin neutralises for the pinned 2.5.0 shape.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: lint/biome
    config:
      needs:
        biome:
          npm:
            version: "2.6.0"
```

- When `npx` is not on `PATH`, the check falls back to a `biome` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.biome.bazel: "<label>"`, or at an explicit path with `needs.biome.path: "<path>"` — no change to the bundled definition required.

Notes:

- Biome is invoked in batch mode (once per changed-file set, not once per file), which is more efficient for large changesets.
- Each finding's remediation is to fix the violation or suppress it with a justified `// biome-ignore lint: <reason>` comment.
- Biome's per-finding severity is preserved end-to-end, so no policy configuration is needed to get the error/warning/info distinction. To make all findings non-blocking, set `policy.severity: warning`, which overrides every finding's severity regardless of what Biome reported.
- See [needs version pinning](external-check-package-contract.md#declarative-mode-fields) for the full `needs` binding schema.

## `lint/oxc`

Purpose:

- Flags [oxlint](https://oxc.rs) violations in changed JS/TS source files and framework single-file components (`*.js`, `*.jsx`, `*.mjs`, `*.cjs`, `*.ts`, `*.tsx`, `*.mts`, `*.cts`, `*.vue`, `*.svelte`, `*.astro`). oxlint is the linter from the [Oxc](https://oxc.rs) toolchain — an extremely fast Rust linter, a quick alternative to ESLint. For `.vue` / `.svelte` / `.astro` it lints the embedded `<script>` (confirmed against the pinned release). oxlint is JS/TS only — there is no markdown/CSS/YAML linting (that is the formatter's job; see [format/oxc](#formatoxc)).

Implementation:

- This is a declarative check (`runtime: declarative-v1`), not built-in Rust code. It runs `oxlint --format=json --no-error-on-unmatched-pattern <files>` over the changed files in one batch invocation and converts the JSON diagnostics to findings (file path, 1-based line/column, rule code, severity, and message). The invocation runs in lint-CHECK mode — it never writes fixes.
- oxlint's per-diagnostic severity is preserved: `error` maps to a checkleft `error` finding, `warning` to `warning`, and anything else to `info`. Each finding's message is prefixed with the rule code (e.g. `eslint(no-debugger): ...`); diagnostics without a rule code (such as parse errors) fall back to an `oxlint:` prefix.
- `--no-error-on-unmatched-pattern` makes an all-ignored file set a no-op (clean exit) instead of a hard error — the analogue of Prettier's `--ignore-unknown`.

Config — none required:

- **Unlike `lint/js`, `lint/oxc` requires no config key.** oxlint is zero-config by design: it ships a built-in default rule set (the `correctness` category) and auto-discovers the repo's own `.oxlintrc.json` / nested configs (invocations run with cwd = repo root) to refine it. With no `.oxlintrc.json` present, it lints with oxlint's defaults; with one present, it is picked up automatically. (ESLint 9+ has no built-in defaults and so forces `lint/js` to make `config_file` mandatory — oxlint does not.)

Tool provisioning and version pinning:

- By default oxlint is provisioned via `npx --yes oxlint@<version>`, pinned to **1.70.0** (the latest stable at authoring time; an operator-confirmed pin is pending). oxlint is distributed as the standalone `oxlint` npm package with an `oxlint` binary. The version is part of the npm package spec, so npx runs exactly that release regardless of any globally-installed copy — a reproducible tool without a separate Bazel JS toolchain.
- oxlint is 1.x/stable, so the JSON reporter shape used by the transform is reliable across patch releases.
- Re-pin the version per repo through the `needs` binding (the package name is inherited from the default `npm` binding):

```yaml
checks:
  - id: lint/oxc
    config:
      needs:
        oxlint:
          npm:
            version: "1.71.0"
```

- When `npx` is not on `PATH`, the check falls back to an `oxlint` binary on `PATH` and warns loudly on stderr that the pinned toolchain was skipped. A repo with a hermetic Bazel JS toolchain can instead point the binding at a Bazel target with `needs.oxlint.bazel: "<label>"`, or at an explicit path with `needs.oxlint.path: "<path>"` — no change to the bundled definition required.

Notes:

- oxlint is invoked in batch mode (once per changed-file set, not once per file), which is more efficient for large changesets.
- Each finding's remediation is to fix the violation or suppress it with a justified `// oxlint-disable-next-line <rule>` comment.
- oxlint's per-finding severity is preserved end-to-end, so no policy configuration is needed to get the error/warning/info distinction. To make all findings non-blocking, set `policy.severity: warning`, which overrides every finding's severity regardless of what oxlint reported.
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
- `exclude_files` (optional array of glob strings; `exclude_globs` is a supported alias — legacy in-`config` position, still honored)

Notes:

- Findings default to `warning`. Override per instance with `[checks.policy].severity`.
- Enable bypass per instance with `[checks.policy].allow_bypass`.
- There is only one bundled check for file size. The `check: file/size` field in CHECKS config lets you create a named instance (e.g. `id: my-size-limit`) of the same underlying implementation — this is the aliasing feature, not a separate check.
- For new configuration, prefer the framework-level `exclude` key (sibling to `config:`) over `exclude_files` inside `config:`. Both are equivalent and enforced by the framework; see [Excluding files from checks](checks-config.md#excluding-files-from-checks).

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
- id: api-surface-docs # local policy label (drives findings/bypass/severity)
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
