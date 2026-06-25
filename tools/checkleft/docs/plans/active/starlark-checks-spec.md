# Checkleft Starlark Checks Spec

**Status:** Draft
**Date:** 2026-06-24

---

## 1. Goals

1. Let users define **evolution checks** (proto, JSON schema, Java API surface, etc.) in typed Starlark with minimal Rust involvement.
2. Opinionated folder structure: one directory = one check. Shared helpers live alongside checks and are importable.
3. Sandbox tiers tied to folder placement — hermetic by default, opt-in network access.
4. Versioned check distribution — pull in third-party or org-published check packages at pinned versions.
5. Optional **fix** functions co-located with checks.
6. Bidirectional: checks can be authored in Starlark *or* Rust. Rust checks and Starlark checks share the same output types and runner pipeline.
7. Hierarchical: repos can define checks at the root; sub-projects can layer on their own.
8. Maximal Starlark typing via `DialectTypes::Enable` — all function signatures, parameters, and return types must carry type annotations.

---

## 2. Folder structure

### 2.1 The `checkleft/` directory

Every repository (or sub-project) that wants custom checks places a `checkleft/` directory at the relevant root. Inside it:

```
repo-root/
├── checkleft/
│   ├── package.toml                       # package manifest (required)
│   ├── lib/                               # shared helper modules (always private)
│   │   ├── matchers.checkleft
│   │   └── proto_helpers.checkleft
│   ├── proto/                             # adapter = proto
│   │   ├── public/                        # exported to consumers
│   │   │   └── evolution/
│   │   │       ├── check.checkleft
│   │   │       └── fix.checkleft
│   │   └── private/                       # local-only, not exported
│   │       └── team_policy/
│   │           └── check.checkleft
│   ├── module_json/                       # adapter = module_json
│   │   └── public/
│   │       └── required_fields/
│   │           └── check.checkleft
│   └── java/                              # adapter = java
│       └── public/
│           └── api_stability/
│               ├── check.checkleft
│               └── fix.checkleft
├── services/
│   └── payments/
│       └── checkleft/                     # nested project-level checks
│           ├── package.toml
│           └── proto/
│               └── private/               # project-specific, not exported
│                   └── billing_compat/
│                       └── check.checkleft
```

### 2.2 Rules

| Path pattern | Role |
|---|---|
| `checkleft/package.toml` | **Package manifest.** Declares metadata and external dependencies. Required. |
| `checkleft/lib/*.checkleft` | **Shared modules.** Importable helpers. Always private — never exported to consumers. |
| `checkleft/<adapter>/public/<name>/check.checkleft` | **Public check.** Exported when this package is consumed as a dependency. |
| `checkleft/<adapter>/private/<name>/check.checkleft` | **Private check.** Runs locally but not exported to consumers. |
| `checkleft/<adapter>/<visibility>/<name>/fix.checkleft` | **Fix definition.** Optional. Must export a `fix()` function. |
| `checkleft/<adapter>/<visibility>/<name>/check_test.checkleft` | **Check test.** Optional. Functional tests for the check. See §12. |
| `checkleft/<adapter>/<visibility>/<name>/*.checkleft` | **Check-local helpers.** Any other `.checkleft` file is a local helper, loadable only from within that check directory. |

The path structure is: `<adapter>/<visibility>/<name>`.

- **`<adapter>`** — selects the Rust format adapter (e.g. `proto`, `module_json`, `java`, `text`). Must match a registered `FormatAdapter::kind()`. This is the structural guarantee that every check under `proto/` uses the proto adapter and receives a `ProtoEvolutionContext`.
- **`<visibility>`** — exactly `public` or `private`. No other values.
- **`<name>`** — the check name. Forms the check ID as `<adapter>/<name>` (e.g. `proto/evolution`). Note: visibility is not part of the check ID.

**Enforcement:**
- A directory containing `check.checkleft` must be exactly three levels deep under `checkleft/` (adapter + visibility + name).
- The second level must be literally `public` or `private`. Anything else is an error.
- The first level must match a registered adapter. Unknown adapter names are an error at discovery time.
- `package.toml` must exist at the `checkleft/` root. Without it, the directory is ignored.
- File extension is always `.checkleft`. No `.star`, `.bzl`, or `.py`.

### 2.3 Nested / hierarchical checks and file scoping

A nested `checkleft/` directory (e.g. `a/b/c/checkleft/`) defines checks that apply to files in that subtree **and all descendant subtrees**.

**Scoping rule: a changed file is checked by every `checkleft/` directory that is an ancestor of (or sibling to) the file's path.** The runner walks upward from each changed file, collecting all `checkleft/` directories on the path to the repo root. All discovered checks whose `applies_to` globs match the file are run.

**Example:**

```
repo/
├── checkleft/                          # root-level checks
│   └── proto/
│       └── evolution/
│           └── check.checkleft         # applies_to: ["**/*.proto"]
├── a/
│   └── b/
│       └── c/
│           ├── checkleft/              # project-level checks
│           │   └── proto/
│           │       └── billing_compat/
│           │           └── check.checkleft  # applies_to: ["**/*.proto"]
│           ├── foo.proto               # changed file
│           └── d/
│               └── e/
│                   └── f/
│                       └── bar.proto   # changed file
```

If `a/b/c/foo.proto` and `a/b/c/d/e/f/bar.proto` are both changed:

- **`a/b/c/foo.proto`** is checked by:
  - `repo/checkleft/proto/evolution/` (root ancestor, glob matches)
  - `repo/a/b/c/checkleft/proto/billing_compat/` (sibling checkleft, glob matches)

- **`a/b/c/d/e/f/bar.proto`** is checked by:
  - `repo/checkleft/proto/evolution/` (root ancestor, glob matches)
  - `repo/a/b/c/checkleft/proto/billing_compat/` (ancestor checkleft, glob matches)

Both proto files get **all** applicable checks run on them. A nested `checkleft/` adds checks for its subtree — it does not remove or replace ancestor checks. Root-level checks always apply repo-wide.

Nested packages can `load()` from ancestor `checkleft/lib/` directories (resolved upward), but not from sibling or child packages.

---

## 3. `package.toml` — the package manifest

Written in TOML. Parsed before any checks. Declares package-level metadata.

Local checks are auto-discovered from the folder structure — they are **not** listed here. `package.toml` has exactly two jobs: declare package identity and pull in external dependencies/version sets.

```toml
# checkleft/package.toml

[package]
name = "myorg/repo-checks"
version = "0.1.0"

# Pull in a curated version set — all its public checks become active.
[version_sets.acme-versionset]
source = "registry://checkleft-hub/acme-versionset"
version = "2025.06.1"

# Pull in an individual external check package.
[dependencies.acme_java_checks]
source = "git://github.com/acme/checkleft-java.git"
version = "1.0.3"
```

### 3.1 `package()` fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `str` | yes | Globally unique package name. Convention: `<org>/<descriptor>`. |
| `version` | `str` | yes | SemVer. Used when this package is consumed as a dependency. |

### 3.2 `version_set()` — curated check collections

Inspired by Amazon's Brazil build system: instead of individually pinning N check packages, you depend on a single **version set** — a curated, tested-together collection that pins a coherent set of check versions. The version set author tests that all included checks work together, so consumers get a single version to track.

```toml
[version_sets.acme-versionset]
source = "registry://checkleft-hub/acme-versionset"
version = "2025.06.1"   # one number, many checks
```

A version set is itself a `package.toml` that re-exports other packages. The version set's manifest:

```toml
# Published as: acme-versionset v2025.06.1
# This file IS the version set — it pins exact versions of constituent packages.

[package]
name = "acme/versionset"
version = "2025.06.1"
kind = "version_set"   # marks this as a version set, not a regular check package

# The version set pins all its constituent packages at tested-together versions.
# Consumers inherit these versions and all public checks automatically.

[includes.proto_evolution]
source = "registry://checkleft-hub/proto-evolution"
version = "0.2.1"

[includes.module_json_checks]
source = "registry://checkleft-hub/module-json"
version = "1.3.0"

[includes.java_api_compat]
source = "registry://checkleft-hub/java-api-compat"
version = "0.8.2"

[includes.security_baseline]
source = "registry://checkleft-hub/security-baseline"
version = "3.1.0"
```

**Note on adapter overlap:** Multiple packages (from the same or different version sets) can provide checks for the same file type. For example, `proto_evolution` and `security_baseline` might both have `proto/` category checks that apply to `**/*.proto`. All applicable checks from all active packages run — there is no conflict. Each package's checks are independently auto-discovered and independently executed.

