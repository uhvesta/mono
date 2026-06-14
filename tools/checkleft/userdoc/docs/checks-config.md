# Configuring checks

`CHECKS.yaml` (preferred) or `CHECKS.toml` defines which checks run and how each check is configured. Both formats are equivalent; YAML is the recommended choice for new repos.

## File location and hierarchy

- Put a root `CHECKS.yaml` at repo root for default policy.
- Add child `CHECKS.yaml` files in subdirectories for scoped overrides.
- For a changed file, checks are resolved from root to that file's directory.
- Child entries override parent entries when `id` is the same.

## Top-level structure

```yaml
# Root CHECKS.yaml (applies repo-wide unless overridden in child directories).
# Each checks entry defines one configured check instance.
checks:
  - id: file/size

    # Optional; defaults to true.
    enabled: true

    # Check-specific config passed to the file/size implementation.
    config:
      max_lines: 500

    # Optional shared policy controls (applied by the framework, not check code).
    policy:
      severity: error
      allow_bypass: true
      bypass_name: BYPASS_FILE_SIZE
```

TOML is also supported:

```toml
# Root CHECKS.toml
[[checks]]
id = "file/size"
enabled = true

[checks.config]
max_lines = 500

[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_FILE_SIZE"
```

## `settings`

Supported keys:

- `include_config_files` (boolean, default `false`)
- `external_checks_url` (string, root config only)

When `false`, changed `CHECKS.yaml` / `CHECKS.toml` files are excluded from check scheduling.

When `external_checks_url` is set in the repository root config, `checkleft`
fetches that remote `CHECKS.yaml` or `CHECKS.toml`, applies it first, and then
merges the local root config and any child configs on top.

## `check_definitions`

Optional top-level section controlling where first-party check definitions are loaded from.

```yaml
check_definitions:
  exec_paths:
    - tools/checkleft/checks   # relative dir(s) containing check definition yaml files
  allow_override_bundled: true
```

Supported keys:

- `exec_paths` (list of strings): relative directories to search for check-definition yaml files. Each definition lives at `<dir>/<name>/check.yaml`. Inherited by child configs.
- `allow_override_bundled` (boolean, default `false`): when `true`, a definition found in `exec_paths` with the same name as a bundled definition takes precedence over the bundled copy. When `false` (the default), bundled definitions win.

`exec_paths` is not allowed in remotely-fetched external configs — a path source would reach into the consuming repo's local filesystem.

**Default behavior (no `check_definitions` section):** a check whose `id` (or `check`) matches the name of a first-party bundled definition resolves to that bundled def automatically — no `implementation:` line needed, no install required.

## `checks` entry

Supported keys:

- `id` (required): check instance ID used in output.
- `check` (optional): check definition name; defaults to `id`.
- `implementation` (optional): explicit external package reference — `generated:<id>` or a checked-in manifest path. For first-party (bundled) and exec-path checks, omit this field; resolution is automatic from the `id`/`check` name.
- `enabled` (optional, default `true`): disable with `false`.
- `config` (optional table): check-specific configuration.
- `policy` (optional table): framework-managed severity/bypass controls.

`policy` keys:

- `severity` (optional `error|warning|info`): overrides finding severity for the check instance.
- `allow_bypass` (optional boolean): enables BYPASS directives for the check instance.
- `bypass_name` (optional string): directive name; defaults to `BYPASS_<ID>` if omitted.

## Pattern: First-party (bundled) check — zero install

First-party checks whose definitions ship inside the `checkleft` binary resolve automatically from `id` (or `check`). No `implementation:` line needed:

```yaml
checks:
  - id: format/bazel
```

With a custom instance ID:

```yaml
checks:
  - id: my-bazel-format
    check: format/bazel
```

## Pattern: Always-head definitions from disk (e.g. mono)

A repo that maintains its own check definitions on disk can point `exec_paths` at them. With `allow_override_bundled: true`, the on-disk copy takes precedence over the bundled snapshot — so the repo always runs its checked-in (head) version:

```yaml
check_definitions:
  exec_paths:
    - tools/checkleft/checks
  allow_override_bundled: true

checks:
  - id: format/bazel   # resolves to tools/checkleft/checks/format/bazel.yaml
```

## Pattern: Multiple instances of one implementation

You can instantiate the same implementation more than once by using unique IDs with `check: ...`.

```yaml
checks:
  - id: forbidden-generated-outputs
    check: forbidden-paths
    config:
      rules:
        - remediation: "Generated outputs must not be checked in. Remove them from the change."
          when: [added, modified, renamed]
          patterns: ["**/target/**", "**/node_modules/**"]

  - id: forbidden-ios-build-dir
    check: forbidden-paths
    config:
      rules:
        - remediation: "iOS build directories must not be checked in. Remove them from the change."
          when: [added, modified, renamed]
          patterns: ["mobile/ios/.build/**"]
```

## Pattern: Repo-local external check from a generated index

```yaml
checks:
  - id: frontend-no-legacy-api
    check: frontend-no-legacy-api
    implementation: generated:frontend-no-legacy-api
```

Generated implementations are resolved through the configured generated index,
for example from a Bazel-produced `check_index` target.

## Pattern: Disable a parent check in a child directory

Root `CHECKS.yaml`:

```yaml
checks:
  - id: file/size
```

`backend/generated/CHECKS.yaml`:

```yaml
checks:
  - id: file/size
    enabled: false
```

## Validation notes

- Unknown `check` implementation IDs produce an error finding.
- Invalid check config shapes are surfaced as check execution errors.
- Invalid `policy.severity` values fail config resolution.
