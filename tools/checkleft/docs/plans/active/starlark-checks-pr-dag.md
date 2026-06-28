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
- Keep `checkleft-package.toml` producer-only: package identity, publishing
  metadata, and version-set membership.
- Keep validation policy in `CHECKS.yaml`: selected packages/version sets, local
  package paths, path scoping, excludes, severity, and policy.
- Exact refs plus hashes on package and version-set selections provide the
  reproducibility boundary. Hash pins are canonical lowercase 64-hex SHA-256
  digests.
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
| 14 | `uhvesta/mono#16` | `abarzega/starlark-checks-java-adapter` | `abarzega/starlark-checks-git-resolver` | Git package resolver |
| 15 | `uhvesta/mono#17` | `abarzega/starlark-checks-git-resolver` | `abarzega/starlark-checks-routing-selection` | Routing and exact package selection |
| 16 | `uhvesta/mono#18` | `abarzega/starlark-checks-routing-selection` | `abarzega/starlark-checks-selector-policy` | Remove selector-local Starlark config |
| 17 | `uhvesta/mono#19` | `abarzega/starlark-checks-selector-policy` | `abarzega/starlark-checks-bazel-author-tests` | Harden Bazel author tests |
| 18 | `uhvesta/mono#20` | `abarzega/starlark-checks-bazel-author-tests` | `abarzega/starlark-checks-enable-spec` | Full enablement model and Linguist mapping |
| 19 | `uhvesta/mono#21` | `abarzega/starlark-checks-enable-spec` | `abarzega/starlark-checks-package-activation-globs` | Package activation path policy |
| 20 | `uhvesta/mono#22` | `abarzega/starlark-checks-package-activation-globs` | `abarzega/starlark-checks-adapter-file-selectors` | Adapter `ext`/`name` selectors and uniqueness |

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
- Parse `checkleft-package.toml` as producer metadata only.
- Discover local checks from `<package_root>/<adapter>/<nested/name>/check.checkleft`.
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
- Filter checks by adapter file selectors before evaluation.
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
  `<package_root>/<adapter>/<nested/name>/check.checkleft` plus sibling
  `testdata/<case>/`.
- Run fixture cases with `before/`, `after/`, `expected.toml`, and optional
  `expected_fix/`.
- Support `checkleft test --update` to regenerate `expected.toml` snapshots from
  actual findings.
- Add `checkleft_test` Bazel author-test integration and use it for the
  checked-in fixture package.
- Use the compiled-from-source `//tools/checkleft:checkleft` binary in
  author-test and validation rules.
- Exercise the full text path: nested check IDs, lib loading, expected findings,
  fixes when present, and path scoping.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test`

### Node 7: `CHECKS.yaml` Activation

Scope:
- Add Starlark package and version-set selection to `CHECKS.yaml`.
- Support local path package directories and local `.tar.gz` package archives
  for iteration.
- Keep package selection, path scoping, and excludes in consumer validation
  policy.
- Make version-set selection activate all checks from all included packages.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test`

### Node 8: Package Tarball And Bazel Packaging

Scope:
- Build publishable `.tar.gz` archives containing `checkleft-package.toml`, selected
  check/fix files, and internal libs required by those checks.
- Exclude transient test artifacts from publishable packages.
- Add `checkleft_package` Bazel integration for check authors to build
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

### Node 14: Git Package Resolver

Scope:
- Resolve `git://` package refs through archive bytes pinned by `sha256`.
- Strip repository package roots into the same archive-root layout used by
  package tarballs.
- Preserve local `path://` directory and archive iteration.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 15: Routing And Exact Package Selection

Scope:
- Clarify and enforce exact package selection keys: kind, source, version, and
  `sha256`.
- Keep package/version-set resolution deterministic without a lockfile or
  transitive dependency solver.
- Preserve duplicate-ref de-duplication only for exact equivalent refs.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 16: Selector Policy Cleanup

Scope:
- Remove configurable/embeddable Starlark package config from `CHECKS.yaml`.
- Treat `checks:` entries for Starlark packages as activation/path selectors
  only.
- Reject selector-local `config` for Starlark package checks.
- Apply top-level global excludes before Starlark package scheduling.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 17: Bazel Author-Test Hardening

Scope:
- Add Bazel coverage for full text package authoring.
- Exercise `checkleft test` all-test discovery, package archive construction,
  `fix.checkleft` inclusion, libs, nested paths, and path-based fixture
  semantics.
- Keep custom-check author iteration through Bazel first-class.

Required verification:
- `bazel test //tools/checkleft:starlark_text_fixture_checkleft_all_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test`

### Node 18: Enablement Model Spec And Linguist Mapping

Scope:
- Document the full Starlark enablement model across package selection, explicit
  activation selectors, path policy, adapter selectors, and global excludes.
- Clarify the public API as producer package identity, check/fix
  implementation, consumer activation, and Rust adapter registration.
- Add `.gitattributes` so GitHub Linguist treats `*.checkleft` as Starlark.

Required verification:
- `bazel test //tools/checkleft:starlark_text_fixture_checkleft_all_test //tools/checkleft:starlark_text_fixture_package_archive_test`

### Node 19: Package Activation Path Policy

Scope:
- Parse `include` and `exclude` on `CHECKS.yaml` Starlark check activation
  selectors.
- Normalize activation globs relative to the declaring `CHECKS.yaml` directory.
- Apply activation globs when selecting packages and when building each
  Starlark check changeset.
- Keep exact duplicate package refs separate when they activate different repo
  areas.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test`
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test //tools/checkleft:starlark_text_package_test //tools/checkleft:starlark_text_fixture_checkleft_test //tools/checkleft:starlark_text_fixture_package_archive_test //tools/checkleft:checks_policy_guard_package_test`

### Node 20: Adapter File Selectors

Scope:
- Replace adapter parseable globs with explicit file selectors: `ext:
  <extension>` and `name: <basename>`.
- Enforce selector uniqueness at adapter registry startup: two adapters cannot
  claim the same extension or basename.
- Filter changed files through adapter selectors before adapter preparation.
- Document and test examples: `ext: proto` matches `a.proto`; `name:
  module-info.json` matches `a/b/c/module-info.json`.

Required verification:
- `bazel test //tools/checkleft:checkleft_lib_test //tools/checkleft:checkleft_bin_test`