**Consumer usage** — depend on the version set and all of its public checks are automatically active. No need to individually activate them:

```toml
# Consumer's package.toml

[package]
name = "myorg/my-repo"
version = "0.1.0"

# One version_set dependency pulls in all public checks from every
# package in the set. No individual check activation needed.
[version_sets.acme-versionset]
source = "registry://checkleft-hub/acme-versionset"
version = "2025.06.1"
# ^ This single line activates proto_evolution/wire_compat,
#   module_json_checks/required_fields, java_api_compat/*,
#   security_baseline/* — every public check from every included package.

# You can still add individual dependencies alongside a version set
# for packages not in the version set.
[dependencies.custom_team_checks]
source = "git://github.com/myteam/checkleft-checks.git"
version = "0.3.0"
```

### 3.3 `[version_sets.<name>]` fields (consumer side)

| TOML key | Type | Required | Description |
|---|---|---|---|
| (table key) | `str` | yes | Local alias for the version set. |
| `source` | `str` | yes | Source URI (same schemes as dependencies). |
| `version` | `str` | yes | Exact version set version pin. |

### 3.4 `[includes.<name>]` fields (version set author side)

| TOML key | Type | Required | Description |
|---|---|---|---|
| (table key) | `str` | yes | Local alias for this constituent package. Consumers use this as the check ID prefix. |
| `source` | `str` | yes | Source URI of the constituent package. |
| `version` | `str` | yes | Exact version pin. The version set author tests this version. |

### 3.5 Version set resolution rules

Resolution is intentionally simple — no complex override logic.

1. **Multiple version sets are allowed, but overlap is a hard error.** If two version sets include the same package (by name), resolution fails immediately. Fix it by removing one of the version sets or choosing a single version set that covers both.
2. **`depend()` can only add packages not in any version set, or upgrade.** If a `depend()` names a package that a version set already includes, the `depend()` version **must** be strictly greater than the version set's pin. Pinning at the same or lower version is an error — use the version set's pin or don't.
3. **Version sets cannot depend on other version sets.** A version set's `package.toml` may only contain `[package]` and `[includes.*]` sections. No `[version_sets.*]` nesting.
4. **All public checks from all version-set-included packages are automatically active.** No individual activation needed.
5. **Private checks** (underscore-prefixed categories/names) in version set packages are **not** activated in consumers — they only run in the package's own repo.

### 3.6 `[dependencies.<name>]` fields

| TOML key | Type | Required | Description |
|---|---|---|---|
| (table key) | `str` | yes | Local alias used to reference this dependency's checks. |
| `source` | `str` | yes | Source URI. Schemes: `registry://`, `git://`, `path://` (local). |
| `version` | `str` | yes | SemVer version or git tag. Exact pin, no ranges. |

### 3.7 Auto-discovery of local checks

**Local checks are never listed in `package.toml`.** They are auto-discovered from the folder structure:

- Any directory matching `checkleft/<category>/<name>/` that contains a `check.checkleft` is a check.
- The check ID is derived from the path: `<category>/<name>` (e.g. `proto/evolution`).
- The `applies_to` globs, `tier`, and `config` are declared **inside `check.checkleft` itself** via a `check_meta()` call at the top of the file (see §4.1).

This means `package.toml` has exactly two jobs:
1. Declare package identity (`package()`).
2. Pull in external dependencies (`version_set()`, `depend()`).

### 3.8 Public vs. private visibility (path semantics)

Visibility is determined entirely by the `public/` or `private/` directory in the path. No convention tricks — it's a literal directory name.

**Checks:**

| Path pattern | Visibility | Description |
|---|---|---|
| `checkleft/<adapter>/public/<name>/` | **Public.** | Exported when this package is consumed via `depend()` or `version_set()`. Activated in consumers. |
| `checkleft/<adapter>/private/<name>/` | **Private.** | Runs locally but not exported to consumers. |

**Libraries (`lib/`):**

| Path pattern | Visibility | Description |
|---|---|---|
| `checkleft/lib/*.checkleft` | **Always private.** | All `lib/` modules are private. Loadable by checks in the same package but never exported. Consumers cannot `load()` from a dependency's `lib/`. |

This means consumers can use a dependency's **public checks** but never import its **helper functions** or run its **private checks**.

**Examples:**

```
checkleft/
├── lib/
│   └── proto_helpers.checkleft                     # private — same-package only
├── proto/
│   ├── public/
│   │   └── evolution/
│   │       └── check.checkleft                     # PUBLIC — exported to consumers
│   └── private/
│       └── team_policy/
│           └── check.checkleft                     # PRIVATE — local only
├── module_json/
│   └── public/
│       └── required_fields/
│           └── check.checkleft                     # PUBLIC — exported to consumers
└── text/
    └── private/
        └── lint_style/
            └── check.checkleft                     # PRIVATE — local only
```

When consumed via `depend()` or `version_set()`:
- `proto/evolution` — **activated** (public)
- `module_json/required_fields` — **activated** (public)
- `proto/team_policy` — **not activated** (private)
- `text/lint_style` — **not activated** (private)
- `lib/*` — **not loadable** by consumer checks

---

## 4. Check and fix entry points

### 4.1 `check.checkleft` — the check file

Every check file must:
1. Call `check_meta()` at the top level to declare metadata (applies_to, tier, config schema).
2. Define exactly one `check()` function with a typed signature. The parameter type depends on the **file format adapter** (see §6).

```python
# checkleft/proto/evolution/check.checkleft

load("//lib/proto_helpers", "is_reserved")

check_meta(
    applies_to: list[str] = ["**/*.proto"],
    tier: str = "hermetic",
    config: dict[str, typing.Any] = {
        "severity": "fail",
    },
)

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    """Checks that proto field removals have reservations."""
    findings: list[Finding] = []
    for delta in ctx.deltas:
        if delta.kind == DeltaKind.field_removed:
            if not is_reserved(ctx, delta):
                findings.append(finding(
                    severity = Severity.fail,
                    message = "removed field {} must be reserved".format(delta.symbol),
                    path = delta.path,
                    line = delta.line,
                ))
    return findings
```

`check_meta()` is required. Without it, the file is not recognized as a check.

| Field | Type | Required | Default | Description |
|---|---|---|---|---|
| `applies_to` | `list[str]` | yes | — | Glob patterns for files this check cares about. |
| `tier` | `str` | no | `"hermetic"` | Sandbox tier. See §5. |
| `config` | `dict[str, typing.Any]` | no | `{}` | Default config passed to `ctx.config`. Consumers can override via `CHECKS.yaml`. |

### 4.2 `fix.checkleft` — the fix file

Optional. Must define a `fix()` function whose signature mirrors the check but returns `list[FileEdit]`.

```python
# checkleft/proto/evolution/fix.checkleft

def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    edits: list[FileEdit] = []
    for f in findings:
        if "must be reserved" in f.message:
            # Build the reservation line
            edits.append(file_edit(
                path = f.location.path,
                old_text = "",  # insert-only
                new_text = "  reserved {};\n".format(extract_field_number(f)),
                after_line = f.location.line,
            ))
    return edits
```

### 4.3 Typing requirements

The Starlark dialect is configured with:

```rust
Dialect {
    enable_types: DialectTypes::Enable,  // type annotations required
    ..Dialect::Standard
}
```

**All** function parameters and return types must have type annotations. The type checker runs before evaluation. Checks that fail type checking are reported as configuration errors, not silently skipped.

### 4.4 Load paths

```python
# Load from the package's lib/ directory
load("//lib/matchers", "glob_match", "path_prefix")

# Load from a check-local helper in the same directory
load(":utils", "extract_field_number")

# Load from an external dependency
load("@proto_evolution_checks//lib/wire", "is_wire_compatible")
```

| Prefix | Resolution |
|---|---|
| `//` | Relative to the enclosing `checkleft/` directory. |
| `:` | Relative to the current check directory. |
| `@<dep_name>//` | Relative to the named dependency's `checkleft/` root. |

---

## 5. Sandbox tiers

Checks declare their required sandbox tier in `package.toml` via the `check()` call. The tier determines what host capabilities the Starlark environment exposes.

### 5.1 Tier definitions

