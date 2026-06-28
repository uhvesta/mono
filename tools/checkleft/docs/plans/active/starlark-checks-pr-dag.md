# Starlark Checks PR DAG

Each PR in this DAG must be independently compilable and testable. A child PR may
depend on its parent, but no PR should contain code that only compiles after a
later PR lands.

The active review stack lives in `uhvesta/mono` so GitHub can show each PR
relative to the previous node branch. The same DAG can be mirrored to
`spinyfin/mono` later if needed.

## Operating Model

- Push every branch to `origin` (`uhvesta/mono`).
- Base every PR on the previous node branch.
- Validate each node with focused `bazel test` targets before pushing.
- When the spec or operating model changes, update the earliest affected node
  first and propagate the change through every dependent branch.
- Keep `package.toml` producer-only: package identity, publishing metadata, and
  version-set membership.
- Keep validation policy in `CHECKS.yaml`: selected packages/version sets, local
  package paths, path scoping, excludes, check configuration, severity, and
  policy.
- Do not introduce `PACKAGE.lock`. Exact refs plus hashes on package and
  version-set selections provide the reproducibility boundary. Hash pins are
  canonical lowercase 64-hex SHA-256 digests.
- Do not model public/private check visibility in v1. A check in a selected
  package is opt-in runnable; a version set opts into all checks from all
  included packages.
- Use Bazel for check-author integration: fixture tests should be schedulable by
  Bazel, and publishable package archives should be buildable by Bazel.

## Current Stack

| Node | PR | Base | Head | Scope |
| --- | --- | --- | --- | --- |
| 0 | `uhvesta/mono#2` | `main` | `uhvesta/starlark-checks-spec` | Spec/design |
| 1 | `uhvesta/mono#4` | `uhvesta/starlark-checks-spec` | `abarzega/starlark-checks-impl` | Evaluator foundation |
| 2 | `uhvesta/mono#5` | `abarzega/starlark-checks-impl` | `abarzega/starlark-checks-discovery` | Package manifest and discovery |
| 3 | `uhvesta/mono#6` | `abarzega/starlark-checks-discovery` | `abarzega/starlark-checks-loader` | `load()` path resolution |
| 4 | `uhvesta/mono#7` | `abarzega/starlark-checks-loader` | `abarzega/starlark-checks-runner` | Local runner integration |
| 5 | `uhvesta/mono#8` | `abarzega/starlark-checks-runner` | `abarzega/starlark-checks-adapters` | Adapter registry and text adapter |
| 6 | `uhvesta/mono#9` | `abarzega/starlark-checks-adapters` | `abarzega/starlark-checks-text-tests` | Text package fixture tests |
| 7 | `uhvesta/mono#10` | `abarzega/starlark-checks-text-tests` | `abarzega/starlark-checks-activation` | `CHECKS.yaml` activation |
| 8 | `uhvesta/mono#11` | `abarzega/starlark-checks-activation` | `abarzega/starlark-checks-packaging` | Package tarball and Bazel packaging |
| 9 | `uhvesta/mono#12` | `abarzega/starlark-checks-packaging` | `abarzega/starlark-checks-policy-guard` | Self-hosted `CHECKS.yaml` policy guard |
| 10 | `uhvesta/mono#13` | `abarzega/starlark-checks-policy-guard` | `abarzega/starlark-checks-fixes` | Starlark fix support |
| 11 | `uhvesta/mono#3` | `abarzega/starlark-checks-fixes` | `abarzega/starlark-checks-proto-adapter` | Proto adapter |
| 12 | `uhvesta/mono#14` | `abarzega/starlark-checks-proto-adapter` | `abarzega/starlark-checks-module-json-adapter` | `module_json` adapter |
| 13 | `uhvesta/mono#15` | `abarzega/starlark-checks-module-json-adapter` | `abarzega/starlark-checks-java-adapter` | Java adapter |

## Node Scopes

### Node 0: Spec/Design

Scope:
- Document the Starlark-backed checkleft API.
- Define the split between producer packaging metadata and consumer validation
  policy.
- Specify package distribution, version sets, text package tests, fixes, Bazel
  check-author integration, and adapter semantics.

Required verification:
- Documentation review.

### Node 1: Evaluator Foundation

Scope:
- Add the Meta `starlark` Rust dependency and Bazel lockfile updates.
- Add `src/starlark/` with an isolated evaluator foundation.
- Evaluate one Starlark check source against a text evolution context.
- Provide hermetic globals for `check_meta`, `finding`, `fail`,
  `fail_but_overridable`, regex helpers, and glob matching.
