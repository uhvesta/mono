# Configuring checks

`CHECKS.toml` defines which checks run and how each check is configured.

## File location and hierarchy

- Put a root `CHECKS.toml` at repo root for default policy.
- Add child `CHECKS.toml` files in subdirectories for scoped overrides.
- For a changed file, checks are resolved from root to that file's directory.
- Child entries override parent entries when `id` is the same.

## Top-level structure

```toml
# Root CHECKS.toml (applies repo-wide unless overridden in child directories).
# Each [[checks]] block defines one configured check instance.
[[checks]]

# Check instance ID (used in output and for override matching in child configs).
id = "file-size"

# Optional; defaults to true.
enabled = true

# Check-specific config passed to the file-size implementation.
[checks.config]
max_lines = 500

# Optional shared policy controls (applied by the framework, not check code).
[checks.policy]
severity = "error"
allow_bypass = true
bypass_name = "BYPASS_FILE_SIZE"
```

## `[settings]`

Supported keys:

- `include_config_files` (boolean, default `false`)
- `external_checks_url` (string, root config only)

When `false`, changed `CHECKS.toml` files are excluded from check scheduling.

When `external_checks_url` is set in the repository root config, `checkleft`
fetches that remote `CHECKS.yaml` or `CHECKS.toml`, applies it first, and then
merges the local root config and any child configs on top.

## `[[checks]]` entry

Supported keys:

- `id` (required): check instance ID used in output.
- `check` (optional): implementation ID; defaults to `id`.
- `implementation` (optional): external package reference, either a checked-in manifest path or `generated:<id>`.
- `enabled` (optional, default `true`): disable with `false`.
- `config` (optional table): check-specific configuration.
- `policy` (optional table): framework-managed severity/bypass controls.

`[checks.policy]` keys:

- `severity` (optional `error|warning|info`): overrides finding severity for the check instance.
- `allow_bypass` (optional boolean): enables BYPASS directives for the check instance.
- `bypass_name` (optional string): directive name; defaults to `BYPASS_<ID>` if omitted.

## Pattern: Multiple instances of one implementation

You can instantiate the same implementation more than once by using unique IDs with `check = ...`.

```toml
[[checks]]
id = "forbidden-generated-outputs"
check = "forbidden-paths"

[[checks.config.rules]]
remediation = "Generated outputs must not be checked in. Remove them from the change."
when = ["added", "modified", "renamed"]
patterns = ["**/target/**", "**/node_modules/**"]

[[checks]]
id = "forbidden-ios-build-dir"
check = "forbidden-paths"

[[checks.config.rules]]
remediation = "iOS build directories must not be checked in. Remove them from the change."
when = ["added", "modified", "renamed"]
patterns = ["mobile/ios/.build/**"]
```

## Pattern: Repo-local external check from a generated index

```toml
[[checks]]
id = "frontend-no-legacy-api"
check = "frontend-no-legacy-api"
implementation = "generated:frontend-no-legacy-api"
```

Generated implementations are resolved through the configured generated index,
for example from a Bazel-produced `check_index` target.

## Pattern: Disable a parent check in a child directory

Root:

```toml
[[checks]]
id = "file-size"
```

`backend/generated/CHECKS.toml`:

```toml
[[checks]]
id = "file-size"
enabled = false
```

## Validation notes

- Unknown `check` implementation IDs produce an error finding.
- Invalid check config shapes are surfaced as check execution errors.
- Invalid `policy.severity` values fail config resolution.