| Tier | ID | Capabilities | Use case |
|---|---|---|---|
| **Hermetic** | `"hermetic"` | Typed data models (injected by the adapter), pure computation, `load()`. No file I/O, no network, no subprocesses. Starlark never sees raw file contents or touches the filesystem. | Most evolution checks. Default. |
| **Network** | `"network"` | Everything in hermetic + HTTP GET requests via `http_get()` built-in. DNS resolution allowed. Still no file I/O or arbitrary exec. | Checks that validate against a remote schema registry, API catalog, or artifact repository. |

### 5.2 Tier enforcement

- Starlark code **never has filesystem access**. All data arrives as typed models injected by the Rust adapter. There is no `read_file()`, `open()`, or equivalent. The Starlark environment is a pure computation sandbox over pre-parsed data.
- The Starlark `Globals` environment is constructed per-tier. Hermetic checks simply never see `http_get` or similar symbols — they don't exist in scope, so misuse is a compile-time name-resolution error, not a runtime denial.
- **Tier escalation is forbidden.** A check declared `hermetic` cannot `load()` a module that was authored for `network` tier. Tier is a property of the check activation in `package.toml`, not of individual `.checkleft` files. All code reachable from a check runs at that check's tier.
- CI environments may **deny the `network` tier entirely** via a runner flag (`--deny-tier=network`). Checks activated at a denied tier are skipped with a warning.

### 5.3 Severity model

Findings have exactly two severity levels. A finding always blocks merge — the only question is whether a human can override it.

| Severity | Starlark constant | CI behavior | Description |
|---|---|---|---|
| **Fail** | `Severity.fail` | Blocks merge. Cannot be overridden. | Hard violation. No exceptions. |
| **Fail-but-overridable** | `Severity.fail_but_overridable` | Blocks merge by default, but can be overridden with a `BYPASS` directive in the PR/commit description. | The change is almost certainly wrong, but there are legitimate exceptions. |

There is no "informational" / "notice" severity. If something doesn't warrant blocking the build, it doesn't belong as a checkleft finding — use linter warnings or comments for that. Checkleft findings are gates.

The `CHECKS.yaml` policy layer can **escalate** a check's severity (`fail_but_overridable` → `fail`) but never **relax** it. This ensures check authors set the floor and operators can only tighten.

Shorthand constructors match the severity names:

```python
fail(message = "...", path = "...")                   # Severity.fail
fail_but_overridable(message = "...", path = "...")   # Severity.fail_but_overridable
```

### 5.4 Tier-specific built-in bindings

**Hermetic tier** (always available):

| Symbol | Type | Description |
|---|---|---|
| `finding(...)` | `fn(...) -> Finding` | Construct a finding. |
| `file_edit(...)` | `fn(...) -> FileEdit` | Construct a file edit (for fixes). |
| `Severity` | `enum{fail, fail_but_overridable}` | Severity constants (see §5.3). |
| `DeltaKind` | `enum{...}` | Format-specific delta kind constants. |
| `print(...)` | `fn(str)` | Debug print (suppressed in CI, shown with `--verbose`). |
| `json_decode(s)` | `fn(str) -> typing.Any` | Parse a JSON string. |
| `json_encode(v)` | `fn(typing.Any) -> str` | Serialize to JSON. |
| `regex_match(pattern, s)` | `fn(str, str) -> bool` | RE2 regex match. |
| `regex_find_all(pattern, s)` | `fn(str, str) -> list[str]` | RE2 find all matches. |
| `glob_match(pattern, path)` | `fn(str, str) -> bool` | Glob pattern match. |

**Network tier** (additional bindings):

| Symbol | Type | Description |
|---|---|---|
| `http_get(url, headers=None, timeout_ms=5000)` | `fn(str, dict[str,str]\|None, int) -> HttpResponse` | HTTP GET. TLS required. No mutations. |
| `HttpResponse` | `struct{status: int, body: str, headers: dict[str,str]}` | Response type. |

---

## 6. File format adapters — the Rust/Starlark bridge

Evolution checks need **parsed representations** of files at two revisions (base and current). Parsing binary formats (protobuf descriptors, Java class files, JSON schemas) in pure Starlark is impractical and slow. This is where Rust earns its keep.

### 6.1 Adapter architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Rust host                             │
│                                                         │
│  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐ │
│  │ ProtoAdapter  │   │ JsonAdapter  │   │ JavaAdapter  │ │
│  │              │   │              │   │              │ │
│  │ parse(base)  │   │ parse(base)  │   │ parse(base)  │ │
│  │ parse(cur)   │   │ parse(cur)   │   │ parse(cur)   │ │
│  │ diff(b,c)    │   │ diff(b,c)    │   │ diff(b,c)    │ │
│  └──────┬───────┘   └──────┬───────┘   └──────┬───────┘ │
│         │                  │                  │         │
│         ▼                  ▼                  ▼         │
│  ┌─────────────────────────────────────────────────────┐ │
│  │         Starlark Globals injection                  │ │
│  │  ctx: ProtoEvolutionContext                         │ │
│  │  ctx: ModuleJsonEvolutionContext                     │ │
│  │  ctx: JavaEvolutionContext                          │ │
│  └─────────────────────────────────────────────────────┘ │
│         │                                               │
│         ▼                                               │
│  ┌─────────────────────────────────────────────────────┐ │
│  │         Starlark evaluator                          │ │
│  │  check(ctx) -> list[Finding]                        │ │
│  └─────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

Each adapter is a Rust trait implementation:

```rust
/// Registered in the adapter registry. The `kind` string (e.g. "proto", "module_json",
/// "java") determines which context type the Starlark check receives.
pub trait FormatAdapter: Send + Sync + 'static {
    /// Unique identifier matching the check category folder name.
    fn kind(&self) -> &str;

    /// Parse files at a given tree version into an opaque descriptor set.
    fn parse(
        &self,
        paths: &[PathBuf],
        tree: &dyn SourceTree,
        version: TreeVersion,
        config: &toml::Value,
    ) -> Result<Box<dyn AdapterOutput>>;

    /// Compute structured deltas between base and current parsed outputs.
    fn diff(
        &self,
        base: &dyn AdapterOutput,
        current: &dyn AdapterOutput,
    ) -> Result<Box<dyn AdapterOutput>>;

    /// Inject the parsed data + deltas into a Starlark GlobalsBuilder.
    /// This is where typed Starlark values (ProtoEvolutionContext, etc.) are allocated.
    fn inject_globals(
        &self,
        globals: &mut GlobalsBuilder,
        base: &dyn AdapterOutput,
        current: &dyn AdapterOutput,
        deltas: &dyn AdapterOutput,
        config: &toml::Value,
        changeset: &ChangeSet,
    ) -> Result<()>;

    /// Return the Starlark type name for the context parameter (e.g. "ProtoEvolutionContext").
    fn context_type_name(&self) -> &str;
}
```

### 6.2 Built-in adapters

#### `proto` — Protobuf evolution

Context type: `ProtoEvolutionContext`

Rust side: invokes `protoc --descriptor_set_out` with `--include_source_info` at both base and current revisions. The resulting `FileDescriptorSet` is enriched with source location info (comments, line/column positions for every element). The Rust adapter parses these descriptor sets, diffs them into `SchemaDelta` values, and injects the typed models into Starlark. Starlark check authors receive a descriptor model that includes comments and source positions — not raw `.proto` text, but the full structured representation that `protoc` produces.

Starlark surface: `ctx.deltas`, `ctx.files`, `ctx.config`, `ctx.registries`, plus all the typed descriptor types (`FileDescriptor`, `MessageDescriptor`, `FieldDescriptor`, etc.) and enum constants (`DeltaKind`, `FieldKind`, `FieldLabel`, etc.) already documented in the proto-evolution branch. Source location info is available on descriptors via `.source_location` (line, column, leading/trailing comments).

#### `module_json` — `module.json` file evolution

Context type: `ModuleJsonEvolutionContext`

Adapters are **not** generic file-format parsers. They are specific to concrete file types with their own schemas and evolution semantics. The `module_json` adapter understands `module.json` files — their required keys, dependency structure, and versioning semantics. A different JSON-based file type (e.g. `package.json`, `tsconfig.json`) would get its own adapter with its own typed model.

Rust side: parses `module.json` files at both revisions into a typed `ModuleJson` model (not generic JSON). Computes structural diffs aware of module-specific semantics (e.g. dependency additions vs. removals, version field changes).