- Map Starlark findings to existing `crate::output::Finding`.
- Keep discovery, package activation, fixes, and non-text adapters out of scope.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

### Node 2: Package Manifest And Directory Discovery

Scope:
- Parse `checkleft/package.toml` as producer metadata only.
- Discover local checks from `checkleft/<adapter>/<nested/name>/check.checkleft`.
- Validate unknown adapters, missing package manifests, and invalid `.checkleft`
  placement.
- Add changeset-scoped ancestor discovery without full-repo walking.
- Return discovered checks without yet wiring automatic runner execution.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

### Node 3: Load Resolution

Scope:
- Implement `load("//lib/name", ...)` and `load(":helper", ...)`.
- Enforce package-local, ancestor-lib, and check-local load boundaries.
- Reject dependency-style `@dep//` imports; package dependencies provide checks,
  not importable libraries.
- Add tests for successful loads and boundary violations.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

### Node 4: Runner Integration For Local Text Checks

Scope:
- Wire discovered local Starlark checks into the existing runner path.
- Filter checks by `check_meta(applies_to = ...)` before evaluation.
- Preserve existing Rust, declarative, and WASM check behavior.
- Emit configuration/runtime failures as checkleft findings or errors according
  to the spec.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

### Node 5: Adapter Registry And Text Adapter

Scope:
- Introduce `FormatAdapter` and `AdapterRegistry`.
- Move the text context builder behind the `text` adapter.
- Group applicable checks by adapter and share parsed output for each
  adapter/file-set pair where practical.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`

### Node 6: Text Package Fixture Tests

Scope:
- Add `checkleft test` for check-author package fixtures.
- Preserve path-based author semantics:
  `checkleft/<adapter>/<nested/name>/check.checkleft` plus sibling
  `testdata/<case>/`.
- Run fixture cases with `before/`, `after/`, `expected.toml`, and optional
  `expected_fix/`.
- Support `checkleft test --update` to regenerate `expected.toml` snapshots from
  actual findings.
- Add `starlark_check_test` Bazel author-test integration and use it for the
  checked-in fixture package.
- Add the Checkleft Bazel toolchain used by author-test and validation rules.
- Exercise the full text path: nested check IDs, lib loading, expected findings,
  fixes when present, and path scoping.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test`

### Node 7: `CHECKS.yaml` Activation

Scope:
- Add Starlark package and version-set selection to `CHECKS.yaml`.
- Support local path package directories and local `.tar.gz` package archives
  for iteration.
- Keep package selection, path scoping, excludes, and check configuration in
  consumer validation policy.
- Make version-set selection activate all checks from all included packages.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test`

### Node 8: Package Tarball And Bazel Packaging

Scope:
- Build publishable `.tar.gz` archives containing `package.toml`, selected
  check/fix files, and internal libs required by those checks.
- Exclude transient test artifacts from publishable packages.
- Add `starlark_check_package` Bazel integration for check authors to build
  package archives.
- Allow consumers to point at local package paths during iteration.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test`

### Node 9: Self-Hosted `CHECKS.yaml` Policy Guard

Scope:
- Add a Starlark policy guard check targeting `CHECKS.yaml` and `CHECKS.toml`.
- Fail downgrades of selected package/version-set versions.
- Fail removal of hardcoded protected entries.
- Keep this policy as a normal Starlark check so organizations can supply it
  through their own always-merged root policy.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 10: Starlark Fix Support

Scope:
- Evaluate sibling `fix.checkleft` files.
- Map Starlark `file_edit(...)` values to existing fix scheduler types.
- Carry relevant finding data from check evaluation into fix evaluation.
- Add fixture coverage for expected fixes.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 11: Proto Adapter

Scope:
- Add the `proto` Starlark adapter as the highest-priority non-text adapter.
- Use an existing descriptor/proto crate path; do not shell out directly to
  `protoc` from the adapter.
- Expose typed proto file, message, field, enum, service, and delta data to
  Starlark.
- Add nested-path fixture coverage and production runner coverage.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 12: `module_json` Adapter

Scope:
- Add typed `module_json` parsing, diffing, and Starlark context values.
- Expose module metadata, dependencies, dev dependencies, custom metadata, and
  structured deltas.
- Add nested-path fixture coverage and production runner coverage.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 13: Java Adapter

Scope:
- Add Java public API extraction using `tree-sitter-java`.
- Expose package/import/class/method/field data and API deltas.
- Add nested-path fixture coverage and production runner coverage.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`
