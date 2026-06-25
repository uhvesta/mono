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
- Output types: `Finding { severity, message, location, remediations, suggested_fix }`
- Current `Severity` enum: `Error, Warning, Info` — needs mapping to spec's `fail / fail_but_overridable`

---

## New code — where things go

```
src/
├── starlark/                          # NEW — all Starlark check infrastructure
│   ├── mod.rs                         # Public API: StarlarkCheckRunner, discover(), types re-export
│   ├── discovery.rs                   # Changeset-scoped checkleft/ directory discovery
│   ├── manifest.rs                    # package.toml parsing (PackageManifest, VersionSet, Dependency)
│   ├── loader.rs                      # Starlark load() resolution (// and : prefixes)
│   ├── check_meta.rs                  # check_meta() built-in function + CheckMeta struct
│   ├── evaluator.rs                   # Starlark Module setup, Globals construction, check(ctx) invocation
│   ├── fix_evaluator.rs               # fix(ctx, findings) invocation + FileEdit collection
│   ├── types.rs                       # Starlark ↔ Rust type bridge (Finding, FileEdit, Severity, etc.)
│   ├── sandbox.rs                     # Tier-based Globals construction (hermetic vs network)
│   └── adapter/                       # Format adapter infrastructure
│       ├── mod.rs                     # FormatAdapter trait + AdapterRegistry
│       ├── proto.rs                   # Proto adapter (protoc → descriptor set → Starlark values)
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
| `starlark/check_meta.rs` | `check_meta()` as a Starlark built-in. Parses and stores `applies_to`, `tier`, `config` from the top-level call. |
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

### Phase 2: Discovery + `package.toml` + load paths

**Goal:** Auto-discover checks from `checkleft/` folder structure, parse `package.toml`, resolve `load()` paths.

**Files:**

| File | What it does |
|---|---|
| `starlark/discovery.rs` | Walk upward from changeset file paths to find ancestor `checkleft/` dirs. For each, scan `<adapter>/<visibility>/<name>/check.checkleft`. Return a list of `DiscoveredCheck { id, adapter, visibility, path, check_meta }`. |
| `starlark/manifest.rs` | Parse `package.toml` into `PackageManifest { package: PackageIdentity, version_sets: Vec<VersionSetRef>, dependencies: Vec<DependencyRef>, exclude_patterns: Vec<String> }`. Validate version set resolution rules (overlap = error, dep version > version set pin, no nested version sets). |
| `starlark/loader.rs` | Custom `FileLoader` impl for Starlark's `load()` statement. Resolve `//lib/foo` → `<checkleft_root>/lib/foo.checkleft`, `:types` → `<check_dir>/types.checkleft`. Enforce: no `@dep//` prefix (deps provide checks only, not importable libs). |

**Integration point:** The runner calls `discovery::discover(changeset)` → gets a list of checks → for each, constructs a `StarlarkCheck` → hands them to the existing runner alongside built-in and external checks. Discovery replaces the current `CheckConfig` resolution path for Starlark checks — they are never listed in `CHECKS.yaml`.

---

### Phase 3: Format adapters (proto, module_json, java)

**Goal:** Ship the three non-trivial adapters. Each one is independent — they can land in any order.

#### Proto adapter (`starlark/adapter/proto.rs`)

- Invoke `protoc --descriptor_set_out --include_source_info` at base and current revisions
- Parse `FileDescriptorSet` (use the `prost` or `protobuf` crate)
- Build `#[derive(StarlarkValue)]` types: `ProtoEvolutionContext`, `FileDescriptor`, `MessageDescriptor`, `FieldDescriptor`, `SchemaDelta`, etc.
- Diff logic: compare base and current descriptor sets → produce `Vec<SchemaDelta>` with `DeltaKind` variants
- Bundle vendored extension `.proto` files and always include them in protoc invocation

This is the largest adapter. Consider splitting into submodules:
```
starlark/adapter/proto/
├── mod.rs          # ProtoAdapter impl
├── descriptor.rs   # Starlark value wrappers for descriptor types
├── diff.rs         # Descriptor set diffing → SchemaDelta
└── invoke.rs       # protoc invocation + descriptor set parsing
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

### Phase 5: Versioned distribution (dependencies + version sets)

**Goal:** Fetch external check packages and activate their public checks.

**Files:**

| File | What it does |
|---|---|
| `starlark/manifest.rs` (update) | Add resolution logic for `[dependencies]` and `[version_sets]`. |
| `starlark/resolver.rs` | Fetch packages from `registry://`, `git://`, `path://`. Cache in `~/.cache/checkleft/packages/<name>/<version>/`. Generate/verify `PACKAGE.lock`. |
| `starlark/visibility.rs` | Enforce `public/` vs `private/` when activating checks from dependencies. Only `public/` checks from deps are activated. |

This is the least urgent phase — local checks work without it.

---

### Phase 6: Functional testing (`checkleft test`)

**Goal:** Run fixture-based tests for check authors.

**Files:**

| File | What it does |
|---|---|
| `starlark/testing.rs` | Scan `testdata/<case>/` dirs. For each: construct a synthetic `ChangeSet` from `before/` + `after/`, run the adapter + check, compare findings against `expected.toml`. If `expected_fix/` exists, run the fix and diff. |

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
| `Severity.fail_but_overridable` | Maps to `crate::output::Severity::Warning` |
| `FileEdit` | `#[derive(StarlarkValue)]` wrapper around `crate::fix::FileEdit` |
| `fix_data` | `OwnedFrozenValue` — opaque Starlark value, passed through from check to fix |
| `check_meta()` | Parsed into `CheckMeta { applies_to, tier, config, source }` at module load time |
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

## Suggested implementation order

```
Phase 1  ──→  Phase 2  ──→  Phase 3 (adapters can be parallel)
                              ├── proto
                              ├── module_json
                              └── java
                                    ↓
                              Phase 4 (fixes)
                                    ↓
                              Phase 5 (distribution)
                                    ↓
                              Phase 6 (testing CLI)
```

Phase 1 is the critical path — it proves the Starlark ↔ Rust bridge works end-to-end. Everything else builds on it. Phase 3 adapters are independent of each other and can be developed in parallel. Phase 5 (distribution) is lowest priority — local checks work without it.