```python
# Starlark surface
ctx.files       # list[ModuleJsonFilePair]  — before/after parsed module.json
ctx.deltas      # list[ModuleJsonDelta]     — structured changes
ctx.config      # dict[str, typing.Any]

# Types
ModuleJsonFilePair.path: str
ModuleJsonFilePair.before: ModuleJson | None
ModuleJsonFilePair.after: ModuleJson | None

ModuleJson.name: str
ModuleJson.version: str
ModuleJson.description: str | None
ModuleJson.dependencies: dict[str, str]     # name -> version constraint
ModuleJson.dev_dependencies: dict[str, str]
ModuleJson.metadata: dict[str, JsonValue]   # remaining keys as generic JSON

ModuleJsonDelta.kind: ModuleJsonDeltaKind  # name_changed, version_changed, description_removed,
                                           # dependency_added, dependency_removed, dependency_version_changed,
                                           # required_key_removed, metadata_changed
ModuleJsonDelta.path: str                  # file path
ModuleJsonDelta.key: str                   # the affected key/dependency name
ModuleJsonDelta.before_value: str | None
ModuleJsonDelta.after_value: str | None
```

#### `java` — Java API surface evolution

Context type: `JavaEvolutionContext`

Rust side: parses `.java` files via tree-sitter-java (syntax-level, no compilation needed). Extracts public API surface: public/protected classes, methods, fields, their signatures and annotations.

```python
# Starlark surface
ctx.files       # list[JavaFilePair]
ctx.deltas      # list[JavaDelta]
ctx.config      # dict[str, typing.Any]

JavaFilePair.path: str
JavaFilePair.before: JavaFile | None
JavaFilePair.after: JavaFile | None

JavaFile.package: str
JavaFile.imports: list[str]
JavaFile.classes: list[JavaClass]

JavaClass.name: str
JavaClass.full_name: str
JavaClass.visibility: str          # "public", "protected", "package", "private"
JavaClass.modifiers: list[str]     # "abstract", "final", "static"
JavaClass.superclass: str | None
JavaClass.interfaces: list[str]
JavaClass.annotations: list[JavaAnnotation]
JavaClass.methods: list[JavaMethod]
JavaClass.fields: list[JavaField]
JavaClass.inner_classes: list[JavaClass]

JavaMethod.name: str
JavaMethod.visibility: str
JavaMethod.return_type: str
JavaMethod.parameters: list[JavaParameter]
JavaMethod.annotations: list[JavaAnnotation]
JavaMethod.modifiers: list[str]

JavaDelta.kind: JavaDeltaKind  # class_removed, method_removed, method_signature_changed,
                               # field_removed, field_type_changed, visibility_narrowed,
                               # annotation_removed, superclass_changed, interface_removed
JavaDelta.path: str
JavaDelta.symbol: str
```

### 6.3 The `text` adapter — generic / no special parsing

For checks that operate on raw file content and line-level diffs (no format-specific parsing), a built-in `text` adapter is provided. This is the escape hatch for checks that don't need a Rust-side parser.

Context type: `TextEvolutionContext`

The `text` adapter parses files into a structured line-level model on the Rust side. Starlark receives parsed data — never raw file handles or filesystem access.

```python
ctx.files           # list[TextFilePair]
ctx.config          # dict[str, typing.Any]
ctx.changeset       # ChangeSetInfo (metadata about the overall change)

TextFilePair.path: str
TextFilePair.before: TextFile | None    # parsed line model at base revision
TextFilePair.after: TextFile | None     # parsed line model at current revision
TextFilePair.added_lines: list[Line]    # lines added in this change (from diff)
TextFilePair.removed_lines: list[Line]  # lines removed in this change (from diff)
TextFilePair.change_kind: ChangeKind    # added, modified, deleted, renamed

TextFile.lines: list[Line]             # all lines in the file
TextFile.line_count: int               # total number of lines

Line.number: int
Line.text: str
```

The `text` adapter is specified by setting the check category to any name that doesn't match a registered format adapter. Alternatively, checks can explicitly opt in via `adapter = "text"` in the `check()` call in `package.toml`.

### 6.4 Registering custom Rust adapters

Third-party or in-repo Rust adapters register via:

```rust
// In the host binary's setup
registry.register_adapter(Box::new(MyCustomAdapter));
```

The adapter's `kind()` return value must match the `<category>` folder name in the check directory structure. This is the linkage: `checkleft/proto/*/check.checkleft` uses the adapter whose `kind() == "proto"`.

---

## 7. Rust-native checks (bidirectional support)

Not everything belongs in Starlark. Performance-critical checks, checks requiring complex binary parsing, or checks that need direct access to the Rust async runtime should remain in Rust.

### 7.1 How Rust checks coexist

Rust checks implement the existing `Check` + `ConfiguredCheck` traits. They are registered in `CheckRegistry` as today. They produce the same `Finding` / `CheckResult` output types.

A `check.checkleft` can delegate to a Rust implementation via `source` in `check_meta()`:

```python
# checkleft/proto/wire_compat_fast/check.checkleft
# This is a thin shim — the real work happens in Rust.

check_meta(
    applies_to: list[str] = ["**/*.proto"],
    tier: str = "hermetic",
    source: str = "rust://protobuf-evolution",  # maps to a registered Check::id()
)

# No check() function needed — source delegates to Rust.
```

When `source` is present in `check_meta()`, the runner delegates to the named Rust check. The `check.checkleft` file still exists (for auto-discovery and metadata) but does not need a `check()` function.

### 7.2 Rust checks calling Starlark policies

A Rust check can **delegate policy decisions** to user-supplied Starlark, exactly as the proto-evolution branch does today. The Rust side does heavy parsing/diffing; the Starlark side decides what constitutes a violation.

This is the recommended pattern for format adapters: the Rust adapter does parsing, the Starlark check does policy.

### 7.3 Decision framework

| Scenario | Use |
|---|---|
| Policy logic over pre-parsed data | Starlark check + Rust adapter |
| Line/text pattern matching | Starlark check + `text` adapter |
| Binary format parsing (protobuf, class files) | Rust adapter |
| Checks needing async I/O or subprocess orchestration | Rust check |
| Simple glob + regex rules | Starlark check + `text` adapter |

---

## 8. Versioned check distribution

### 8.1 Package identity

Every `checkleft/` directory with a `package.toml` is a distributable package. The `package(name, version)` call establishes identity.

### 8.2 Resolution

Dependencies declared via `depend()` are resolved at `checkleft` startup before any checks run:

1. **`registry://`** — fetched from a check registry (HTTP API). The registry serves tarballs of `checkleft/` directory trees. Cached locally in `~/.cache/checkleft/packages/<name>/<version>/`.
2. **`git://`** — cloned at the specified tag. Sparse checkout of the `checkleft/` directory only. Cached similarly.
3. **`path://`** — local filesystem path. For monorepo cross-project dependencies. No caching; always reads live. Relative to the repo root.

### 8.3 Version pinning

- Only exact versions are supported. No ranges, no `^`, no `~`.
- A lockfile `checkleft/PACKAGE.lock` is generated/updated on resolution, recording SHA256 of fetched content.
- The lockfile is checked into version control.
- `checkleft update <dep_name> <new_version>` updates the pin and lockfile.

### 8.4 Publishing

Out of scope for v1. Packages are distributed via git tags or manual registry upload. A `checkleft publish` command is a future addition.

---

## 9. Execution pipeline

### 9.1 Discovery

```
1. Walk from repo root, find all checkleft/ directories.
2. For each, parse package.toml.
3. Resolve depend() entries (fetch/cache as needed).
4. Collect all check() entries across all packages.
5. Scope each check's changeset to its package's subtree.
```

### 9.2 Per-check execution

```
1. Filter changeset by applies_to globs.
2. If no matching files changed, skip (zero-cost).
3. Determine adapter from check category (or explicit adapter= override).
4. If Starlark check:
   a. Adapter.parse(base_files, tree, TreeVersion::Base)
   b. Adapter.parse(current_files, tree, TreeVersion::Current)
   c. Adapter.diff(base, current)
   d. Build Starlark Globals for the declared tier.
   e. Adapter.inject_globals(...) — adds typed context.
   f. Load check.checkleft, resolve load() imports.
   g. Type-check the module (DialectTypes::Enable).
   h. Evaluate check(ctx) -> list[Finding].
   i. If fix requested and fix.checkleft exists:
      - Evaluate fix(ctx, findings) -> list[FileEdit].
      - Apply edits via WritableSandbox.
5. If Rust check (source: "rust://..."):
   a. Delegate to Check::configure() + ConfiguredCheck::run() as today.
6. Collect findings, apply policy (severity override, bypass directives).
```

### 9.3 Concurrency

