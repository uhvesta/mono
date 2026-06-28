# Starlark Checks — Implementation Plan

**Status:** Draft
**Date:** 2026-06-25
**Design spec:** [`starlark-checks-spec.md`](./starlark-checks-spec.md)

This document is the implementation roadmap. It describes what Rust code to write, where it goes, and in what order. Read the design spec for the full API surface and user-facing semantics.

---

## Existing architecture (what we have)

```
src/
├── check.rs              # Check + ConfiguredCheck traits, CheckRegistry
├── checks/               # 11 built-in Rust checks (typo, bazel, code_patterns, etc.)
├── config.rs             # CHECKS.yaml/CHECKS.toml parsing → CheckConfig
├── external/
│   ├── mod.rs            # ExternalCheckPackage, implementation ref types
│   ├── bundled.rs        # Bundled declarative + WASM checks (include_str!)
│   ├── declarative/      # Framework-owned invocation (YAML manifests → tool exec → findings)
│   ├── runtime.rs        # WASM Component Model executor
│   └── sandbox.rs        # Capability sandbox
├── input.rs              # ChangeSet, ChangedFile, SourceTree trait
├── output.rs             # Finding, CheckResult, Severity, Location
├── runner.rs             # Async check orchestration
└── fix/                  # Fix scheduler + safety
```

Key points:
- `Check` trait → `configure()` → `ConfiguredCheck` → `run(changeset, tree) → CheckResult`
- External checks are either **declarative** (YAML manifest → run a tool → parse output) or **component** (WASM)
- `SourceTree` trait provides `read_file`, `exists`, `list_dir`, `glob`
- `tree-sitter-starlark` is already a dependency (used by Bazel checks for syntax parsing)
- Output types: `Finding { severity, message, location/file span, remediation, fix_data }`
- Current `Severity` enum: `Error, Warning, Info` — needs mapping to spec's `fail / fail_but_overridable`

---

## New code — where things go

```
src/
├── starlark/                          # NEW — all Starlark check infrastructure
│   ├── mod.rs                         # Public API: StarlarkCheckRunner, discover(), types re-export
│   ├── discovery.rs                   # Package directory discovery
│   ├── manifest.rs                    # checkleft-package.toml parsing (PackageManifest)
│   ├── loader.rs                      # Starlark load() resolution (// and : prefixes)
│   ├── check_meta.rs                  # check_meta() built-in function + CheckMeta struct
│   ├── evaluator.rs                   # Starlark Module setup, Globals construction, check(ctx) invocation
│   ├── fix_evaluator.rs               # fix(ctx, findings) invocation + FileEdit collection
│   ├── types.rs                       # Starlark ↔ Rust type bridge (Finding, FileEdit, Severity, etc.)
│   ├── sandbox.rs                     # Tier-based Globals construction (hermetic vs network)
│   └── adapter/                       # Format adapter infrastructure
│       ├── mod.rs                     # FormatAdapter trait + AdapterRegistry
│       ├── proto.rs                   # Proto adapter (native descriptor provider → Starlark values)
│       ├── module_json.rs             # module.json adapter
│       ├── java.rs                    # Java adapter (tree-sitter-java → API surface model)
│       └── text.rs                    # Text adapter (line model)
```

This is a new top-level module under `src/`. It does **not** live inside `external/` — Starlark checks are a distinct execution tier alongside built-in, declarative, and component checks.

---

## Implementation phases

### Phase 1: Core evaluation loop (MVP — one hardcoded adapter)

**Goal:** Run a single Starlark `check.checkleft` file against a `text` adapter context and produce `Finding` values. No discovery, no packages, no fixes — just prove the Starlark ↔ Rust bridge works.

**Files:**

