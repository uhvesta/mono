# Typed JS Repo-Local Check Example

This example shows the `exec-v1` Bazel-backed local-check flow using the
public `@checkleft/exec` API package and an `aspect_rules_js` `js_binary`
launcher.

The example is intentionally written as runnable `.mjs` with `// @ts-check`
and imports `@checkleft/exec` the same way a consuming Bazel repo would after
linking the package with `npm_link_package(...)`. Unlike the earlier
shell-wrapper version, the example now uses Bazel's Node launcher directly so
the only handwritten check code is the check itself.

## Files

- `check.mjs`: the executable check entrypoint
- `check_test.mjs`: a unit test that imports `runCheck` from `check.mjs`
- `CHECKS.toml`: example repo config that points at `generated:no-debug-logging`
- `fixture.ts`: sample file the check will flag
- `BUILD.bazel`: `npm_link_package`, `js_binary`, `js_test`, `local_check`,
  and `checkleft(...)` wiring with `checks = [...]`

## What The Check Does

It scans changed JS/TS files for configured debug-call patterns, defaulting to
`console.log`.

## Build The Example

```bash
bazel build //tools/checkleft/examples/typed_js_local_check:run_example
```

## Run The Unit Test

```bash
bazel test //tools/checkleft/examples/typed_js_local_check:check_test
```

## Run The Check Binary Through Bazel

```bash
cat <<'EOF' | CHECKLEFT_REPO_ROOT="$PWD" bazel run //tools/checkleft/examples/typed_js_local_check:no_debug_logging_bin
{
  "changeset": {
    "changed_files": [
      {
        "path": "tools/checkleft/examples/typed_js_local_check/fixture.ts",
        "kind": "modified",
        "old_path": null
      }
    ],
    "file_line_deltas": {},
    "file_diffs": {},
    "commit_description": null,
    "pr_description": null,
    "change_id": null,
    "repository": null
  },
  "config": {
    "forbidden_calls": ["console.log"],
    "include_extensions": [".ts"]
  }
}
EOF
```

## Bazel Wiring

`run_example` still shows the intended repo-level wrapper target:

```starlark
checkleft(
    name = "run_example",
    checks = [":no-debug-logging"],
)
```

The example package also links the public API package locally so the bare
import resolves under Bazel:

```starlark
npm_link_package(
    name = "node_modules/@checkleft/exec",
    root_package = "",
)
```

`CHECKS.toml` is included to show what a consuming repo would configure when it
registers the generated check in its own repo config.