- Starlark checks run on a blocking thread pool (`spawn_blocking`), same as WASM components today.
- Each check gets its own `Module` and `Evaluator` — no shared mutable state.
- Adapter parsing (Rust) can run concurrently across checks.

### 9.4 Error handling

| Error class | Behavior |
|---|---|
| `package.toml` parse error | Fatal. Package is skipped with error diagnostic. |
| `load()` resolution failure | Fatal for that check. Other checks in the package still run. |
| Type-check failure in `.checkleft` | Fatal for that check. Reported as a configuration error finding. |
| Runtime error in `check()` | Check fails. Finding with `severity: fail` and the Starlark traceback. |
| Adapter parse failure | Check fails with error. Starlark code is not invoked. |

---

## 10. Starlark dialect and type system

### 10.1 Dialect settings

```rust
Dialect {
    enable_types: DialectTypes::Enable,
    enable_load: true,
    enable_keyword_only_arguments: true,
    enable_f_strings: true,
    ..Dialect::Standard
}
```

### 10.2 Type annotations

All user-defined functions **must** have type annotations on every parameter and the return type:

```python
# GOOD — compiles
def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    ...

def helper(deltas: list[SchemaDelta], prefix: str) -> list[SchemaDelta]:
    ...

# BAD — type-check error, will not run
def check(ctx):
    ...
```

### 10.3 Built-in types available in all tiers

```
# Output types
Finding(severity: Severity, message: str, path: str, line: int | None, column: int | None, remediation: str | None, suggested_fix: SuggestedFix | None)
FileEdit(path: str, old_text: str, new_text: str, after_line: int | None)
SuggestedFix(description: str, edits: list[FileEdit])
Severity  # enum: fail, fail_but_overridable
Location(path: str, line: int | None, column: int | None)

# Standard Starlark types
str, int, float, bool, list, dict, None

# Utility
ChangeKind  # enum: added, modified, deleted, renamed
```

### 10.4 Adapter-injected types

Each adapter injects its own typed structs into the Starlark environment. These are `StarlarkValue` implementations on the Rust side, with `StarlarkAttrs` and `starlark_value` derives providing typed attribute access.

Users reference them in type annotations:

```python
def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    ...

def my_helper(msg: MessageDescriptor) -> bool:
    ...
```

The type checker validates these at load time.

---

## 11. Dogfood examples

### 11.1 Proto evolution check

```
checkleft/
├── package.toml
├── lib/
│   └── proto_helpers.checkleft
└── proto/
    └── evolution/
        ├── check.checkleft
        └── fix.checkleft
```

**`package.toml`:**
```toml
[package]
name = "mono/checks"
version = "0.1.0"

# No external dependencies yet — local checks are auto-discovered from the folder structure.
```

**`lib/proto_helpers.checkleft`:**
```python
def has_reservation(msg: MessageDescriptor, field_number: int) -> bool:
    for r in msg.reserved_ranges:
        if r.start <= field_number and field_number < r.end:
            return True
    return False

def is_internal_package(pkg: str) -> bool:
    return pkg.startswith("internal.") or ".internal." in pkg
```

**`proto/evolution/check.checkleft`:**
```python
load("//lib/proto_helpers", "has_reservation", "is_internal_package")

check_meta(
    applies_to: list[str] = ["**/*.proto"],
    tier: str = "hermetic",
    config: dict[str, typing.Any] = {
        "extension_registries": ["proto/options.proto"],
    },
)

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    for delta in ctx.deltas:
        # Skip internal packages — they have no compatibility contract
        if is_internal_package(delta.symbol):
            continue

        if delta.kind == DeltaKind.field_removed:
            findings.append(finding(
                severity = Severity.fail,
                message = "removed field {} must be reserved to prevent reuse".format(delta.symbol),
                path = delta.path,
                remediation = "Add a `reserved` statement for the removed field number.",
            ))

        if delta.kind == DeltaKind.field_number_changed:
            findings.append(finding(
                severity = Severity.fail,
                message = "field number changed for {} — this breaks wire compatibility".format(delta.symbol),
                path = delta.path,
            ))

        if delta.kind == DeltaKind.field_type_changed:
            findings.append(finding(
                severity = Severity.fail,
                message = "field type changed for {} ({} -> {})".format(
                    delta.symbol,
                    delta.before_kind,
                    delta.after_kind,
                ),
                path = delta.path,
            ))

    return findings
```

**`proto/evolution/fix.checkleft`:**
```python
def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    edits: list[FileEdit] = []
    for f in findings:
        if "must be reserved" in f.message:
            for delta in ctx.deltas:
                if delta.kind == DeltaKind.field_removed and delta.path == f.path:
                    edits.append(file_edit(
                        path = f.path,
                        old_text = "",
                        new_text = "  reserved {};\n".format(delta.before_number),
                        after_line = f.line,
                    ))
    return edits
```

### 11.2 `module.json` required-fields check

```
checkleft/
└── module_json/
    └── required_fields/
        └── check.checkleft
```

**`module_json/required_fields/check.checkleft`:**
```python
check_meta(
    applies_to: list[str] = ["**/module.json"],
    tier: str = "hermetic",
)

def check(ctx: ModuleJsonEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []
    for pair in ctx.files:
        if pair.after == None:
            continue
        after: ModuleJson = pair.after

        # The typed model gives us direct access to known fields
        if after.name == "":
            findings.append(fail(
                message = "module.json 'name' must not be empty",
                path = pair.path,
            ))
        if after.version == "":
            findings.append(fail(
                message = "module.json 'version' must not be empty",
                path = pair.path,
            ))

        # Use structured deltas for evolution violations
        for delta in ctx.deltas:
            if delta.path != pair.path:
                continue
            if delta.kind == ModuleJsonDeltaKind.required_key_removed:
                findings.append(fail(
                    message = "module.json required key '{}' was removed".format(delta.key),
                    path = pair.path,
                ))
            if delta.kind == ModuleJsonDeltaKind.dependency_removed:
                findings.append(fail_but_overridable(
                    message = "dependency '{}' was removed — downstream consumers may break".format(delta.key),
                    path = pair.path,
                ))
    return findings
```

### 11.3 Java API stability check

```
checkleft/
└── java/
    └── api_stability/
        └── check.checkleft
```

**`java/api_stability/check.checkleft`:**
```python
check_meta(
    applies_to: list[str] = ["**/*.java"],
    tier: str = "hermetic",
    config: dict[str, typing.Any] = {
        "track_visibility": ["public", "protected"],
    },
)

def check(ctx: JavaEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []
    tracked: list[str] = ctx.config.get("track_visibility", ["public"])

    for delta in ctx.deltas:
        if delta.kind == JavaDeltaKind.method_removed:
            findings.append(finding(
                severity = Severity.fail,
                message = "public API method removed: {}".format(delta.symbol),
                path = delta.path,
                remediation = "Deprecate the method with @Deprecated before removing.",
            ))

        if delta.kind == JavaDeltaKind.visibility_narrowed:
            findings.append(finding(
                severity = Severity.fail_but_overridable,
                message = "visibility narrowed for {}: this is a breaking change for downstream consumers".format(
                    delta.symbol,
                ),
                path = delta.path,
            ))

        if delta.kind == JavaDeltaKind.method_signature_changed:
            findings.append(finding(
                severity = Severity.fail,
                message = "method signature changed for {}".format(delta.symbol),
                path = delta.path,
                remediation = "Add a new overload instead of changing the existing signature.",
            ))

    return findings
```

---

## 12. Functional testing for checks

Check authors need to test their checks without committing broken code to see if the check catches it. Each check directory can contain a `check_test.checkleft` file with test cases.

### 12.1 Test file structure

```python
# checkleft/proto/evolution/check_test.checkleft

load(":check", "check")

def test_field_removal_is_caught() -> None:
    result: list[Finding] = check(test_context(
        before = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                    int32 age = 2;
                }
            """),
        },
        after = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                }
            """),
        },
    ))
    assert_eq(len(result), 1)
    assert_contains(result[0].message, "must be reserved")
    assert_eq(result[0].severity, Severity.fail)

def test_adding_field_is_allowed() -> None:
    result: list[Finding] = check(test_context(
        before = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                }
            """),
        },
        after = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                    string email = 2;
                }
            """),
        },
    ))
    assert_eq(len(result), 0)

def test_file_deletion_with_config() -> None:
    result: list[Finding] = check(test_context(
        before = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User { string name = 1; }
            """),
        },
        after = {},  # file deleted
        config = {"severity": "fail"},
    ))
    assert_true(len(result) > 0)
```