| File | What it does |
|---|---|
| `starlark/mod.rs` | Module root. Expose `StarlarkCheckRunner`. |
| `starlark/types.rs` | `#[derive(StarlarkValue)]` impls for `Finding`, `Severity`, `FileEdit`, `Location`. The `finding()` and `fail()` / `fail_but_overridable()` constructor functions as Starlark globals. |
| `starlark/sandbox.rs` | Build a `GlobalsBuilder` for the hermetic tier: inject `finding`, `fail`, `fail_but_overridable`, `Severity`, `DeltaKind`, `regex_match`, `regex_find_all`, `glob_match`, `print`. |
| `starlark/check_meta.rs` | `check_meta()` as a Starlark built-in. Parses and stores `tier` from the top-level call. |
| `starlark/evaluator.rs` | Load a `.checkleft` file into a `Module`, configure `Dialect { enable_types: DialectTypes::Enable }`, attach globals, evaluate, call `check(ctx)`, collect `Vec<Finding>`. |
| `starlark/adapter/mod.rs` | `FormatAdapter` trait definition (as per spec §6.1). `AdapterRegistry` for registration. |
| `starlark/adapter/text.rs` | `TextAdapter` — parse files into `TextFilePair` / `TextFile` / `Line` Starlark values. Simplest adapter, good for proving the pipeline. |