### 12.2 Test built-ins

Tests get additional built-ins beyond the normal check environment:

| Symbol | Type | Description |
|---|---|---|
| `test_context(before, after, config=None)` | `fn(dict, dict, dict\|None) -> Context` | Build a synthetic context from file content maps. The adapter parses + diffs the content just as it would in a real run. |
| `proto(content)` | `fn(str) -> str` | Marker for proto file content (enables adapter-specific parsing in `test_context`). |
| `json(content)` | `fn(str) -> str` | Marker for JSON file content. |
| `java(content)` | `fn(str) -> str` | Marker for Java file content. |
| `text(content)` | `fn(str) -> str` | Marker for plain text content. |
| `assert_eq(a, b)` | `fn(Any, Any)` | Assert equality. Fails with diff on mismatch. |
| `assert_true(cond)` | `fn(bool)` | Assert truthy. |
| `assert_contains(haystack, needle)` | `fn(str, str)` | Assert substring presence. |
| `assert_finding_count(findings, n)` | `fn(list[Finding], int)` | Assert exact finding count. |

### 12.3 Running tests

```bash
# Run all check tests in the package
checkleft test

# Run tests for a specific check
checkleft test proto/evolution

# Run a specific test function
checkleft test proto/evolution::test_field_removal_is_caught
```

### 12.4 Test discovery

- Any function in `check_test.checkleft` whose name starts with `test_` is a test case.
- Tests must have no parameters and return `None`.
- Tests are type-checked with the same `DialectTypes::Enable` setting as checks.
- Test failures include the Starlark traceback and assertion details.
- Tests run hermetically regardless of the check's declared tier — `test_context()` provides all data synthetically.

### 12.5 Fix testing

Fix functions can be tested similarly:

```python
# checkleft/proto/evolution/check_test.checkleft

load(":check", "check")
load(":fix", "fix")

def test_fix_adds_reservation() -> None:
    ctx: ProtoEvolutionContext = test_context(
        before = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                    int32 age = 2;
                }
            """),
        },
        after = {
            "api/v1/user.proto": proto("""
                syntax = "proto3";
                package api.v1;
                message User {
                    string name = 1;
                }
            """),
        },
    )
    findings: list[Finding] = check(ctx)
    edits: list[FileEdit] = fix(ctx, findings)
    assert_eq(len(edits), 1)
    assert_contains(edits[0].new_text, "reserved 2")
```

---

## 13. Integration with existing checkleft infrastructure

### 12.1 CHECKS.yaml / CHECKS.toml compatibility

Starlark-defined checks appear in `CHECKS.yaml` the same way external checks do today:

```yaml
checks:
  - id: proto/evolution
    check: starlark://checkleft/proto/evolution  # points at the checkleft/ directory
    policy:
      severity: fail
      allow_bypass: true
    exclude_patterns:
      - "vendor/**"
```

The `starlark://` scheme tells the runner to resolve the check from the `checkleft/` folder structure rather than from a declarative YAML or WASM component.

However, `package.toml` is the **preferred** way to activate Starlark checks. `CHECKS.yaml` integration exists for repos that want to mix Starlark checks with existing declarative/WASM checks in a single config file.

When both `package.toml` and `CHECKS.yaml` activate the same check ID, `CHECKS.yaml` policy fields (severity override, bypass, exclusions) take precedence — they are the operator-level override layer.

### 12.2 Output compatibility

Starlark checks produce `Finding` values that map 1:1 to the existing `crate::output::Finding`:

| Starlark `Finding` field | Rust `Finding` field |
|---|---|
| `severity` | `severity` |
| `message` | `message` |
| `path` + `line` + `column` | `location: Option<Location>` |
| `remediation` | `remediation: Option<String>` |
| `suggested_fix` | `suggested_fix: Option<SuggestedFix>` |

### 12.3 Fix compatibility

Starlark `fix()` functions return `list[FileEdit]` which maps to the existing `Vec<FileEdit>` consumed by `WritableSandbox`. The existing fix scheduler (`src/fix/scheduler.rs`) orchestrates Starlark fixes identically to WASM component fixes.

### 12.4 Progress reporting

The runner reports Starlark check progress through the existing `ProgressReporter` trait. Each Starlark check registers its `applicable_file_count` (derived from `applies_to` glob matching) and ticks progress as files are processed by the adapter.

---

## 14. Future extensions

### 13.1 Additional sandbox tiers

If needed later, new tiers slot in naturally:

| Tier | Capabilities |
|---|---|
| `"exec"` | Everything in `network` + subprocess execution via `exec()` built-in. |
| `"full"` | Unrestricted. Equivalent to a Rust check. For trusted first-party checks only. |

### 13.2 Additional format adapters

The adapter system is open for extension:

- **`yaml`** — YAML schema evolution (Kubernetes CRDs, OpenAPI specs).
- **`graphql`** — GraphQL schema evolution.
- **`swift`** — Swift API surface (via tree-sitter-swift).
- **`typescript`** — TypeScript declaration file (`.d.ts`) evolution.

Each adapter is a Rust crate implementing `FormatAdapter`. No changes to the Starlark infrastructure needed.

### 13.3 Check composition

A future `compose()` built-in could let checks delegate to sub-checks:

```python
def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    findings: list[Finding] = compose([
        wire_compat_check,
        naming_convention_check,
        deprecation_check,
    ], ctx)
    return findings
```

### 13.4 Interactive fix preview

A `checkleft fix --preview` mode that shows proposed edits in a TUI diff viewer before applying.

---

## 15. API reference by example

This section provides concrete, copy-pasteable examples of every key operation a check author will perform. These examples define the target API surface.

### 15.1 Constructing findings

```python
# Minimal finding — just severity, message, and path
findings.append(finding(
    severity = Severity.fail,
    message = "field was removed without reservation",
    path = "api/v1/service.proto",
))

# Finding with line and column
findings.append(finding(
    severity = Severity.fail_but_overridable,
    message = "method visibility narrowed from public to protected",
    path = "src/com/acme/Api.java",
    line = 42,
    column = 5,
))

# Finding with remediation guidance
findings.append(finding(
    severity = Severity.fail,
    message = "required key 'version' removed from module.json",
    path = "services/auth/module.json",
    remediation = "Restore the 'version' key. It is required by the module loader.",
))

# Finding with an inline suggested fix
findings.append(finding(
    severity = Severity.fail_but_overridable,
    message = "deprecated field 'old_name' should use reserved",
    path = "api/v1/user.proto",
    line = 15,
    suggested_fix = suggested_fix(
        description = "Add reserved statement for field number 3",
        edits = [file_edit(
            path = "api/v1/user.proto",
            old_text = "  // old_name was here\n",
            new_text = "  reserved 3;\n  reserved \"old_name\";\n",
        )],
    ),
))

# Shorthand constructors for common severities
findings.append(fail(
    message = "service removed",
    path = "api/v1/service.proto",
))
findings.append(fail_but_overridable(
    message = "enum value name changed",
    path = "api/v1/status.proto",
    line = 8,
))
findings.append(fail_but_overridable(
    message = "new field added (non-breaking)",
    path = "api/v1/user.proto",
))
```

### 15.2 Constructing file edits (for fixes)

```python
# Replace existing text
edits.append(file_edit(
    path = "api/v1/user.proto",
    old_text = "  string old_name = 3;\n",
    new_text = "  reserved 3;\n  reserved \"old_name\";\n",
))

# Insert after a specific line (old_text empty = insert-only)
edits.append(file_edit(
    path = "api/v1/user.proto",
    old_text = "",
    new_text = "  reserved 7;\n",
    after_line = 22,
))

# Delete text (new_text empty = delete-only)
edits.append(file_edit(
    path = "src/main/Unused.java",
    old_text = "import com.acme.deprecated.OldClient;\n",
    new_text = "",
))
```

### 15.3 Loading shared helpers

```python
# From the package's lib/ directory
load("//lib/proto_helpers", "has_reservation", "is_internal_package")

# From a check-local helper in the same check directory
load(":utils", "extract_field_number", "format_symbol")

# From an external versioned dependency
load("@acme_checks//lib/wire", "is_wire_compatible", "BREAKING_KINDS")

# Multiple symbols from one module
load("//lib/matchers", "glob_match", "path_prefix", "is_generated_file")
```

### 15.4 Defining shared helper modules

**`checkleft/lib/proto_helpers.checkleft`:**
```python
def has_reservation(msg: MessageDescriptor, field_number: int) -> bool:
    """Check if a message reserves the given field number."""
    for r in msg.reserved_ranges:
        if r.start <= field_number and field_number < r.end:
            return True
    return False

def is_internal_package(pkg: str) -> bool:
    """Internal packages have no compatibility contract."""
    return pkg.startswith("internal.") or ".internal." in pkg

def find_field_by_number(msg: MessageDescriptor, number: int) -> FieldDescriptor | None:
    """Look up a field descriptor by its field number."""
    for field in msg.fields:
        if field.number == number:
            return field
    return None

# Constants are fine too
WIRE_INCOMPATIBLE_TYPE_CHANGES: dict[str, list[str]] = {
    "int32": ["string", "bytes", "message"],
    "string": ["int32", "int64", "message", "bytes"],
    "message": ["string", "int32", "enum"],
}
```

### 15.5 Working with the proto evolution context

```python
def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    # --- Iterating deltas (most common pattern) ---
    for delta in ctx.deltas:
        if delta.kind == DeltaKind.field_removed:
            findings.append(fail(
                message = "field {} removed".format(delta.symbol),
                path = delta.path,
            ))

    # --- Filtering deltas with helpers ---
    removed: list[SchemaDelta] = filter_deltas(ctx, kind = DeltaKind.field_removed)
    for delta in removed:
        findings.append(fail(message = "removed: " + delta.symbol, path = delta.path))

    # Shorthand filter functions
    for delta in removed_fields(ctx):
        pass  # ...
    for delta in removed_messages(ctx):
        pass  # ...
    for delta in changed_field_numbers(ctx):
        pass  # ...
    for delta in option_changed_deltas(ctx):
        pass  # ...

    # --- Inspecting descriptors directly ---
    for pair in ctx.files:
        if pair.after == None:
            continue
        after: FileDescriptor = pair.after
        for msg in after.messages:
            for field in msg.fields:
                if field.name.startswith("_"):
                    findings.append(fail_but_overridable(
                        message = "field name {} starts with underscore".format(field.full_name),
                        path = pair.path,
                    ))

    # --- Comparing before/after ---
    for pair in ctx.files:
        if pair.before != None and pair.after != None:
            before_pkg: str = pair.before.package
            after_pkg: str = pair.after.package
            if before_pkg != after_pkg:
                findings.append(fail(
                    message = "package changed from {} to {}".format(before_pkg, after_pkg),
                    path = pair.path,
                ))

    # --- Using delta detail fields ---
    for delta in ctx.deltas:
        if delta.kind == DeltaKind.field_type_changed:
            findings.append(fail(
                message = "field {} type changed: {} -> {}".format(
                    delta.symbol, delta.before_kind, delta.after_kind,
                ),
                path = delta.path,
            ))
        if delta.kind == DeltaKind.method_signature_changed:
            findings.append(fail(
                message = "RPC {} signature changed: ({} -> {}) to ({} -> {})".format(
                    delta.symbol,
                    delta.before_input_type, delta.before_output_type,
                    delta.after_input_type, delta.after_output_type,
                ),
                path = delta.path,
            ))

    # --- Inspecting custom options via extension registries ---
    for pair in ctx.files:
        if pair.after == None:
            continue
        for msg in pair.after.messages:
            for ext in msg.options.extensions:
                if ext.full_name == "acme.deprecated" and has_option(msg.options, "acme.deprecated"):
                    if bool_option(msg.options, "acme.deprecated"):
                        findings.append(fail_but_overridable(
                            message = "message {} is deprecated".format(msg.full_name),
                            path = pair.path,
                        ))

    # --- Accessing check config ---
    severity_override: str = ctx.config.get("severity", "error")
    ignored_packages: list[str] = ctx.config.get("ignored_packages", [])

    return findings
```

### 15.6 Proto check: blocking proto file deletion

```python
# checkleft/proto/no_deletion/check.checkleft

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    """Proto files must never be deleted — they represent a published contract."""
    findings: list[Finding] = []
    for pair in ctx.files:
        if pair.before != None and pair.after == None:
            # File existed at base, gone at current = deleted
            findings.append(fail(
                message = "proto file '{}' was deleted — proto files represent a published wire contract and must not be removed".format(pair.path),
                path = pair.path,
                remediation = "Mark all messages/services as deprecated instead of deleting the file. If this is a rename/move, use the proto/move_detection check alongside this one.",
            ))
    return findings
```

### 15.7 Proto check: detecting moves vs. deletions

A move (rename/relocate) is semantically fine if the package and content remain the same. The adapter gives us `ChangeKind.renamed` in the changeset and `before`/`after` descriptors on file pairs — we can use both to distinguish a real deletion from a harmless move.

```python
# checkleft/proto/move_detection/check.checkleft

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    """Allow proto file moves/renames as long as the package stays the same."""
    findings: list[Finding] = []

    # Build a lookup of all "after" packages across all files.
    # If a file was deleted but its package still exists in another file,
    # it was likely moved.
    after_packages: dict[str, str] = {}  # package -> new file path
    for pair in ctx.files:
        if pair.after != None:
            after_packages[pair.after.package] = pair.path

    for pair in ctx.files:
        if pair.before == None or pair.after != None:
            continue
        # File was deleted (before exists, after is None)
        old_package: str = pair.before.package

        if old_package in after_packages:
            # The package survived in a different file — this is a move.
            new_path: str = after_packages[old_package]

            # Verify the message/service surface is preserved
            old_messages: list[str] = [m.full_name for m in pair.before.messages]
            # Find the new file's pair to inspect its messages
            new_pair_messages: list[str] = []
            for other in ctx.files:
                if other.path == new_path and other.after != None:
                    new_pair_messages = [m.full_name for m in other.after.messages]

            missing: list[str] = [m for m in old_messages if m not in new_pair_messages]
            if missing:
                findings.append(fail(
                    message = "proto file '{}' moved to '{}' but lost messages: {}".format(
                        pair.path, new_path, ", ".join(missing),
                    ),
                    path = pair.path,
                    remediation = "Ensure all messages from the original file exist in the new location.",
                ))
            # else: clean move, no finding
        else:
            # Package is gone entirely — real deletion
            findings.append(fail(
                message = "proto file '{}' was deleted and package '{}' no longer exists anywhere".format(
                    pair.path, old_package,
                ),
                path = pair.path,
                remediation = "Proto files must not be deleted. Deprecate instead, or move to a new path while preserving the package.",
            ))

    return findings
```

### 15.8 Working with the `module.json` evolution context

```python
def check(ctx: ModuleJsonEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    # --- Iterate file pairs ---
    for pair in ctx.files:
        if pair.after == None:
            # File was deleted
            findings.append(fail(
                message = "module.json was deleted",
                path = pair.path,
            ))
            continue

        # The typed model gives direct access to known fields — no generic JSON traversal
        after: ModuleJson = pair.after
        if after.name == "":
            findings.append(fail(
                message = "'name' must not be empty",
                path = pair.path,
            ))

        # Typed dependency access
        for dep_name, dep_version in after.dependencies.items():
            if not dep_version.startswith("^"):
                findings.append(fail_but_overridable(
                    message = "dependency '{}' should use caret version range, got '{}'".format(
                        dep_name, dep_version,
                    ),
                    path = pair.path,
                ))

    # --- Use structured deltas ---
    for delta in ctx.deltas:
        if delta.kind == ModuleJsonDeltaKind.required_key_removed:
            findings.append(fail(
                message = "required key '{}' was removed".format(delta.key),
                path = delta.path,
            ))
        if delta.kind == ModuleJsonDeltaKind.dependency_removed:
            findings.append(fail_but_overridable(
                message = "dependency '{}' was removed (was version '{}')".format(
                    delta.key, delta.before_value,
                ),
                path = delta.path,
            ))
        if delta.kind == ModuleJsonDeltaKind.version_changed:
            findings.append(fail_but_overridable(
                message = "module version changed from '{}' to '{}'".format(
                    delta.before_value, delta.after_value,
                ),
                path = delta.path,
            ))

    return findings
```

### 15.9 Working with the Java evolution context