**Key Rust crate:** [`starlark`](https://crates.io/crates/starlark) (Facebook's Starlark implementation). Add to `Cargo.toml`:
```toml
starlark = { version = "0.12", features = ["typing"] }
```

**Integration point:** Implement `Check` trait on a new `StarlarkCheck` struct so it plugs into the existing `CheckRegistry` and runner. The `configure()` method takes the parsed `CheckMeta`, the `run()` method invokes the evaluator.

**Testable via:** A Rust integration test that loads a `.checkleft` file from `testdata/`, evaluates it against a synthetic `ChangeSet`, and asserts on the returned findings.

---

### Phase 2: Discovery + `checkleft-package.toml` + load paths

**Goal:** Auto-discover checks from package folder structure, parse `checkleft-package.toml`, resolve `load()` paths.

**Files:**

| File | What it does |
|---|---|
| `starlark/discovery.rs` | Scan package roots for `<adapter>/<name>/check.checkleft`. Return a list of `DiscoveredCheck { id, adapter, path, check_meta, package }`. |
| `starlark/manifest.rs` | Parse `checkleft-package.toml` into `PackageManifest { package: PackageIdentity, publish: PublishMetadata }`. Validate producer metadata only: package identity and publishing metadata. |
| `starlark/loader.rs` | Custom `FileLoader` impl for Starlark's `load()` statement. Resolve `//lib/foo` → `<checkleft_root>/lib/foo.checkleft`, `:types` → `<check_dir>/types.checkleft`. Enforce: no `@dep//` prefix (deps provide checks only, not importable libs). |

**Integration point:** `CHECKS.yaml` remains the consumer validation policy. It selects local packages and fetched packages. The runner resolves those package refs, calls discovery for the selected package roots, and hands discovered Starlark checks to the existing runner alongside built-in and external checks.

---

### Phase 3: Format adapters (proto, module_json, java)

**Goal:** Ship the three non-trivial adapters. Each one is independent — they can land in any order.

#### Proto adapter (`starlark/adapter/proto.rs`)

- Use the existing descriptor/proto crate path for descriptor generation/parsing; do not directly invoke `protoc` from the adapter.
- Parse `FileDescriptorSet` through the repository's native descriptor representation.
- Build `#[derive(StarlarkValue)]` types: `ProtoEvolutionContext`, `FileDescriptor`, `MessageDescriptor`, `FieldDescriptor`, `SchemaDelta`, etc.
- Diff logic: compare base and current descriptor sets → produce `Vec<SchemaDelta>` with `DeltaKind` variants
- Make vendored extension `.proto` files available through the descriptor provider

This is the largest adapter. Consider splitting into submodules:
```
starlark/adapter/proto/
├── mod.rs          # ProtoAdapter impl
├── descriptor.rs   # Starlark value wrappers for descriptor types
├── diff.rs         # Descriptor set diffing → SchemaDelta
└── provider.rs     # descriptor provider integration + descriptor set parsing
```

#### module_json adapter (`starlark/adapter/module_json.rs`)

- Parse `module.json` files with `serde_json` into a typed `ModuleJson` struct
- Diff: compare before/after → `Vec<ModuleJsonDelta>`
- Starlark values: `ModuleJsonEvolutionContext`, `ModuleJsonFilePair`, `ModuleJson`, `ModuleJsonDelta`

#### Java adapter (`starlark/adapter/java.rs`)

- Parse `.java` files with `tree-sitter-java` (already a dependency)
- Extract public API surface: classes, methods, fields, signatures, annotations
- Diff: compare before/after API surface → `Vec<JavaDelta>`
- Starlark values: `JavaEvolutionContext`, `JavaFilePair`, `JavaFile`, `JavaClass`, `JavaMethod`, `JavaDelta`

---

### Phase 4: Fix evaluation + fix_data

**Goal:** Run `fix.checkleft` files and produce `FileEdit` values. Wire into the existing fix scheduler.

**Files:**

| File | What it does |
|---|---|
| `starlark/fix_evaluator.rs` | Load `fix.checkleft`, call `fix(ctx, findings) → list[FileEdit]`. The `findings` list carries typed `fix_data` structs from the check evaluation (kept alive in the Starlark heap via `OwnedFrozenValue`). |
| `starlark/types.rs` (update) | Add `FileEdit` Starlark value, `file_edit()` constructor. Map to existing `crate::fix::FileEdit`. |

**Integration point:** The fix scheduler (`src/fix/scheduler.rs`) already orchestrates fixes from external checks. Starlark fixes produce the same `Vec<FileEdit>` output — plug into the existing pipeline.

---

### Phase 5: Versioned distribution and `CHECKS.yaml` activation

**Goal:** Fetch external check packages selected in `CHECKS.yaml` and activate the requested checks.

**Files:**

| File | What it does |
|---|---|
| `config.rs` (update) | Add `checkleft_packages` parsing to `CHECKS.yaml`: registry/git packages, local path packages, and activation/path selectors. |
| `starlark/manifest.rs` (update) | Keep producer metadata parsing focused on package identity and publishing metadata. |
| `starlark/resolver.rs` | Fetch packages from `registry://`, `git://`, `path://`. Cache fetched packages by `<name>/<version>/<sha256>`. Verify `sha256` before loading. `path://` supports live package directories and local publishable `.tar.gz` archives. Do not generate a lockfile. |
| `starlark/package.rs` | Resolve selected package refs. Individual packages support `all` or `explicit` activation. |

Package refs validate `sha256` pins as canonical lowercase 64-hex digests
before any fetch or package discovery step. `path://`
refs may omit `sha256` for local iteration; any supplied hash must still be
canonical. Local archive refs verify the archive bytes when `sha256` is present,
then discover checks from the archive-root package layout.

This phase makes Starlark checks a first-class `CHECKS.yaml` policy input without overloading `checkleft-package.toml`.

---

### Phase 6: Functional testing (`checkleft test`)

**Goal:** Run fixture-based tests for check authors.

**Files:**

| File | What it does |
|---|---|
| `starlark/testing.rs` | Preserve path-based author semantics: discover checks from `<adapter>/<nested/name>/check.checkleft`, scan sibling `testdata/<case>/` dirs, construct a synthetic `ChangeSet` from `before/` + `after/`, run the adapter + check, compare findings against `expected.toml`. If `expected_fix/` exists, run the fix and diff. |
| `bazel/defs.bzl` | Expose `checkleft_test` so check authors can schedule the real `checkleft test` CLI from Bazel. The checkleft binary is referenced directly as a compiled-from-source target. |

**CLI integration:** Add `checkleft test [check_id] [--update]` subcommand to `main.rs`.

---

## Adapter output sharing (performance)

Per the spec (§10.2), multiple checks under the same adapter share one parsed output. Implementation:

```rust
// In the runner, before dispatching checks:
let adapter_outputs: HashMap<String, Arc<dyn AdapterOutput>> = HashMap::new();

for adapter_kind in unique_adapters {
    let base = adapter.parse(paths, tree, TreeVersion::Base)?;
    let current = adapter.parse(paths, tree, TreeVersion::Current)?;
    let deltas = adapter.diff(&*base, &*current)?;
    adapter_outputs.insert(adapter_kind, Arc::new((base, current, deltas)));
}

// Each check borrows from the shared Arc
for check in checks_for_adapter {
    let output = adapter_outputs.get(&check.adapter_kind).unwrap();
    // inject_globals borrows output, allocates Starlark values in check's own Module heap
}
```

This is a runner-level concern, not an adapter concern. Implement in `starlark/mod.rs` or directly in `runner.rs`.

---

## Type mapping: spec → Rust

| Spec type | Rust representation |
|---|---|
| `Finding` | `#[derive(StarlarkValue)]` wrapper around `crate::output::Finding` |
| `Severity.fail` | Maps to `crate::output::Severity::Error` |
| `Severity.fail_but_overridable` | Maps to a blocking finding with `overridable = true`; GitHub annotation level remains `failure` |
| `FileEdit` | `#[derive(StarlarkValue)]` wrapper around `crate::fix::FileEdit` |
| `fix_data` | `OwnedFrozenValue` — opaque Starlark value, passed through from check to fix |
| `check_meta()` | Parsed into `CheckMeta { tier }` at module load time |
| `struct(...)` (user-defined) | Native Starlark `Struct` — no special Rust type needed |
| `load("//lib/foo", "bar")` | Custom `FileLoader` impl resolving to `.checkleft` files |

---

## Dependency: `starlark` crate

The [`starlark`](https://github.com/facebook/starlark-rust) crate (by Meta) provides:
- `Module`, `Evaluator`, `GlobalsBuilder` — core evaluation
- `#[starlark_module]` macro for defining built-in functions
- `#[derive(StarlarkValue)]`, `#[starlark_value]` for custom types
- `DialectTypes::Enable` for type checking
- `FileLoader` trait for custom `load()` resolution
- `FrozenValue` / `OwnedFrozenValue` for passing values between evaluations

`tree-sitter-starlark` (already a dep) is for **parsing** Starlark syntax. The `starlark` crate is for **evaluating** it. Both are needed.

---

## PR DAG and implementation order

For the active stack, create PRs relative to branches in `uhvesta/mono` so reviewers can see the native DAG. Upstream `spinyfin/mono` PRs can be mirrored later.

Already-started nodes:

1. Node 0: spec/design.
2. Node 1: evaluator foundation.
3. Node 2: package manifest/discovery scaffolding.
4. Node 3: `load()` path resolution.
5. Node 4: local Starlark runner integration.
6. Node 5: text adapter registry.

Next nodes:

1. Node 6: `checkleft test` for text packages and Bazel author-test integration. Preserve path-based author semantics (`<adapter>/<nested/name>/check.checkleft` plus sibling `testdata/<case>/`).
2. Node 7: `CHECKS.yaml` activation for Starlark packages.
3. Node 8: package tarball and Bazel packaging target for check authors.
4. Node 9: self-hosted Starlark guard check for `CHECKS.yaml` policy integrity.
5. Node 10: Starlark fix support.
6. Node 11: proto adapter. This is the highest-priority non-text adapter; use the existing descriptor/proto/native C++ path and do not directly invoke `protoc`.
7. Node 12: module_json adapter.
8. Node 13: Java adapter.

Each node must compile and pass its focused Bazel validation before pushing. Text remains the full lifecycle proof path; proto is the first non-text adapter because it is the highest product priority.