```python
def check(ctx: JavaEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    # --- Iterate file pairs for direct inspection ---
    for pair in ctx.files:
        if pair.before == None or pair.after == None:
            continue
        for cls in pair.after.classes:
            if cls.visibility == "public":
                # Check that public classes have @since annotation
                has_since: bool = False
                for ann in cls.annotations:
                    if ann.name == "Since":
                        has_since = True
                if not has_since:
                    findings.append(fail_but_overridable(
                        message = "public class {} lacks @Since annotation".format(cls.full_name),
                        path = pair.path,
                    ))

    # --- Use deltas for evolution violations ---
    for delta in ctx.deltas:
        if delta.kind == JavaDeltaKind.method_removed:
            findings.append(fail(
                message = "public method removed: {}".format(delta.symbol),
                path = delta.path,
                remediation = "Mark with @Deprecated(forRemoval=true) for at least one release before removing.",
            ))

        if delta.kind == JavaDeltaKind.visibility_narrowed:
            findings.append(fail(
                message = "visibility narrowed for {}, this is a binary-incompatible change".format(delta.symbol),
                path = delta.path,
            ))

        if delta.kind == JavaDeltaKind.superclass_changed:
            findings.append(fail_but_overridable(
                message = "superclass changed for {}".format(delta.symbol),
                path = delta.path,
            ))

        if delta.kind == JavaDeltaKind.interface_removed:
            findings.append(fail(
                message = "interface removed from {}: downstream casts will break".format(delta.symbol),
                path = delta.path,
            ))

    return findings
```

### 15.10 Working with the text adapter (generic checks)

```python
def check(ctx: TextEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    for pair in ctx.files:
        if pair.after == None:
            continue

        # Scan added lines for patterns (only lines introduced in this change)
        for line in pair.added_lines:
            if regex_match(r"TODO\(nobody\)", line.text):
                findings.append(fail_but_overridable(
                    message = "TODO assigned to 'nobody' — assign to a real owner",
                    path = pair.path,
                    line = line.number,
                ))

            if regex_match(r"(?i)password\s*=\s*['\"]", line.text):
                findings.append(fail(
                    message = "possible hardcoded password",
                    path = pair.path,
                    line = line.number,
                ))

        # Full-file model checks (parsed line model, not raw string)
        after: TextFile = pair.after
        if after.line_count > 1000:
            findings.append(fail_but_overridable(
                message = "file exceeds 1000 lines ({})".format(after.line_count),
                path = pair.path,
            ))

    return findings
```

### 15.11 Writing a fix function

```python
# proto/evolution/fix.checkleft

load(":check", "RESERVED_PATTERN")  # can load from own check file

def fix(ctx: ProtoEvolutionContext, findings: list[Finding]) -> list[FileEdit]:
    """Generate file edits to auto-fix findings where possible."""
    edits: list[FileEdit] = []

    for f in findings:
        # Only fix findings we know how to handle
        if "must be reserved" not in f.message:
            continue

        # Find the corresponding delta for context
        for delta in ctx.deltas:
            if delta.kind == DeltaKind.field_removed and delta.path == f.path and delta.symbol in f.message:
                # Read the current file to find insertion point
                for pair in ctx.files:
                    if pair.path == f.path and pair.after != None:
                        for msg in pair.after.messages:
                            # Insert reserved statement after the last field
                            last_field_line: int = 0
                            for field in msg.fields:
                                if field.number > 0:
                                    last_field_line = max(last_field_line, field.number)
                            edits.append(file_edit(
                                path = f.path,
                                old_text = "",
                                new_text = "  reserved {};\n  reserved \"{}\";\n".format(
                                    delta.before_number,
                                    delta.symbol.split(".")[-1],
                                ),
                                after_line = last_field_line,
                            ))
    return edits
```

### 15.12 Package manifest with a version set

```toml
# checkleft/package.toml
# Instead of pinning 5+ individual check packages, depend on one version set.

[package]
name = "acme/payments"
version = "1.0.0"

# One version set gives us proto, module_json, java, and security checks.
# All public checks from every included package are automatically active.
[version_sets.acme-versionset]
source = "registry://checkleft-hub/acme-versionset"
version = "2025.06.1"

# Override one package from the version set to use a newer version
[dependencies.proto_evolution]
source = "registry://checkleft-hub/proto-evolution"
version = "0.3.0"   # newer than what the version set pins

# Add a package not in the version set at all
[dependencies.custom_team_checks]
source = "git://github.com/myteam/checkleft-checks.git"
version = "0.3.0"

# Local checks (checkleft/proto/evolution/, etc.) are auto-discovered.
# package.toml is purely for external deps.
```

### 15.13 Network tier: checking against a remote registry

```python
# checkleft/proto/registry_sync/check.checkleft
# Tier: network (declared in check_meta below)

def check(ctx: ProtoEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []
    registry_url: str = ctx.config["registry_url"]

    for pair in ctx.files:
        if pair.after == None:
            continue

        # Fetch the registered schema version from the remote registry
        resp: HttpResponse = http_get(
            "{}/schemas/{}".format(registry_url, pair.after.package),
            headers = {"Accept": "application/json"},
            timeout_ms = 5000,
        )

        if resp.status == 404:
            # New schema, not yet registered — that's fine
            continue

        if resp.status != 200:
            findings.append(fail(
                message = "failed to fetch schema registry for {}: HTTP {}".format(
                    pair.after.package, resp.status,
                ),
                path = pair.path,
            ))
            continue

        registered: dict[str, typing.Any] = json_decode(resp.body)
        registered_version: int = registered.get("version", 0)

        # Check that we're not regressing below the registered version
        for msg in pair.after.messages:
            for field in msg.fields:
                if field.number > registered.get("max_field_number", 0):
                    findings.append(fail_but_overridable(
                        message = "field {} extends beyond registered schema v{}".format(
                            field.full_name, registered_version,
                        ),
                        path = pair.path,
                    ))

    return findings
```

### 15.14 Using `regex_match` and `glob_match` utilities

```python
def check(ctx: TextEvolutionContext) -> list[Finding]:
    findings: list[Finding] = []

    for pair in ctx.files:
        if pair.after == None:
            continue

        # glob_match for path-based filtering within the check
        if glob_match("**/generated/**", pair.path):
            continue  # skip generated files

        if glob_match("*_test.java", pair.path):
            continue  # skip test files

        for line in pair.added_lines:
            # regex_match returns bool
            if regex_match(r"System\.exit\(\d+\)", line.text):
                findings.append(fail(
                    message = "System.exit() call in non-test code",
                    path = pair.path,
                    line = line.number,
                ))

            # regex_find_all returns list[str] of all matches
            todos: list[str] = regex_find_all(r"TODO\((\w+)\)", line.text)
            for owner in todos:
                if owner == "fixme" or owner == "hack":
                    findings.append(fail_but_overridable(
                        message = "TODO assigned to '{}' — use a real username".format(owner),
                        path = pair.path,
                        line = line.number,
                    ))

    return findings
```

### 15.15 Using `json_decode` / `json_encode` in text-adapter checks

The `json_decode` / `json_encode` built-ins are available for text-adapter checks that need to work with JSON content from the parsed line model. They are utility functions, not a replacement for typed adapters.

```python
def check(ctx: TextEvolutionContext) -> list[Finding]:
    """Validate that tsconfig.json files have strict mode enabled."""
    findings: list[Finding] = []

    for pair in ctx.files:
        if pair.after == None:
            continue
        if not glob_match("**/tsconfig.json", pair.path):
            continue

        # Reconstruct the file content from the line model
        content: str = "\n".join([line.text for line in pair.after.lines])
        parsed: dict[str, typing.Any] = json_decode(content)

        compiler_opts: dict[str, typing.Any] = parsed.get("compilerOptions", {})
        if not compiler_opts.get("strict", False):
            findings.append(fail_but_overridable(
                message = "tsconfig.json must have compilerOptions.strict = true",
                path = pair.path,
            ))

    return findings
```

---

## 16. Summary of conventions

| Convention | Rule |
|---|---|
| File extension | `.checkleft` always |
| Check location | `checkleft/<category>/<name>/check.checkleft` |
| Fix location | `checkleft/<category>/<name>/fix.checkleft` |
| Shared code | `checkleft/lib/*.checkleft` |
| Check-local helpers | `checkleft/<category>/<name>/<anything>.checkleft` (not `check` or `fix`) |
| Package manifest | `checkleft/package.toml` |
| Lockfile | `checkleft/PACKAGE.lock` (auto-generated, checked in) |
| Check ID | `<category>/<name>` (e.g. `proto/evolution`) |
| Type annotations | Required on all function signatures |
| Default sandbox | `hermetic` |
| Adapter linkage | `<category>` folder name matches `FormatAdapter::kind()` |
| Rust check override | `source: "rust://..."` in `check()` call |
